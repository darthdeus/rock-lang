//! `rock sim-report` -- the aggregate reader over the persistent `rock sim`
//! history that `sim_observability::SimRunRecorder` writes to
//! `.sim-runs/<run-id>/` (`ROCK_SIM_LOG_DIR` overrides the root).
//!
//! Every speed pass on this branch has re-derived the same rankings by hand
//! from records that were already on disk. This reads them in aggregate so the
//! next pass diffs two runs instead of rewriting the analysis.
//!
//! The interpretation rules come from `docs/sim-observability-plan.md` and are
//! load-bearing here:
//!
//! * `worker_started` -> `worker_first_entry` is **startup latency**.
//! * An entry envelope (`entry_start` -> `entry_end`) **includes the native
//!   reset and state restoration** when a shared worker is in use. It is not
//!   pure sim-body time, and nothing in this reader's output says otherwise.
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

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// The only schema this reader knows how to interpret.
const SCHEMA_VERSION: i64 = 1;

/// Printed with every decomposition so no consumer can read the envelope as
/// pure sim-body time. `docs/sim-observability-plan.md` is the source.
const ENVELOPE_QUALIFIER: &str = "an entry envelope includes the native reset and state restoration \
     when a shared worker is in use, so it is not a measure of sim-body work alone";

// ---------------------------------------------------------------------------
// Locating runs
// ---------------------------------------------------------------------------

/// The history root: the explicit flag, then `ROCK_SIM_LOG_DIR`, then
/// `.sim-runs` -- the same order `SimRunRecorder::begin` resolves.
pub fn resolve_root(explicit: Option<&str>) -> PathBuf {
    if let Some(dir) = explicit {
        return PathBuf::from(dir);
    }
    std::env::var_os("ROCK_SIM_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".sim-runs"))
}

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
pub fn list_runs(root: &Path) -> Result<Vec<(String, PathBuf)>> {
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
    runs.sort_by(|left, right| run_sort_key(&left.0).cmp(&run_sort_key(&right.0)));
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
pub struct RunRecord {
    pub run_id: String,
    pub dir: PathBuf,
    events: Vec<Value>,
    skipped_records: usize,
    timings: Option<Value>,
}

impl RunRecord {
    pub fn load(run_id: &str, dir: &Path) -> Result<Self> {
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
/// one; the author's hand analysis of the batch-9 MAG run used the same rule.
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
    part: i64,
    worker_attempt: i64,
    start_ms: i64,
    end_ms: i64,
}

impl Envelope {
    fn duration_ms(&self) -> i64 {
        self.end_ms - self.start_ms
    }
}

/// How many logical entries a lane was planned to carry.
fn lane_entry_count(lane: &Value) -> usize {
    lane.get("entries")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
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

/// Normalize a lane's `isolation_reason` into the classes the history actually
/// contains. The raw reasons carry per-lane detail (which `$OUT` path, which
/// env var), which is exactly what makes them useless to group on.
fn isolation_class(reason: Option<&str>) -> &'static str {
    match reason.map(str::trim).filter(|r| !r.is_empty()) {
        None => "none",
        Some(r) if r.contains("SIM_WORKER_ISOLATED") => "SIM_WORKER_ISOLATED",
        Some(r) if r.contains("$OUT") => "$OUT interpolation",
        Some(r) if r.contains("hot-reload") => "hot-reload",
        Some(_) => "other",
    }
}

/// Recovery causes carry the specific resources or occurrence that triggered
/// them; group on the kind so a census is readable.
fn recovery_class(cause: &str) -> &'static str {
    let lowered = cause.to_lowercase();
    if lowered.contains("destroyed native resources") {
        "destroyed native resources"
    } else if lowered.contains("timed out") || lowered.contains("timeout") {
        "timeout"
    } else if lowered.contains("refus") {
        "native refusal"
    } else if lowered.contains("process") || lowered.contains("exit") {
        "worker process failure"
    } else {
        "other"
    }
}

/// `[["SIM_SKIP_RENDER", "0"], ...]` -> `SIM_SKIP_RENDER=0,...`. The empty
/// signature is the default boot and is named so it reads in a table.
fn render_signature(signature: &Value) -> String {
    let Some(pairs) = signature.as_array() else {
        return signature.to_string();
    };
    if pairs.is_empty() {
        return "<default>".to_string();
    }
    pairs
        .iter()
        .map(|pair| match pair.as_array() {
            Some(kv) if kv.len() == 2 => format!(
                "{}={}",
                kv[0].as_str().unwrap_or_default(),
                kv[1].as_str().unwrap_or_default()
            ),
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join(",")
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
pub struct SimRunAnalysis {
    run_id: String,
    dir: PathBuf,
    skipped_records: usize,
    top: usize,

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

    // Boot amortization.
    entries_per_worker_median: Option<i64>,
    entries_per_worker_max: usize,
    single_entry_workers: usize,
    single_entry_boot_ms: i64,
    single_entry_work_ms: i64,

    // Lanes.
    lanes_planned: usize,
    single_entry_lanes: usize,
    by_signature: Vec<(String, usize, usize)>,
    by_isolation_class: Vec<(&'static str, usize, usize)>,

    // Recoveries.
    recoveries: Vec<(String, String)>,

    occupancy: Occupancy,
    timings: Option<TimingsView>,
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

pub fn analyze(record: &RunRecord, top: usize) -> SimRunAnalysis {
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
    let mut verdict_by_entry: HashMap<(i64, i64), String> = HashMap::new();
    for result in record.of("entry_result") {
        if let Some(verdict) = text(result, "verdict") {
            *verdicts.entry(verdict.to_string()).or_insert(0) += 1;
            if let (Some(occurrence), Some(attempt)) =
                (int(result, "occurrence_id"), int(result, "worker_attempt"))
            {
                verdict_by_entry.insert((occurrence, attempt), verdict.to_string());
            }
        }
    }

    // Envelopes ------------------------------------------------------------
    // Keyed by occurrence + worker attempt + chain part, which is what makes a
    // repeated name identifiable. A duplicate start keeps the first.
    let mut open: HashMap<(i64, i64, i64), &Value> = HashMap::new();
    for start in record.of("entry_start") {
        let key = (
            int(start, "occurrence_id").unwrap_or(-1),
            int(start, "worker_attempt").unwrap_or(-1),
            int(start, "part").unwrap_or(0),
        );
        open.entry(key).or_insert(start);
    }
    let mut envelopes = Vec::new();
    let mut closed: HashMap<(i64, i64, i64), ()> = HashMap::new();
    let mut entries_ended_without_start = 0usize;
    for end in record.of("entry_end") {
        let key = (
            int(end, "occurrence_id").unwrap_or(-1),
            int(end, "worker_attempt").unwrap_or(-1),
            int(end, "part").unwrap_or(0),
        );
        let Some(start) = open.get(&key) else {
            entries_ended_without_start += 1;
            continue;
        };
        if closed.insert(key, ()).is_some() {
            continue;
        }
        envelopes.push(Envelope {
            name: text(end, "name")
                .or_else(|| text(start, "name"))
                .unwrap_or("<unnamed>")
                .to_string(),
            occurrence_id: key.0,
            part: key.2,
            worker_attempt: key.1,
            start_ms: ms(start),
            end_ms: ms(end),
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

    let mut counts: Vec<i64> = workers.iter().map(|w| w.entries as i64).collect();
    counts.sort_unstable();
    let single: Vec<&Worker> = workers.iter().filter(|w| w.entries == 1).collect();

    // Lanes ----------------------------------------------------------------
    let lanes: Vec<&Value> = record.of("lane_planned").collect();
    let mut signature_census: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut isolation_census: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    for lane in &lanes {
        let signature = lane
            .get("boot_signature")
            .map(render_signature)
            .unwrap_or_else(|| "<absent>".to_string());
        let slot = signature_census.entry(signature).or_insert((0, 0));
        slot.0 += 1;
        slot.1 += lane_entry_count(lane);
        let class = isolation_class(text(lane, "isolation_reason"));
        let slot = isolation_census.entry(class).or_insert((0, 0));
        slot.0 += 1;
        slot.1 += lane_entry_count(lane);
    }
    let mut by_signature: Vec<(String, usize, usize)> = signature_census
        .into_iter()
        .map(|(signature, (lanes, entries))| (signature, lanes, entries))
        .collect();
    by_signature.sort_by(|left, right| {
        right
            .2
            .cmp(&left.2)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| left.0.cmp(&right.0))
    });
    let mut by_isolation_class: Vec<(&'static str, usize, usize)> = isolation_census
        .into_iter()
        .map(|(class, (lanes, entries))| (class, lanes, entries))
        .collect();
    by_isolation_class.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));

    let recoveries: Vec<(String, String)> = record
        .of("worker_recovery")
        .map(|event| {
            let cause = text(event, "cause").unwrap_or("<unrecorded>").to_string();
            (recovery_class(&cause).to_string(), cause)
        })
        .collect();

    // Occupancy ------------------------------------------------------------
    let requested_jobs = run_config
        .and_then(|value| int(value, "requested_jobs"))
        .or_else(|| run_start.and_then(|value| int(value, "requested_jobs")));
    let effective_jobs = run_config.and_then(|value| int(value, "effective_jobs"));
    let base_ms = wall_ms.unwrap_or(observed_span_ms);
    let jobs = effective_jobs.or(requested_jobs).unwrap_or(0);
    let occupancy = occupancy(&workers, base_ms, jobs);

    // timings.json ---------------------------------------------------------
    let envelope_by_key: HashMap<(i64, i64), i64> = envelopes
        .iter()
        .map(|envelope| ((envelope.occurrence_id, envelope.part), envelope.duration_ms()))
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
            let key = (
                int(row, "occurrence_id").unwrap_or(-1),
                int(row, "part").unwrap_or(0),
            );
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
            accepted: value.get("accepted").and_then(Value::as_bool).unwrap_or(false),
            duration_total_ms,
            envelope_mismatches,
            missing_from_events,
        }
    });

    SimRunAnalysis {
        run_id: record.run_id.clone(),
        dir: record.dir.clone(),
        skipped_records: record.skipped_records,
        top,
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
        startup_latency_ms,
        startup_boots: startups.len(),
        startup_median_ms: upper_median(&startups),
        remainder_ms,
        entries_per_worker_median: upper_median(&counts),
        entries_per_worker_max: counts.last().copied().unwrap_or(0) as usize,
        single_entry_workers: single.len(),
        single_entry_boot_ms: single.iter().filter_map(|w| w.startup_ms).sum(),
        single_entry_work_ms: single.iter().map(|w| w.work_ms).sum(),
        lanes_planned: lanes.len(),
        single_entry_lanes: lanes.iter().filter(|lane| lane_entry_count(lane) == 1).count(),
        by_signature,
        by_isolation_class,
        recoveries,
        occupancy,
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
        ranked.truncate(self.top);
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
        ranked.truncate(self.top);
        ranked
    }

    fn recovery_census(&self) -> Vec<(&str, usize, &str)> {
        let mut census: BTreeMap<&str, (usize, &str)> = BTreeMap::new();
        for (class, cause) in &self.recoveries {
            let slot = census
                .entry(class.as_str())
                .or_insert((0, cause.as_str()));
            slot.0 += 1;
        }
        let mut rows: Vec<(&str, usize, &str)> = census
            .into_iter()
            .map(|(class, (count, example))| (class, count, example))
            .collect();
        rows.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
        rows
    }

    fn ideal_ms_at_effective_jobs(&self) -> Option<i64> {
        let jobs = self.effective_jobs.or(self.requested_jobs)?;
        (jobs > 0).then(|| self.inside_envelopes_ms / jobs)
    }

    pub fn render_json(&self) -> Result<String> {
        let mut run = Map::new();
        run.insert("run_id".into(), json!(self.run_id));
        run.insert("directory".into(), json!(self.dir.to_string_lossy()));
        run.insert("status".into(), json!(self.status));
        run.insert("complete".into(), json!(self.complete));
        run.insert("run_end_present".into(), json!(self.run_end_present));
        run.insert("wall_ms".into(), json!(self.wall_ms));
        run.insert("observed_span_ms".into(), json!(self.observed_span_ms));
        run.insert("planned_entries".into(), json!(self.planned_entries));
        run.insert("finalized_entries".into(), json!(self.finalized_entries));
        run.insert("verdicts".into(), json!(self.verdicts));
        run.insert("error".into(), json!(self.error));
        run.insert("project".into(), json!(self.project));
        run.insert("selection".into(), json!(self.selection));
        run.insert("revision".into(), json!(self.revision));
        run.insert("dirty".into(), json!(self.dirty));
        run.insert("requested_jobs".into(), json!(self.requested_jobs));
        run.insert("effective_jobs".into(), json!(self.effective_jobs));
        run.insert("boots".into(), json!(self.workers.len()));
        run.insert("recoveries".into(), json!(self.recoveries.len()));
        run.insert("skipped_records".into(), json!(self.skipped_records));

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
            "envelope_qualifier": ENVELOPE_QUALIFIER,
        });

        let slowest: Vec<Value> = self
            .slowest()
            .into_iter()
            .map(|envelope| {
                json!({
                    "name": envelope.name,
                    "occurrence_id": envelope.occurrence_id,
                    "part": envelope.part,
                    "worker_attempt": envelope.worker_attempt,
                    "start_ms": envelope.start_ms,
                    "end_ms": envelope.end_ms,
                    "duration_ms": envelope.duration_ms(),
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
            "entries_per_worker_median": self.entries_per_worker_median,
            "entries_per_worker_max": self.entries_per_worker_max,
            "single_entry_workers": {
                "workers": self.single_entry_workers,
                "boot_ms": self.single_entry_boot_ms,
                "work_ms": self.single_entry_work_ms,
            },
            "worst_boot_to_work": worst,
        });

        let signatures: Vec<Value> = self
            .by_signature
            .iter()
            .map(|(signature, lanes, entries)| {
                json!({ "signature": signature, "lanes": lanes, "entries": entries })
            })
            .collect();
        let classes: Vec<Value> = self
            .by_isolation_class
            .iter()
            .map(|(class, lanes, entries)| {
                json!({ "class": class, "lanes": lanes, "entries": entries })
            })
            .collect();
        let lanes = json!({
            "lanes_planned": self.lanes_planned,
            "single_entry_lanes": self.single_entry_lanes,
            "distinct_boot_signatures": self.by_signature.len(),
            "entries_in_top_two_signatures":
                self.by_signature.iter().take(2).map(|row| row.2).sum::<usize>(),
            "by_signature": signatures,
            "by_isolation_class": classes,
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

        let recoveries = json!({
            "total": self.recoveries.len(),
            "by_cause": self.recovery_census().into_iter().map(|(class, count, example)| {
                json!({ "cause": class, "count": count, "example": example })
            }).collect::<Vec<_>>(),
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
            "run": Value::Object(run),
            "time": time,
            "slowest_entries": slowest,
            "boot_amortization": boot,
            "lanes": lanes,
            "occupancy": occupancy,
            "recoveries": recoveries,
            "timings": timings,
        }))?)
    }

    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let push = |out: &mut String, line: String| {
            out.push_str(&line);
            out.push('\n');
        };

        push(&mut out, format!("sim run {}", self.run_id));
        push(&mut out, format!("  record          {}", self.dir.display()));
        push(
            &mut out,
            format!(
                "  status          {}{}",
                self.status,
                if self.run_end_present {
                    String::new()
                } else {
                    " (no run_end: the invocation ended before the parent wrote it)".to_string()
                }
            ),
        );
        if let Some(error) = &self.error {
            push(&mut out, format!("  error           {error}"));
        }
        match self.wall_ms {
            Some(wall) => push(&mut out, format!("  wall            {}", secs(wall))),
            None => push(
                &mut out,
                format!(
                    "  wall            unavailable; observed span {}",
                    secs(self.observed_span_ms)
                ),
            ),
        }
        push(
            &mut out,
            format!(
                "  entries         {} planned / {} finalized",
                self.planned_entries, self.finalized_entries
            ),
        );
        let verdicts = self
            .verdicts
            .iter()
            .map(|(verdict, count)| format!("{count} {verdict}"))
            .collect::<Vec<_>>()
            .join(", ");
        push(
            &mut out,
            format!(
                "  verdicts        {}",
                if verdicts.is_empty() {
                    "none recorded".to_string()
                } else {
                    verdicts
                }
            ),
        );
        push(
            &mut out,
            format!(
                "  source          {}{}",
                self.revision.as_deref().unwrap_or("unavailable"),
                match self.dirty {
                    Some(true) => " (dirty)",
                    Some(false) => " (clean)",
                    None => "",
                }
            ),
        );
        push(
            &mut out,
            format!(
                "  jobs            {} requested / {} effective",
                self.requested_jobs
                    .map(|j| j.to_string())
                    .unwrap_or_else(|| "?".into()),
                self.effective_jobs
                    .map(|j| j.to_string())
                    .unwrap_or_else(|| "?".into())
            ),
        );
        push(
            &mut out,
            format!(
                "  workers         {} boots, {} recoveries",
                self.workers.len(),
                self.recoveries.len()
            ),
        );
        if self.skipped_records > 0 {
            push(
                &mut out,
                format!(
                    "  skipped         {} record(s) this reader could not interpret",
                    self.skipped_records
                ),
            );
        }

        push(&mut out, String::new());
        push(&mut out, "time decomposition".to_string());
        push(
            &mut out,
            format!(
                "  worker span     {}   ({} worker(s))",
                secs(self.worker_span_total_ms),
                self.workers.len()
            ),
        );
        push(
            &mut out,
            format!(
                "    in envelopes  {}  {:>5.1}%",
                secs(self.inside_envelopes_ms),
                pct(self.inside_envelopes_ms, self.worker_span_total_ms)
            ),
        );
        push(
            &mut out,
            format!(
                "    startup       {}  {:>5.1}%   {} boot(s), median {}",
                secs(self.startup_latency_ms),
                pct(self.startup_latency_ms, self.worker_span_total_ms),
                self.startup_boots,
                self.startup_median_ms
                    .map(|v| format!("{v} ms"))
                    .unwrap_or_else(|| "n/a".into())
            ),
        );
        push(
            &mut out,
            format!(
                "    remainder     {}  {:>5.1}%",
                secs(self.remainder_ms),
                pct(self.remainder_ms, self.worker_span_total_ms)
            ),
        );
        if self.drain_overhang_ms > 0 || self.workers_without_end > 0 {
            push(
                &mut out,
                format!(
                    "  drain overhang  {} (entry lines read after the reap); {} worker(s) never reaped",
                    secs(self.drain_overhang_ms),
                    self.workers_without_end
                ),
            );
        }
        push(
            &mut out,
            format!(
                "  envelopes       {} over {} entry/entries",
                secs(self.inside_envelopes_ms),
                self.envelopes.len()
            ),
        );
        if self.entries_started_without_end > 0 || self.entries_ended_without_start > 0 {
            push(
                &mut out,
                format!(
                    "  unmatched       {} started without an end, {} ended without a start",
                    self.entries_started_without_end, self.entries_ended_without_start
                ),
            );
        }
        if let (Some(wall), Some(ideal)) = (self.wall_ms, self.ideal_ms_at_effective_jobs()) {
            push(
                &mut out,
                format!(
                    "  vs wall         {} wall, {} ideal at {} job(s)",
                    secs(wall),
                    secs(ideal),
                    self.effective_jobs.or(self.requested_jobs).unwrap_or(0)
                ),
            );
        }
        push(&mut out, format!("  note: {ENVELOPE_QUALIFIER}."));
        push(
            &mut out,
            "  note: worker_started -> worker_first_entry is startup latency.".to_string(),
        );

        push(&mut out, String::new());
        push(
            &mut out,
            format!("slowest entry envelopes (top {})", self.top),
        );
        for envelope in self.slowest() {
            push(
                &mut out,
                format!(
                    "  {:>9}  {}  (occurrence {}, worker {})",
                    secs(envelope.duration_ms()),
                    envelope.name,
                    envelope.occurrence_id,
                    envelope.worker_attempt
                ),
            );
        }

        push(&mut out, String::new());
        push(&mut out, "boot amortization".to_string());
        push(
            &mut out,
            format!(
                "  entries/worker  median {}, max {}",
                self.entries_per_worker_median
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "n/a".into()),
                self.entries_per_worker_max
            ),
        );
        push(
            &mut out,
            format!(
                "  {} worker(s) ran exactly one entry: {} of boot to do {} of work",
                self.single_entry_workers,
                secs(self.single_entry_boot_ms),
                secs(self.single_entry_work_ms)
            ),
        );
        push(&mut out, "  worst boot:work".to_string());
        for (worker, ratio) in self.worst_boot_to_work() {
            push(
                &mut out,
                format!(
                    "    {:>6.2}x  {}  (boot {}, work {}, {} entry/entries)",
                    ratio,
                    worker.first_entry_name.as_deref().unwrap_or("<unknown>"),
                    secs(worker.startup_ms.unwrap_or(0)),
                    secs(worker.work_ms),
                    worker.entries
                ),
            );
        }

        push(&mut out, String::new());
        push(&mut out, "lane and boot-signature census".to_string());
        push(
            &mut out,
            format!(
                "  lanes           {} planned, {} single-entry",
                self.lanes_planned, self.single_entry_lanes
            ),
        );
        push(
            &mut out,
            format!(
                "  signatures      {} distinct; top two cover {} entry/entries",
                self.by_signature.len(),
                self.by_signature
                    .iter()
                    .take(2)
                    .map(|row| row.2)
                    .sum::<usize>()
            ),
        );
        for (signature, lanes, entries) in self.by_signature.iter().take(self.top) {
            push(
                &mut out,
                format!("    {entries:>5} entries  {lanes:>4} lanes  {signature}"),
            );
        }
        push(&mut out, "  lanes by isolation reason".to_string());
        for (class, lanes, entries) in &self.by_isolation_class {
            push(
                &mut out,
                format!("    {lanes:>5} lanes  {entries:>5} entries  {class}"),
            );
        }

        if !self.recoveries.is_empty() {
            push(&mut out, String::new());
            push(&mut out, "recoveries".to_string());
            for (class, count, example) in self.recovery_census() {
                push(&mut out, format!("  {count:>4}  {class}"));
                push(&mut out, format!("        e.g. {example}"));
            }
        }

        push(&mut out, String::new());
        push(&mut out, "worker occupancy".to_string());
        for (live, span) in self.occupancy.histogram.iter().rev() {
            push(
                &mut out,
                format!(
                    "  {live:>3} live  {:>9}  {:>5.1}% of {}",
                    secs(*span),
                    pct(*span, self.occupancy.base_ms),
                    if self.wall_ms.is_some() {
                        "wall"
                    } else {
                        "observed span"
                    }
                ),
            );
        }
        push(
            &mut out,
            format!("  mean live       {:.2}", self.occupancy.mean_live),
        );
        push(
            &mut out,
            format!(
                "  ramp to full    {}",
                self.occupancy
                    .ramp_to_full_ms
                    .map(secs)
                    .unwrap_or_else(|| "never reached the effective job count".into())
            ),
        );
        push(
            &mut out,
            format!(
                "  under-occupied tail  {}",
                secs(self.occupancy.under_occupied_tail_ms)
            ),
        );
        push(
            &mut out,
            format!(
                "  idle            {} before the first worker, {} after the last reap",
                secs(self.occupancy.idle_before_first_worker_ms),
                secs(self.occupancy.idle_after_last_worker_ms)
            ),
        );

        push(&mut out, String::new());
        match &self.timings {
            Some(view) => {
                push(&mut out, "timings.json".to_string());
                push(
                    &mut out,
                    format!(
                        "  {} entries, {} passed, {} completed, accepted={}",
                        view.entries, view.passed, view.completed, view.accepted
                    ),
                );
                push(
                    &mut out,
                    format!("  duration total  {}", secs(view.duration_total_ms)),
                );
                if view.envelope_mismatches > 0 || view.missing_from_events > 0 {
                    push(
                        &mut out,
                        format!(
                            "  {} entry/entries disagree with the recorded envelope, {} absent from events.jsonl",
                            view.envelope_mismatches, view.missing_from_events
                        ),
                    );
                }
            }
            None => push(
                &mut out,
                "timings.json  absent (this record predates it, or the invocation \
                 ended before it was written)"
                    .to_string(),
            ),
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Cross-run comparison
// ---------------------------------------------------------------------------

/// Cheap event-name peek so the cross-run scan does not deserialize every line
/// of every retained record. The recorder writes compact JSON with sorted
/// keys; a line this cannot read falls back to a full parse.
fn peek_event(line: &str) -> Option<&str> {
    let start = line.find("\"event\":\"")? + "\"event\":\"".len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

const CROSS_RUN_EVENTS: [&str; 4] = ["run_start", "entry_start", "entry_end", "entry_result"];

/// One named entry's validated envelope in one retained run -- the same join
/// the `docs/sim-observability-plan.md` jq recipe performs, without the shell.
fn cross_run_rows(root: &Path, name: &str) -> Result<(Vec<Value>, usize, usize)> {
    let runs = list_runs(root)?;
    let scanned = runs.len();
    let mut unreadable = 0usize;
    let mut rows = Vec::new();
    for (run_id, dir) in runs {
        let Ok(text_body) = std::fs::read_to_string(dir.join("events.jsonl")) else {
            unreadable += 1;
            continue;
        };
        let mut revision: Option<String> = None;
        let mut starts: Vec<Value> = Vec::new();
        let mut ends: HashMap<(i64, i64, i64), i64> = HashMap::new();
        let mut results: HashMap<(i64, i64), String> = HashMap::new();
        for line in text_body.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(event) = peek_event(line) {
                if !CROSS_RUN_EVENTS.contains(&event) {
                    continue;
                }
            }
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if value.get("schema_version").and_then(Value::as_i64) != Some(SCHEMA_VERSION) {
                continue;
            }
            match value.get("event").and_then(Value::as_str) {
                Some("run_start") => {
                    let source = value.get("source");
                    if source.and_then(|s| text(s, "status")) == Some("available") {
                        revision = source.and_then(|s| text(s, "revision")).map(str::to_string);
                    }
                }
                Some("entry_start") if text(&value, "name") == Some(name) => starts.push(value),
                Some("entry_end") if text(&value, "name") == Some(name) => {
                    ends.insert(
                        (
                            int(&value, "occurrence_id").unwrap_or(-1),
                            int(&value, "worker_attempt").unwrap_or(-1),
                            int(&value, "part").unwrap_or(0),
                        ),
                        ms(&value),
                    );
                }
                Some("entry_result") => {
                    if let (Some(occurrence), Some(attempt), Some(verdict)) = (
                        int(&value, "occurrence_id"),
                        int(&value, "worker_attempt"),
                        text(&value, "verdict"),
                    ) {
                        results.insert((occurrence, attempt), verdict.to_string());
                    }
                }
                _ => {}
            }
        }
        for start in starts {
            let occurrence = int(&start, "occurrence_id").unwrap_or(-1);
            let attempt = int(&start, "worker_attempt").unwrap_or(-1);
            let part = int(&start, "part").unwrap_or(0);
            let end = ends.get(&(occurrence, attempt, part)).copied();
            let verdict = results.get(&(occurrence, attempt)).cloned();
            rows.push(json!({
                "run_id": run_id,
                "source_revision": revision,
                "name": name,
                "occurrence_id": occurrence,
                "worker_attempt": attempt,
                "part": part,
                "verdict": verdict,
                "status": if end.is_some() && verdict.is_some() { "FINAL" } else { "INCOMPLETE" },
                "entry_elapsed_ms": end.map(|at| at - ms(&start)),
            }));
        }
    }
    Ok((rows, scanned, unreadable))
}

fn render_cross_run(name: &str, rows: &[Value], scanned: usize, unreadable: usize, format: &str) -> Result<String> {
    let mut measured: Vec<i64> = rows
        .iter()
        .filter_map(|row| row["entry_elapsed_ms"].as_i64())
        .collect();
    measured.sort_unstable();
    let summary = json!({
        "runs_scanned": scanned,
        "runs_unreadable": unreadable,
        "rows": rows.len(),
        "measured": measured.len(),
        "min_ms": measured.first(),
        "median_ms": upper_median(&measured),
        "max_ms": measured.last(),
    });
    if format == "json" {
        return Ok(serde_json::to_string_pretty(&json!({
            "mode": "cross_run",
            "name": name,
            "summary": summary,
            "rows": rows,
            "envelope_qualifier": ENVELOPE_QUALIFIER,
        }))?);
    }
    let mut out = String::new();
    out.push_str(&format!(
        "`{name}` across {scanned} retained run(s): {} envelope(s), {} measured\n",
        rows.len(),
        measured.len()
    ));
    if unreadable > 0 {
        out.push_str(&format!("  {unreadable} run(s) could not be read\n"));
    }
    out.push_str(&format!("note: {ENVELOPE_QUALIFIER}.\n\n"));
    for row in rows {
        out.push_str(&format!(
            "  {:<26}  {:<10}  occ {:<5} attempt {:<3} part {:<2}  {:<10}  {:<10}  {}\n",
            row["run_id"].as_str().unwrap_or("?"),
            row["source_revision"]
                .as_str()
                .map(|r| r.chars().take(10).collect::<String>())
                .unwrap_or_else(|| "unavailable".into()),
            row["occurrence_id"],
            row["worker_attempt"],
            row["part"],
            row["verdict"].as_str().unwrap_or("-"),
            row["status"].as_str().unwrap_or("-"),
            row["entry_elapsed_ms"]
                .as_i64()
                .map(|v| format!("{v} ms"))
                .unwrap_or_else(|| "-".into()),
        ));
    }
    if !measured.is_empty() {
        out.push_str(&format!(
            "\n  min {} ms / median {} ms / max {} ms\n",
            measured[0],
            upper_median(&measured).unwrap_or(0),
            measured[measured.len() - 1]
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(
    log_dir: Option<&str>,
    run_id: Option<&str>,
    entry_name: Option<&str>,
    top: usize,
    format: &str,
) -> Result<()> {
    if top == 0 {
        bail!("--top must be greater than zero")
    }
    if !matches!(format, "pretty" | "json") {
        bail!("sim-report format must be `pretty` or `json`, got `{format}`")
    }
    let root = resolve_root(log_dir);

    if let Some(name) = entry_name {
        let (rows, scanned, unreadable) = cross_run_rows(&root, name)?;
        print!("{}", render_cross_run(name, &rows, scanned, unreadable, format)?);
        return Ok(());
    }

    let (id, dir) = resolve_run(&root, run_id)?;
    let record = RunRecord::load(&id, &dir)?;
    let analysis = analyze(&record, top);
    match format {
        "json" => println!("{}", analysis.render_json()?),
        _ => print!("{}", analysis.render_text()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_reasons_collapse_to_the_classes_the_history_contains() {
        assert_eq!(isolation_class(None), "none");
        assert_eq!(isolation_class(Some("")), "none");
        assert_eq!(
            isolation_class(Some("declares @sim_env(SIM_WORKER_ISOLATED)")),
            "SIM_WORKER_ISOLATED"
        );
        assert_eq!(
            isolation_class(Some(
                "@sim_env(\"MAG_TELEMETRY_ROOT\", \"$OUT/telemetry\") interpolates $OUT, \
                 which resolves before the process starts"
            )),
            "$OUT interpolation"
        );
        assert_eq!(
            isolation_class(Some("hot-reload lane: mutates the one session and overlay")),
            "hot-reload"
        );
    }

    #[test]
    fn boot_signatures_render_as_their_environment_difference() {
        assert_eq!(render_signature(&json!([])), "<default>");
        assert_eq!(
            render_signature(&json!([["SIM_SKIP_RENDER", "0"], ["STONE_WINDOW", "1440x900"]])),
            "SIM_SKIP_RENDER=0,STONE_WINDOW=1440x900"
        );
    }

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
    fn the_event_peek_matches_a_full_parse() {
        let line = r#"{"elapsed_ms":170,"event":"run_start","run_id":"x","schema_version":1}"#;
        assert_eq!(peek_event(line), Some("run_start"));
        assert_eq!(peek_event("not json"), None);
    }
}
