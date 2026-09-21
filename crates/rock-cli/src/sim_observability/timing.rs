use std::collections::HashMap;
use std::path::Path;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct TimingPolicy {
    pub(super) absolute_ms: Option<u64>,
    pub(super) regression_factor: Option<f64>,
    pub(super) regression_delta_ms: Option<u64>,
    pub(super) minimum_samples: Option<usize>,
}

/// What has to match before two samples of one sim describe the same cost.
///
/// Every field answers the same question: does it change what the sim costs,
/// or only where the scheduler happened to put it? The worker's slot index and
/// sibling composition answer the second, and they move on nearly every run,
/// so they are not here. What they were standing in for is real and is kept as
/// `worker_cold_start`: the first entry in a worker pays process startup, JIT
/// warmup and first asset load, and the ones after it do not.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct TimingIdentity {
    pub(super) project: Option<String>,
    pub(super) workload: Option<String>,
    pub(super) runtime: Option<String>,
    pub(super) machine: Option<String>,
    pub(super) backend: Option<String>,
    pub(super) renderer: Option<String>,
    pub(super) mode: Option<String>,
    pub(super) jobs: Option<usize>,
    pub(super) worker_kind: Option<String>,
    pub(super) worker_cold_start: Option<bool>,
    pub(super) boot_signature: Option<Vec<(String, String)>>,
}

impl TimingIdentity {
    fn comparable(&self) -> bool {
        self.workload.is_some()
            && self.runtime.is_some()
            && self.project.is_some()
            && self.machine.is_some()
            && self.backend.is_some()
            && self.renderer.is_some()
            && self.mode.is_some()
            && self.jobs.is_some()
            && self.worker_kind.is_some()
            && self.worker_cold_start.is_some()
            && self.boot_signature.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TimingWarningKind {
    Absolute,
    Historical,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct TimingWarning {
    pub(super) kind: TimingWarningKind,
    pub(super) label: String,
    pub(super) duration_ms: u64,
    pub(super) baseline_ms: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct HistoricalTimings {
    samples: HashMap<(String, String), Vec<u64>>,
}

impl HistoricalTimings {
    #[cfg(test)]
    fn insert(&mut self, label: &str, identity: TimingIdentity, duration_ms: u64) {
        self.samples
            .entry((label.to_string(), identity.key()))
            .or_default()
            .push(duration_ms);
    }

    fn samples(&self, label: &str, identity: &TimingIdentity) -> &[u64] {
        self.samples
            .get(&(label.to_string(), identity.key()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

fn evaluate_entry(
    label: &str,
    duration_ms: u64,
    policy: &TimingPolicy,
    baseline: &[u64],
) -> Vec<TimingWarning> {
    let mut warnings = Vec::new();
    if policy.absolute_ms.is_some_and(|limit| duration_ms >= limit) {
        warnings.push(TimingWarning { kind: TimingWarningKind::Absolute,
            label: label.into(), duration_ms, baseline_ms: None });
    }
    if let (Some(factor), Some(delta), Some(count)) =
        (policy.regression_factor, policy.regression_delta_ms, policy.minimum_samples)
    {
        if count > 0 && baseline.len() >= count {
            // The first accepted population stays fixed as later revisions slow down.
            let mut values = baseline[..count].to_vec();
            values.sort_unstable();
            let median = if count % 2 == 0 {
                (values[count / 2 - 1] as f64 + values[count / 2] as f64) / 2.0
            } else { values[count / 2] as f64 };
            if duration_ms as f64 > median * factor
                && duration_ms as f64 - median >= delta as f64
            {
                warnings.push(TimingWarning { kind: TimingWarningKind::Historical,
                    label: label.into(), duration_ms, baseline_ms: Some(median) });
            }
        }
    }
    warnings
}

impl TimingIdentity {
    fn key(&self) -> String {
        format!("{:x}", Sha256::digest(format!("{self:?}")))
    }
}

impl TimingPolicy {
    pub(super) fn from_env() -> Self {
        Self::from_settings(|name| std::env::var(name).ok())
    }

    fn from_settings(setting: impl Fn(&str) -> Option<String>) -> Self {
        let number = |name: &str, minimum: f64, default: f64| -> Option<f64> {
            let Some(value) = setting(name) else { return Some(default); };
            match value.parse::<f64>() {
                Ok(n) if n.is_finite() && n >= minimum && n < u64::MAX as f64 / 1000.0 => Some(n),
                _ => {
                    log::warn!(target: "SIM", "invalid {name}={value:?}; timing warning setting disabled");
                    None
                }
            }
        };
        let policy = Self {
            absolute_ms: number("ROCK_SIM_WARN_SECS", 0.001, 10.0).map(|n| (n * 1000.0) as u64),
            regression_factor: number("ROCK_SIM_WARN_FACTOR", 1.0, 2.0),
            regression_delta_ms: number("ROCK_SIM_WARN_DELTA_SECS", 0.001, 1.0).map(|n| (n * 1000.0) as u64),
            minimum_samples: number("ROCK_SIM_WARN_SAMPLES", 1.0, 5.0).and_then(|n| {
                if n.fract() == 0.0 && n <= usize::MAX as f64 { Some(n as usize) }
                else { log::warn!(target: "SIM", "ROCK_SIM_WARN_SAMPLES must be an integer; historical warnings disabled"); None }
            }),
        };
        let configured = [policy.regression_factor.is_some(), policy.regression_delta_ms.is_some(), policy.minimum_samples.is_some()];
        if configured.iter().any(|v| *v) && !configured.iter().all(|v| *v) {
            log::warn!(target: "SIM", "historical warnings require ROCK_SIM_WARN_FACTOR, ROCK_SIM_WARN_DELTA_SECS and ROCK_SIM_WARN_SAMPLES");
        }
        policy
    }
}

/// The environment settings that describe what a sim costs. Settings that only
/// steer where the recorder writes are excluded, because the whole host block
/// is hashed into every timing identity.
fn ambient_settings(
    vars: impl Iterator<Item = (String, String)>,
) -> std::collections::BTreeMap<String, String> {
    vars.filter(|(key, _)| {
        ["STONE_", "SIM_", "MAG_", "ROCK_", "SDL_"].iter().any(|prefix| key.starts_with(prefix))
            && key != "ROCK_SIM_LOG_DIR" && !key.starts_with("ROCK_SIM_WARN_")
            // `rock-log` stamps a fresh wall-clock origin here at process
            // entry so a driver and its respawned children share one log
            // clock. It is different on every invocation and says nothing
            // about cost; recorded, it would make every identity unique.
            // `run_start` already carries `started_at_unix_ms`.
            && key != "ROCK_LOG_EPOCH_MS"
    }).collect()
}

pub(super) fn host_context() -> Value {
    let machine = std::process::Command::new("uname").arg("-srmn").output().ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok());
    json!({ "machine": machine, "arch": std::env::consts::ARCH,
        "logical_cpus": std::thread::available_parallelism().ok().map(|n| n.get()),
        "ambient": ambient_settings(std::env::vars()) })
}

type EntryKey = (u64, u64, usize);

pub(super) struct Timings {
    policy: TimingPolicy,
    history: HistoricalTimings,
    /// The most recent finished duration of each (project, entry name),
    /// under any identity or verdict. Scheduling input, never a baseline.
    latest: HashMap<(String, String), u64>,
    host: Value,
    config: Value,
    source_clean: bool,
    quiet: Value,
    workers: HashMap<u64, Value>,
    runtime: HashMap<EntryKey, Value>,
    active: HashMap<EntryKey, (u64, String, Option<TimingIdentity>)>,
    phases: HashMap<String, u64>,
    results: HashMap<u64, bool>,
    rows: Vec<Value>,
}

impl Timings {
    pub(super) fn new(root: &Path, policy: TimingPolicy, host: Value) -> Self {
        let mut history = HistoricalTimings::default();
        let mut latest = HashMap::new();
        match std::fs::read_dir(root) {
            Ok(dirs) => {
                let mut paths = dirs.filter_map(|dir| match dir {
                    Ok(dir) => Some(dir.path().join("timings.json")),
                    Err(error) => { log::warn!(target: "SIM", "cannot inspect timing history: {error}"); None }
                }).collect::<Vec<_>>();
                // Run ids start with a fixed-width millisecond stamp, so this is
                // oldest first and a later record overwrites `latest`.
                paths.sort();
                for path in paths {
                    let data = match std::fs::read(&path) {
                        Ok(data) => data,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(e) => { log::warn!(target: "SIM", "cannot read {}: {e}", path.display()); continue; }
                    };
                    let value: Value = match serde_json::from_slice(&data) {
                        Ok(value) => value,
                        Err(e) => { log::warn!(target: "SIM", "invalid timing history {}: {e}", path.display()); continue; }
                    };
                    if value["schema_version"] != 1 { continue; }
                    if let Some(project) = value["project"].as_str() {
                        for row in value["entries"].as_array().into_iter().flatten() {
                            if row["completed"] != true { continue; }
                            if let (Some(name), Some(ms)) = (row["name"].as_str(), row["duration_ms"].as_u64()) {
                                latest.insert((project.to_string(), name.to_string()), ms);
                            }
                        }
                    }
                    let Some(count) = policy.minimum_samples else { continue };
                    // Records written before per-entry acceptance have no
                    // `baseline_eligible`; for them the old run-level
                    // `accepted` is the only answer available, and it is
                    // exactly the set they already contributed.
                    if !value["baseline_eligible"].as_bool()
                        .unwrap_or_else(|| value["accepted"] == true) { continue; }
                    for row in value["entries"].as_array().into_iter().flatten() {
                        if row["passed"] != true || row["completed"] != true { continue; }
                        if let (Some(name), Some(key), Some(ms)) =
                            (row["name"].as_str(), row["identity"].as_str(), row["duration_ms"].as_u64()) {
                            let samples = history.samples.entry((name.into(), key.into())).or_default();
                            if samples.len() < count { samples.push(ms); }
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => log::warn!(target: "SIM", "cannot inspect timing history: {e}"),
        }
        Self { policy, history, latest, host, config: Value::Null, source_clean: false, quiet: Value::Null,
            workers: HashMap::new(), runtime: HashMap::new(), active: HashMap::new(),
            phases: HashMap::new(), results: HashMap::new(), rows: Vec::new() }
    }

    fn identity(&self, key: EntryKey) -> Option<TimingIdentity> {
        let worker = self.workers.get(&key.0)?;
        let runtime = self.runtime.get(&key)?;
        if runtime["width"].as_u64()? == 0 || runtime["height"].as_u64()? == 0
            || runtime["jit"].as_str()?.is_empty() || runtime["wgpu"].as_bool().is_none() { return None; }
        self.host["machine"].as_str()?;
        let entries = worker["entries"].as_array()?;
        let position = entries.iter().position(|entry| entry["occurrence_id"] == key.1)?;
        let id = TimingIdentity {
            project: self.config["project_path"].as_str().map(str::to_owned),
            workload: Some(json!({ "config": self.config, "quiet": self.quiet }).to_string()), runtime: Some(json!({ "context": runtime, "part": key.2 }).to_string()),
            machine: Some(self.host.to_string()), backend: Some("skull".into()),
            renderer: Some(if runtime["wgpu"] == true { "wgpu" } else { "gl" }.into()),
            mode: self.config["effective_mode"].as_str().map(str::to_owned),
            jobs: self.config["effective_jobs"].as_u64().map(|n| n as usize),
            worker_kind: worker["kind"].as_str().map(str::to_owned),
            worker_cold_start: Some(position == 0),
            boot_signature: serde_json::from_value(worker["boot_signature"].clone()).ok(),
        };
        id.comparable().then_some(id)
    }

    fn complete(
        &mut self,
        key: EntryKey,
        now: u64,
        completed: bool,
        frame_count: Option<Value>,
    ) -> Vec<TimingWarning> {
        let Some((start, name, identity)) = self.active.remove(&key) else { return Vec::new(); };
        let ms = now.saturating_sub(start);
        let baseline = identity.as_ref().map(|id| self.history.samples(&name, id)).unwrap_or(&[]);
        let warnings = evaluate_entry(&name, ms, &self.policy, baseline);
        self.rows.push(json!({ "name": name, "occurrence_id": key.1, "part": key.2,
            "duration_ms": ms, "completed": completed, "frame_count": frame_count.unwrap_or(Value::Null),
            "identity": identity.map(|id| id.key()) }));
        warnings
    }

    pub(super) fn observe(&mut self, name: &str, fields: &Value, now: u64) -> Vec<TimingWarning> {
        let key = (fields["worker_attempt"].as_u64().unwrap_or(0),
            fields["occurrence_id"].as_u64().unwrap_or(0), fields["part"].as_u64().unwrap_or(0) as usize);
        match name {
            "run_start" => {
                self.source_clean = fields["source"]["dirty"] == false;
                self.quiet = fields["quiet"].clone();
            },
            "run_config" => self.config = fields.clone(),
            "worker_spawn" => { self.workers.insert(key.0, fields.clone()); },
            "entry_runtime_context" => { self.runtime.insert(key, fields["context"].clone()); },
            "entry_start" => {
                let identity = self.identity(key);
                self.active.insert(key, (now, fields["name"].as_str().unwrap_or("unknown").into(), identity));
            },
            "entry_end" => return self.complete(key, now, true, fields.get("frame_count").cloned()),
            "entry_result" => {
                self.results.insert(key.1, fields["verdict"] == "PASS" && fields["timed_out"] == false);
                let unfinished = self.active.keys().filter(|entry| entry.1 == key.1).copied().collect::<Vec<_>>();
                return unfinished.into_iter().flat_map(|entry| self.complete(entry, now, false, None)).collect();
            },
            "phase_start" => { if let Some(phase) = fields["phase"].as_str() { self.phases.insert(phase.into(), now); } },
            "phase_end" => {
                if let Some(phase) = fields["phase"].as_str() {
                    if let Some(start) = self.phases.remove(phase) {
                        return evaluate_entry(&format!("phase {phase}"), now.saturating_sub(start), &self.policy, &[]);
                    }
                }
            },
            _ => {},
        }
        Vec::new()
    }

    /// Whether this run's passing, completed rows may become baseline samples.
    ///
    /// AUTHOR DECISION: SIM TIMING BASELINE ACCEPTANCE, resolved per entry. The
    /// unit being measured is the entry, so the run's PASS/FAIL verdict - a
    /// fact about the *other* entries - does not veto it. The two conditions
    /// kept are not statements about siblings:
    ///
    /// * `source_clean`: a dirty tree may hold modified production code, so its
    ///   durations belong to no commit and cannot be compared against one.
    /// * `complete`: this is `planned == finalized`, so an incomplete run is one
    ///   the driver never accounted for - a global deadline, a kill, an
    ///   interrupt. Those causes press on the whole process, biasing every entry
    ///   in the run the same way, and a median cannot average out a bias its
    ///   samples share.
    ///
    /// `accepted` is left alone rather than expressed as `baseline_eligible &&
    /// PASS`, which it happens to equal today: `rock sim-report` prints it and
    /// 750 records already carry it, so a later change to baseline eligibility
    /// must not silently move what those records meant.
    fn baseline_eligible(&self, finished: &Value) -> bool {
        self.source_clean && finished["complete"] == true
    }

    pub(super) fn save(&self, path: &Path, finished: &Value) -> std::io::Result<()> {
        let entries = self.rows.iter().map(|row| {
            let mut row = row.clone();
            row["passed"] = json!(self.results.get(&row["occurrence_id"].as_u64().unwrap_or(0)) == Some(&true));
            row
        }).collect::<Vec<_>>();
        let summary = json!({ "schema_version": 1, "host": self.host,
            "project": self.config["project_path"],
            "warning_policy": { "absolute_ms": self.policy.absolute_ms,
                "regression_factor": self.policy.regression_factor,
                "regression_delta_ms": self.policy.regression_delta_ms,
                "minimum_samples": self.policy.minimum_samples },
            "accepted": self.source_clean && finished["complete"] == true && finished["status"] == "PASS",
            "baseline_eligible": self.baseline_eligible(finished),
            "entries": entries });
        let pending = path.with_extension("json.pending");
        std::fs::write(&pending, serde_json::to_vec(&summary)?)?;
        std::fs::rename(pending, path)
    }

    pub(super) fn latest_duration_ms(&self, name: &str) -> Option<u64> {
        let project = self.config["project_path"].as_str()?;
        self.latest.get(&(project.to_string(), name.to_string())).copied()
    }

    pub(super) fn message(&self, warning: &TimingWarning) -> String {
        if let Some(baseline) = warning.baseline_ms {
            format!("sim timing regression: {} took {:.3}s; baseline median {:.3}s from {} accepted samples (>{:.2}x and +{:.3}s)",
                warning.label, warning.duration_ms as f64 / 1000.0, baseline / 1000.0,
                self.policy.minimum_samples.unwrap_or(0), self.policy.regression_factor.unwrap_or(0.0),
                self.policy.regression_delta_ms.unwrap_or(0) as f64 / 1000.0)
        } else {
            format!("slow sim: {} took {:.3}s (warning budget {:.3}s)", warning.label,
                warning.duration_ms as f64 / 1000.0, self.policy.absolute_ms.unwrap_or(0) as f64 / 1000.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dispatch order reads what an entry cost most recently in this project,
    /// whatever the tree or verdict was: a failing or dirty run still spent
    /// that long. Only an entry that never finished has no cost to report.
    #[test]
    fn the_latest_finished_duration_per_project_is_kept_for_dispatch() {
        let root = tempfile::tempdir().unwrap();
        let record = |dir: &str, project: &str, duration: u64, finished: bool| {
            let path = root.path().join(dir);
            std::fs::create_dir(&path).unwrap();
            let mut timing = recorder(root.path());
            enter(&mut timing, false, 1280);
            timing.observe("run_config", &json!({ "project_path": project, "effective_mode": "workers", "effective_jobs": 4 }), 0);
            if finished {
                end(&mut timing, duration, false);
            } else {
                timing.observe("entry_result", &json!({ "worker_attempt": 1, "occurrence_id": 1,
                    "verdict": "FAIL", "timed_out": true }), duration);
            }
            timing.save(&path.join("timings.json"), &json!({ "complete": false, "status": "FAIL" })).unwrap();
        };
        record("01", "mag", 5_000, true);
        record("02", "mag", 3_000, true);
        record("03", "testbed", 9_000, true);
        record("04", "mag", 60_000, false);
        let mut next = recorder(root.path());
        enter(&mut next, true, 1280);
        assert_eq!(next.latest_duration_ms("sim"), Some(3_000));
        assert_eq!(next.latest_duration_ms("never_ran"), None);
    }

    #[test]
    fn warning_defaults_and_independent_overrides() {
        assert_eq!(TimingPolicy::from_settings(|_| None), policy());
        let overridden = TimingPolicy::from_settings(|name| {
            (name == "ROCK_SIM_WARN_SECS").then(|| "5".into())
        });
        assert_eq!(overridden, TimingPolicy { absolute_ms: Some(5_000), ..policy() });
    }

    fn policy() -> TimingPolicy {
        TimingPolicy {
            absolute_ms: Some(10_000),
            regression_factor: Some(2.0),
            regression_delta_ms: Some(1_000),
            minimum_samples: Some(5),
        }
    }

    fn identity(machine: &str, backend: &str) -> TimingIdentity {
        TimingIdentity {
            project: Some("mag".into()),
            workload: Some("routine".into()),
            runtime: Some("1280x720 GL JIT on".into()),
            machine: Some(machine.into()),
            backend: Some(backend.into()),
            renderer: Some("gl".into()),
            mode: Some("workers".into()),
            jobs: Some(4),
            worker_kind: Some("shared".into()),
            worker_cold_start: Some(false),
            boot_signature: Some(vec![("SIM_SKIP_RENDER".into(), "1".into())]),
        }
    }

    #[test]
    fn absolute_warning_uses_explicit_threshold() {
        let p = policy();
        assert!(evaluate_entry("slow", 10_000, &p, &[])
            .iter()
            .any(|warning| warning.kind == TimingWarningKind::Absolute));
        assert!(!evaluate_entry("fast", 9_999, &p, &[])
            .iter()
            .any(|warning| warning.kind == TimingWarningKind::Absolute));
    }

    #[test]
    fn historical_warning_requires_all_regression_guards() {
        let p = policy();
        let baseline = [1_000, 1_000, 1_000, 1_000, 1_000];
        assert!(evaluate_entry("slow", 3_000, &p, &baseline)
            .iter()
            .any(|warning| warning.kind == TimingWarningKind::Historical));
        assert!(!evaluate_entry("factor_boundary", 2_000, &p, &baseline)
            .iter()
            .any(|warning| warning.kind == TimingWarningKind::Historical));
        assert!(!evaluate_entry("delta_boundary", 1_999, &p, &baseline)
            .iter()
            .any(|warning| warning.kind == TimingWarningKind::Historical));
        assert!(!evaluate_entry("too_few", 3_000, &p, &[1_000, 1_000, 1_000, 1_000])
            .iter()
            .any(|warning| warning.kind == TimingWarningKind::Historical));
    }

    #[test]
    fn historical_samples_require_exact_context_identity() {
        let mut history = HistoricalTimings::default();
        let expected = identity("studio-a", "skull");
        history.insert("sim", expected.clone(), 1_000);
        assert_eq!(history.samples("sim", &expected), &[1_000]);

        for unlike in [
            identity("studio-b", "skull"),
            identity("studio-a", "bone"),
        ] {
            assert!(history.samples("sim", &unlike).is_empty());
        }

        let mut unknown = expected;
        unknown.worker_cold_start = None;
        assert!(!unknown.comparable());
        assert!(history.samples("sim", &unknown).is_empty());
    }
    fn recorder(root: &Path) -> Timings {
        Timings::new(root, policy(), json!({ "machine": "fixture", "logical_cpus": 4 }))
    }

    fn enter(timings: &mut Timings, clean: bool, width: u64) {
        timings.observe("run_start", &json!({ "source": { "dirty": !clean } }), 0);
        timings.observe("run_config", &json!({ "project_path": "mag", "effective_mode": "workers", "effective_jobs": 4 }), 0);
        timings.observe("worker_spawn", &json!({ "worker_attempt": 1, "kind": "shared",
            "entries": [{ "occurrence_id": 1, "name": "sim" }], "boot_signature": [] }), 0);
        timings.observe("entry_runtime_context", &json!({ "worker_attempt": 1, "occurrence_id": 1, "part": 0,
            "context": { "width": width, "height": 720, "wgpu": false, "jit": "on" } }), 0);
        timings.observe("entry_start", &json!({ "worker_attempt": 1, "occurrence_id": 1, "part": 0, "name": "sim" }), 0);
    }

    fn end(timings: &mut Timings, duration: u64, passed: bool) -> Vec<TimingWarning> {
        let warnings = timings.observe("entry_end", &json!({ "worker_attempt": 1, "occurrence_id": 1, "part": 0, "frame_count": 7 }), duration);
        timings.observe("entry_result", &json!({ "worker_attempt": 1, "occurrence_id": 1,
            "verdict": if passed { "PASS" } else { "FAIL" }, "timed_out": false }), duration);
        warnings
    }

    /// Runs 0-2 are each excluded for a different reason - a dirty tree, an
    /// entry that failed, a run that was never fully accounted for - and each
    /// is slow enough to be obvious in the median if it leaked in. The run's
    /// PASS/FAIL verdict is deliberately not among them: per-entry acceptance
    /// no longer vetoes on it, and the tests above cover it instead.
    #[test]
    fn persisted_baselines_exclude_bad_runs_and_never_follow_a_regression_upward() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..8 {
            let dir = root.path().join(format!("{index:02}"));
            std::fs::create_dir(&dir).unwrap();
            let mut timing = recorder(root.path());
            enter(&mut timing, index != 0, 1280);
            end(&mut timing, if index < 3 { 100_000 } else { 1_000 }, index != 1);
            timing.save(&dir.join("timings.json"),
                &json!({ "complete": index != 2, "status": "PASS" })).unwrap();
        }
        let mut candidate = recorder(root.path());
        enter(&mut candidate, true, 1280);
        let warnings = end(&mut candidate, 3_000, true);
        assert_eq!(warnings.iter().find_map(|warning| warning.baseline_ms), Some(1_000.0));
        let later = root.path().join("99");
        std::fs::create_dir(&later).unwrap();
        candidate.save(&later.join("timings.json"), &json!({ "complete": true, "status": "PASS" })).unwrap();
        let mut next = recorder(root.path());
        enter(&mut next, true, 1280);
        assert_eq!(end(&mut next, 3_000, true).iter().find_map(|warning| warning.baseline_ms), Some(1_000.0));
        let mut different = recorder(root.path());
        enter(&mut different, true, 2560);
        assert!(end(&mut different, 3_000, true).is_empty());
    }

    /// One invocation of the same suite. Every field here is something that
    /// moves between two ordinary `rock sim` runs of the identical selection:
    /// `rock-log` stamps a fresh shared epoch into the environment, and the
    /// scheduler packs the sim next to whichever siblings are still queued.
    #[derive(Clone)]
    struct Invocation {
        epoch: u64,
        before: Vec<&'static str>,
        after: Vec<&'static str>,
        jobs: u64,
        machine: &'static str,
        boot: Vec<(&'static str, &'static str)>,
        dirty: bool,
    }

    impl Default for Invocation {
        fn default() -> Self {
            Self { epoch: 1_788_000_000_000, before: Vec::new(), after: vec!["sibling"],
                jobs: 8, machine: "Darwin Mac 25.5.0 arm64\n",
                boot: vec![("SIM_SKIP_RENDER", "1")], dirty: false }
        }
    }

    /// The host block a real invocation records, built through the same
    /// projection production uses rather than a hand-written constant.
    fn invocation_host(run: &Invocation) -> Value {
        let ambient = ambient_settings([
            ("ROCK_BUILD_COMMAND".to_string(), "sim".to_string()),
            ("ROCK_LOG_EPOCH_MS".to_string(), run.epoch.to_string()),
            ("ROCK_SIM_LOG_DIR".to_string(), "/tmp/history".to_string()),
        ].into_iter());
        json!({ "machine": run.machine, "arch": "aarch64", "logical_cpus": 14,
            "ambient": ambient })
    }

    /// Drive one accepted invocation to completion and return the recorder
    /// together with the warnings raised for the sim under observation.
    fn invoke(root: &Path, run: &Invocation, duration: u64) -> (Timings, Vec<TimingWarning>) {
        let mut entries = Vec::new();
        for (index, name) in run.before.iter().enumerate() {
            entries.push(json!({ "occurrence_id": 100 + index, "name": name }));
        }
        entries.push(json!({ "occurrence_id": 1, "name": "sim" }));
        for (index, name) in run.after.iter().enumerate() {
            entries.push(json!({ "occurrence_id": 200 + index, "name": name }));
        }
        let mut timing = Timings::new(root, policy(), invocation_host(run));
        timing.observe("run_start", &json!({ "source": { "dirty": run.dirty }, "quiet": false }), 0);
        timing.observe("run_config", &json!({ "project_path": "mag", "resolved_entries": ["sim"],
            "effective_mode": "workers", "effective_jobs": run.jobs }), 0);
        timing.observe("worker_spawn", &json!({ "worker_attempt": 1, "kind": "shared",
            "entries": entries, "boot_signature": run.boot }), 0);
        timing.observe("entry_runtime_context", &json!({ "worker_attempt": 1, "occurrence_id": 1,
            "part": 0, "context": { "width": 1280, "height": 720, "wgpu": false, "jit": "on" } }), 0);
        timing.observe("entry_start", &json!({ "worker_attempt": 1, "occurrence_id": 1, "part": 0,
            "name": "sim" }), 0);
        let warnings = timing.observe("entry_end",
            &json!({ "worker_attempt": 1, "occurrence_id": 1, "part": 0 }), duration);
        timing.observe("entry_result", &json!({ "worker_attempt": 1, "occurrence_id": 1,
            "verdict": "PASS", "timed_out": false }), duration);
        (timing, warnings)
    }

    /// Five runs of one sim, differing only in the churn a repeat invocation
    /// always produces, each saved as history under the given terminal record.
    fn history_of(root: &Path, template: &Invocation, finished: &Value) {
        for index in 0..5u64 {
            let dir = root.join(format!("{index:02}"));
            std::fs::create_dir(&dir).unwrap();
            let run = Invocation {
                epoch: template.epoch + index * 263_772,
                after: vec!["sibling"; index as usize + 1],
                ..template.clone()
            };
            let (timing, _) = invoke(root, &run, 1_000);
            timing.save(&dir.join("timings.json"), finished).unwrap();
        }
    }

    /// The same five runs under a terminal record that passes outright.
    fn accepted_history(root: &Path) {
        history_of(root, &Invocation::default(),
            &json!({ "complete": true, "status": "PASS" }));
    }

    /// A candidate run of the same sim, distinct from every history entry.
    fn candidate(template: &Invocation) -> Invocation {
        Invocation { epoch: 1_788_009_999_999, ..template.clone() }
    }

    #[test]
    fn the_recorded_host_keeps_cost_settings_and_drops_per_invocation_ones() {
        let environment = |epoch: &str| {
            [("ROCK_BUILD_COMMAND", "sim"), ("SIM_DT", "0.0333333333"),
             ("ROCK_LOG_EPOCH_MS", epoch), ("ROCK_SIM_LOG_DIR", "/tmp/history"),
             ("ROCK_SIM_WARN_SECS", "30"), ("PATH", "/usr/bin")]
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .into_iter()
        };
        let first = ambient_settings(environment("1788822511117"));
        assert_eq!(first, ambient_settings(environment("1788822774949")),
            "two invocations on one machine must record the same ambient settings");
        assert_eq!(first.get("SIM_DT").map(String::as_str), Some("0.0333333333"),
            "settings that change what a sim costs stay recorded");
        assert_eq!(first.get("ROCK_BUILD_COMMAND").map(String::as_str), Some("sim"));
        for excluded in ["ROCK_LOG_EPOCH_MS", "ROCK_SIM_LOG_DIR", "ROCK_SIM_WARN_SECS", "PATH"] {
            assert!(!first.contains_key(excluded), "{excluded} must stay out of the host");
        }
    }

    #[test]
    fn repeated_runs_of_one_sim_accumulate_a_baseline() {
        let root = tempfile::tempdir().unwrap();
        accepted_history(root.path());
        let (_, warnings) = invoke(root.path(), &candidate(&Invocation::default()), 3_000);
        assert_eq!(
            warnings.iter().find_map(|warning| warning.baseline_ms),
            Some(1_000.0),
            "five accepted runs of the same sim must form a baseline; identity churn \
             that has nothing to do with cost keeps the historical detector dead"
        );
    }

    /// AUTHOR DECISION: SIM TIMING BASELINE ACCEPTANCE - per-entry acceptance.
    /// The MAG suite carries known failures, so its run status is permanently
    /// `FAIL`. Under run-level acceptance that suite can never form a baseline,
    /// which leaves the historical detector dead for exactly the long sims that
    /// most need a regression guard.
    #[test]
    fn a_sim_that_passed_inside_a_failing_run_still_forms_a_baseline() {
        let root = tempfile::tempdir().unwrap();
        history_of(root.path(), &Invocation::default(),
            &json!({ "complete": true, "status": "FAIL" }));
        let (_, warnings) = invoke(root.path(), &candidate(&Invocation::default()), 3_000);
        assert_eq!(
            warnings.iter().find_map(|warning| warning.baseline_ms),
            Some(1_000.0),
            "a sim that passed contributes its own cost even when a sibling \
             failed; the run verdict is a fact about the other entries"
        );
    }

    /// The risk per-entry acceptance knowingly takes on: a run can fail because
    /// the machine was disturbed, and the sims that passed in it may have been
    /// slowed by the same disturbance. The median of at least five samples is
    /// the mitigation the author relied on; this pins that it holds.
    #[test]
    fn one_inflated_sample_from_a_failing_run_does_not_move_the_median() {
        let root = tempfile::tempdir().unwrap();
        let template = Invocation::default();
        for index in 0..5u64 {
            let dir = root.path().join(format!("{index:02}"));
            std::fs::create_dir(&dir).unwrap();
            let run = Invocation {
                epoch: template.epoch + index * 263_772,
                after: vec!["sibling"; index as usize + 1],
                ..template.clone()
            };
            // The first run failed on a loaded machine, and the sim that passed
            // inside it took forty times its quiet-machine cost.
            let (timing, _) = invoke(root.path(), &run,
                if index == 0 { 40_000 } else { 1_000 });
            timing.save(&dir.join("timings.json"), &json!({ "complete": true,
                "status": if index == 0 { "FAIL" } else { "PASS" } })).unwrap();
        }
        let (_, warnings) = invoke(root.path(), &candidate(&template), 3_000);
        assert_eq!(
            warnings.iter().find_map(|warning| warning.baseline_ms),
            Some(1_000.0),
            "one sample inflated by a disturbed machine must not lift the \
             baseline and mute the regressions it exists to catch"
        );
    }

    /// The two run-level vetoes per-entry acceptance keeps. Neither is a
    /// statement about sibling entries, so neither is what the decision drops.
    #[test]
    fn a_baseline_still_refuses_entries_from_a_dirty_or_unaccounted_run() {
        for (reason, template, finished) in [
            ("a working tree that may hold modified production code",
                Invocation { dirty: true, ..Invocation::default() },
                json!({ "complete": true, "status": "FAIL" })),
            ("a run whose planned entries were never all accounted for",
                Invocation::default(),
                json!({ "complete": false, "status": "FAIL" })),
        ] {
            let root = tempfile::tempdir().unwrap();
            history_of(root.path(), &template, &finished);
            let (_, warnings) = invoke(root.path(), &candidate(&template), 3_000);
            assert!(
                warnings.iter().all(|warning| warning.kind != TimingWarningKind::Historical),
                "per-entry acceptance must not admit samples from {reason}"
            );
        }
    }

    /// The 750 records written before this field existed carry no
    /// `baseline_eligible`. They stay readable and contribute exactly the set
    /// they always did - the ones the old run-level gate marked `accepted`.
    #[test]
    fn records_predating_the_new_field_fall_back_to_accepted() {
        for (accepted, expected) in [(true, Some(1_000.0)), (false, None)] {
            let root = tempfile::tempdir().unwrap();
            accepted_history(root.path());
            for index in 0..5u64 {
                let path = root.path().join(format!("{index:02}")).join("timings.json");
                let mut value: Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let object = value.as_object_mut().unwrap();
                object.remove("baseline_eligible");
                object.insert("accepted".into(), json!(accepted));
                std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            let (_, warnings) = invoke(root.path(), &candidate(&Invocation::default()), 3_000);
            assert_eq!(
                warnings.iter().find_map(|warning| warning.baseline_ms), expected,
                "a record from before per-entry acceptance contributes exactly \
                 when the old gate accepted it"
            );
        }
    }

    /// Run over both populations a baseline can now be built from: a suite that
    /// passed outright, and one that failed while the sim under observation
    /// passed. Per-entry acceptance widens what reaches the history, so the
    /// identity has to keep refusing unlike contexts out of the wider set too.
    #[test]
    fn a_baseline_still_refuses_samples_from_an_unlike_context() {
        for status in ["PASS", "FAIL"] {
            let root = tempfile::tempdir().unwrap();
            history_of(root.path(), &Invocation::default(),
                &json!({ "complete": true, "status": status }));
            for (reason, unlike) in [
                ("another machine", Invocation {
                    machine: "Darwin Jakubs-host-c.local 25.5.0 arm64\n", ..Invocation::default() }),
                ("another parallelism", Invocation { jobs: 1, ..Invocation::default() }),
                ("another boot signature", Invocation {
                    boot: vec![("SIM_SKIP_RENDER", "0")], ..Invocation::default() }),
                ("a warm process rather than a cold one", Invocation {
                    before: vec!["sibling"], after: Vec::new(), ..Invocation::default() }),
            ] {
                let (_, warnings) = invoke(root.path(), &unlike, 3_000);
                assert!(
                    warnings.iter().all(|warning| warning.kind != TimingWarningKind::Historical),
                    "a sample from {reason} is not comparable and must not be judged against \
                     a baseline built from {status} runs"
                );
            }
        }
    }

    #[test]
    fn failed_open_entries_and_cleanup_phases_still_warn() {
        let root = tempfile::tempdir().unwrap();
        let mut timing = recorder(root.path());
        enter(&mut timing, false, 1280);
        let warnings = timing.observe("entry_result", &json!({ "worker_attempt": 1,
            "occurrence_id": 1, "verdict": "FAIL", "timed_out": true }), 12_000);
        assert_eq!(warnings.len(), 1);
        assert_eq!(timing.rows[0]["completed"], false);
        timing.observe("phase_start", &json!({ "phase": "reload_restore:sim" }), 12_000);
        let warnings = timing.observe("phase_end", &json!({ "phase": "reload_restore:sim", "success": false }), 25_000);
        assert_eq!(warnings[0].duration_ms, 13_000);
        assert_eq!(warnings[0].label, "phase reload_restore:sim");
    }

}
