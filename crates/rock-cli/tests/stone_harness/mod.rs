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

        let exe = rock_binary_path();
        let mut command = Command::new(&exe);
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
        let deadline = Instant::now() + timeout;
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
                }
                last_change = Instant::now();
            }
            if saw_required_event && last_change.elapsed() >= quiescent {
                return self.reload_log();
            }
            if Instant::now() >= deadline {
                let expectation = baseline.map_or_else(
                    || "the event stream to settle".to_string(),
                    |n| format!("an event after baseline {n}"),
                );
                panic!(
                    "await_settled: expected {}, never quiet for {}ms within {}ms (last_count={}, saw_required_event={})",
                    expectation,
                    quiescent.as_millis(),
                    timeout.as_millis(),
                    last_count,
                    saw_required_event,
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

/// How many host-projection leaves a `--dump-state` dump carries.
///
/// The CLASSIFICATION is the engine's (`stone/engine/host_projection.rock`):
/// a field whose declared type is a handle newtype is serialized as
/// `"<host_projection>"`, or as `"<host_projection:<digest>>"` when the thing
/// it names has logical content of its own -- a CPU image's pixels. So a dump
/// is already free of process-local allocation identity when it reaches a
/// comparison here, and this only counts the markers, as the DENOMINATOR: a
/// green comparison is evidence only when there was something to exclude.
///
/// It replaced a test-only policy keyed on a wrapper's `__ty` plus a field
/// spelling, which could drift from the engine silently and which deleted a
/// `CpuImage`'s pixels along with its handle. The controls for the
/// classification itself live in `games/testbed/tests/sims/b3_host_projection.rock`,
/// where they can drive the real serializer.
pub fn count_host_projections(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::String(s) => usize::from(s.starts_with("<host_projection")),
        serde_json::Value::Object(obj) => obj.values().map(count_host_projections).sum(),
        serde_json::Value::Array(items) => items.iter().map(count_host_projections).sum(),
        _ => 0,
    }
}

/// The replay row tags the format gained over time, with the version that
/// introduced each. This mirrors the admission gates in `__replay_load`
/// (`stone/engine/replay.rock`): a row BELOW its gate counts as malformed, not
/// as an ignored unknown, and sixteen of them make the loader call the file
/// corrupt and exit 4.
const ROW_SINCE: &[(&str, u32)] = &[
    ("p ", 9),
    ("s ", 11),
    ("i ", 12),
    ("j ", 14),
    ("a ", 15),
    ("u ", 16),
];

/// Rewrite a log this build wrote into one a version-`to` reader accepts.
///
/// Rewriting only the header does not produce a legacy log: every row the
/// format gained after `to` is still in the file, and the reader rejects the
/// whole log as corrupt before it reaches whatever the fixture wanted to test.
/// The header-only rewrites predate v15 and v16, which added a per-observation
/// row each.
///
/// Covers v6 and up. Below v6 the `t` row itself has a different arity and
/// below v5 the `env` grammar does, neither of which this rewrites.
pub fn downgrade_replay(text: &str, to: u32) -> String {
    assert!(
        (6..rock_replay_protocol::REPLAY_VERSION).contains(&to),
        "downgrade_replay covers v6..v{}, not v{to}",
        rock_replay_protocol::REPLAY_VERSION - 1
    );
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if line == rock_replay_protocol::replay_header() {
            out.push(format!("STONE_REPLAY {to}"));
            continue;
        }
        if ROW_SINCE.iter().any(|(tag, since)| *since > to && line.starts_with(tag)) {
            continue;
        }
        // v10 added the per-phase columns to a timing row; a v9 reader reads
        // the total alone and counts a row carrying more as malformed.
        if to == 9 {
            if let Some(rest) = line.strip_prefix("p ") {
                let f: Vec<&str> = rest.split_whitespace().collect();
                out.push(format!("p {} {} {}", f[0], f[1], f[2]));
                continue;
            }
        }
        out.push(line.to_string());
    }
    out.join("\n") + "\n"
}

/// Drop the dump-state envelope keys and everything the engine names as
/// non-reproducible, leaving a dump two runs of the same session must agree on.
///
/// The name set comes from the dump itself
/// (`stone_dump_nonreproducible_json`, `stone/engine/stone_diag.rock`) rather
/// than from a list kept here: the engine owns those globals, and the last time
/// a test kept its own list the always-on hitch ledger landed as a registered
/// global and turned `replay_reproduces_dump_state_byte_identical` red with
/// JIT trace counters nobody had told the comparison about.
pub fn strip_nonreproducible(v: &mut serde_json::Value) {
    let names: Vec<String> = v
        .get("__stone_nonreproducible")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items.iter().filter_map(|n| n.as_str().map(str::to_string)).collect()
        })
        .unwrap_or_default();
    assert!(
        !names.is_empty(),
        "the dump must publish __stone_nonreproducible; without it this \
         comparison silently stops excluding wall-clock state"
    );
    for name in names {
        let (package, field) = name.split_once('.').unwrap_or_else(||
            panic!("__stone_nonreproducible names `package.field`, got {name:?}"));
        if let Some(pkg) = v.get_mut(package).and_then(serde_json::Value::as_object_mut) {
            pkg.remove(field);
        }
    }
    if let Some(root) = v.as_object_mut() {
        // The test-observability envelope: replay-only counters that
        // legitimately differ between a record run and its replay.
        root.remove("__stone_diag");
        root.remove("__stone_nonreproducible");
    }
}

/// The registered packages of a dump-state object, in dump order.
///
/// A dump is package keys plus the `__stone_`-prefixed envelope keys the
/// runner splices on (`stone_dump_state_envelope`), and only the former hold
/// globals. Every caller walking "every package" needs the same skip, and the
/// one that open-coded it panicked the day the envelope gained a key whose
/// value is not an object.
pub fn dump_packages(
    v: &serde_json::Value,
) -> impl Iterator<Item = (&String, &serde_json::Map<String, serde_json::Value>)> {
    v.as_object()
        .expect("dump is an object")
        .iter()
        .filter(|(key, _)| !key.starts_with("__stone_"))
        .map(|(key, value)| {
            (key, value.as_object().unwrap_or_else(||
                panic!("dump package {key} is an object, got {value}")))
        })
}

/// `("GAME_ART_VAULT", <root>)` for a fixture that substitutes `HOME`.
///
/// `rock_host::art_vault::root` reads `$GAME_ART_VAULT`, else
/// `$HOME/projects/game-art-vault`. A fixture that points `HOME` at a temp
/// directory to isolate the preferences dir therefore moves the art vault too,
/// and every `$GAME_ART_VAULT` entry in a game's `[assets].overlays` stops
/// expanding -- the game refuses to start, with a message about the vault
/// rather than about the fixture. Resolve it from the REAL environment first
/// and pass it through.
pub fn art_vault_env() -> (&'static str, String) {
    let root = rock_host::art_vault::root().expect("game-art vault");
    (rock_host::art_vault::ENV, root.to_string_lossy().into_owned())
}
