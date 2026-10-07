//! Opt-in compile records. Boundary samples are wall time and process memory;
//! compiler-owned internal timers are separate because parallel unit timers
//! overlap and public APIs do not expose internal memory sample boundaries.

use serde_json::{Value, json};
use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static COMMAND: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

pub(crate) fn set_command(command: &'static str) {
    let _ = COMMAND.set(command);
}

thread_local! {
    static PENDING: RefCell<Option<Pending>> = const { RefCell::new(None) };
}

struct Pending {
    path: PathBuf,
    started: Instant,
    boundary: Instant,
    record: Value,
    phases: Vec<Value>,
    timings: Vec<Value>,
    totals: Value,
}

/// An outer compile owns the record; helpers can enter without duplicating it.
/// A rejected/unwound compile still publishes its completed phases.
pub(crate) struct Scope(bool);

impl Scope {
    pub(crate) fn begin(entry: &str) -> Self {
        if PENDING.with(|pending| pending.borrow().is_some()) {
            return Self(false);
        }
        let Some(path) = std::env::var_os("ROCK_COMPILE_STATS").filter(|path| !path.is_empty()) else {
            return Self(false)
        };
        let command = COMMAND.get().copied().unwrap_or("other");
        let kind = if std::env::var_os("ROCK_SIM_COMPILE_RECEIPT_PREPARE").is_some() {
            "sim compile child"
        } else {
            match command {
                "test" | "test-compile" => "test",
                "compile" | "compile-single" | "build" | "package" => "build",
                _ => "other",
            }
        };
        #[cfg(not(feature = "sequential"))]
        let threads = rayon::current_num_threads();
        #[cfg(feature = "sequential")]
        let threads = 1;
        let now = Instant::now();
        let pending = Pending {
            path: PathBuf::from(path), started: now, boundary: now,
            record: json!({
                "schema": 1,
                "timestamp_unix_ms": SystemTime::now().duration_since(UNIX_EPOCH)
                    .unwrap_or_default().as_millis() as u64,
                "pid": std::process::id(), "kind": kind, "command": command,
                "entry": entry, "rock_num_threads": std::env::var("ROCK_NUM_THREADS").ok(),
                "rayon_threads": threads, "job_id": std::env::var("ROCK_JOB_ID").ok(),
                "compile_slot": null,
            }),
            phases: Vec::new(), timings: Vec::new(),
            totals: json!({"source_files": null, "source_bytes": null,
                "production": null, "tests": null, "sims": null,
                "concrete_plans": null, "chunks": null, "dependency_edges": null,
                "emitted_lua_bytes": null}),
        };
        PENDING.with(|state| *state.borrow_mut() = Some(pending));
        Self(true)
    }

    pub(crate) fn for_sources(sources: &crate::SourceFiles) -> Self {
        let entry = sources.files.first().map(|file| file.borrow().file.to_id_path())
            .unwrap_or_else(|| "<empty>".to_string());
        Self::begin(&entry)
    }

    pub(crate) fn success(self) {
        if self.0 { finish("ok"); }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if self.0 { finish("error"); }
    }
}

pub(crate) fn phase(name: &str) {
    PENDING.with(|state| {
        let mut state = state.borrow_mut();
        let Some(pending) = state.as_mut() else { return };
        let now = Instant::now();
        let mut sample = crate::allocator::process_memory();
        sample["name"] = json!(name);
        sample["wall_ms"] = json!((now - pending.boundary).as_secs_f64() * 1000.0);
        pending.phases.push(sample);
        pending.boundary = now;
    });
}

/// The slot wait is its own phase so the compile phases exclude it.
pub(crate) fn slot(acquisition: &crate::compile_slots::SlotAcquisition) {
    phase("compile slot");
    PENDING.with(|state| {
        if let Some(pending) = state.borrow_mut().as_mut() {
            pending.record["compile_slot"] = json!({
                "slot": acquisition.slot,
                "waited_ms": acquisition.waited.as_secs_f64() * 1000.0,
            });
        }
    });
}

/// Called in existing source enumeration loops, never by another source walk.
/// Implicit cached stdlib modules are excluded; discovered dependencies count.
pub(crate) fn source(path: &str, bytes: usize) {
    PENDING.with(|state| {
        let mut state = state.borrow_mut();
        let Some(pending) = state.as_mut() else { return };
        if pending.totals["source_bytes"].is_null() {
            pending.totals["source_files"] = json!(0);
            pending.totals["source_bytes"] = json!(0);
            for category in ["production", "tests", "sims"] {
                pending.totals[category] = json!({"files": 0, "bytes": 0});
            }
        }
        let mut components = path.split(['/', '\\']);
        let category = if components.any(|component| component == "tests") {
            if components.next() == Some("sims") { "sims" } else { "tests" }
        } else { "production" };
        for (key, count) in [("source_files", 1), ("source_bytes", bytes)] {
            pending.totals[key] = json!(pending.totals[key].as_u64().unwrap() + count as u64);
        }
        for (key, count) in [("files", 1), ("bytes", bytes)] {
            pending.totals[category][key] = json!(pending.totals[category][key].as_u64().unwrap() + count as u64);
        }
    });
}

/// A live compile reuses source text. Its file count is a container length;
/// leave bytes/splits unavailable unless existing feed loops counted them.
pub(crate) fn known_sources(files: usize) {
    PENDING.with(|state| {
        if let Some(pending) = state.borrow_mut().as_mut() {
            if pending.totals["source_files"].is_null() {
                pending.totals["source_files"] = json!(files);
            }
        }
    });
}

pub(crate) fn timings(group: &str, timings: &[(&str, Duration)]) {
    PENDING.with(|state| {
        let mut state = state.borrow_mut();
        let Some(pending) = state.as_mut() else { return };
        pending.timings.extend(timings.iter().map(|(name, elapsed)| json!({
            "group": group, "name": name, "ms": elapsed.as_secs_f64() * 1000.0,
            "clock": if group == "skull" && !(name.starts_with("setup:") || name.starts_with("join:")
                || name.starts_with("incr:") || name.starts_with("  ") || *name == "scaffold+callgraph+order") {
                "sum_of_unit_wall"
            } else { "wall" },
        })));
    });
}

pub(crate) fn emission(emit: &skull::mir_emit::MirEmit) {
    phase("codegen");
    timings("skull", &emit.phases);
    PENDING.with(|state| {
        let mut state = state.borrow_mut();
        let Some(pending) = state.as_mut() else { return };
        pending.record["released"] = json!({
            "graph_instances": emit.release_stats.graph_instances,
            "chunks": emit.release_stats.chunks,
            "concrete_plans": emit.release_stats.concrete_plans,
            "retained_dependency_edges": emit.release_stats.retained_dependency_edges,
            "retained_static_keys": emit.release_stats.retained_static_keys,
        });
        pending.totals["concrete_plans"] = json!(emit.compile_counts.concrete_plans);
        pending.totals["chunks"] = json!(emit.compile_counts.chunks);
        pending.totals["dependency_edges"] = json!(emit.compile_counts.dependency_edges);
        pending.totals["emitted_lua_bytes"] = json!(emit.lua.len());
    });
}

pub(crate) fn finish(status: &str) {
    let pending = PENDING.try_with(|state| state.borrow_mut().take()).ok().flatten();
    let Some(mut pending) = pending else { return };
    pending.record["status"] = json!(status);
    pending.record["wall_ms"] = json!(pending.started.elapsed().as_secs_f64() * 1000.0);
    pending.record["peak_rss_bytes"] = crate::allocator::process_memory()["peak_rss_bytes"].clone();
    pending.record["phases"] = json!(pending.phases);
    pending.record["internal_timings"] = json!(pending.timings);
    pending.record["totals"] = pending.totals;
    if let Err(error) = append(&pending.path, &pending.record) {
        log::warn!(target: "COMPILE_STATS", "cannot append {}: {error}", pending.path.display());
    }
}

fn append(path: &Path, record: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    // O_APPEND assigns each write an end offset; the advisory lock also keeps
    // a short write/retry from splitting a line around another process's line.
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 { break }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted { return Err(error) }
        }
    }
    file.write_all(&line)
}
