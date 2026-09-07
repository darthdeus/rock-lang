//! Persistent lifecycle records for ordinary `rock sim` invocations.
//!
//! The recorder is deliberately parent-side and boundary-oriented. It writes
//! one JSON object per line, so an interrupted invocation keeps the evidence
//! observed before the process stopped. Recording failures are diagnostics and
//! never change the sim result.

use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

mod timing;

const SCHEMA_VERSION: u64 = 1;

#[derive(Clone)]
pub struct SimRunRecorder {
    inner: Arc<Mutex<State>>,
}

struct State {
    run_id: String,
    events_path: Option<PathBuf>,
    writer: Option<BufWriter<File>>,
    started: Instant,
    next_occurrence_id: u64,
    next_worker_id: u64,
    planned: usize,
    completed: BTreeSet<u64>,
    finished: bool,
    warning_emitted: bool,
    timings: timing::Timings,
}

static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

impl SimRunRecorder {
    /// Start a recording under the configured history root. Relative roots
    /// are resolved against the caller's current directory; discovering the
    /// workspace does not change that directory.
    pub fn begin(
        project: &str,
        selection: &str,
        quiet: bool,
        requested_jobs: Option<usize>,
        requested_mode: &str,
    ) -> Self {
        let root = std::env::var_os("ROCK_SIM_LOG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".sim-runs"));
        Self::begin_at_with_root(&root, project, selection, quiet, requested_jobs, requested_mode)
    }

    /// Start a recording under an explicit root. This is public so the
    /// recorder contract can be tested without writing into the repository.
    pub fn begin_at(
        root: &Path,
        project: &str,
        selection: &str,
        quiet: bool,
        requested_jobs: Option<usize>,
        requested_mode: &str,
    ) -> Self {
        Self::begin_at_with_root(root, project, selection, quiet, requested_jobs, requested_mode)
    }

    fn begin_at_with_root(
        root: &Path,
        project: &str,
        selection: &str,
        quiet: bool,
        requested_jobs: Option<usize>,
        requested_mode: &str,
    ) -> Self {
        let started = Instant::now();
        let started_at_unix_ms = unix_now_ms();
        let (run_id, events_path, writer, initial_warning) = match create_history(root) {
            Ok((run_id, events_path, writer)) => (run_id, Some(events_path), Some(writer), None),
            Err(e) => (
                String::new(),
                None,
                None,
                Some(format!("could not create persistent sim history under `{}`: {e:#}", root.display())),
            ),
        };
        let timing_host = timing::host_context();
        let recorder = Self {
            inner: Arc::new(Mutex::new(State {
                run_id,
                events_path,
                writer,
                started,
                next_occurrence_id: 1,
                next_worker_id: 1,
                planned: 0,
                completed: BTreeSet::new(),
                finished: false,
                warning_emitted: false,
                timings: timing::Timings::new(root, timing::TimingPolicy::from_env(), timing_host.clone()),
            })),
        };
        if let Some(message) = initial_warning {
            recorder.warn_once(message);
        } else {
            recorder.event(
                "run_start",
                json!({
                    "timing_host": timing_host,
                    "project_arg": project,
                    "selection": selection,
                    "quiet": quiet,
                    "requested_jobs": requested_jobs,
                    "requested_mode": requested_mode,
                    "started_at_unix_ms": started_at_unix_ms,
                    "source": source_metadata(),
                }),
            );
        }
        recorder
    }

    pub fn events_path(&self) -> Option<PathBuf> {
        self.inner.lock().ok().and_then(|state| state.events_path.clone())
    }

    /// Allocate one logical occurrence ID for each requested entry. Duplicate
    /// names intentionally receive different IDs; recovery reuses the ID from
    /// the original plan rather than creating a second logical entry.
    pub fn plan_entries(&self, specs: &[String]) -> Vec<u64> {
        let mut ids = Vec::with_capacity(specs.len());
        for (sequence, spec) in specs.iter().enumerate() {
            let id = match self.inner.lock() {
                Ok(mut state) => {
                    let id = state.next_occurrence_id;
                    state.next_occurrence_id += 1;
                    state.planned += 1;
                    id
                }
                Err(_) => {
                    self.warn_once("sim history lock was poisoned while planning entries".into());
                    continue;
                }
            };
            ids.push(id);
            self.event(
                "entry_planned",
                json!({
                    "occurrence_id": id,
                    "sequence": sequence,
                    "spec": spec,
                    "parts": spec.split(',').map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>(),
                }),
            );
        }
        ids
    }

    pub fn configure(
        &self,
        project_path: &Path,
        resolved_entries: &[String],
        requested_jobs: Option<usize>,
        effective_jobs: usize,
        requested_mode: &str,
        effective_mode: &str,
        update_snapshots: bool,
    ) {
        self.event(
            "run_config",
            json!({
                "project_path": project_path,
                "resolved_entries": resolved_entries,
                "requested_jobs": requested_jobs,
                "effective_jobs": effective_jobs,
                "requested_mode": requested_mode,
                "effective_mode": effective_mode,
                "update_snapshots": update_snapshots,
            }),
        );
    }

    pub fn phase_start(&self, phase: &str) {
        self.event("phase_start", json!({ "phase": phase }));
    }

    pub fn phase_end(&self, phase: &str, success: bool) {
        self.event(
            "phase_end",
            json!({ "phase": phase, "success": success }),
        );
    }

    pub fn phase<T>(&self, name: &str, operation: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        self.phase_start(name);
        match operation() {
            Ok(value) => {
                self.phase_end(name, true);
                Ok(value)
            }
            Err(error) => {
                self.phase_end(name, false);
                Err(error)
            }
        }
    }

    pub fn lane_planned(
        &self,
        lane_id: usize,
        kind: &str,
        entries: &[(u64, String)],
        signature: &[(String, String)],
        reason: Option<&str>,
    ) {
        self.event(
            "lane_planned",
            json!({
                "lane_id": lane_id,
                "kind": kind,
                "entries": entries.iter().map(|(id, name)| json!({
                    "occurrence_id": id,
                    "name": name,
                })).collect::<Vec<_>>(),
                "boot_signature": signature,
                "isolation_reason": reason,
            }),
        );
    }

    pub fn worker_spawn(
        &self,
        entries: &[(u64, String)],
        signature: &[(String, String)],
        kind: &str,
    ) -> u64 {
        let worker_id = match self.inner.lock() {
            Ok(mut state) => {
                let id = state.next_worker_id;
                state.next_worker_id += 1;
                id
            }
            Err(_) => {
                self.warn_once("sim history lock was poisoned while allocating a worker ID".into());
                0
            }
        };
        self.event(
            "worker_spawn",
            json!({
                "worker_attempt": worker_id,
                "kind": kind,
                "entries": entries.iter().map(|(id, name)| json!({
                    "occurrence_id": id,
                    "name": name,
                })).collect::<Vec<_>>(),
                "boot_signature": signature,
            }),
        );
        worker_id
    }

    pub fn worker_started(&self, worker_id: u64, pid: u32) {
        self.event(
            "worker_started",
            json!({ "worker_attempt": worker_id, "pid": pid }),
        );
    }

    pub fn worker_first_entry(&self, worker_id: u64, occurrence_id: u64, name: &str, part: usize) {
        self.event(
            "worker_first_entry",
            json!({
                "worker_attempt": worker_id,
                "occurrence_id": occurrence_id,
                "name": name,
                "part": part,
            }),
        );
    }

    pub fn entry_runtime_context(&self, worker_id: u64, occurrence_id: u64, part: usize, context: Value) {
        self.event("entry_runtime_context", json!({ "worker_attempt": worker_id,
            "occurrence_id": occurrence_id, "part": part, "context": context }));
    }

    pub fn entry_start(&self, worker_id: u64, occurrence_id: u64, name: &str, part: usize) {
        self.event(
            "entry_start",
            json!({
                "worker_attempt": worker_id,
                "occurrence_id": occurrence_id,
                "name": name,
                "part": part,
            }),
        );
    }

    pub fn entry_end(&self, worker_id: u64, occurrence_id: u64, name: &str, part: usize) {
        self.event(
            "entry_end",
            json!({
                "worker_attempt": worker_id,
                "occurrence_id": occurrence_id,
                "name": name,
                "part": part,
            }),
        );
    }

    pub fn entry_result(
        &self,
        occurrence_id: u64,
        worker_id: u64,
        verdict: &str,
        frame_count: Option<usize>,
        expects_pass: usize,
        timed_out: bool,
        exit_code: Option<i32>,
    ) {
        if let Ok(mut state) = self.inner.lock() {
            state.completed.insert(occurrence_id);
        } else {
            self.warn_once("sim history lock was poisoned while recording an entry result".into());
        }
        self.event(
            "entry_result",
            json!({
                "occurrence_id": occurrence_id,
                "worker_attempt": worker_id,
                "verdict": verdict,
                "frame_count": frame_count,
                "expects_pass": expects_pass,
                "timed_out": timed_out,
                "exit_code": exit_code,
            }),
        );
    }

    pub fn artifact_check_start(&self, occurrence_id: u64) {
        self.event("artifact_check_start", json!({ "occurrence_id": occurrence_id }));
    }

    pub fn artifact_check_end(&self, occurrence_id: u64, success: bool) {
        self.event(
            "artifact_check_end",
            json!({ "occurrence_id": occurrence_id, "success": success }),
        );
    }

    pub fn worker_end(
        &self,
        worker_id: u64,
        status: &str,
        exit_code: Option<i32>,
        signal: Option<i32>,
        timed_out: bool,
    ) {
        self.event(
            "worker_end",
            json!({
                "worker_attempt": worker_id,
                "status": status,
                "exit_code": exit_code,
                "signal": signal,
                "timed_out": timed_out,
            }),
        );
    }

    pub fn recovery(
        &self,
        round: usize,
        cause: &str,
        prior_worker: Option<u64>,
        responsible_occurrence_id: Option<u64>,
        successors: &[(u64, String)],
    ) {
        self.event(
            "worker_recovery",
            json!({
                "round": round,
                "cause": cause,
                "prior_worker_attempt": prior_worker,
                "responsible_occurrence_id": responsible_occurrence_id,
                "successors": successors.iter().map(|(id, name)| json!({
                    "occurrence_id": id,
                    "name": name,
                })).collect::<Vec<_>>(),
            }),
        );
    }

    /// Write the terminal record. A missing terminal event is intentionally
    /// meaningful: it identifies a process that stopped before the runner
    /// could account for it.
    pub fn finish(&self, success: bool, error: Option<&str>) {
        let (planned, finalized, already_finished) = match self.inner.lock() {
            Ok(mut state) => {
                let already_finished = state.finished;
                state.finished = true;
                (state.planned, state.completed.len(), already_finished)
            }
            Err(_) => {
                self.warn_once("sim history lock was poisoned while finishing the run".into());
                return;
            }
        };
        if already_finished {
            return;
        }
        self.event(
            "run_end",
            json!({
                "status": if !success {
                    "FAIL"
                } else if planned == finalized {
                    "PASS"
                } else {
                    "INCOMPLETE"
                },
                "error": error,
                "planned_entries": planned,
                "finalized_entries": finalized,
                "complete": planned == finalized,
            }),
        );
        if let Some(path) = self.events_path() {
            log::info!(target: "SIM", "sim history: {}", path.display());
        }
    }

    fn event(&self, name: &str, fields: Value) {
        let mut warning = None;
        let mut timing_messages = Vec::new();
        match self.inner.lock() {
            Ok(mut state) => {
                let elapsed_ms = state.started.elapsed().as_millis() as u64;
                let timing_warnings = state.timings.observe(name, &fields, elapsed_ms);
                let mut records = vec![(name, fields.clone())];
                for timing in timing_warnings {
                    timing_messages.push(state.timings.message(&timing));
                    records.push(("timing_warning", json!({ "name": timing.label,
                        "kind": match timing.kind {
                            timing::TimingWarningKind::Absolute => "absolute",
                            timing::TimingWarningKind::Historical => "historical",
                        },
                        "duration_ms": timing.duration_ms, "baseline_ms": timing.baseline_ms })));
                }
                for (name, fields) in records {
                    let mut object = fields.as_object().cloned().unwrap_or_default();
                    object.insert("schema_version".into(), Value::from(SCHEMA_VERSION));
                    object.insert("event".into(), Value::from(name));
                    object.insert("run_id".into(), Value::from(state.run_id.clone()));
                    object.insert("elapsed_ms".into(), Value::from(elapsed_ms));
                    if let Some(writer) = state.writer.as_mut() {
                        if let Err(e) = serde_json::to_writer(&mut *writer, &Value::Object(object))
                            .map_err(std::io::Error::other)
                            .and_then(|_| writer.write_all(b"\n"))
                            .and_then(|_| writer.flush())
                        {
                            state.writer = None;
                            if !state.warning_emitted {
                                state.warning_emitted = true;
                                warning = Some(format!("sim history write failed: {e:#}"));
                            }
                        }
                    }
                }
                if name == "run_end" {
                    if let Some(path) = state.events_path.as_ref() {
                        if let Err(e) = state.timings.save(&path.with_file_name("timings.json"), &fields) {
                            warning = Some(format!("sim timing summary write failed: {e}"));
                        }
                    }
                }
            }
            Err(_) => warning = Some(format!("sim history lock was poisoned while writing `{name}`")),
        }
        for message in timing_messages {
            log::warn!(target: "SIM", "{message}");
        }
        if let Some(message) = warning {
            log::warn!(target: "SIM", "{message}; sim verdicts are unchanged");
        }
    }

    fn warn_once(&self, message: String) {
        let should_emit = self
            .inner
            .lock()
            .map(|mut state| {
                if state.warning_emitted {
                    false
                } else {
                    state.warning_emitted = true;
                    true
                }
            })
            .unwrap_or(true);
        if should_emit {
            log::warn!(target: "SIM", "{message}; sim verdicts are unchanged");
        }
    }
}

fn create_history(root: &Path) -> anyhow::Result<(String, PathBuf, BufWriter<File>)> {
    fs::create_dir_all(root)
        .map_err(|e| anyhow::anyhow!("creating history root: {e}"))?;
    let stamp = unix_now_ms().unwrap_or(0);
    let pid = std::process::id();
    for _ in 0..1000 {
        let counter = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let run_id = format!("{stamp}-{pid}-{counter}");
        let dir = root.join(&run_id);
        match fs::create_dir(&dir) {
            Ok(()) => {
                let events_path = dir.join("events.jsonl");
                let file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&events_path)
                    .map_err(|e| anyhow::anyhow!("creating `{}`: {e}", events_path.display()))?;
                return Ok((run_id, events_path, BufWriter::new(file)));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(anyhow::anyhow!("creating `{}`: {e}", dir.display())),
        }
    }
    Err(anyhow::anyhow!("could not allocate a unique run directory"))
}

fn unix_now_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn source_metadata() -> Value {
    let source_root = std::env::current_dir().ok();
    let revision = source_root
        .as_deref()
        .and_then(|root| git_output(root, &["rev-parse", "HEAD"]));
    let dirty = revision.as_ref().and_then(|_| {
        source_root.as_deref().and_then(|root| {
            git_output(root, &["status", "--porcelain", "--untracked-files=normal"])
                .map(|status| !status.is_empty())
        })
    });
    json!({
        "revision": revision,
        "dirty": dirty,
        "status": if revision.is_some() { "available" } else { "unavailable" },
    })
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).current_dir(root).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_history_directories_are_created() {
        let root = tempfile::tempdir().expect("history root");
        let a = SimRunRecorder::begin_at(root.path(), "p", "a", false, None, "single");
        let b = SimRunRecorder::begin_at(root.path(), "p", "b", false, None, "single");
        assert_ne!(a.events_path(), b.events_path());
    }
}
