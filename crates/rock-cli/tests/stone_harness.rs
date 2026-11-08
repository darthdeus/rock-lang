//! Owns a temporary project and the headless watcher process.

#![allow(dead_code)]

use rock_core::clock::Instant;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// Backstop for `await_ready`: child spawn plus the seeded initial compile.
/// Completion-led: ready or child death ends the wait; this only catches hangs.
const READY_TIMEOUT: Duration = Duration::from_secs(180);

/// Stall budget granted by each new event while settling: progress extends
/// the wait, so a slow-but-moving compile stream settles on its own events,
/// not on the clock. Bounded by one grant past the first-event budget.
const SETTLE_STALL_BUDGET: Duration = Duration::from_secs(60);

pub struct StoneHarness {
    tempdir: tempfile::TempDir,
    child: Mutex<Child>,
    stdin: ChildStdin,
    events: Arc<Mutex<Vec<Event>>>,
    stderr_buf: Arc<Mutex<String>>,
    seq: AtomicU64,
}

impl StoneHarness {
    /// Spawn a new harness with the given seed files (path -> content,
    /// paths relative to tempdir root). Blocks until the child emits
    /// `ready`.
    pub fn spawn(seed: &[(&str, &str)]) -> Self {
        Self::spawn_with_env(seed, &[])
    }

    /// [`Self::spawn`] with extra environment for the child `rock stone-test`.
    /// Used to drive a process-scoped emission switch (which the parent test
    /// binary must not set for itself -- it shares a process with every other
    /// test here).
    pub fn spawn_with_env(seed: &[(&str, &str)], env: &[(&str, &str)]) -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        for (path, content) in seed {
            let full = tempdir.path().join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, content).unwrap();
        }

        let mut command = Command::new(env!("CARGO_BIN_EXE_rock"));
        command
            .env("ROCK_NUM_THREADS", "1")
            .env("ROCK_INVOCATION_LOG", "off");
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command
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

        // stderr drain thread -- captures into a buffer so timeout panics
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
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let kind = v.get("kind").and_then(|s| s.as_str()).unwrap_or("");
                if kind == "event" {
                    let ev = parse_event(&v);
                    events_w.lock().unwrap().push(ev);
                }
                // responses are not captured in Phase A -- `ping`/`shutdown`
                // are fire-and-forget for tests. Future phases may queue
                // them by seq.
            }
        });

        let h = Self {
            tempdir,
            child: Mutex::new(child),
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
    /// nothing -- caller does `fs::read_to_string` if needed.
    pub fn compiled_lua_path(&self) -> PathBuf {
        self.tempdir.path().join(".stone-test-out").join("main.lua")
    }

    /// Read the most recent compiled Lua output. Convenience wrapper.
    pub fn compiled_lua(&self) -> String {
        std::fs::read_to_string(self.compiled_lua_path())
            .expect("compiled_lua: main.lua not present (no successful compile yet?)")
    }

    /// Path to the skull `incremental.lua` overlay (the chunk a body edit
    /// emits -- `main.lua` itself is left unchanged on a hot reload).
    pub fn overlay_path(&self) -> PathBuf {
        self.tempdir
            .path()
            .join(".stone-test-out")
            .join("incremental.lua")
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
                    n_total,
                    snapshot.len(),
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Count completion events, including failed compiles.
    pub fn event_count(&self) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, Event::Reload(_) | Event::CompileError(_)))
            .count()
    }

    /// Wait for the system to *settle*: keep checking until the completion
    /// event stream hasn't grown for `quiescent` ms, or `timeout` elapses.
    /// Returns the full reload log at that point.
    ///
    /// Use this for "make N writes, then assert how many reloads
    /// coalesced": the exact final count is host-dependent, but the
    /// settled count once events stop arriving is stable.
    pub fn await_settled(&self, quiescent: Duration, timeout: Duration) -> Vec<ReloadEvent> {
        self.await_settled_impl(None, quiescent, timeout)
    }

    /// Like `await_settled`, but require an event after the call begins.
    /// Compile errors count as events even though they are not reloads.
    pub fn await_write_settled(&self, quiescent: Duration, timeout: Duration) -> Vec<ReloadEvent> {
        self.await_settled_after(self.event_count(), quiescent, timeout)
    }

    /// Wait for at least one completion event after `baseline`, then wait for
    /// the event stream to become quiet.
    ///
    /// A quiet stream before the first event is not a settled stream: under
    /// load, the child may still be starting or compiling the requested
    /// change. This barrier keeps that delay inside `timeout` instead of
    /// turning it into a false empty result.
    pub fn await_settled_after(
        &self,
        baseline: usize,
        quiescent: Duration,
        timeout: Duration,
    ) -> Vec<ReloadEvent> {
        self.await_settled_impl(Some(baseline), quiescent, timeout)
    }

    fn await_settled_impl(
        &self,
        baseline: Option<usize>,
        quiescent: Duration,
        timeout: Duration,
    ) -> Vec<ReloadEvent> {
        let started = Instant::now();
        // First-event budget: the initial IncrementalProject::init takes ~17s at
        // full-suite load, so this must dwarf any per-compile latency. Each
        // new event then grants a stall budget, capped so a steady drip of
        // events still trips the backstop instead of hanging the suite.
        let hard_cap = started + timeout + SETTLE_STALL_BUDGET;
        let mut deadline = started + timeout;
        let mut last_count = self.event_count();
        let mut saw_required_event = baseline.map_or(true, |n| last_count > n);
        let mut last_change = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(20));
            let now_count = self.event_count();
            if now_count != last_count {
                last_count = now_count;
                if baseline.map_or(true, |n| now_count > n) {
                    saw_required_event = true;
                    deadline = (Instant::now() + SETTLE_STALL_BUDGET).min(hard_cap);
                }
                last_change = Instant::now();
            }
            if saw_required_event && last_change.elapsed() >= quiescent {
                return self.reload_log();
            }
            // The child only exits mid-test on a crash (shutdown comes from
            // Drop at test end): fail fast with its stderr, not the backstop.
            if let Some(status) = self.child_exit() {
                panic!(
                    "await_settled: child exited ({status}) while waiting for {}, waited {}ms (last_count={}, saw_required_event={})\n{}",
                    settle_expectation(baseline),
                    started.elapsed().as_millis(),
                    last_count,
                    saw_required_event,
                    self.failure_context(),
                );
            }
            if Instant::now() >= deadline {
                panic!(
                    "await_settled: expected {}, never quiet for {}ms (last_count={}, saw_required_event={}, waited {}ms)\n{}",
                    settle_expectation(baseline),
                    quiescent.as_millis(),
                    last_count,
                    saw_required_event,
                    started.elapsed().as_millis(),
                    self.failure_context(),
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
        let deadline = Instant::now() + READY_TIMEOUT;
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
            if let Some(status) = self.child_exit() {
                panic!(
                    "stone-test child exited ({status}) before emitting ready\n{}",
                    self.failure_context(),
                );
            }
            if Instant::now() >= deadline {
                panic!(
                    "stone-test child never emitted ready within {}s\n{}",
                    READY_TIMEOUT.as_secs(),
                    self.failure_context(),
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Exit status if the child is gone, without blocking.
    fn child_exit(&self) -> Option<std::process::ExitStatus> {
        self.child.lock().unwrap().try_wait().ok().flatten()
    }

    /// Child liveness line for timeout diagnostics. Locks nothing but the child.
    fn child_status(&self) -> String {
        match self.child.lock().unwrap().try_wait() {
            Ok(None) => "alive".to_string(),
            Ok(Some(status)) => format!("exited: {status}"),
            Err(e) => format!("wait error: {e}"),
        }
    }

    /// What the child was doing when a wait gave up: liveness, observed
    /// events, and the tail of its stderr. Each store is cloned under its
    /// own lock and released before formatting, so this nests no locks.
    fn failure_context(&self) -> String {
        let status = self.child_status();
        let events = self.events.lock().unwrap().clone();
        let stderr = self.stderr_buf.lock().unwrap().clone();
        let event_summary = if events.len() <= 16 {
            format!("{events:?}")
        } else {
            format!(
                "{} events, last 8: {:?}",
                events.len(),
                &events[events.len() - 8..]
            )
        };
        let err_lines: Vec<&str> = stderr.lines().collect();
        let err_tail = if err_lines.len() <= 30 {
            stderr
        } else {
            err_lines[err_lines.len() - 30..].join("\n")
        };
        format!(
            "child: {status}\nevents observed: {event_summary}\nchild stderr (tail):\n{err_tail}"
        )
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
        let mut child = self.child.lock().unwrap();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => return,
            }
        }
    }
}

fn settle_expectation(baseline: Option<usize>) -> String {
    baseline.map_or_else(
        || "the event stream to settle".to_string(),
        |n| format!("an event after baseline {n}"),
    )
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
