//! `rock sim-report` -- a read-only report over one retained `rock sim` run
//! written by `sim_observability::SimRunRecorder` to
//! `.sim-runs/<run-id>/` (`ROCK_SIM_LOG_DIR` overrides the root).
//!
//! Interpretation rules:
//!
//! * `worker_started` -> `worker_first_entry` is **startup latency**.
//! * A worker may be reaped before its reader drains already-buffered lines,
//!   so `entry_*` events can legitimately follow that worker's `worker_end`.
//!   Worker spans are widened over that drain rather than assuming an order,
//!   and the overhang is reported on its own line so the child-process
//!   lifetime stays recoverable.
//! * `PASS` / `FAIL` / `INCOMPLETE` come from `run_end`. A record with no
//!   `run_end` is an incomplete invocation and has no authoritative wall time.
//!
//! The reader is strictly read-only over the history: it never writes, prunes
//! or mutates a run record. History is deliberately never auto-pruned.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// The only schema this reader knows how to interpret.
const SCHEMA_VERSION: i64 = 1;
const DEFAULT_TOP: usize = 20;

// ---------------------------------------------------------------------------
// Locating runs
// ---------------------------------------------------------------------------

/// Run ids are `<unix-ms>-<pid>-<counter>`. Sorting on the parsed parts keeps
/// "most recent" correct when the millisecond field changes width; ids that do
/// not parse sort last by name so a hand-copied directory still resolves.
fn run_sort_key(id: &str) -> (u8, u128, u64, u64, String) {
    let mut parts = id.splitn(3, '-');
    let ms = parts.next().and_then(|p| p.parse::<u128>().ok());
    let pid = parts.next().and_then(|p| p.parse::<u64>().ok());
    let counter = parts.next().and_then(|p| p.parse::<u64>().ok());
    match (ms, pid, counter) {
        (Some(ms), Some(pid), Some(counter)) => (0, ms, pid, counter, String::new()),
        _ => (1, 0, 0, 0, id.to_string()),
    }
}

/// Every retained run under `root`, oldest first. A directory without an
/// `events.jsonl` is not a run record and is skipped.
pub(crate) fn list_runs(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let entries = std::fs::read_dir(root)
        .with_context(|| format!("reading the sim history root `{}`", root.display()))?;
    let mut runs = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("listing `{}`", root.display()))?;
        let dir = entry.path();
        if !dir.join("events.jsonl").is_file() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        runs.push((id, dir));
    }
    runs.sort_by_key(|left| run_sort_key(&left.0));
    Ok(runs)
}

/// Resolve `run_id` under `root`. With no id, the most recent retained run.
/// An id that is itself a path to a run directory is honored so a record
/// copied out of the history still reads.
fn resolve_run(root: &Path, run_id: Option<&str>) -> Result<(String, PathBuf)> {
    let runs = list_runs(root)?;
    let Some(id) = run_id else {
        return runs.into_iter().next_back().ok_or_else(|| {
            anyhow::anyhow!(
                "no retained sim runs under `{}` -- run `rock sim` first, or point \
                 --sim-log-dir at the history root",
                root.display()
            )
        });
    };
    if let Some(found) = runs.iter().find(|(name, _)| name == id) {
        return Ok(found.clone());
    }
    let direct = Path::new(id);
    if direct.join("events.jsonl").is_file() {
        let name = direct
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| id.to_string());
        return Ok((name, direct.to_path_buf()));
    }
    bail!(
        "no run `{id}` under `{}` ({} retained run(s)); pass a run id, a run \
         directory, or nothing for the most recent",
        root.display(),
        runs.len()
    )
}

// ---------------------------------------------------------------------------
// Reading one record
// ---------------------------------------------------------------------------

/// One parsed run record. `skipped_records` counts lines this reader declined
/// to interpret (unparsable, or a `schema_version` it does not know); they are
/// surfaced rather than silently dropped.
pub(crate) struct RunRecord {
    pub run_id: String,
    pub dir: PathBuf,
    events: Vec<Value>,
    skipped_records: usize,
    timings: Option<Value>,
}

impl RunRecord {
    pub(crate) fn load(run_id: &str, dir: &Path) -> Result<Self> {
        let path = dir.join("events.jsonl");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading `{}`", path.display()))?;
        let mut events = Vec::new();
        let mut skipped = 0usize;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(value) => {
                    let version = value.get("schema_version").and_then(Value::as_i64);
                    if version != Some(SCHEMA_VERSION) || value.get("event").is_none() {
                        skipped += 1;
                        continue;
                    }
                    events.push(value);
                }
                Err(_) => skipped += 1,
            }
        }
        // `timings.json` is optional: it postdates part of the retained
        // history, and an interrupted invocation never writes it.
        let timings_path = dir.join("timings.json");
        let timings = match std::fs::read_to_string(&timings_path) {
            Ok(text) => serde_json::from_str::<Value>(&text).ok(),
            Err(_) => None,
        };
        Ok(Self {
            run_id: run_id.to_string(),
            dir: dir.to_path_buf(),
            events,
            skipped_records: skipped,
            timings,
        })
    }

    fn of(&self, event: &str) -> impl Iterator<Item = &Value> {
        self.events
            .iter()
            .filter(move |value| value.get("event").and_then(Value::as_str) == Some(event))
    }

    fn first(&self, event: &str) -> Option<&Value> {
        self.of(event).next()
    }
}

fn ms(value: &Value) -> i64 {
    value.get("elapsed_ms").and_then(Value::as_i64).unwrap_or(0)
}

fn int(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn pct(part: i64, whole: i64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        round1(part as f64 * 100.0 / whole as f64)
    }
}

fn secs(value: i64) -> String {
    format!("{:.1} s", value as f64 / 1000.0)
}

/// The upper median of a sorted slice (`v[len / 2]`). Chosen so an even-length
/// sample reports a value that actually occurred rather than an interpolated
/// one; the author's hand analysis of the batch-9 game run used the same rule.
fn upper_median(sorted: &[i64]) -> Option<i64> {
    (!sorted.is_empty()).then(|| sorted[sorted.len() / 2])
}

// ---------------------------------------------------------------------------
// Derived views
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Envelope {
    name: String,
    occurrence_id: i64,
    worker_attempt: i64,
    start_ms: i64,
    end_ms: i64,
    frame_count: Option<i64>,
    frame_timing: Option<FrameTiming>,
    frame_timing_invalid: bool,
}

impl Envelope {
    fn duration_ms(&self) -> i64 {
        self.end_ms - self.start_ms
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct FrameTiming {
    frames: i64,
    draw_frames: i64,
    submitted_frames: i64,
    logic_us: i64,
    render_cpu_us: i64,
    present_us: i64,
}

impl FrameTiming {
    fn parse(value: Option<&Value>, frame_count: Option<i64>) -> (Option<Self>, bool) {
        let object = match value {
            None | Some(Value::Null) => return (None, false),
            Some(value @ Value::Object(_)) => value,
            Some(_) => return (None, true),
        };
        let Ok(timing) = Self::deserialize(object) else {
            return (None, true);
        };
        if [
            timing.frames,
            timing.draw_frames,
            timing.submitted_frames,
            timing.logic_us,
            timing.render_cpu_us,
            timing.present_us,
        ]
        .iter()
        .any(|value| *value < 0)
            || frame_count.is_some_and(|count| count != timing.frames)
            || timing.draw_frames > timing.frames
            || timing.submitted_frames > timing.draw_frames
        {
            return (None, true);
        }
        (Some(timing), false)
    }
}

#[derive(Default)]
struct FrameTimingStats {
    known: usize,
    missing: usize,
    invalid: usize,
    frames: i64,
    draw_frames: i64,
    submitted_frames: i64,
    logic_us: i64,
    render_cpu_us: i64,
    present_us: i64,
}

impl FrameTimingStats {
    fn from_envelopes(envelopes: &[Envelope]) -> Self {
        let mut stats = Self::default();
        for envelope in envelopes {
            if envelope.frame_timing_invalid {
                stats.invalid += 1;
            } else if let Some(timing) = &envelope.frame_timing {
                stats.known += 1;
                stats.frames = stats.frames.saturating_add(timing.frames);
                stats.draw_frames = stats.draw_frames.saturating_add(timing.draw_frames);
                stats.submitted_frames = stats
                    .submitted_frames
                    .saturating_add(timing.submitted_frames);
                stats.logic_us = stats.logic_us.saturating_add(timing.logic_us);
                stats.render_cpu_us = stats.render_cpu_us.saturating_add(timing.render_cpu_us);
                stats.present_us = stats.present_us.saturating_add(timing.present_us);
            } else {
                stats.missing += 1;
            }
        }
        stats
    }
}

struct FrameCountStats {
    known: usize,
    missing: usize,
    total: i64,
    min: Option<i64>,
    median: Option<i64>,
    max: Option<i64>,
}

impl FrameCountStats {
    fn from_envelopes(envelopes: &[Envelope]) -> Self {
        let mut counts: Vec<i64> = envelopes.iter().filter_map(|e| e.frame_count).collect();
        counts.sort_unstable();
        Self {
            known: counts.len(),
            missing: envelopes.len().saturating_sub(counts.len()),
            total: counts.iter().copied().sum(),
            min: counts.first().copied(),
            median: upper_median(&counts),
            max: counts.last().copied(),
        }
    }
}

/// How many logical entries a lane was planned to carry.
struct Worker {
    attempt: i64,
    started_ms: i64,
    /// `worker_end`, the child lifetime boundary. Absent when the invocation
    /// ended before the child was reaped.
    ended_ms: Option<i64>,
    /// End of the observed span: the reap, widened over any entry line the
    /// reader drained afterwards.
    span_end_ms: i64,
    startup_ms: Option<i64>,
    first_entry_name: Option<String>,
    entries: usize,
    work_ms: i64,
}

/// Time spent at each simultaneous-live-worker count, plus the ramp and tail
/// against the effective job count.
struct Occupancy {
    histogram: BTreeMap<i64, i64>,
    mean_live: f64,
    ramp_to_full_ms: Option<i64>,
    under_occupied_tail_ms: i64,
    /// Idle before the first worker and after the last one is reaped. Split
    /// out because the 0-live histogram bucket lumps them, and a hand analysis
    /// that divides by "wall minus the trailing idle" instead of wall reports
    /// a different saturation percentage from the same records.
    idle_before_first_worker_ms: i64,
    idle_after_last_worker_ms: i64,
    base_ms: i64,
}

fn occupancy(workers: &[Worker], base_ms: i64, jobs: i64) -> Occupancy {
    let mut deltas: Vec<(i64, i64)> = Vec::with_capacity(workers.len() * 2);
    for worker in workers {
        let start = worker.started_ms.clamp(0, base_ms);
        let end = worker.span_end_ms.clamp(start, base_ms);
        deltas.push((start, 1));
        deltas.push((end, -1));
    }
    deltas.sort_unstable();

    let mut histogram: BTreeMap<i64, i64> = BTreeMap::new();
    let mut live = 0i64;
    let mut previous = 0i64;
    let mut ramp = None;
    let mut last_full_end = None;
    for (at, delta) in deltas {
        if at > previous {
            *histogram.entry(live).or_insert(0) += at - previous;
            if jobs > 0 && live >= jobs {
                last_full_end = Some(at);
            }
            previous = at;
        }
        live += delta;
        if jobs > 0 && live >= jobs && ramp.is_none() {
            ramp = Some(at);
        }
    }
    if base_ms > previous {
        *histogram.entry(live).or_insert(0) += base_ms - previous;
        if jobs > 0 && live >= jobs {
            last_full_end = Some(base_ms);
        }
    }

    let weighted: i64 = histogram.iter().map(|(count, span)| count * span).sum();
    let first_start = workers.iter().map(|w| w.started_ms).min().unwrap_or(0);
    let last_end = workers.iter().map(|w| w.span_end_ms).max().unwrap_or(0);
    Occupancy {
        idle_before_first_worker_ms: first_start.clamp(0, base_ms),
        idle_after_last_worker_ms: (base_ms - last_end).max(0),
        histogram,
        mean_live: if base_ms == 0 {
            0.0
        } else {
            round2(weighted as f64 / base_ms as f64)
        },
        ramp_to_full_ms: ramp,
        // With no saturated interval the whole record is the tail.
        under_occupied_tail_ms: base_ms - last_full_end.unwrap_or(0),
        base_ms,
    }
}

/// Everything one run's records say, already reduced. Built once and rendered
/// either as a table or as JSON.
pub(crate) struct SimRunAnalysis {
    run_id: String,
    dir: PathBuf,
    skipped_records: usize,

    // Header.
    status: String,
    complete: bool,
    run_end_present: bool,
    wall_ms: Option<i64>,
    observed_span_ms: i64,
    planned_entries: i64,
    finalized_entries: i64,
    verdicts: BTreeMap<String, i64>,
    error: Option<String>,
    project: Option<String>,
    selection: Option<String>,
    revision: Option<String>,
    dirty: Option<bool>,
    requested_jobs: Option<i64>,
    effective_jobs: Option<i64>,

    admissions: Vec<AdmissionTiming>,
    invalid_admissions: usize,

    // Time decomposition.
    envelopes: Vec<Envelope>,
    workers: Vec<Worker>,
    entries_started_without_end: usize,
    entries_ended_without_start: usize,
    workers_without_end: usize,
    child_lifetime_total_ms: i64,
    drain_overhang_ms: i64,
    worker_span_total_ms: i64,
    inside_envelopes_ms: i64,
    startup_latency_ms: i64,
    startup_boots: usize,
    startup_median_ms: Option<i64>,
    remainder_ms: i64,

    occupancy: Occupancy,
    frame_counts: FrameCountStats,
    frame_timing: FrameTimingStats,
    timings: Option<TimingsView>,
}

struct AdmissionTiming {
    name: String,
    admission_ms: u64,
    checks_ms: u64,
    wait_ms_by_reason: BTreeMap<String, u64>,
}

struct TimingsView {
    entries: usize,
    passed: usize,
    completed: usize,
    accepted: bool,
    duration_total_ms: i64,
    envelope_mismatches: usize,
    missing_from_events: usize,
}

pub(crate) fn analyze(record: &RunRecord) -> SimRunAnalysis {
    let observed_span_ms = record.events.iter().map(ms).max().unwrap_or(0);

    // Header ---------------------------------------------------------------
    let run_end = record.first("run_end");
    let run_start = record.first("run_start");
    let run_config = record.first("run_config");

    let status = run_end
        .and_then(|value| text(value, "status"))
        .unwrap_or("INCOMPLETE")
        .to_string();
    let complete = run_end
        .and_then(|value| value.get("complete").and_then(Value::as_bool))
        .unwrap_or(false);
    let wall_ms = run_end.map(ms);
    let planned_entries = run_end
        .and_then(|value| int(value, "planned_entries"))
        .unwrap_or_else(|| record.of("entry_planned").count() as i64);
    let finalized_entries = run_end
        .and_then(|value| int(value, "finalized_entries"))
        .unwrap_or_else(|| record.of("entry_result").count() as i64);

    let source = run_start.and_then(|value| value.get("source"));
    let available = source.and_then(|s| text(s, "status")) == Some("available");
    let revision = available
        .then(|| source.and_then(|s| text(s, "revision")).map(str::to_string))
        .flatten();
    let dirty = available
        .then(|| source.and_then(|s| s.get("dirty").and_then(Value::as_bool)))
        .flatten();

    let mut verdicts: BTreeMap<String, i64> = BTreeMap::new();
    for result in record.of("entry_result") {
        if let Some(verdict) = text(result, "verdict") {
            *verdicts.entry(verdict.to_string()).or_insert(0) += 1;
        }
    }

    let mut admissions = Vec::new();
    let mut invalid_admissions = 0;
    for event in record.of("worker_admission") {
        let parsed = (|| {
            Some(AdmissionTiming {
                name: event.get("name")?.as_str()?.to_owned(),
                admission_ms: event.get("admission_ms")?.as_u64()?,
                checks_ms: event.get("checks_ms")?.as_u64()?,
                wait_ms_by_reason: serde_json::from_value(event.get("wait_ms_by_reason")?.clone())
                    .ok()?,
            })
        })();
        match parsed {
            Some(timing) => admissions.push(timing),
            None => invalid_admissions += 1,
        }
    }
    admissions.sort_by_key(|row| std::cmp::Reverse(row.admission_ms));

    // Envelopes ------------------------------------------------------------
    // Keyed by occurrence + worker attempt, which is what makes a repeated
    // name identifiable. A duplicate start keeps the first.
    let mut open: HashMap<(i64, i64), &Value> = HashMap::new();
    for start in record.of("entry_start") {
        let key = (
            int(start, "occurrence_id").unwrap_or(-1),
            int(start, "worker_attempt").unwrap_or(-1),
        );
        open.entry(key).or_insert(start);
    }
    let mut envelopes = Vec::new();
    let mut closed = HashSet::new();
    let mut entries_ended_without_start = 0usize;
    for end in record.of("entry_end") {
        let key = (
            int(end, "occurrence_id").unwrap_or(-1),
            int(end, "worker_attempt").unwrap_or(-1),
        );
        let Some(start) = open.get(&key) else {
            entries_ended_without_start += 1;
            continue;
        };
        if !closed.insert(key) {
            continue;
        }
        let frame_count = int(end, "frame_count");
        let (frame_timing, frame_timing_invalid) =
            FrameTiming::parse(end.get("frame_timing"), frame_count);
        envelopes.push(Envelope {
            name: text(end, "name")
                .or_else(|| text(start, "name"))
                .unwrap_or("<unnamed>")
                .to_string(),
            occurrence_id: key.0,
            worker_attempt: key.1,
            start_ms: ms(start),
            end_ms: ms(end),
            frame_count,
            frame_timing,
            frame_timing_invalid,
        });
    }
    envelopes.sort_by_key(|envelope| (envelope.start_ms, envelope.occurrence_id));
    let entries_started_without_end = open.len() - closed.len();

    // Workers --------------------------------------------------------------
    let mut started: BTreeMap<i64, i64> = BTreeMap::new();
    for event in record.of("worker_started") {
        if let Some(attempt) = int(event, "worker_attempt") {
            started.entry(attempt).or_insert_with(|| ms(event));
        }
    }
    let mut ended: HashMap<i64, i64> = HashMap::new();
    for event in record.of("worker_end") {
        if let Some(attempt) = int(event, "worker_attempt") {
            ended.entry(attempt).or_insert_with(|| ms(event));
        }
    }
    let mut first_entry: HashMap<i64, (i64, String)> = HashMap::new();
    for event in record.of("worker_first_entry") {
        if let Some(attempt) = int(event, "worker_attempt") {
            first_entry.entry(attempt).or_insert_with(|| {
                (
                    ms(event),
                    text(event, "name").unwrap_or("<unnamed>").to_string(),
                )
            });
        }
    }
    // The documented drain ordering: buffered entry lines can arrive after the
    // reap, so the observed span covers them.
    let mut last_entry_event: HashMap<i64, i64> = HashMap::new();
    for event in record.events.iter().filter(|value| {
        matches!(
            value.get("event").and_then(Value::as_str),
            Some("entry_start") | Some("entry_end")
        )
    }) {
        if let Some(attempt) = int(event, "worker_attempt") {
            let at = ms(event);
            let slot = last_entry_event.entry(attempt).or_insert(at);
            *slot = (*slot).max(at);
        }
    }

    let mut per_worker_entries: HashMap<i64, (usize, i64)> = HashMap::new();
    for envelope in &envelopes {
        let slot = per_worker_entries
            .entry(envelope.worker_attempt)
            .or_insert((0, 0));
        slot.0 += 1;
        slot.1 += envelope.duration_ms();
    }

    let mut workers = Vec::new();
    for (attempt, started_ms) in &started {
        let ended_ms = ended.get(attempt).copied();
        // An unreaped worker was still open when recording stopped; its span
        // runs to the last thing the record observed, never past it.
        let base_end = ended_ms.unwrap_or(observed_span_ms);
        let span_end_ms = base_end
            .max(last_entry_event.get(attempt).copied().unwrap_or(base_end))
            .max(*started_ms);
        let (entries, work_ms) = per_worker_entries.get(attempt).copied().unwrap_or((0, 0));
        workers.push(Worker {
            attempt: *attempt,
            started_ms: *started_ms,
            ended_ms,
            span_end_ms,
            startup_ms: first_entry.get(attempt).map(|(at, _)| at - started_ms),
            first_entry_name: first_entry.get(attempt).map(|(_, name)| name.clone()),
            entries,
            work_ms,
        });
    }

    let workers_without_end = workers.iter().filter(|w| w.ended_ms.is_none()).count();
    let child_lifetime_total_ms: i64 = workers
        .iter()
        .filter_map(|w| w.ended_ms.map(|end| end - w.started_ms))
        .sum();
    let drain_overhang_ms: i64 = workers
        .iter()
        .filter_map(|w| w.ended_ms.map(|end| (w.span_end_ms - end).max(0)))
        .sum();
    let worker_span_total_ms: i64 = workers.iter().map(|w| w.span_end_ms - w.started_ms).sum();
    let inside_envelopes_ms: i64 = envelopes.iter().map(Envelope::duration_ms).sum();
    let mut startups: Vec<i64> = workers.iter().filter_map(|w| w.startup_ms).collect();
    startups.sort_unstable();
    let startup_latency_ms: i64 = startups.iter().sum();
    let remainder_ms = worker_span_total_ms - inside_envelopes_ms - startup_latency_ms;

    // Occupancy ------------------------------------------------------------
    let requested_jobs = run_config
        .and_then(|value| int(value, "requested_jobs"))
        .or_else(|| run_start.and_then(|value| int(value, "requested_jobs")));
    let effective_jobs = run_config.and_then(|value| int(value, "effective_jobs"));
    let base_ms = wall_ms.unwrap_or(observed_span_ms);
    let jobs = effective_jobs.or(requested_jobs).unwrap_or(0);
    let occupancy = occupancy(&workers, base_ms, jobs);
    let frame_counts = FrameCountStats::from_envelopes(&envelopes);
    let frame_timing = FrameTimingStats::from_envelopes(&envelopes);

    // timings.json ---------------------------------------------------------
    let envelope_by_key: HashMap<i64, i64> = envelopes
        .iter()
        .map(|envelope| (envelope.occurrence_id, envelope.duration_ms()))
        .collect();
    let timings = record.timings.as_ref().map(|value| {
        let rows = value
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut passed = 0usize;
        let mut completed = 0usize;
        let mut duration_total_ms = 0i64;
        let mut envelope_mismatches = 0usize;
        let mut missing_from_events = 0usize;
        for row in &rows {
            if row.get("passed").and_then(Value::as_bool) == Some(true) {
                passed += 1;
            }
            if row.get("completed").and_then(Value::as_bool) == Some(true) {
                completed += 1;
            }
            let duration = int(row, "duration_ms").unwrap_or(0);
            duration_total_ms += duration;
            let key = int(row, "occurrence_id").unwrap_or(-1);
            match envelope_by_key.get(&key) {
                Some(measured) if *measured == duration => {}
                Some(_) => envelope_mismatches += 1,
                None => missing_from_events += 1,
            }
        }
        TimingsView {
            entries: rows.len(),
            passed,
            completed,
            accepted: value
                .get("accepted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            duration_total_ms,
            envelope_mismatches,
            missing_from_events,
        }
    });

    SimRunAnalysis {
        run_id: record.run_id.clone(),
        dir: record.dir.clone(),
        skipped_records: record.skipped_records,
        status,
        complete,
        run_end_present: run_end.is_some(),
        wall_ms,
        observed_span_ms,
        planned_entries,
        finalized_entries,
        verdicts,
        error: run_end
            .and_then(|value| text(value, "error"))
            .map(str::to_string),
        project: run_config
            .and_then(|value| text(value, "project_path"))
            .or_else(|| run_start.and_then(|value| text(value, "project_arg")))
            .map(str::to_string),
        selection: run_start
            .and_then(|value| text(value, "selection"))
            .map(str::to_string),
        revision,
        dirty,
        requested_jobs,
        effective_jobs,
        entries_started_without_end,
        entries_ended_without_start,
        workers_without_end,
        child_lifetime_total_ms,
        drain_overhang_ms,
        worker_span_total_ms,
        inside_envelopes_ms,
        admissions,
        invalid_admissions,
        startup_latency_ms,
        startup_boots: startups.len(),
        startup_median_ms: upper_median(&startups),
        remainder_ms,
        occupancy,
        frame_counts,
        frame_timing,
        timings,
        envelopes,
        workers,
    }
}

impl SimRunAnalysis {
    fn slowest(&self) -> Vec<&Envelope> {
        let mut ranked: Vec<&Envelope> = self.envelopes.iter().collect();
        ranked.sort_by(|left, right| {
            right
                .duration_ms()
                .cmp(&left.duration_ms())
                .then_with(|| left.name.cmp(&right.name))
        });
        ranked.truncate(DEFAULT_TOP);
        ranked
    }

    /// Workers whose boot dominates the work they did. Restricted to workers
    /// that actually ran an entry, so the ratio has a denominator.
    fn worst_boot_to_work(&self) -> Vec<(&Worker, f64)> {
        let mut ranked: Vec<(&Worker, f64)> = self
            .workers
            .iter()
            .filter_map(|worker| {
                let boot = worker.startup_ms?;
                (worker.work_ms > 0).then(|| (worker, boot as f64 / worker.work_ms as f64))
            })
            .collect();
        ranked.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| right.0.startup_ms.cmp(&left.0.startup_ms))
                .then_with(|| left.0.attempt.cmp(&right.0.attempt))
        });
        ranked.truncate(DEFAULT_TOP);
        ranked
    }

    fn ideal_ms_at_effective_jobs(&self) -> Option<i64> {
        let jobs = self.effective_jobs.or(self.requested_jobs)?;
        (jobs > 0).then(|| self.inside_envelopes_ms / jobs)
    }

    pub(crate) fn render_json(&self) -> Result<String> {
        let run = json!({
            "run_id": self.run_id,
            "directory": self.dir.to_string_lossy(),
            "status": self.status,
            "complete": self.complete,
            "run_end_present": self.run_end_present,
            "wall_ms": self.wall_ms,
            "observed_span_ms": self.observed_span_ms,
            "planned_entries": self.planned_entries,
            "finalized_entries": self.finalized_entries,
            "verdicts": self.verdicts,
            "error": self.error,
            "project": self.project,
            "selection": self.selection,
            "revision": self.revision,
            "dirty": self.dirty,
            "requested_jobs": self.requested_jobs,
            "effective_jobs": self.effective_jobs,
            "boots": self.workers.len(),
            "skipped_records": self.skipped_records,
        });

        let time = json!({
            "entries_with_envelope": self.envelopes.len(),
            "entries_started_without_end": self.entries_started_without_end,
            "entries_ended_without_start": self.entries_ended_without_start,
            "envelope_total_ms": self.inside_envelopes_ms,
            "workers_without_end": self.workers_without_end,
            "child_lifetime_total_ms": self.child_lifetime_total_ms,
            "drain_overhang_ms": self.drain_overhang_ms,
            "worker_span_total_ms": self.worker_span_total_ms,
            "inside_envelopes_ms": self.inside_envelopes_ms,
            "inside_envelopes_pct": pct(self.inside_envelopes_ms, self.worker_span_total_ms),
            "startup_latency_ms": self.startup_latency_ms,
            "startup_latency_pct": pct(self.startup_latency_ms, self.worker_span_total_ms),
            "startup_boots": self.startup_boots,
            "startup_median_ms": self.startup_median_ms,
            "remainder_ms": self.remainder_ms,
            "remainder_pct": pct(self.remainder_ms, self.worker_span_total_ms),
            "wall_ms": self.wall_ms,
            "ideal_ms_at_effective_jobs": self.ideal_ms_at_effective_jobs(),
            "frame_counts": {
                "known_entries": self.frame_counts.known,
                "missing_entries": self.frame_counts.missing,
                "total": self.frame_counts.total,
                "min": self.frame_counts.min,
                "median": self.frame_counts.median,
                "max": self.frame_counts.max,
            },
            "frame_timing": {
                "known_entries": self.frame_timing.known,
                "missing_entries": self.frame_timing.missing,
                "invalid_entries": self.frame_timing.invalid,
                "frames": self.frame_timing.frames,
                "draw_frames": self.frame_timing.draw_frames,
                "submitted_frames": self.frame_timing.submitted_frames,
                "logic_us": self.frame_timing.logic_us,
                "render_cpu_us": self.frame_timing.render_cpu_us,
                "present_us": self.frame_timing.present_us,
            },
        });

        let slowest: Vec<Value> = self
            .slowest()
            .into_iter()
            .map(|envelope| {
                json!({
                    "name": envelope.name,
                    "occurrence_id": envelope.occurrence_id,
                    "worker_attempt": envelope.worker_attempt,
                    "start_ms": envelope.start_ms,
                    "end_ms": envelope.end_ms,
                    "duration_ms": envelope.duration_ms(),
                    "frame_count": envelope.frame_count,
                    "frame_timing": envelope.frame_timing,
                })
            })
            .collect();

        let worst: Vec<Value> = self
            .worst_boot_to_work()
            .into_iter()
            .map(|(worker, ratio)| {
                json!({
                    "name": worker.first_entry_name,
                    "worker_attempt": worker.attempt,
                    "entries": worker.entries,
                    "boot_ms": worker.startup_ms,
                    "work_ms": worker.work_ms,
                    "ratio": round2(ratio),
                })
            })
            .collect();

        let boot = json!({
            "workers": self.workers.len(),
            "worst_boot_to_work": worst,
        });

        let histogram: Vec<Value> = self
            .occupancy
            .histogram
            .iter()
            .map(|(live, span)| {
                json!({
                    "live_workers": live,
                    "ms": span,
                    "pct": pct(*span, self.occupancy.base_ms),
                })
            })
            .collect();
        let occupancy = json!({
            "wall_ms": self.wall_ms,
            "base_ms": self.occupancy.base_ms,
            "mean_live_workers": self.occupancy.mean_live,
            "histogram": histogram,
            "ramp_to_full_ms": self.occupancy.ramp_to_full_ms,
            "under_occupied_tail_ms": self.occupancy.under_occupied_tail_ms,
            "idle_before_first_worker_ms": self.occupancy.idle_before_first_worker_ms,
            "idle_after_last_worker_ms": self.occupancy.idle_after_last_worker_ms,
        });

        let timings = match &self.timings {
            Some(view) => json!({
                "present": true,
                "entries": view.entries,
                "passed": view.passed,
                "completed": view.completed,
                "accepted": view.accepted,
                "duration_total_ms": view.duration_total_ms,
                "envelope_mismatches": view.envelope_mismatches,
                "missing_from_events": view.missing_from_events,
            }),
            None => json!({ "present": false }),
        };

        Ok(serde_json::to_string_pretty(&json!({
            "mode": "run",
            "run": run,
            "time": time,
            "slowest_entries": slowest,
            "boot": boot,
            "admission": {
                "observed_workers": self.admissions.len(),
                "invalid_observations": self.invalid_admissions,
                "worker_ms": (!self.admissions.is_empty()).then(|| self.admissions.iter().map(|r| r.admission_ms).sum::<u64>()),
                "observations": self.admissions.iter().map(|row| json!({
                    "name": row.name, "admission_ms": row.admission_ms,
                    "checks_ms": row.checks_ms, "wait_ms_by_reason": row.wait_ms_by_reason,
                })).collect::<Vec<_>>(),
            },
            "occupancy": occupancy,
            "timings": timings,
        }))?)
    }
}

impl fmt::Display for SimRunAnalysis {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(out, "sim run {}", self.run_id)?;
        writeln!(out, "  record          {}", self.dir.display())?;
        writeln!(
            out,
            "  status          {}{}",
            self.status,
            if self.run_end_present {
                String::new()
            } else {
                " (no run_end: the invocation ended before the parent wrote it)".to_string()
            },
        )?;
        if let Some(error) = &self.error {
            writeln!(out, "  error           {error}")?;
        }
        match self.wall_ms {
            Some(wall) => writeln!(out, "  wall            {}", secs(wall))?,
            None => writeln!(
                out,
                "  wall            unavailable; observed span {}",
                secs(self.observed_span_ms),
            )?,
        }
        writeln!(
            out,
            "  entries         {} planned / {} finalized",
            self.planned_entries, self.finalized_entries,
        )?;
        let verdicts = self
            .verdicts
            .iter()
            .map(|(verdict, count)| format!("{count} {verdict}"))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            out,
            "  verdicts        {}",
            if verdicts.is_empty() {
                "none recorded".to_string()
            } else {
                verdicts
            },
        )?;
        writeln!(
            out,
            "  source          {}{}",
            self.revision.as_deref().unwrap_or("unavailable"),
            match self.dirty {
                Some(true) => " (dirty)",
                Some(false) => " (clean)",
                None => "",
            },
        )?;
        writeln!(
            out,
            "  jobs            {} requested / {} effective",
            self.requested_jobs
                .map(|j| j.to_string())
                .unwrap_or_else(|| "?".into()),
            self.effective_jobs
                .map(|j| j.to_string())
                .unwrap_or_else(|| "?".into()),
        )?;
        writeln!(out, "  workers         {} boots", self.workers.len())?;
        if self.skipped_records > 0 {
            writeln!(
                out,
                "  skipped         {} record(s) this reader could not interpret",
                self.skipped_records,
            )?;
        }

        writeln!(out)?;
        writeln!(out, "time decomposition")?;
        writeln!(
            out,
            "  worker span     {}   ({} worker(s))",
            secs(self.worker_span_total_ms),
            self.workers.len(),
        )?;
        writeln!(
            out,
            "    in envelopes  {}  {:>5.1}%",
            secs(self.inside_envelopes_ms),
            pct(self.inside_envelopes_ms, self.worker_span_total_ms),
        )?;
        writeln!(
            out,
            "    startup       {}  {:>5.1}%   {} boot(s), median {}",
            secs(self.startup_latency_ms),
            pct(self.startup_latency_ms, self.worker_span_total_ms),
            self.startup_boots,
            self.startup_median_ms
                .map(|v| format!("{v} ms"))
                .unwrap_or_else(|| "n/a".into()),
        )?;
        writeln!(
            out,
            "    remainder     {}  {:>5.1}%",
            secs(self.remainder_ms),
            pct(self.remainder_ms, self.worker_span_total_ms),
        )?;
        if self.drain_overhang_ms > 0 || self.workers_without_end > 0 {
            writeln!(
                out,
                "  drain overhang  {} (entry lines read after the reap); {} worker(s) never reaped",
                secs(self.drain_overhang_ms),
                self.workers_without_end,
            )?;
        }
        writeln!(
            out,
            "  envelopes       {} over {} entry/entries",
            secs(self.inside_envelopes_ms),
            self.envelopes.len(),
        )?;
        if self.entries_started_without_end > 0 || self.entries_ended_without_start > 0 {
            writeln!(
                out,
                "  unmatched       {} started without an end, {} ended without a start",
                self.entries_started_without_end, self.entries_ended_without_start,
            )?;
        }
        if let (Some(wall), Some(ideal)) = (self.wall_ms, self.ideal_ms_at_effective_jobs()) {
            writeln!(
                out,
                "  vs wall         {} wall, {} ideal at {} job(s)",
                secs(wall),
                secs(ideal),
                self.effective_jobs.or(self.requested_jobs).unwrap_or(0),
            )?;
        }
        writeln!(
            out,
            "  note: worker_started -> worker_first_entry is startup latency."
        )?;
        writeln!(
            out,
            "  frames          {}/{} known, total {}, min {}, median {}, max {}",
            self.frame_counts.known,
            self.frame_counts.known + self.frame_counts.missing,
            self.frame_counts.total,
            self.frame_counts
                .min
                .map(|value| value.to_string())
                .unwrap_or_else(|| "n/a".into()),
            self.frame_counts
                .median
                .map(|value| value.to_string())
                .unwrap_or_else(|| "n/a".into()),
            self.frame_counts
                .max
                .map(|value| value.to_string())
                .unwrap_or_else(|| "n/a".into()),
        )?;
        let frame_timing_entries =
            self.frame_timing.known + self.frame_timing.missing + self.frame_timing.invalid;
        writeln!(
            out,
            "  frame phases    {}/{} known, {} frames, {} draw, {} submitted; logic {:.3}s, render CPU {:.3}s, present {:.3}s",
            self.frame_timing.known,
            frame_timing_entries,
            self.frame_timing.frames,
            self.frame_timing.draw_frames,
            self.frame_timing.submitted_frames,
            self.frame_timing.logic_us as f64 / 1_000_000.0,
            self.frame_timing.render_cpu_us as f64 / 1_000_000.0,
            self.frame_timing.present_us as f64 / 1_000_000.0,
        )?;
        if self.frame_timing.invalid > 0 {
            writeln!(
                out,
                "  frame phases    {} invalid entry timing record(s); totals exclude them",
                self.frame_timing.invalid,
            )?;
        }
        writeln!(
            out,
            "  note: render CPU ends before swap; it is not GPU time, and present may include sync wait."
        )?;
        writeln!(out)?;
        writeln!(out, "slowest entry envelopes (top {DEFAULT_TOP})")?;
        for envelope in self.slowest() {
            writeln!(
                out,
                "  {:>9}  {}  (occurrence {}, worker {}, frames {})",
                secs(envelope.duration_ms()),
                envelope.name,
                envelope.occurrence_id,
                envelope.worker_attempt,
                envelope
                    .frame_count
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "n/a".into()),
            )?;
        }

        writeln!(out)?;
        writeln!(out, "boot")?;
        writeln!(out, "  worst boot:work")?;
        for (worker, ratio) in self.worst_boot_to_work() {
            writeln!(
                out,
                "    {:>6.2}x  {}  (boot {}, work {}, {} entry/entries)",
                ratio,
                worker.first_entry_name.as_deref().unwrap_or("<unknown>"),
                secs(worker.startup_ms.unwrap_or(0)),
                secs(worker.work_ms),
                worker.entries,
            )?;
        }

        writeln!(out)?;
        writeln!(
            out,
            "worker admission (overlapping worker-time, not gate wall time)"
        )?;
        if self.admissions.is_empty() {
            writeln!(out, "  unavailable: no completed admission observations")?;
        }
        for row in self.admissions.iter().take(5) {
            writeln!(
                out,
                "  {}: {:.3}s admission, {:.3}s checks; wait_ms_by_reason={:?}",
                row.name,
                row.admission_ms as f64 / 1000.0,
                row.checks_ms as f64 / 1000.0,
                row.wait_ms_by_reason,
            )?;
        }
        if self.invalid_admissions > 0 {
            writeln!(
                out,
                "  {} invalid admission observations",
                self.invalid_admissions,
            )?;
        }
        writeln!(out, "worker occupancy")?;
        for (live, span) in self.occupancy.histogram.iter().rev() {
            writeln!(
                out,
                "  {live:>3} live  {:>9}  {:>5.1}% of {}",
                secs(*span),
                pct(*span, self.occupancy.base_ms),
                if self.wall_ms.is_some() {
                    "wall"
                } else {
                    "observed span"
                },
            )?;
        }
        writeln!(out, "  mean live       {:.2}", self.occupancy.mean_live)?;
        writeln!(
            out,
            "  ramp to full    {}",
            self.occupancy
                .ramp_to_full_ms
                .map(secs)
                .unwrap_or_else(|| "never reached the effective job count".into()),
        )?;
        writeln!(
            out,
            "  under-occupied tail  {}",
            secs(self.occupancy.under_occupied_tail_ms),
        )?;
        writeln!(
            out,
            "  idle            {} before the first worker, {} after the last reap",
            secs(self.occupancy.idle_before_first_worker_ms),
            secs(self.occupancy.idle_after_last_worker_ms),
        )?;

        writeln!(out)?;
        match &self.timings {
            Some(view) => {
                writeln!(out, "timings.json")?;
                writeln!(
                    out,
                    "  {} entries, {} passed, {} completed, accepted={}",
                    view.entries, view.passed, view.completed, view.accepted,
                )?;
                writeln!(out, "  duration total  {}", secs(view.duration_total_ms))?;
                if view.envelope_mismatches > 0 || view.missing_from_events > 0 {
                    writeln!(
                        out,
                        "  {} entry/entries disagree with the recorded envelope, {} absent from events.jsonl",
                        view.envelope_mismatches, view.missing_from_events,
                    )?;
                }
            }
            None => writeln!(
                out,
                "timings.json  absent (this record predates it, or the invocation \
                 ended before it was written)"
            )?,
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub(crate) fn run(log_dir: Option<&str>, run_id: Option<&str>, format: &str) -> Result<()> {
    if !matches!(format, "pretty" | "json") {
        bail!("sim-report format must be `pretty` or `json`, got `{format}`")
    }
    let root = crate::sim_observability::history_root(log_dir);
    let (id, dir) = resolve_run(&root, run_id)?;
    let record = RunRecord::load(&id, &dir)?;
    let analysis = analyze(&record);
    match format {
        "json" => println!("{}", analysis.render_json()?),
        _ => print!("{analysis}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_even_sample_reports_a_value_that_occurred() {
        assert_eq!(upper_median(&[]), None);
        assert_eq!(upper_median(&[1, 2]), Some(2));
        assert_eq!(upper_median(&[1, 2, 3]), Some(2));
    }

    #[test]
    fn run_ids_order_by_their_parsed_parts_not_their_spelling() {
        let mut ids = vec!["999999999-1-0", "1700000000000-2-0", "1700000000000-10-0"];
        ids.sort_by_key(|id| run_sort_key(id));
        assert_eq!(
            ids,
            vec!["999999999-1-0", "1700000000000-2-0", "1700000000000-10-0"]
        );
    }

    #[test]
    fn report_retains_per_entry_frame_timing() {
        let record = RunRecord {
            run_id: "fixture".into(),
            dir: PathBuf::from("/tmp/fixture"),
            skipped_records: 0,
            timings: None,
            events: vec![
                json!({ "event": "run_start", "elapsed_ms": 0,
                    "source": { "status": "unavailable" } }),
                json!({ "event": "run_config", "elapsed_ms": 0,
                    "effective_jobs": 1, "project_path": "game" }),
                json!({ "event": "worker_spawn", "elapsed_ms": 0,
                    "worker_attempt": 1, "entries": [{
                        "occurrence_id": 1, "name": "fixture"
                    }] }),
                json!({ "event": "worker_started", "elapsed_ms": 0,
                    "worker_attempt": 1 }),
                json!({ "event": "worker_first_entry", "elapsed_ms": 1,
                    "worker_attempt": 1, "occurrence_id": 1,
                    "name": "fixture" }),
                json!({ "event": "entry_start", "elapsed_ms": 1,
                    "worker_attempt": 1, "occurrence_id": 1,
                    "name": "fixture" }),
                json!({ "event": "entry_end", "elapsed_ms": 11,
                    "worker_attempt": 1, "occurrence_id": 1,
                    "name": "fixture", "frame_count": 3,
                    "frame_timing": {
                        "frames": 3, "draw_frames": 2, "submitted_frames": 1,
                        "logic_us": 1000, "render_cpu_us": 2000,
                        "present_us": 300
                    } }),
                json!({ "event": "entry_result", "elapsed_ms": 11,
                    "worker_attempt": 1, "occurrence_id": 1,
                    "verdict": "PASS", "timed_out": false }),
                json!({ "event": "worker_end", "elapsed_ms": 12,
                    "worker_attempt": 1 }),
                json!({ "event": "run_end", "elapsed_ms": 12,
                    "complete": true, "planned_entries": 1,
                    "finalized_entries": 1, "status": "PASS" }),
            ],
        };
        let rendered: Value =
            serde_json::from_str(&analyze(&record).render_json().unwrap()).unwrap();
        assert_eq!(rendered["time"]["frame_timing"]["known_entries"], 1);
        assert_eq!(rendered["time"]["frame_timing"]["frames"], 3);
        assert_eq!(rendered["time"]["frame_timing"]["draw_frames"], 2);
        assert_eq!(rendered["time"]["frame_timing"]["render_cpu_us"], 2000);
        assert_eq!(
            rendered["slowest_entries"][0]["frame_timing"]["present_us"],
            300
        );
    }

    #[test]
    fn old_envelopes_keep_new_timing_unknown() {
        let record = RunRecord {
            run_id: "old".into(),
            dir: PathBuf::from("/tmp/old"),
            skipped_records: 0,
            timings: None,
            events: vec![
                json!({ "event": "entry_start", "elapsed_ms": 1,
                    "worker_attempt": 1, "occurrence_id": 1,
                    "name": "old" }),
                json!({ "event": "entry_end", "elapsed_ms": 3,
                    "worker_attempt": 1, "occurrence_id": 1,
                    "name": "old", "frame_count": 2 }),
            ],
        };
        let rendered: Value =
            serde_json::from_str(&analyze(&record).render_json().unwrap()).unwrap();
        assert_eq!(rendered["time"]["frame_timing"]["known_entries"], 0);
        assert_eq!(rendered["time"]["frame_timing"]["missing_entries"], 1);
        assert_eq!(rendered["time"]["frame_timing"]["invalid_entries"], 0);
    }
}
