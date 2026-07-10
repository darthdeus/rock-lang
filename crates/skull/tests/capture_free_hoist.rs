//! P3 of docs/closure-env-records-plan.md: a lambda whose scope captures NOTHING
//! hoists to a top-level `function <enclosing>__lambda<k>(<params>) ... end` and
//! is referenced by that global name at the creation site (a raw Lua function
//! value -- no record, no `__call`, zero alloc). A CAPTURING lambda is untouched
//! (stays an inline closure). Naming `k` = the `Inst::Lambda`'s inst-arena index
//! order (shared with the future P2.6 record naming).

mod common;
use common::{emit_module, function_body};

fn run_luajit(prog: &str, tag: &str) -> String {
    use std::process::Command;
    let path = std::env::temp_dir().join(format!("skull_capture_free_{}_{tag}.lua", std::process::id()));
    std::fs::write(&path, prog).expect("write program");
    let out = Command::new("luajit").arg(&path).output().expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "luajit exited non-zero.\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout.trim().to_string()
}

// `free_loop` creates a capture-free lambda (`() => { sink(42) }` closes over
// nothing) in a loop; `capturing_loop` creates one that captures the loop
// binding `x`. Same shape, different capture status.
const FIXTURE: &str = r#"
@nomangle inline(lua) fn sink(a: int) = "_G.__sink = a"

fn free_loop() -> [fn()] {
    let fns: [fn()] = []
    for i in 0..3 {
        fns.push(() => { sink(42) })
    }
    return fns
}

fn capturing_loop() -> [fn() -> int] {
    let fns: [fn() -> int] = []
    for i in 0..3 {
        let x = i
        fns.push(() => x)
    }
    return fns
}

fn main() { let _ = free_loop() let _ = capturing_loop() }
"#;

/// SHAPE (RED until P3): the capture-free lambda hoists to a top-level
/// `free_loop__lambda0` and the creation site references it by name -- no inline
/// `function(` closure in `free_loop`'s body.
#[test]
fn capture_free_lambda_hoists_to_named_toplevel() {
    let lua = emit_module(FIXTURE);
    assert!(
        lua.contains("function free_loop__lambda0("),
        "the capture-free lambda must hoist to a top-level `free_loop__lambda0`:\n{lua}"
    );
    let body = function_body(&lua, "free_loop");
    assert!(
        body.contains("free_loop__lambda0"),
        "the creation site must reference the hoisted fn by name:\n{body}"
    );
    // An anonymous inline closure renders `function(` (no space); the enclosing
    // fn header is `function free_loop(` (space + name), so `function(` is the
    // precise marker of a leftover inline closure.
    assert!(
        !body.contains("function("),
        "a hoisted capture-free lambda must leave NO inline closure in the enclosing body:\n{body}"
    );
}

/// SHAPE (regression guard): a CAPTURING lambda is untouched -- still an inline
/// closure in the enclosing body, not hoisted.
#[test]
fn capturing_lambda_stays_inline() {
    let lua = emit_module(FIXTURE);
    let body = function_body(&lua, "capturing_loop");
    assert!(
        body.contains("function("),
        "a capturing lambda must still render an inline closure (unchanged this phase):\n{body}"
    );
    assert!(
        !lua.contains("function capturing_loop__lambda"),
        "a capturing lambda must NOT hoist to a named top-level fn:\n{lua}"
    );
}

/// RUNTIME (RED until P3): two creations of the SAME capture-free lambda are the
/// SAME fn value (a shared top-level global), not fresh per-creation closures.
#[test]
fn hoisted_lambda_shared_across_creations() {
    let src = r#"
@nomangle inline(lua) fn sink(a: int) = "_G.__sink = a"
fn mk() -> fn() { return () => { sink(1) } }
fn main() { let _ = mk() }
"#;
    let lua = emit_module(src);
    let driver = "\nlocal a = mk()\nlocal b = mk()\nprint(rawequal(a, b))\n";
    let out = run_luajit(&format!("{lua}{driver}"), "shared");
    assert_eq!(out, "true", "two creations of a capture-free lambda must share one hoisted fn value; got: {out}");
}

/// RUNTIME (behavior preserved): a hoisted lambda is still callable through a
/// fn-typed value, and a lambda-local `let` stays PER-CALL (two calls don't
/// share state) -- "6\n6", never "6\n7".
#[test]
fn hoisted_lambda_locals_stay_per_call() {
    let src = r#"
fn counter_fn() -> fn() -> int {
    return () => { let n = 5 n = n + 1 return n }
}
fn main() {
    let f = counter_fn()
    print("{f()}")
    print("{f()}")
}
"#;
    let lua = emit_module(src);
    let out = run_luajit(&format!("{lua}\nmain()\n"), "percall");
    assert_eq!(out, "6\n6", "a hoisted lambda's local must be per-call (not shared); got: {out}");
}
