// Generic async subprocess pipe. Spawns a child with stdin/stdout piped; a
// reader thread buffers stdout lines into a queue that the game drains
// non-blocking each frame. Built for the chess UCI (Stockfish) bridge but
// engine-agnostic. "Spawn, don't link" also keeps a spawned GPL tool at
// arm's-length (mere aggregation).
//
// Handles are 1-based indices into a global registry; 0 means spawn failed.

use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::io::{BufRead, BufReader, Write};
use std::os::raw::c_char;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

struct Proc {
    child: Child,
    stdin: ChildStdin,
    lines: Arc<Mutex<VecDeque<String>>>,
    alive: Arc<AtomicBool>,
    // Owns the buffer behind the pointer returned by poll_line; kept alive until
    // the next poll_line call on this handle (single-consumer: the game thread).
    last_line: CString,
}

static PROCS: Mutex<Vec<Option<Proc>>> = Mutex::new(Vec::new());

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// Spawn `path` (a bare name is resolved on PATH). Returns a 1-based handle, or
/// 0 on failure (logged, never silent).
#[no_mangle]
pub extern "C" fn stone_proc_spawn(path: *const c_char) -> i32 {
    let path = cstr(path);
    let mut child = match Command::new(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            ::log::warn!(target: "STONE/PROC", "spawn '{}' failed: {}", path, e);
            return 0;
        }
    };
    let stdin = match child.stdin.take() {
        Some(s) => s,
        None => return 0,
    };
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => return 0,
    };

    let lines = Arc::new(Mutex::new(VecDeque::new()));
    let alive = Arc::new(AtomicBool::new(true));
    {
        let lines = lines.clone();
        let alive = alive.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => lines.lock().unwrap().push_back(l),
                    Err(_) => break,
                }
            }
            alive.store(false, Ordering::SeqCst);
        });
    }

    let proc = Proc {
        child,
        stdin,
        lines,
        alive,
        last_line: CString::default(),
    };
    let mut regs = PROCS.lock().unwrap();
    // Reuse a freed slot if one exists, else append.
    if let Some(i) = regs.iter().position(|p| p.is_none()) {
        regs[i] = Some(proc);
        (i as i32) + 1
    } else {
        regs.push(Some(proc));
        regs.len() as i32
    }
}

/// Tear down every OCCUPIED slot: kill the child, reap it, and free the slot.
///
/// Ownership is the registry's, and only the registry knows it. `alive` is
/// stored by the reader thread when the child's stdout reaches EOF, which is a
/// different fact: a child that closes stdout and keeps running, and a child
/// that has already exited but was never reaped, both report `alive == false`
/// while still holding their slot. A caller that tore down "live" handles would
/// leak both the process and the slot, and the next spawn would be pushed off
/// handle 1 -- so the operation belongs here, over slot occupancy, rather than
/// being assembled from `stone_proc_alive` on the Rock side.
///
/// Returns how many slots could NOT be torn down. A slot is freed only once its
/// child is confirmed terminated, so a caller may treat 0 as proof rather than
/// as a request that was issued.
///
/// `wait` is the child-side observation and the reason failures propagate: it
/// returns only after the OS has reaped a TERMINATED child, so `Ok` is evidence
/// the process stopped. Emptying the slot regardless would make any later
/// occupancy check self-fulfilling -- it would report a clean registry whether
/// or not a single child actually died.
#[no_mangle]
pub extern "C" fn stone_proc_reset() -> i32 {
    let mut regs = PROCS.lock().unwrap();
    let mut failed = 0;
    for (i, slot) in regs.iter_mut().enumerate() {
        let Some(mut p) = slot.take() else { continue };
        // An already-exited child is not a failure to kill; older `std` reports
        // that as `InvalidInput`, and `wait` below is the oracle either way.
        if let Err(e) = p.child.kill() {
            if e.kind() != std::io::ErrorKind::InvalidInput {
                ::log::error!(target: "STONE/PROC", "reset: kill handle {} failed: {}", i + 1, e);
                *slot = Some(p);
                failed += 1;
                continue;
            }
        }
        if let Err(e) = p.child.wait() {
            ::log::error!(target: "STONE/PROC", "reset: wait handle {} failed: {}", i + 1, e);
            *slot = Some(p);
            failed += 1;
        }
    }
    failed
}

/// How many slots the registry currently has OCCUPIED, regardless of whether
/// each child's stdout is still open. The post-teardown assertion: counting
/// `alive` handles instead would report zero for a leak this is meant to catch.
#[no_mangle]
pub extern "C" fn stone_proc_occupied_count() -> i32 {
    PROCS.lock().unwrap().iter().filter(|s| s.is_some()).count() as i32
}

fn with_proc<R>(handle: i32, f: impl FnOnce(&mut Proc) -> R) -> Option<R> {
    if handle <= 0 {
        return None;
    }
    let mut regs = PROCS.lock().unwrap();
    regs.get_mut((handle - 1) as usize)?.as_mut().map(f)
}

/// Write `line` to the child's stdin (a trailing newline is added).
#[no_mangle]
pub extern "C" fn stone_proc_send(handle: i32, line: *const c_char) {
    let s = cstr(line);
    with_proc(handle, |p| {
        if writeln!(p.stdin, "{}", s)
            .and_then(|_| p.stdin.flush())
            .is_err()
        {
            ::log::warn!(target: "STONE/PROC", "send to handle {} failed (child gone?)", handle);
        }
    });
}

/// Pop the next buffered stdout line, or "" if none is queued. The returned
/// pointer is valid until the next poll_line call on this handle.
#[no_mangle]
pub extern "C" fn stone_proc_poll_line(handle: i32) -> *const c_char {
    let ptr = with_proc(handle, |p| {
        let next = p.lines.lock().unwrap().pop_front().unwrap_or_default();
        p.last_line = CString::new(next).unwrap_or_default();
        p.last_line.as_ptr()
    });
    ptr.unwrap_or(c"".as_ptr())
}

/// Is the child still running (reader thread not yet at EOF)?
#[no_mangle]
pub extern "C" fn stone_proc_alive(handle: i32) -> u8 {
    // LuaJIT's numeric-bool cdef requires a whole canonical byte, not C _Bool.
    u8::from(with_proc(handle, |p| p.alive.load(Ordering::SeqCst)).unwrap_or(false))
}

/// Kill the child and free the handle.
#[no_mangle]
pub extern "C" fn stone_proc_kill(handle: i32) {
    if handle <= 0 {
        return;
    }
    let mut regs = PROCS.lock().unwrap();
    if let Some(slot) = regs.get_mut((handle - 1) as usize) {
        if let Some(mut p) = slot.take() {
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
    }
}

// Both tests spawn through Linux `sh`; the helpers have no other-platform users.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// `PROCS` is one process-wide registry and `stone_proc_reset` is a
    /// whole-registry operation by design, so two tests running as threads of one
    /// binary tear down each other's children. Serialize them.
    ///
    /// Poison-tolerant on purpose: without it the first test to panic turns every
    /// sibling's failure into a poison error, and the run stops reporting which
    /// assertion actually failed.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn registry_guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A child with a BOUNDED natural lifetime, run as `sh` replacing itself via
    /// `exec` so the handle's pid IS the long-running process rather than a
    /// shell parent whose grandchild outlives it. `stone_proc_spawn` takes no
    /// arguments, which is why the command arrives over stdin.
    ///
    /// Bounded because an unkillable child must fail the test quickly instead of
    /// stalling it, and because a child that exits by itself would let a broken
    /// teardown pass: `sleep` with no operand is an immediate error exit, so it
    /// proves nothing about kill.
    #[cfg(target_os = "linux")]
    const NATURAL_LIFETIME: Duration = Duration::from_secs(2);

    #[cfg(target_os = "linux")]
    fn spawn_bounded(prefix: &str) -> (i32, u32) {
        // A slot left by an earlier test makes every occupancy assertion below
        // measure the leak instead of the child under test, and `reset` returning
        // 0 would still be satisfied by tearing that stranger down.
        assert_eq!(
            stone_proc_occupied_count(),
            0,
            "the registry was not empty at spawn, so this test's occupancy counts are not its own"
        );
        let sh = std::ffi::CString::new("sh").unwrap();
        let h = stone_proc_spawn(sh.as_ptr());
        assert!(h > 0, "spawn failed");
        let pid = PROCS.lock().unwrap()[(h - 1) as usize]
            .as_ref()
            .expect("the slot is occupied")
            .child
            .id();

        let script = std::ffi::CString::new(format!("{prefix}exec sleep 2")).unwrap();
        stone_proc_send(h, script.as_ptr());

        // Wait for the `exec` to land, so the pid under test is the sleeping
        // process and not a shell still reading its script.
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .map(|c| c.trim() == "sleep")
                .unwrap_or(false)
            {
                return (h, pid);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the child never reached `sleep`, so it was not alive to be killed");
    }

    /// The child-side observation: `stone_proc_reset` reporting a clean registry
    /// is only evidence if the process actually stopped. The pid is captured
    /// before the reset, `/proc/<pid>` must be gone after it, and the reset must
    /// finish well inside the child's natural lifetime -- otherwise "the process
    /// is gone" would just mean the test outlasted it.
    #[cfg(target_os = "linux")]
    #[test]
    fn reset_terminates_the_child_and_not_merely_the_slot() {
        let _registry = registry_guard();
        let (_h, pid) = spawn_bounded("");
        assert!(
            std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the child must be running before the reset, or this proves nothing"
        );

        let started = Instant::now();
        assert_eq!(
            stone_proc_reset(),
            0,
            "reset reported a slot it could not tear down"
        );
        let elapsed = started.elapsed();

        assert_eq!(stone_proc_occupied_count(), 0, "a slot survived the reset");
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the slot was freed while pid {pid} is still in the process table"
        );
        assert!(
            elapsed * 2 < NATURAL_LIFETIME,
            "reset took {elapsed:?}, close enough to the child's own {NATURAL_LIFETIME:?}              lifetime that it may have waited the process out rather than killed it"
        );
    }

    /// The occupancy count must SEE a child whose stdout has closed, which is
    /// the exact case `stone_proc_alive` misreports.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_that_closed_stdout_still_occupies_its_slot() {
        let _registry = registry_guard();
        let (h, pid) = spawn_bounded("exec 1>&-; ");

        // The reader thread needs to observe EOF before `alive` flips.
        let deadline = Instant::now() + Duration::from_secs(1);
        while stone_proc_alive(h) != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            stone_proc_alive(h),
            0,
            "the child never closed stdout, so the arms agree trivially"
        );
        assert_eq!(
            stone_proc_occupied_count(),
            1,
            "occupancy must still see the child that `alive` has already written off"
        );

        let started = Instant::now();
        assert_eq!(stone_proc_reset(), 0);
        let elapsed = started.elapsed();
        assert_eq!(stone_proc_occupied_count(), 0);
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "pid {pid} survived a reset that reported success"
        );
        assert!(
            elapsed * 2 < NATURAL_LIFETIME,
            "reset took {elapsed:?}; it may have waited"
        );
    }
}
