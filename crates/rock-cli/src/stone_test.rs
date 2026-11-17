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
use std::sync::mpsc::channel;

/// Internal control messages the worker thread reacts to.
enum Ctl {
    /// File watcher fired with a debounced batch.
    Files(HashSet<PathBuf>),
    /// Stdin requested shutdown -- drain and exit.
    Shutdown,
}

pub(crate) fn run_stone_test(root: &Path) -> Result<()> {
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

    let (ctl_tx, ctl_rx) = channel::<Ctl>();

    // embed_string inputs are sources the extension filter can't know; the
    // set comes from the compiled program, refreshed after every build
    // (mirrors `rock stone`).
    let comptime_watch: crate::file_watcher::SharedWatchSet = Default::default();

    // Watcher thread: forwards FileChangeEvents into ctl_rx.
    let _debouncer = {
        let (fc_tx, fc_rx) = channel::<crate::file_watcher::FileChangeEvent>();
        // Same debounce as `rock stone`, so a burst of saves is one reload.
        let watcher = crate::file_watcher::FileWatcher::default_for_rock_sources()
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
                        write_message(&json!({
                            "kind": "response",
                            "ok": false,
                            "error": format!("bad json: {}", e),
                        }));
                        continue;
                    }
                };
                let seq = msg.get("seq").cloned().unwrap_or(Value::Null);
                let cmd = msg.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
                match cmd {
                    "ping" => write_message(&json!({
                        "seq": seq, "kind": "response", "ok": true,
                    })),
                    "shutdown" => {
                        write_message(&json!({
                            "seq": seq, "kind": "response", "ok": true,
                        }));
                        let _ = ctl_tx.send(Ctl::Shutdown);
                        return;
                    }
                    other => write_message(&json!({
                        "seq": seq, "kind": "response", "ok": false,
                        "error": format!("unknown cmd: {}", other),
                    })),
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
    let mut reload_n = 0u64;
    let mut compile = |files: Vec<PathBuf>| {
        let start = Instant::now();
        let outcome = match &mut stone {
            None => IncrementalProject::init(&dir, &main_path, &incremental_path).map(|project| {
                stone = Some(project);
                Some("restart")
            }),
            Some(project) => project
                .recompile(&files, &main_path, &incremental_path)
                .and_then(|reload| match reload {
                    SkullReload::Incremental { .. } => Ok(Some("hot")),
                    SkullReload::Full { .. } => Ok(Some("restart")),
                    // The harness has no runtime to restart, so finish the deferred write here.
                    SkullReload::ShapeChanged { .. } | SkullReload::FullRequired { .. } => {
                        project.force_full(&main_path, &incremental_path)?;
                        Ok(Some("restart"))
                    }
                    SkullReload::NoChange => Ok(None),
                }),
        };
        let compile_ms = start.elapsed().as_secs_f64() * 1000.0;
        if let Some(project) = &stone {
            comptime_watch.replace_with_hashes(project.comptime_watch_files_with_hashes());
        }
        let mut event = match outcome {
            Ok(None) => return,
            Ok(Some(decision)) => json!({
                "type": "reload", "decision": decision, "compile_ms": compile_ms,
            }),
            Err(error) => json!({"type": "compile_error", "message": format!("{error:#}")}),
        };
        reload_n += 1;
        event["kind"] = json!("event");
        event["n"] = json!(reload_n);
        event["files"] = json!(
            files
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
        );
        write_message(&event);
    };

    let initial_files = collect_rock_files(&root)?;
    if !initial_files.is_empty() {
        compile(initial_files);
    }
    write_message(&json!({"kind": "event", "type": "ready"}));

    while let Ok(Ctl::Files(paths)) = ctl_rx.recv() {
        let paths: HashSet<_> = paths
            .into_iter()
            .filter(|path| !path.starts_with(&output_path))
            .collect();
        if !paths.is_empty() {
            compile(paths.into_iter().collect());
        }
    }

    // Wait for stdin reader to finish so we don't drop a pending response.
    let _ = stdin_handle.join();
    Ok(())
}

fn write_message(value: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

/// Recursive scan for `.rock` files under `root`, ignoring our own output
/// directory. Used to detect whether an initial seed exists.
fn collect_rock_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let output_marker = root.join(".stone-test-out");
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|error| {
            anyhow::anyhow!("read initial source directory `{}`: {error}", dir.display())
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                anyhow::anyhow!(
                    "read entry in initial source directory `{}`: {error}",
                    dir.display()
                )
            })?;
            let path = entry.path();
            if path == output_marker {
                continue;
            }
            let meta = entry.metadata().map_err(|error| {
                anyhow::anyhow!("inspect initial source path `{}`: {error}", path.display())
            })?;
            if meta.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rock") {
                out.push(path);
            }
        }
    }
    Ok(out)
}
