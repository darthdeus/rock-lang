//! One JSON line per dev-loop `rock` invocation, appended to `ROCK_INVOCATION_LOG`
//! or, in builds that default to it, a per-user state log, so
//! edit-to-feedback times can be mined with a log consumer.
//!
//! The record is written once, at exit: `finish` from `main` or `exit`, and an
//! `atexit` fallback for paths that leave through `std::process::exit` outside
//! this crate or unwind out of `main` (exit code `null`). HEAD and branch are
//! read from the repository files; dirty state comes from one background
//! `git status` started only once the command has run for `GIT_DELAY`, so it
//! overlaps a long command and never contends with a short one. Recording never changes the command's outcome:
//! write errors are logged and dropped.

use bone_core::prelude::Instant;
use serde_json::{Map, Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEMA: u64 = 1;
const LOG_ENV: &str = "ROCK_INVOCATION_LOG";
/// Set for children so a nested `rock` (the sim compile child) is marked as
/// such instead of reading as a separate loop step.
pub const PARENT_ENV: &str = "ROCK_INVOCATION_PARENT";
/// Context variables copied into `env` when set.
const CONTEXT_ENV: &[&str] = &["ROCK_TEST_FILTER", "ROCK_TEST_SHARDS"];
/// `git status` takes 30-100 ms of stat-heavy work on the Rock tree; run
/// alongside a 100 ms command it slowed that command by about 60 ms. Starting
/// it after this delay keeps short commands free of it (`git_status:
/// "skipped"`) while every command long enough to be a loop step gets it.
const GIT_DELAY: Duration = Duration::from_millis(300);
/// How long the record waits for a started probe once the command is done;
/// a slower one is killed (`git_status: "late"`).
const GIT_GRACE: Duration = Duration::from_millis(3);

/// The delayed probe: `None` until started; `finished` stops a probe thread
/// that wakes after the record is written.
struct GitProbe {
    child: Option<Child>,
    finished: bool,
}
static GIT_PROBE: Mutex<GitProbe> = Mutex::new(GitProbe {
    child: None,
    finished: false,
});
/// Dirty paths stat'ed for the newest edit time; a larger dirty set records
/// the newest among the first this many.
const MTIME_STAT_CAP: usize = 4000;

struct Pending {
    pid: u32,
    path: PathBuf,
    start: SystemTime,
    started: Instant,
    repo_root: Option<PathBuf>,
    record: Map<String, Value>,
    counts: Map<String, Value>,
    phases: Map<String, Value>,
}

static STATE: Mutex<Option<Pending>> = Mutex::new(None);

/// The commands that make up the edit-compile-test loop.
pub fn is_loop_command(name: &str) -> bool {
    crate::registry::CliCommand::named(name).is_some_and(|command| {
        (crate::registry::registered().command_metadata)(command).log_invocation
    })
}

/// Whether an unset `ROCK_INVOCATION_LOG` logs to the per-user state
/// directory. A build that does not want to record its users' working
/// directories and branches by default turns this off in its registrations.
static STATE_DIR_DEFAULT: AtomicBool = AtomicBool::new(true);

/// Write records only where `ROCK_INVOCATION_LOG` names a path.
pub fn require_explicit_log_path() {
    STATE_DIR_DEFAULT.store(false, Ordering::Relaxed);
}

/// Where records go: `ROCK_INVOCATION_LOG` (a path, or `off`), else, unless
/// [`require_explicit_log_path`] was called, `$XDG_STATE_HOME/rock/invocations.jsonl`
/// or `~/.local/state/rock/invocations.jsonl`. `None` when disabled or no home.
pub fn log_path() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os(LOG_ENV).filter(|v| !v.is_empty()) {
        return (value != "off").then(|| PathBuf::from(value));
    }
    if !STATE_DIR_DEFAULT.load(Ordering::Relaxed) {
        return None;
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    Some(state.join("rock").join("invocations.jsonl"))
}

static COMMAND: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// The running `rock` subcommand, or "other" before `main` names it.
pub(crate) fn command() -> &'static str {
    COMMAND.get().copied().unwrap_or("other")
}

/// Start recording this invocation if it is a loop command and the log is on.
/// Call once from `main`, single-threaded, after its environment setup.
pub fn begin(command: &'static str, backend: &str) {
    let _ = COMMAND.set(command);
    if !is_loop_command(command) {
        return;
    }
    let Some(path) = log_path() else { return };
    let start = SystemTime::now();
    let started = Instant::now();
    let cwd = std::env::current_dir().ok();
    let repo_root = cwd.as_deref().and_then(find_repo_root);
    let (head, branch) = repo_root.as_deref().map(read_head).unwrap_or((None, None));
    let parent = std::env::var(PARENT_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    let pid = std::process::id();
    // SAFETY: called from `main` before any thread or child is spawned.
    unsafe { std::env::set_var(PARENT_ENV, pid.to_string()) };

    let mut record = Map::new();
    record.insert("schema".into(), json!(SCHEMA));
    record.insert("kind".into(), json!(command));
    record.insert("backend".into(), json!(backend));
    record.insert("argv".into(), json!(redact_var_values(std::env::args())));
    record.insert("exe".into(), json!(std::env::current_exe().ok()));
    record.insert("host".into(), json!(hostname()));
    record.insert("pid".into(), json!(pid));
    let ppid = unsafe { libc::getppid() };
    record.insert("ppid".into(), json!(ppid));
    record.insert("parent_comm".into(), json!(process_name(ppid)));
    record.insert("nested_in".into(), json!(parent));
    record.insert("cwd".into(), json!(cwd));
    record.insert("repo_root".into(), json!(repo_root));
    record.insert("head".into(), json!(head));
    record.insert("branch".into(), json!(branch));
    let env: Map<String, Value> = CONTEXT_ENV
        .iter()
        .copied()
        .chain(
            crate::registry::registered()
                .invocation_context_keys
                .iter()
                .map(|(key, _)| *key),
        )
        .filter_map(|k| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| (k.to_string(), json!(v)))
        })
        .collect();
    record.insert("env".into(), Value::Object(env));

    let repo_root_for_probe = repo_root.clone();
    let pending = Pending {
        pid,
        path,
        start,
        started,
        repo_root,
        record,
        counts: Map::new(),
        phases: Map::new(),
    };
    if let Ok(mut state) = STATE.lock() {
        *state = Some(pending);
    }
    if let Some(root) = repo_root_for_probe {
        std::thread::spawn(move || {
            std::thread::sleep(GIT_DELAY);
            if let Ok(mut probe) = GIT_PROBE.lock() {
                if !probe.finished {
                    probe.child = spawn_git_status(&root);
                }
            }
        });
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
    let child = match GIT_PROBE.try_lock() {
        Ok(mut probe) => {
            probe.finished = true;
            probe.child.take()
        }
        Err(_) => None,
    };
    let (git_status, git) = match (child, &p.repo_root) {
        (Some(child), root) => collect_git(child, root.as_deref()),
        (None, Some(_)) => ("skipped", None),
        (None, None) => ("no_repo", None),
    };
    let r = &mut p.record;
    r.insert("start_utc".into(), json!(utc(p.start)));
    r.insert("end_utc".into(), json!(utc(end)));
    r.insert("start_unix_ms".into(), json!(unix_ms(p.start)));
    r.insert("end_unix_ms".into(), json!(unix_ms(end)));
    r.insert("wall_ms".into(), json!(wall_ms));
    r.insert("exit_code".into(), json!(exit_code));
    r.insert("exit_path".into(), json!(exit_path));
    let (dirty, dirty_paths, dirty_mtime_ms) = match git {
        Some(g) => (
            json!(g.dirty_paths > 0),
            json!(g.dirty_paths),
            json!(g.dirty_mtime_ms),
        ),
        None => (Value::Null, Value::Null, Value::Null),
    };
    r.insert("git_status".into(), json!(git_status));
    r.insert("dirty".into(), dirty);
    r.insert("dirty_paths".into(), dirty_paths);
    r.insert("dirty_mtime_ms".into(), dirty_mtime_ms);
    r.insert(
        "counts".into(),
        Value::Object(std::mem::take(&mut p.counts)),
    );
    r.insert(
        "phases_ms".into(),
        Value::Object(std::mem::take(&mut p.phases)),
    );
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
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let written = file.write(line.as_bytes())?;
    if written != line.len() {
        return Err(std::io::Error::other(format!(
            "short write: {written} of {} bytes",
            line.len()
        )));
    }
    Ok(())
}

fn find_repo_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

fn spawn_git_status(root: &Path) -> Option<Child> {
    // `--no-optional-locks`: a background status must never take index.lock
    // and fail the author's concurrent `git commit`.
    let spawned = Command::new("git")
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=normal",
        ])
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
    dirty_paths: usize,
    dirty_mtime_ms: Option<u64>,
}

/// `("done", state)`, or `("late" | "failed", None)`.
fn collect_git(mut child: Child, root: Option<&Path>) -> (&'static str, Option<GitState>) {
    let deadline = Instant::now() + GIT_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return ("failed", None),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_micros(500)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return ("late", None);
            }
        }
    }
    let mut out = Vec::new();
    match child
        .stdout
        .as_mut()
        .map(|s| std::io::Read::read_to_end(s, &mut out))
    {
        Some(Ok(_)) => ("done", Some(parse_status(&out, root))),
        _ => ("failed", None),
    }
}

/// HEAD's commit and branch, read from the repository files (no git
/// process): `.git` may be a worktree's `gitdir:` file, refs may be loose or
/// packed in the common dir.
fn read_head(root: &Path) -> (Option<String>, Option<String>) {
    let dot_git = root.join(".git");
    let git_dir = match std::fs::read_to_string(&dot_git) {
        Ok(text) => match text.trim().strip_prefix("gitdir: ") {
            Some(dir) => root.join(dir),
            None => return (None, None),
        },
        Err(_) => dot_git,
    };
    let common_dir = std::fs::read_to_string(git_dir.join("commondir"))
        .map(|rel| git_dir.join(rel.trim()))
        .unwrap_or_else(|_| git_dir.clone());
    let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) else {
        return (None, None);
    };
    let head = head.trim();
    let Some(refname) = head.strip_prefix("ref: ") else {
        return (Some(head.to_string()), None);
    };
    let branch = refname.strip_prefix("refs/heads/").map(str::to_string);
    let loose = [&git_dir, &common_dir]
        .iter()
        .find_map(|dir| std::fs::read_to_string(dir.join(refname)).ok())
        .map(|sha| sha.trim().to_string());
    let sha = loose.or_else(|| {
        let packed = std::fs::read_to_string(common_dir.join("packed-refs")).ok()?;
        packed.lines().find_map(|line| {
            let (sha, name) = line.split_once(' ')?;
            (name == refname).then(|| sha.to_string())
        })
    });
    (sha, branch)
}

/// Parse `git status --porcelain=v2 -z`.
fn parse_status(out: &[u8], root: Option<&Path>) -> GitState {
    let mut state = GitState {
        dirty_paths: 0,
        dirty_mtime_ms: None,
    };
    let mut fields = out.split(|b| *b == 0).map(String::from_utf8_lossy);
    let mut stat_budget = MTIME_STAT_CAP;
    while let Some(entry) = fields.next() {
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
                if let Ok(mtime) =
                    std::fs::symlink_metadata(root.join(&path)).and_then(|m| m.modified())
                {
                    let ms = unix_ms(mtime);
                    state.dirty_mtime_ms = Some(state.dirty_mtime_ms.map_or(ms, |cur| cur.max(ms)));
                }
            }
        }
    }
    state
}

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    // Short and lowercased, as job supervisor spells host names.
    Some(name.split('.').next().unwrap_or(&name).to_lowercase())
}

/// The parent's process name (`zsh`, `just`, `python3`, a cargo test binary).
fn process_name(pid: libc::pid_t) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim().to_string())
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

/// Command-line arguments with every `--var NAME=VALUE` value replaced:
/// build variables carry secrets such as TELEMETRY_TOKEN, and recorded
/// command lines reach logs and other processes' diagnostics.
pub(crate) fn redact_var_values(args: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut value_next = false;
    for arg in args {
        let (prefix, assignment) = if value_next {
            ("", arg.as_str())
        } else if let Some(rest) = arg.strip_prefix("--var=") {
            ("--var=", rest)
        } else {
            value_next = arg == "--var";
            out.push(arg);
            continue;
        };
        value_next = false;
        let name = assignment
            .split_once('=')
            .map_or(assignment, |(name, _)| name);
        out.push(format!("{prefix}{name}=<redacted>"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_argv_redacts_every_var_value() {
        let args = [
            "rock",
            "package",
            "example",
            "--var",
            "TOKEN=s3cret",
            "--channel",
            "steam",
            "--var=K=v=2",
            "--var",
        ];
        let redacted = redact_var_values(args.iter().map(|arg| arg.to_string()));
        assert_eq!(
            redacted,
            [
                "rock",
                "package",
                "example",
                "--var",
                "TOKEN=<redacted>",
                "--channel",
                "steam",
                "--var=K=<redacted>",
                "--var"
            ]
        );
    }

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
        let out = b"1 .M N... 100644 100644 100644 aaa bbb src/a b.rs\0\
2 R. N... 100644 100644 100644 aaa bbb R100 new.rs\0old.rs\0\
? untracked.txt\0";
        let s = parse_status(out, None);
        assert_eq!(s.dirty_paths, 3);
    }

    #[test]
    fn head_reads_worktree_loose_and_packed_refs() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path().join("main.git");
        let wt_git = common.join("worktrees/wt");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::create_dir_all(common.join("refs/heads")).unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        std::fs::write(wt_git.join("HEAD"), "ref: refs/heads/feat\n").unwrap();
        std::fs::write(
            common.join("packed-refs"),
            "# pack-refs\nabc refs/heads/feat\n",
        )
        .unwrap();
        assert_eq!(read_head(&wt), (Some("abc".into()), Some("feat".into())));
        std::fs::write(common.join("refs/heads/feat"), "def\n").unwrap();
        assert_eq!(read_head(&wt), (Some("def".into()), Some("feat".into())));
        std::fs::write(wt_git.join("HEAD"), "123\n").unwrap();
        assert_eq!(read_head(&wt), (Some("123".into()), None));
    }

    #[test]
    fn dirty_mtime_is_the_newest_dirty_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.rs"), "x").unwrap();
        let s = parse_status(b"? x.rs\0? gone.rs\0", Some(dir.path()));
        assert_eq!(s.dirty_paths, 2);
        let mtime = unix_ms(
            std::fs::metadata(dir.path().join("x.rs"))
                .unwrap()
                .modified()
                .unwrap(),
        );
        assert_eq!(s.dirty_mtime_ms, Some(mtime));
    }
}
