use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

const SLOW_MS: u64 = 10_000;

pub(super) struct TimingWarning {
    pub(super) label: String,
    pub(super) duration_ms: u64,
}

fn slow_warning(label: &str, duration_ms: u64) -> Vec<TimingWarning> {
    if duration_ms >= SLOW_MS {
        vec![TimingWarning {
            label: label.into(),
            duration_ms,
        }]
    } else {
        Vec::new()
    }
}

/// Workload settings retained with the run, excluding recording destinations
/// and the per-invocation log clock.
fn ambient_settings(
    vars: impl Iterator<Item = (String, String)>,
    prefixes: &[String],
) -> std::collections::BTreeMap<String, String> {
    vars.filter(|(key, _)| {
        (["STONE_", "SIM_", "ROCK_", "SDL_"]
            .iter()
            .any(|prefix| key.starts_with(prefix))
            || prefixes.iter().any(|prefix| key.starts_with(prefix)))
            && key != "ROCK_SIM_LOG_DIR"
            && key != "ROCK_LOG_EPOCH_MS"
    })
    .collect()
}

pub(super) fn host_context(project: &str) -> anyhow::Result<Value> {
    let project = rock_engine_host::project::resolve_project_dir(project)?;
    let config = rock_host::stick::StickConfig::from_path_optional(&project.join("stick.toml"))?;
    let prefixes = config
        .and_then(|c| c.sim)
        .map(|s| s.environment_prefixes)
        .unwrap_or_default();
    let machine = std::process::Command::new("uname")
        .arg("-srmn")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok());
    Ok(json!({ "machine": machine, "arch": std::env::consts::ARCH,
        "logical_cpus": std::thread::available_parallelism().ok().map(|n| n.get()),
        "ambient": ambient_settings(std::env::vars(), &prefixes) }))
}

type EntryKey = (u64, u64);

pub(super) struct Timings {
    /// The most recent finished duration of each (project, entry name),
    /// on this host, under any workload or verdict. Scheduling input, never a baseline.
    latest: HashMap<(String, String), u64>,
    host: Value,
    project: Option<String>,
    source_files: HashMap<String, String>,
    source_clean: bool,
    active: HashMap<EntryKey, (u64, String)>,
    phases: HashMap<String, u64>,
    results: HashMap<u64, bool>,
    rows: Vec<Value>,
}

impl Timings {
    pub(super) fn new(root: &Path, host: Value) -> Self {
        let mut latest = HashMap::new();
        match std::fs::read_dir(root) {
            Ok(dirs) => {
                let mut paths = dirs
                    .filter_map(|dir| match dir {
                        Ok(dir) => Some(dir.path().join("timings.json")),
                        Err(error) => {
                            log::warn!(target: "SIM", "cannot inspect timing history: {error}");
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                // Run ids start with a fixed-width millisecond stamp, so this is
                // oldest first and a later record overwrites `latest`.
                paths.sort();
                for path in paths {
                    let data = match std::fs::read(&path) {
                        Ok(data) => data,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(e) => {
                            log::warn!(target: "SIM", "cannot read {}: {e}", path.display());
                            continue;
                        }
                    };
                    let value: Value = match serde_json::from_slice(&data) {
                        Ok(value) => value,
                        Err(e) => {
                            log::warn!(target: "SIM", "invalid timing history {}: {e}", path.display());
                            continue;
                        }
                    };
                    if value["schema_version"] != 1 {
                        continue;
                    }
                    if let Some(project) = value["project"].as_str().filter(|_| {
                        host["machine"].as_str().is_some()
                            && value["host"]["machine"] == host["machine"]
                    }) {
                        for row in value["entries"].as_array().into_iter().flatten() {
                            if row["completed"] != true {
                                continue;
                            }
                            if let (Some(name), Some(ms)) =
                                (row["name"].as_str(), row["duration_ms"].as_u64())
                            {
                                latest.insert((project.to_string(), name.to_string()), ms);
                            }
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!(target: "SIM", "cannot inspect timing history: {e}"),
        }
        Self {
            latest,
            host,
            project: None,
            source_files: HashMap::new(),
            source_clean: false,
            active: HashMap::new(),
            phases: HashMap::new(),
            results: HashMap::new(),
            rows: Vec::new(),
        }
    }

    pub(super) fn set_source_files(&mut self, source_files: &HashMap<String, String>) {
        self.source_files.clone_from(source_files);
    }

    fn complete(
        &mut self,
        key: EntryKey,
        now: u64,
        completed: bool,
        frame_count: Option<Value>,
        frame_timing: Option<Value>,
    ) -> Vec<TimingWarning> {
        let Some((start, name)) = self.active.remove(&key) else {
            return Vec::new();
        };
        let ms = now.saturating_sub(start);
        let source_file = self
            .source_files
            .get(&name)
            .map_or(Value::Null, |path| json!(path));
        let warnings = slow_warning(&name, ms);
        self.rows.push(json!({ "name": name, "occurrence_id": key.1,
            "source_file": source_file, "duration_ms": ms, "completed": completed,
            "frame_count": frame_count.unwrap_or(Value::Null),
            "frame_timing": frame_timing.unwrap_or(Value::Null) }));
        warnings
    }

    pub(super) fn observe(&mut self, name: &str, fields: &Value, now: u64) -> Vec<TimingWarning> {
        let key = (
            fields["worker_attempt"].as_u64().unwrap_or(0),
            fields["occurrence_id"].as_u64().unwrap_or(0),
        );
        match name {
            "run_start" => {
                self.source_clean = fields["source"]["dirty"] == false;
            }
            "run_config" => self.project = fields["project_path"].as_str().map(str::to_owned),
            "entry_start" => {
                self.active.insert(
                    key,
                    (now, fields["name"].as_str().unwrap_or("unknown").into()),
                );
            }
            "entry_end" => {
                return self.complete(
                    key,
                    now,
                    true,
                    fields.get("frame_count").cloned(),
                    fields.get("frame_timing").cloned(),
                );
            }
            "entry_result" => {
                self.results.insert(
                    key.1,
                    fields["verdict"] == "PASS" && fields["timed_out"] == false,
                );
                let unfinished = self
                    .active
                    .keys()
                    .filter(|entry| entry.1 == key.1)
                    .copied()
                    .collect::<Vec<_>>();
                return unfinished
                    .into_iter()
                    .flat_map(|entry| self.complete(entry, now, false, None, None))
                    .collect();
            }
            "phase_start" => {
                if let Some(phase) = fields["phase"].as_str() {
                    self.phases.insert(phase.into(), now);
                }
            }
            "phase_end" => {
                if let Some(phase) = fields["phase"].as_str() {
                    if let Some(start) = self.phases.remove(phase) {
                        return slow_warning(&format!("phase {phase}"), now.saturating_sub(start));
                    }
                }
            }
            _ => {}
        }
        Vec::new()
    }

    pub(super) fn save(&self, path: &Path, finished: &Value) -> std::io::Result<()> {
        let entries = self
            .rows
            .iter()
            .map(|row| {
                let mut row = row.clone();
                row["passed"] = json!(
                    self.results
                        .get(&row["occurrence_id"].as_u64().unwrap_or(0))
                        == Some(&true)
                );
                row
            })
            .collect::<Vec<_>>();
        let summary = json!({ "schema_version": 1, "host": self.host,
            "project": self.project,
            "accepted": self.source_clean && finished["complete"] == true && finished["status"] == "PASS",
            "entries": entries });
        let pending = path.with_extension("json.pending");
        std::fs::write(&pending, serde_json::to_vec(&summary)?)?;
        std::fs::rename(pending, path)
    }

    pub(super) fn latest_duration_ms(&self, name: &str) -> Option<u64> {
        let project = self.project.as_ref()?;
        self.latest
            .get(&(project.clone(), name.to_string()))
            .copied()
    }

    pub(super) fn message(&self, warning: &TimingWarning) -> String {
        format!(
            "slow sim: {} took {:.3}s (warning budget {:.3}s)",
            warning.label,
            warning.duration_ms as f64 / 1000.0,
            SLOW_MS as f64 / 1000.0
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_warning_uses_the_fixed_threshold() {
        assert!(slow_warning("sim", 9_999).is_empty());
        let warnings = slow_warning("sim", 10_000);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].label, "sim");
        assert_eq!(warnings[0].duration_ms, 10_000);
    }

    #[test]
    fn timing_artifact_keeps_completed_and_interrupted_entries_for_consumers() {
        let root = tempfile::tempdir().unwrap();
        let mut timing = recorder(root.path());
        timing.set_source_files(&HashMap::from([(
            "sim".into(),
            "tests/sims/example.rock".into(),
        )]));
        enter(&mut timing, false);
        end(&mut timing, 1234, false);
        timing.observe(
            "entry_start",
            &json!({ "worker_attempt": 2, "occurrence_id": 2, "name": "interrupted" }),
            1500,
        );
        timing.observe(
            "entry_result",
            &json!({ "worker_attempt": 2, "occurrence_id": 2,
                "verdict": "FAIL", "timed_out": true }),
            2000,
        );
        let path = root.path().join("timings.json");
        timing
            .save(&path, &json!({ "complete": true, "status": "FAIL" }))
            .unwrap();
        let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["project"], "game");
        let rows = value["entries"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["name"], "sim");
        assert_eq!(rows[0]["source_file"], "tests/sims/example.rock");
        assert_eq!(rows[0]["duration_ms"], 1234);
        assert_eq!(rows[0]["completed"], true);
        assert_eq!(rows[1]["name"], "interrupted");
        assert_eq!(rows[1]["source_file"], Value::Null);
        assert_eq!(rows[1]["duration_ms"], 500);
        assert_eq!(rows[1]["completed"], false);
    }

    #[test]
    fn scheduling_history_ignores_foreign_hosts_stale_schemas_and_unfinished_rows() {
        let root = tempfile::tempdir().unwrap();
        let mut previous = recorder(root.path());
        enter(&mut previous, true);
        end(&mut previous, 7_321, true);
        retain(
            root.path(),
            "01",
            &previous,
            &json!({"complete": true, "status": "PASS"}),
        );
        let retained = root.path().join("01");
        let original: Value =
            serde_json::from_slice(&std::fs::read(retained.join("timings.json")).unwrap()).unwrap();
        let lookup = |host: Value| {
            let mut next = Timings::new(root.path(), host);
            enter(&mut next, true);
            next.latest_duration_ms("sim")
        };
        let host = original["host"].clone();
        assert_eq!(
            lookup(host.clone()),
            original["entries"][0]["duration_ms"].as_u64()
        );
        assert_eq!(lookup(json!({"machine": "foreign host"})), None);
        for invalid in ["schema_version", "completed"] {
            let mut stale = original.clone();
            if invalid == "schema_version" {
                stale["schema_version"] = json!(0);
            } else {
                stale["entries"][0]["completed"] = json!(false);
            }
            std::fs::write(
                retained.join("timings.json"),
                serde_json::to_vec(&stale).unwrap(),
            )
            .unwrap();
            assert_eq!(lookup(host.clone()), None, "{invalid}");
        }
        std::fs::remove_file(retained.join("timings.json")).unwrap();
        assert_eq!(lookup(host), None);
    }

    #[test]
    fn the_latest_finished_duration_per_project_is_kept_for_dispatch() {
        let root = tempfile::tempdir().unwrap();
        let record = |dir: &str, project: &str, duration: u64, finished: bool| {
            let mut timing = recorder(root.path());
            enter(&mut timing, false);
            timing.observe(
                "run_config",
                &json!({ "project_path": project, "effective_jobs": 4 }),
                0,
            );
            if finished {
                end(&mut timing, duration, false);
            } else {
                timing.observe(
                    "entry_result",
                    &json!({ "worker_attempt": 1, "occurrence_id": 1,
                    "verdict": "FAIL", "timed_out": true }),
                    duration,
                );
            }
            retain(
                root.path(),
                dir,
                &timing,
                &json!({ "complete": false, "status": "FAIL" }),
            );
        };
        record("01", "game", 5_000, true);
        record("02", "game", 3_000, true);
        record("03", "sample", 9_000, true);
        record("04", "game", 60_000, false);
        let mut next = recorder(root.path());
        enter(&mut next, true);
        assert_eq!(next.latest_duration_ms("sim"), Some(3_000));
        assert_eq!(next.latest_duration_ms("never_ran"), None);
    }

    fn retain(root: &Path, name: &str, timing: &Timings, finished: &Value) {
        let dir = root.join(name);
        std::fs::create_dir(&dir).unwrap();
        timing.save(&dir.join("timings.json"), finished).unwrap();
    }

    fn recorder(root: &Path) -> Timings {
        Timings::new(root, json!({ "machine": "fixture", "logical_cpus": 4 }))
    }

    fn enter(timings: &mut Timings, clean: bool) {
        timings.observe("run_start", &json!({ "source": { "dirty": !clean } }), 0);
        timings.observe(
            "run_config",
            &json!({ "project_path": "game", "effective_jobs": 4 }),
            0,
        );
        timings.observe(
            "entry_start",
            &json!({ "worker_attempt": 1, "occurrence_id": 1, "name": "sim" }),
            0,
        );
    }

    fn end(timings: &mut Timings, duration: u64, passed: bool) -> Vec<TimingWarning> {
        let warnings = timings.observe(
            "entry_end",
            &json!({ "worker_attempt": 1, "occurrence_id": 1, "frame_count": 7 }),
            duration,
        );
        timings.observe(
            "entry_result",
            &json!({ "worker_attempt": 1, "occurrence_id": 1,
            "verdict": if passed { "PASS" } else { "FAIL" }, "timed_out": false }),
            duration,
        );
        warnings
    }

    #[test]
    fn completed_timing_rows_retain_the_compiled_sim_source_file() {
        let root = tempfile::tempdir().unwrap();
        let mut timing = recorder(root.path());
        timing.observe("run_start", &json!({ "source": { "dirty": false } }), 0);
        timing.set_source_files(&HashMap::from([(
            "sim".to_string(),
            "tests/sims/example.rock".to_string(),
        )]));
        timing.observe("run_config", &json!({ "project_path": "game" }), 0);
        timing.observe(
            "entry_start",
            &json!({ "worker_attempt": 1, "occurrence_id": 1,
            "name": "sim" }),
            0,
        );
        timing.observe(
            "entry_end",
            &json!({ "worker_attempt": 1, "occurrence_id": 1,
            "frame_count": 7 }),
            1000,
        );

        assert_eq!(timing.rows[0]["source_file"], "tests/sims/example.rock");
    }

    #[test]
    fn the_recorded_host_keeps_cost_settings_and_drops_per_invocation_ones() {
        let environment = |epoch: &str| {
            [
                ("ROCK_BUILD_COMMAND", "sim"),
                ("SIM_DT", "0.0333333333"),
                ("ROCK_LOG_EPOCH_MS", epoch),
                ("ROCK_SIM_LOG_DIR", "/tmp/history"),
                ("PATH", "/usr/bin"),
            ]
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .into_iter()
        };
        let first = ambient_settings(environment("1788822511117"), &[]);
        assert_eq!(
            first,
            ambient_settings(environment("1788822774949"), &[]),
            "two invocations on one machine must record the same ambient settings"
        );
        assert_eq!(
            first.get("SIM_DT").map(String::as_str),
            Some("0.0333333333"),
            "settings that change what a sim costs stay recorded"
        );
        assert_eq!(
            first.get("ROCK_BUILD_COMMAND").map(String::as_str),
            Some("sim")
        );
        for excluded in ["ROCK_LOG_EPOCH_MS", "ROCK_SIM_LOG_DIR", "PATH"] {
            assert!(
                !first.contains_key(excluded),
                "{excluded} must stay out of the host"
            );
        }
    }

    #[test]
    fn failed_open_entries_and_cleanup_phases_still_warn() {
        let root = tempfile::tempdir().unwrap();
        let mut timing = recorder(root.path());
        enter(&mut timing, false);
        let warnings = timing.observe(
            "entry_result",
            &json!({ "worker_attempt": 1,
            "occurrence_id": 1, "verdict": "FAIL", "timed_out": true }),
            12_000,
        );
        assert_eq!(warnings.len(), 1);
        assert_eq!(timing.rows[0]["completed"], false);
        timing.observe(
            "phase_start",
            &json!({ "phase": "reload_restore:sim" }),
            12_000,
        );
        let warnings = timing.observe(
            "phase_end",
            &json!({ "phase": "reload_restore:sim", "success": false }),
            25_000,
        );
        assert_eq!(warnings[0].duration_ms, 13_000);
        assert_eq!(warnings[0].label, "phase reload_restore:sim");
    }
}
