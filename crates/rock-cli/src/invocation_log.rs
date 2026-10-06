//! One JSON line per dev-loop `rock` invocation, appended to a per-user log so
//! edit-to-feedback times can be mined with `tools/loop-stats.py`
//! (docs/agent-build-and-runtime.md, "Invocation log").
//!
//! The record is written once, at exit: `finish` from `main` or `exit`, and an
//! `atexit` fallback for paths that leave through `std::process::exit` outside
//! this crate or unwind out of `main` (exit code `null`). Git state comes from
//! one background `git status` started at `begin`, so it overlaps the command
//! instead of adding to it. Recording never changes the command's outcome:
//! write errors are logged and dropped.

use serde_json::{Map, Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SCHEMA: u64 = 1;
const LOG_ENV: &str = "ROCK_INVOCATION_LOG";
/// Set for children so a nested `rock` (the sim compile child) is marked as
/// such instead of reading as a separate loop step.
pub const PARENT_ENV: &str = "ROCK_INVOCATION_PARENT";
/// Context variables copied into `env` when set.
const CONTEXT_ENV: &[&str] = &[
    "ROCK_JOB_ID",
    "ROCK_JOBS_BY",
    "ROCK_JOBS_SELF",
    "ROCK_CHANGE_ID",
    "ROCK_TEST_FILTER",
    "ROCK_TEST_SHARDS",
];
/// Upper bound on waiting for the background git probe after the command is
/// done; a slower probe is killed and its fields recorded as null.
const GIT_WAIT: Duration = Duration::from_millis(1500);
/// Dirty paths stat'ed for the newest edit time; a larger dirty set records
/// the newest among the first this many.
const MTIME_STAT_CAP: usize = 4000;

struct Pending {
    pid: u32,
    path: PathBuf,
    start: SystemTime,
    started: Instant,
    repo_root: Option<PathBuf>,
    git: Option<Child>,
    record: Map<String, Value>,
    counts: Map<String, Value>,
    phases: Map<String, Value>,
}

static STATE: Mutex<Option<Pending>> = Mutex::new(None);

/// The commands that make up the edit-compile-test loop.
pub fn is_loop_command(name: &str) -> bool {
    matches!(
        name,
        "compile" | "compile-single" | "test" | "test-compile" | "run" | "frame" | "stone"
            | "watch" | "sim" | "lint" | "stone-test" | "web" | "fmt"
    )
}

/// Where records go: `ROCK_INVOCATION_LOG` (a path, or `off`), else
/// `$XDG_STATE_HOME/rock/invocations.jsonl`, else
/// `~/.local/state/rock/invocations.jsonl`. `None` when disabled or no home.
pub fn log_path() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os(LOG_ENV).filter(|v| !v.is_empty()) {
        return (value != "off").then(|| PathBuf::from(value));
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("rock").join("invocations.jsonl"))
}

/// Start recording this invocation if it is a loop command and the log is on.
/// Call once from `main`, single-threaded, before any child process spawns.
pub fn begin(command: &'static str, backend: &str) {
    if !is_loop_command(command) {
        return;
    }
    let Some(path) = log_path() else { return };
    let start = SystemTime::now();
    let started = Instant::now();
    let cwd = std::env::current_dir().ok();
    let repo_root = cwd.as_deref().and_then(find_repo_root);
    let git = repo_root.as_deref().and_then(spawn_git_status);
    let parent = std::env::var(PARENT_ENV).ok().and_then(|v| v.parse::<u64>().ok());
    let pid = std::process::id();
    // SAFETY: called from `main` before any thread or child is spawned.
    unsafe { std::env::set_var(PARENT_ENV, pid.to_string()) };

    let mut record = Map::new();
    record.insert("schema".into(), json!(SCHEMA));
    record.insert("kind".into(), json!(command));
    record.insert("backend".into(), json!(backend));
    record.insert("argv".into(), json!(std::env::args().collect::<Vec<_>>()));
    record.insert("exe".into(), json!(std::env::current_exe().ok()));
    record.insert("host".into(), json!(hostname()));
    record.insert("pid".into(), json!(pid));
    let ppid = unsafe { libc::getppid() };
    record.insert("ppid".into(), json!(ppid));
    record.insert("parent_comm".into(), json!(process_name(ppid)));
    record.insert("nested_in".into(), json!(parent));
    record.insert("cwd".into(), json!(cwd));
    record.insert("repo_root".into(), json!(repo_root));
    let env: Map<String, Value> = CONTEXT_ENV
        .iter()
        .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| (k.to_string(), json!(v))))
        .collect();
    record.insert("env".into(), Value::Object(env));

    let pending = Pending {
        pid,
        path,
        start,
        started,
        repo_root,
        git,
        record,
        counts: Map::new(),
        phases: Map::new(),
    };
    if let Ok(mut state) = STATE.lock() {
        *state = Some(pending);
    }
    // SAFETY: registering a plain `extern "C" fn` with no captured state.
    if unsafe { libc::atexit(finish_at_exit) } != 0 {
        log::warn!(target: "CLI", "invocation log: atexit registration failed; abnormal exits go unrecorded");
    }
}

/// Record counts the command already computed (selected/passed/failed...).
/// Later values for the same key overwrite earlier ones.
pub fn note_counts(counts: &[(&str, usize)]) {
    with_pending(|p| {
        for (k, v) in counts {
            p.counts.insert((*k).to_string(), json!(v));
        }
    });
}

/// Record one phase duration the command already measured.
pub fn note_phase(name: &str, ms: u64) {
    with_pending(|p| {
        p.phases.insert(name.to_string(), json!(ms));
    });
}

/// Record an extra top-level field (a selection flag, a linked run id).
pub fn note(key: &str, value: Value) {
    with_pending(|p| {
        p.record.insert(key.to_string(), value);
    });
}

fn with_pending(f: impl FnOnce(&mut Pending)) {
    if let Ok(mut state) = STATE.lock() {
        if let Some(p) = state.as_mut() {
            f(p);
        }
    }
}

/// Write the record (once). `exit_code` is `None` when the process is leaving
/// by a path that did not report one.
pub fn finish(exit_code: Option<i32>, exit_path: &str) {
    let pending = match STATE.try_lock() {
        Ok(mut state) => state.take(),
        // Another thread holds the lock while the process exits; dropping
        // the record is better than deadlocking the exit.
        Err(_) => None,
    };
    let Some(p) = pending else { return };
    // A forked child inherits the pending record; only the owner writes it.
    if p.pid != std::process::id() {
        return;
    }
    let line = render(p, exit_code, exit_path);
    if let Err(e) = append_line(&line.0, &line.1) {
        log::warn!(target: "CLI", "invocation log: could not append to {}: {e}", line.0.display());
    }
}

/// `std::process::exit`, recording the code first. Use for every exit the
/// CLI takes on purpose, so the record carries the real exit code.
pub fn exit(code: i32) -> ! {
    finish(Some(code), "exit");
    std::process::exit(code)
}

extern "C" fn finish_at_exit() {
    finish(None, "atexit");
}

fn render(mut p: Pending, exit_code: Option<i32>, exit_path: &str) -> (PathBuf, String) {
    let wall_ms = p.started.elapsed().as_millis() as u64;
    let end = SystemTime::now();
    let git = p.git.take().and_then(|child| collect_git(child, p.repo_root.as_deref()));
    let r = &mut p.record;
    r.insert("start_utc".into(), json!(utc(p.start)));
    r.insert("end_utc".into(), json!(utc(end)));
    r.insert("start_unix_ms".into(), json!(unix_ms(p.start)));
    r.insert("end_unix_ms".into(), json!(unix_ms(end)));
    r.insert("wall_ms".into(), json!(wall_ms));
    r.insert("exit_code".into(), json!(exit_code));
    r.insert("exit_path".into(), json!(exit_path));
    let (head, branch, dirty, dirty_paths, dirty_mtime_ms) = match git {
        Some(g) => (json!(g.head), json!(g.branch), json!(g.dirty_paths > 0), json!(g.dirty_paths), json!(g.dirty_mtime_ms)),
        None => (Value::Null, Value::Null, Value::Null, Value::Null, Value::Null),
    };
    r.insert("head".into(), head);
    r.insert("branch".into(), branch);
    r.insert("dirty".into(), dirty);
    r.insert("dirty_paths".into(), dirty_paths);
    r.insert("dirty_mtime_ms".into(), dirty_mtime_ms);
    r.insert("counts".into(), Value::Object(std::mem::take(&mut p.counts)));
    r.insert("phases_ms".into(), Value::Object(std::mem::take(&mut p.phases)));
    let mut line = Value::Object(std::mem::take(r)).to_string();
    line.push('\n');
    (p.path, line)
}

/// One `write` on an `O_APPEND` descriptor, so concurrent invocations never
/// interleave within a line.
fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let written = file.write(line.as_bytes())?;
    if written != line.len() {
        return Err(std::io::Error::other(format!("short write: {written} of {} bytes", line.len())));
    }
    Ok(())
}

fn find_repo_root(start: &Path) -> Option<PathBuf> {
    start.ancestors().find(|dir| dir.join(".git").exists()).map(Path::to_path_buf)
}

fn spawn_git_status(root: &Path) -> Option<Child> {
    // `--no-optional-locks`: a background status must never take index.lock
    // and fail the author's concurrent `git commit`.
    let spawned = Command::new("git")
        .args(["--no-optional-locks", "status", "--porcelain=v2", "--branch", "-z", "--untracked-files=normal"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(child) => Some(child),
        Err(e) => {
            log::debug!(target: "CLI", "invocation log: git status did not start: {e}");
            None
        }
    }
}

struct GitState {
    head: Option<String>,
    branch: Option<String>,
    dirty_paths: usize,
    dirty_mtime_ms: Option<u64>,
}

fn collect_git(mut child: Child, root: Option<&Path>) -> Option<GitState> {
    let deadline = Instant::now() + GIT_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            Ok(None) | Err(_) => {
                log::debug!(target: "CLI", "invocation log: git status exceeded {GIT_WAIT:?}; recording no git state");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = Vec::new();
    std::io::Read::read_to_end(child.stdout.as_mut()?, &mut out).ok()?;
    Some(parse_status(&out, root))
}

/// Parse `git status --porcelain=v2 --branch -z`.
fn parse_status(out: &[u8], root: Option<&Path>) -> GitState {
    let mut state = GitState { head: None, branch: None, dirty_paths: 0, dirty_mtime_ms: None };
    let mut fields = out.split(|b| *b == 0).map(String::from_utf8_lossy);
    let mut stat_budget = MTIME_STAT_CAP;
    while let Some(entry) = fields.next() {
        if let Some(oid) = entry.strip_prefix("# branch.oid ") {
            state.head = (oid != "(initial)").then(|| oid.to_string());
            continue;
        }
        if let Some(head) = entry.strip_prefix("# branch.head ") {
            state.branch = (head != "(detached)").then(|| head.to_string());
            continue;
        }
        let path = match entry.chars().next() {
            // ordinary: 8 space-separated fields then the path
            Some('1') => entry.splitn(9, ' ').nth(8).map(str::to_string),
            // rename/copy: 9 fields then the path; the origin path follows as
            // its own NUL-terminated field
            Some('2') => {
                let p = entry.splitn(10, ' ').nth(9).map(str::to_string);
                fields.next();
                p
            }
            Some('u') => entry.splitn(11, ' ').nth(10).map(str::to_string),
            Some('?') => entry.get(2..).map(str::to_string),
            _ => None,
        };
        let Some(path) = path else { continue };
        state.dirty_paths += 1;
        if stat_budget > 0 {
            stat_budget -= 1;
            if let Some(root) = root {
                // A deleted path has no mtime; the newest surviving edit stands.
                if let Ok(mtime) = std::fs::symlink_metadata(root.join(&path)).and_then(|m| m.modified()) {
                    let ms = unix_ms(mtime);
                    state.dirty_mtime_ms = Some(state.dirty_mtime_ms.map_or(ms, |cur| cur.max(ms)));
                }
            }
        }
    }
    state
}

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// RFC 3339 UTC with milliseconds, e.g. `2026-10-06T12:34:56.789Z`.
fn utc(t: SystemTime) -> String {
    let ms = unix_ms(t);
    let secs = ms / 1000;
    let (days, sod) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60,
        ms % 1000
    )
}

fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; the result is NUL-terminated
    // on success (truncation leaves the last byte zero from initialization).
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) };
    if rc != 0 {
        return None;
    }
    let len = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..len]).into_owned();
    // Short and lowercased, as rock-jobs spells host names.
    Some(name.split('.').next().unwrap_or(&name).to_lowercase())
}

/// The parent's process name (`zsh`, `just`, `python3`, a cargo test binary).
fn process_name(pid: libc::pid_t) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for its length.
        let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formats_known_instants() {
        assert_eq!(utc(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let t = UNIX_EPOCH + Duration::from_millis(1_791_288_896_789);
        assert_eq!(utc(t), "2026-10-06T12:14:56.789Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(utc(leap), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn porcelain_v2_parse_counts_every_dirty_kind() {
        let out = b"# branch.oid abc123\0# branch.head loop-telemetry\0\
1 .M N... 100644 100644 100644 aaa bbb src/a b.rs\0\
2 R. N... 100644 100644 100644 aaa bbb R100 new.rs\0old.rs\0\
? untracked.txt\0";
        let s = parse_status(out, None);
        assert_eq!(s.head.as_deref(), Some("abc123"));
        assert_eq!(s.branch.as_deref(), Some("loop-telemetry"));
        assert_eq!(s.dirty_paths, 3);
    }

    #[test]
    fn porcelain_v2_detached_head_has_no_branch() {
        let s = parse_status(b"# branch.oid abc\0# branch.head (detached)\0", None);
        assert_eq!(s.branch, None);
        assert_eq!(s.dirty_paths, 0);
    }

    #[test]
    fn dirty_mtime_is_the_newest_dirty_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.rs"), "x").unwrap();
        let s = parse_status(b"? x.rs\0? gone.rs\0", Some(dir.path()));
        assert_eq!(s.dirty_paths, 2);
        let mtime = unix_ms(std::fs::metadata(dir.path().join("x.rs")).unwrap().modified().unwrap());
        assert_eq!(s.dirty_mtime_ms, Some(mtime));
    }
}
