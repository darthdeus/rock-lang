//! Long-lived headless watch process driven over JSON-line stdio.
//!
//! Uses the same incremental compiler as `rock stone`, without a game runtime.
//!
//! ## Protocol
//!
//! Line-delimited JSON in both directions.
//!
//! Request (stdin): `{"seq": <int>, "cmd": <str>, "args": <obj>}`
//! Response (stdout): `{"seq": <int>, "kind": "response", "ok": <bool>, ...}`
//! Event (stdout): `{"kind": "event", "type": <str>, ...}` -- no seq.
//!
//! Commands:
//! - `ping` -> `{ok: true}`
//! - `shutdown` -> `{ok: true}`, then exits.
//!
//! Events emitted:
//! - `reload` `{n, files, decision: "hot"|"restart", compile_ms}`
//! - `compile_error` `{n, files, message}`
//! - `ready` (once at startup)

use crate::skull_backend::{IncrementalProject, SkullReload};
use anyhow::Result;
use rock_core::clock::Instant;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::channel;

/// One reload event sent to the controlling process.
#[derive(Clone)]
struct ReloadRecord {
    n: u64,
    files: Vec<PathBuf>,
    decision: &'static str, // "hot" | "restart"
    compile_ms: f64,
    error: Option<String>,
}

impl ReloadRecord {
    fn to_event_json(&self) -> Value {
        if let Some(err) = &self.error {
            json!({
                "kind": "event",
                "type": "compile_error",
                "n": self.n,
                "files": self.files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "message": err,
            })
        } else {
            json!({
                "kind": "event",
                "type": "reload",
                "n": self.n,
                "files": self.files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "decision": self.decision,
                "compile_ms": self.compile_ms,
            })
        }
    }
}

/// Internal control messages the worker thread reacts to.
enum Ctl {
    /// File watcher fired with a debounced batch.
    Files(HashSet<PathBuf>),
    /// Stdin requested shutdown -- drain and exit.
    Shutdown,
}

pub fn run_stone_test(root: &Path) -> Result<()> {
    let root = root.canonicalize()?;
    if !root.is_dir() {
        anyhow::bail!("stone-test root must be a directory: {}", root.display());
    }

    // Output directory for compiled Lua, under root so the harness's tempdir
    // cleanup sweeps it.
    let output_path = root.join(".stone-test-out");
    std::fs::create_dir_all(&output_path)?;

    // Skull is the default backend for every execution path; the stone-test
    // harness compiles via `IncrementalProject` (bone `Session` + skull codegen -- the
    // same incremental compiler `rock stone` drives), NOT the legacy rock
    // pipeline. `init` produces the full `main.lua`; `recompile` emits the
    // overlay or a full rebuild and returns the hot-vs-restart decision.
    let dir = root.to_string_lossy().to_string();
    let main_path = output_path.join("main.lua");
    let incremental_path = output_path.join("incremental.lua");
    let mut stone: Option<IncrementalProject> = None;

    // Stdout writes must be serialized: events and responses share the
    // same fd, and a half-written line corrupts the protocol.
    let stdout_mu = Arc::new(std::sync::Mutex::new(()));

    let (ctl_tx, ctl_rx) = channel::<Ctl>();

    // embed_string inputs are sources the extension filter can't know; the
    // set comes from the compiled program, refreshed after every build
    // (mirrors `rock stone`).
    let comptime_watch: crate::file_watcher::SharedWatchSet = Default::default();

    // Watcher thread: forwards FileChangeEvents into ctl_rx.
    let _debouncer = {
        let (fc_tx, fc_rx) = channel::<crate::file_watcher::FileChangeEvent>();
        let watcher = crate::file_watcher::FileWatcher::new()
            .with_metadata_filter(true)
            // "toml" mirrors `rock stone`: a stick.toml edit is a session
            // rebuild (declared inputs re-resolve), not a dropped event.
            .with_extensions(vec!["rock".to_string(), "toml".to_string()])
            .with_shared_extra_files(comptime_watch.clone());
        let debouncer = watcher.watch(vec![root.clone()], fc_tx)?;
        let ctl_tx = ctl_tx.clone();
        std::thread::spawn(move || {
            while let Ok(ev) = fc_rx.recv() {
                if ctl_tx.send(Ctl::Files(ev.paths)).is_err() {
                    break;
                }
            }
        });
        debouncer
    };

    // Stdin commands run separately so shutdown can reach the compile loop.
    let stdin_handle = {
        let ctl_tx = ctl_tx.clone();
        let stdout_mu = stdout_mu.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let reader = BufReader::new(stdin.lock());
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let msg: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        write_response(
                            &stdout_mu,
                            &json!({
                                "kind": "response",
                                "ok": false,
                                "error": format!("bad json: {}", e),
                            }),
                        );
                        continue;
                    }
                };
                let seq = msg.get("seq").cloned().unwrap_or(Value::Null);
                let cmd = msg.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
                match cmd {
                    "ping" => write_response(
                        &stdout_mu,
                        &json!({
                            "seq": seq, "kind": "response", "ok": true,
                        }),
                    ),
                    "shutdown" => {
                        write_response(
                            &stdout_mu,
                            &json!({
                                "seq": seq, "kind": "response", "ok": true,
                            }),
                        );
                        let _ = ctl_tx.send(Ctl::Shutdown);
                        return;
                    }
                    other => write_response(
                        &stdout_mu,
                        &json!({
                            "seq": seq, "kind": "response", "ok": false,
                            "error": format!("unknown cmd: {}", other),
                        }),
                    ),
                }
            }
            // EOF on stdin: shut the worker down.
            let _ = ctl_tx.send(Ctl::Shutdown);
        })
    };

    // Mirror `rock stone`: do one initial compile on startup if there are
    // .rock files in `root`, before the watcher loop. `IncrementalProject::init`
    // builds the full `main.lua`; the harness's `await_reload_count(1)` then
    // sees this seed reload, which production also produces.
    let mut reload_n: u64 = 0;
    let initial_files = collect_rock_files(&root);
    if !initial_files.is_empty() {
        let t0 = Instant::now();
        let result = IncrementalProject::init(&dir, &main_path, &incremental_path);
        let compile_ms = t0.elapsed().as_secs_f64() * 1000.0;
        reload_n += 1;
        let record = match result {
            Ok(st) => {
                comptime_watch.replace_with_hashes(st.comptime_watch_files_with_hashes());
                stone = Some(st);
                ReloadRecord {
                    n: reload_n,
                    files: initial_files,
                    decision: "restart", // initial build is a full main.lua
                    compile_ms,
                    error: None,
                }
            }
            Err(e) => ReloadRecord {
                n: reload_n,
                files: initial_files,
                decision: "restart",
                compile_ms,
                error: Some(format!("{:#}", e)),
            },
        };
        emit_event(&stdout_mu, &record.to_event_json());
    }

    // Announce readiness so the harness knows the watcher is armed and
    // any initial-compile event has been emitted.
    emit_event(&stdout_mu, &json!({"kind": "event", "type": "ready"}));

    // Worker loop: react to file events and run compiles via IncrementalProject.
    while let Ok(ctl) = ctl_rx.recv() {
        match ctl {
            Ctl::Shutdown => break,
            Ctl::Files(paths) => {
                // Filter paths to those under root (skips our own output dir).
                let in_scope: HashSet<PathBuf> = paths
                    .into_iter()
                    .filter(|p| !p.starts_with(&output_path))
                    .collect();
                if in_scope.is_empty() {
                    continue;
                }
                let changed_vec: Vec<PathBuf> = in_scope.iter().cloned().collect();

                let t0 = Instant::now();
                // A body edit emits an overlay (`hot`); an interface/structural
                // edit (e.g. a global's type change) does a full rebuild
                // (`restart`). If the seed compile failed, retry a full init.
                let outcome: Result<Option<&'static str>> = match &mut stone {
                    Some(st) => st
                        .recompile(&changed_vec, &main_path, &incremental_path)
                        .and_then(|r| match r {
                            SkullReload::Full { .. } => Ok(Some("restart")),
                            SkullReload::Incremental { .. } => Ok(Some("hot")),
                            // A shape change defers the main.lua write (the live
                            // stone loop restarts the process); this harness has
                            // no live process, so materialize the program now
                            // and report the full-reload event.
                            SkullReload::ShapeChanged { .. } => {
                                st.force_full(&main_path, &incremental_path)?;
                                Ok(Some("restart"))
                            }
                            SkullReload::FullRequired { .. } => {
                                st.force_full(&main_path, &incremental_path)?;
                                Ok(Some("restart"))
                            }
                            SkullReload::NoChange => Ok(None),
                        }),
                    None => {
                        IncrementalProject::init(&dir, &main_path, &incremental_path).map(|st| {
                            stone = Some(st);
                            Some("restart")
                        })
                    }
                };
                let compile_ms = t0.elapsed().as_secs_f64() * 1000.0;
                let files: Vec<PathBuf> = in_scope.into_iter().collect();
                if let Some(st) = stone.as_ref() {
                    comptime_watch.replace_with_hashes(st.comptime_watch_files_with_hashes());
                }

                match outcome {
                    Ok(None) => {} // no source-level change -> no reload event
                    Ok(Some(decision)) => {
                        reload_n += 1;
                        emit_event(
                            &stdout_mu,
                            &ReloadRecord {
                                n: reload_n,
                                files,
                                decision,
                                compile_ms,
                                error: None,
                            }
                            .to_event_json(),
                        );
                    }
                    Err(e) => {
                        reload_n += 1;
                        emit_event(
                            &stdout_mu,
                            &ReloadRecord {
                                n: reload_n,
                                files,
                                decision: "restart",
                                compile_ms,
                                error: Some(format!("{:#}", e)),
                            }
                            .to_event_json(),
                        );
                    }
                }
            }
        }
    }

    // Wait for stdin reader to finish so we don't drop a pending response.
    let _ = stdin_handle.join();
    Ok(())
}

fn emit_event(stdout_mu: &Arc<std::sync::Mutex<()>>, v: &Value) {
    let _g = stdout_mu.lock().unwrap();
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", v);
    let _ = out.flush();
}

fn write_response(stdout_mu: &Arc<std::sync::Mutex<()>>, v: &Value) {
    let _g = stdout_mu.lock().unwrap();
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", v);
    let _ = out.flush();
}

/// Recursive scan for `.rock` files under `root`, ignoring our own output
/// directory. Used to detect whether an initial seed exists.
fn collect_rock_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let output_marker = root.join(".stone-test-out");
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path == output_marker {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rock") {
                out.push(path);
            }
        }
    }
    out
}
