//! Harness for `rock stone-test`. Phase A: headless watcher only.
//!
//! Each `StoneHarness` owns a tempdir + a child `rock stone-test` process.
//! Tests write files via the harness, await reload events, and assert
//! against the captured event log. `Drop` SIGTERMs the child and wipes
//! the tempdir.
//!
//! See `docs/stone-test-harness-plan.md` for the protocol and phasing.

#![allow(dead_code)] // grows over phases B/C

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct ReloadEvent {
    pub n: u64,
    pub files: Vec<PathBuf>,
    pub decision: String, // "hot" | "restart"
    pub compile_ms: f64,
}

#[derive(Clone, Debug)]
pub struct CompileErrorEvent {
    pub n: u64,
    pub files: Vec<PathBuf>,
    pub message: String,
}

#[derive(Clone, Debug)]
pub enum Event {
    Reload(ReloadEvent),
    CompileError(CompileErrorEvent),
    Ready,
    Other(Value),
}

pub struct StoneHarness {
    tempdir: tempfile::TempDir,
    child: Child,
    stdin: ChildStdin,
    events: Arc<Mutex<Vec<Event>>>,
    stderr_buf: Arc<Mutex<String>>,
    seq: AtomicU64,
}

impl StoneHarness {
    /// Spawn a new harness with the given seed files (path → content,
    /// paths relative to tempdir root). Blocks until the child emits
    /// `ready`.
    pub fn spawn(seed: &[(&str, &str)]) -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        for (path, content) in seed {
            let full = tempdir.path().join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, content).unwrap();
        }

        let exe = rock_binary_path();
        let mut child = Command::new(&exe)
            .arg("stone-test")
            .arg(tempdir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn rock stone-test");

        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let stderr = child.stderr.take().expect("child stderr");

        let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        let events_w = events.clone();

        // stderr drain thread — captures into a buffer so timeout panics
        // can include it. Without this, a wedged child fills its stderr
        // pipe and blocks.
        let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let stderr_buf_w = stderr_buf.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                stderr_buf_w.lock().unwrap().push_str(&line);
                stderr_buf_w.lock().unwrap().push('\n');
            }
        });

        // stdout reader thread: parse each line, push events / responses.
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let kind = v.get("kind").and_then(|s| s.as_str()).unwrap_or("");
                if kind == "event" {
                    let ev = parse_event(&v);
                    events_w.lock().unwrap().push(ev);
                }
                // responses are not captured in Phase A — `ping`/`shutdown`
                // are fire-and-forget for tests. Future phases may queue
                // them by seq.
            }
        });

        let h = Self {
            tempdir,
            child,
            stdin,
            events,
            stderr_buf,
            seq: AtomicU64::new(0),
        };
        h.await_ready();
        h
    }

    pub fn root(&self) -> &Path {
        self.tempdir.path()
    }

    /// Path to `main.lua` produced by the most recent compile. Reads
    /// nothing — caller does `fs::read_to_string` if needed.
    pub fn compiled_lua_path(&self) -> PathBuf {
        self.tempdir.path().join(".stone-test-out").join("main.lua")
    }

    /// Read the most recent compiled Lua output. Convenience wrapper.
    pub fn compiled_lua(&self) -> String {
        std::fs::read_to_string(self.compiled_lua_path())
            .expect("compiled_lua: main.lua not present (no successful compile yet?)")
    }

    /// Path to the skull `incremental.lua` overlay (the chunk a body edit
    /// emits — `main.lua` itself is left unchanged on a hot reload).
    pub fn overlay_path(&self) -> PathBuf {
        self.tempdir.path().join(".stone-test-out").join("incremental.lua")
    }

    /// Read the overlay, or empty string if no body edit has produced one yet.
    pub fn overlay_lua(&self) -> String {
        std::fs::read_to_string(self.overlay_path()).unwrap_or_default()
    }

    /// Write a file relative to the tempdir root. Triggers the watcher.
    pub fn write_file(&self, rel: &str, content: &str) {
        let full = self.tempdir.path().join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, content).expect("write_file");
    }

    /// Block until at least `n_after` reload events have been observed,
    /// or the timeout elapses. `n_after` is the total reload-event count
    /// to wait for, not a delta.
    pub fn await_reload_count(&self, n_total: usize, timeout: Duration) -> Vec<ReloadEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            let snapshot = self.reload_log();
            if snapshot.len() >= n_total {
                return snapshot;
            }
            if Instant::now() >= deadline {
                panic!(
                    "await_reload_count: timed out waiting for {} reloads, observed {}",
                    n_total, snapshot.len(),
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait for the system to *settle*: keep checking until the reload
    /// log hasn't grown for `quiescent` ms, or `timeout` elapses.
    /// Returns the full reload log at that point.
    ///
    /// Use this for "make N writes, then assert how many reloads
    /// coalesced": the exact final count is host-dependent, but the
    /// settled count once events stop arriving is stable.
    pub fn await_settled(&self, quiescent: Duration, timeout: Duration) -> Vec<ReloadEvent> {
        let deadline = Instant::now() + timeout;
        let mut last_count = self.reload_log().len();
        let mut last_change = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(20));
            let now_count = self.reload_log().len();
            if now_count != last_count {
                last_count = now_count;
                last_change = Instant::now();
            }
            if last_change.elapsed() >= quiescent {
                return self.reload_log();
            }
            if Instant::now() >= deadline {
                panic!(
                    "await_settled: never quiet for {}ms within {}ms (last_count={})",
                    quiescent.as_millis(),
                    timeout.as_millis(),
                    last_count,
                );
            }
        }
    }

    /// Snapshot of all reload (success) events seen so far.
    pub fn reload_log(&self) -> Vec<ReloadEvent> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Event::Reload(r) => Some(r.clone()),
                _ => None,
            })
            .collect()
    }

    /// Snapshot of all compile-error events seen so far.
    pub fn error_log(&self) -> Vec<CompileErrorEvent> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Event::CompileError(e) => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    fn await_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let saw_ready = self
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, Event::Ready));
            if saw_ready {
                return;
            }
            if Instant::now() >= deadline {
                let stderr = self.stderr_buf.lock().unwrap().clone();
                let events = self.events.lock().unwrap().clone();
                panic!(
                    "stone-test child never emitted ready within 15s\nevents observed: {:?}\nchild stderr:\n{}",
                    events, stderr,
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&mut self, cmd: &str, args: Value) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let msg = json!({"seq": seq, "cmd": cmd, "args": args});
        let line = format!("{}\n", msg);
        let _ = self.stdin.write_all(line.as_bytes());
        let _ = self.stdin.flush();
    }
}

impl Drop for StoneHarness {
    fn drop(&mut self) {
        // Ask politely first, then SIGKILL if the child wedges.
        self.send("shutdown", json!({}));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => return,
            }
        }
    }
}

fn parse_event(v: &Value) -> Event {
    let ty = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
    match ty {
        "ready" => Event::Ready,
        "reload" => Event::Reload(ReloadEvent {
            n: v.get("n").and_then(|n| n.as_u64()).unwrap_or(0),
            files: v
                .get("files")
                .and_then(|f| f.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(PathBuf::from))
                        .collect()
                })
                .unwrap_or_default(),
            decision: v
                .get("decision")
                .and_then(|s| s.as_str())
                .unwrap_or("hot")
                .to_string(),
            compile_ms: v.get("compile_ms").and_then(|n| n.as_f64()).unwrap_or(0.0),
        }),
        "compile_error" => Event::CompileError(CompileErrorEvent {
            n: v.get("n").and_then(|n| n.as_u64()).unwrap_or(0),
            files: v
                .get("files")
                .and_then(|f| f.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(PathBuf::from))
                        .collect()
                })
                .unwrap_or_default(),
            message: v
                .get("message")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        _ => Event::Other(v.clone()),
    }
}

/// Resolve the path to the `rock` binary built by cargo. Mirrors how
/// `game_snapshot_tests.rs` finds it.
fn rock_binary_path() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set for integration tests of the same crate.
    if let Some(p) = option_env!("CARGO_BIN_EXE_rock") {
        return PathBuf::from(p);
    }
    // Fallback: target/debug/rock relative to the workspace.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let mut p = PathBuf::from(manifest_dir);
    p.pop(); // workspace root
    p.push("target");
    p.push("debug");
    p.push("rock");
    p
}

/// Native resource IDENTITY in a `--dump-state` dump, by the `__ty` of the
/// wrapper that carries it.
///
/// A GL/image/mesh id is a process-local allocation counter, not simulation
/// state: two processes that simulate identically still mint their ids in
/// whatever order they happened to create resources. Dump-state used to render
/// every one of these as the string `"<cdata>"`, because a Stone handle crossed
/// `extern(ffi) -> int` as a boxed `uint64_t` -- so a comparison over two dumps
/// was blind to the whole class. It is a Rock number now, which is what makes
/// the difference visible at all.
///
/// The list is by `__ty` and not by field spelling: `data`, `image`, `handle`
/// and `shader` are ordinary names that a game may use for ordinary state.
pub fn strip_native_handles(v: &mut serde_json::Value) -> usize {
    fn go(v: &mut serde_json::Value, n: &mut usize) {
        match v {
            serde_json::Value::Object(obj) => {
                let fields: &[&str] = match obj.get("__ty").and_then(|t| t.as_str()) {
                    Some("WatcherShader") => &["shader"],
                    Some("CpuImage") => &["data", "image"],
                    Some("GpuMesh") => &["handle"],
                    _ => &[],
                };
                for f in fields {
                    if obj.remove(*f).is_some() {
                        *n += 1;
                    }
                }
                for (_, child) in obj.iter_mut() {
                    go(child, n);
                }
            }
            serde_json::Value::Array(items) => {
                for child in items {
                    go(child, n);
                }
            }
            _ => {}
        }
    }
    let mut n = 0;
    go(v, &mut n);
    n
}
