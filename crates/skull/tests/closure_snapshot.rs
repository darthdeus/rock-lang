//! Codegen-shape contract for how a capturing lambda snapshots its captures into
//! an env record (P2.6 of docs/closure-env-records-plan.md). The pre-P2.6 IIFE
//! by-value snapshot is gone: every capture is a record FIELD instead. A
//! single-assignment capture is snapshotted BY VALUE (the field holds the value);
//! a mutable capture is BOXED and the field holds the shared box table, so a later
//! write is observed by reference. Neither renders an inline `function(` closure
//! at the creation site. Record mechanics in depth: `env_record_hoist.rs`.

mod common;
use common::{emit_module, function_body, run_luajit};

/// True if `body` declares a box cell -- a `local c<N> = {}` line (the box-init
/// shape). Matched precisely rather than via the bare `= {}` substring, which an
/// empty vec/map literal or a prelude table also produces.
fn declares_box(body: &str) -> bool {
    body.lines().any(|l| {
        // The empty box table literal `{}`. The shared closure-conversion pass
        // materializes it at the owner scope entry, and regalloc may route it
        // through a temp (`r<N> = {}` then stored into the box cell) or assign the
        // cell directly; pre-cutover codegen wrote it inline as `local c<N> = {}`.
        // The caller's `.v = ` assertion confirms it is a box (not an empty
        // map/tuple literal).
        let t = l.trim();
        let t = t.strip_prefix("local ").unwrap_or(t);
        t.strip_prefix(['c', 'r'])
            .and_then(|r| r.strip_suffix(" = {}"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

// A closure created ONCE (no loop), capturing single-assignment cells a, b --
// the `sprite_mesh_submit` shape: `let a = ..; let b = ..; defer(() => use(a,b))`.
// `defer` STORES the closure in a global so it escapes (can't be inlined away),
// matching `queue_custom_draw` storing the draw fn in a batch for flush time.
const NONLOOP_CAPTURE: &str = r#"
@nomangle inline(lua) fn use_two(a: int, b: int) = "_G.__sink = (a or 0) + (b or 0)"

fn submit(x: int) -> fn() {
    let a = x + 1
    let b = x + 2
    return () => { use_two(a, b) }
}

fn main() { let f = submit(3) f() }
"#;

#[test]
fn nonloop_capture_hoists_to_by_value_record() {
    let lua = emit_module(NONLOOP_CAPTURE);
    let body = function_body(&lua, "submit");
    assert!(
        body.contains("setmetatable({ __fn = submit__lambda0"),
        "the capture hoists to an env record, not an inline closure:\n{body}"
    );
    assert!(
        !body.contains("function("),
        "no inline closure at the creation site:\n{body}"
    );
    assert!(
        !body.contains("return function"),
        "no by-value snapshot IIFE (deleted in P2.6):\n{body}"
    );
    // The single-assignment captures are by-value fields -- no box declared.
    assert!(!declares_box(&body), "single-assignment captures are by value, not boxed:\n{body}");
}

// A closure created INSIDE a loop, capturing a cell assigned inside the loop --
// each iteration's env record must snapshot ITS value (per-iteration), which the
// by-value field does directly (the field reads the loop-body local each time).
const LOOP_CAPTURE: &str = r#"
@nomangle inline(lua) fn use_one(a: int) = "_G.__sink = (a or 0)"

fn submit_loop(n: int) -> [fn() -> int] {
    let fns: [fn() -> int] = []
    for i in 0..n {
        let a = i + 1
        fns.push(() => a)
    }
    return fns
}

fn main() {
    let fns = submit_loop(3)
    for f in fns { print(f()) }
}
"#;

#[test]
fn loop_capture_snapshots_per_iteration_via_record() {
    let lua = emit_module(LOOP_CAPTURE);
    let body = function_body(&lua, "submit_loop");
    assert!(
        body.contains("setmetatable({ __fn = submit_loop__lambda"),
        "a loop-created capture hoists to a per-iteration env record:\n{body}"
    );
    assert!(!body.contains("return function"), "no by-value snapshot IIFE (deleted in P2.6):\n{body}");
    let out = run_luajit(&format!("{lua}\nmain()\n"), "loopcap");
    assert_eq!(out, "1\n2\n3", "each record must snapshot its iteration's value; got:\n{out}");
}

// A captured cell mutated after capture (multi-store) lowers to a BOX
// (`local c<N> = {}`, every store/load through `.v`); the record field holds the
// shared box table (by reference), so the closure observes the later write. NOT
// snapshotted by value.
const MUTATED_CAPTURE: &str = r#"
@nomangle inline(lua) fn use_one(a: int) = "_G.__sink = (a or 0)"

fn capture_box() -> fn() {
    let n = 0
    let g = () => { use_one(n) }
    n = 5
    return g
}

fn main() { let h = capture_box() h() }
"#;

#[test]
fn mutated_capture_lowers_to_box_threaded_into_record() {
    let lua = emit_module(MUTATED_CAPTURE);
    let body = function_body(&lua, "capture_box");
    assert!(declares_box(&body), "a mutable capture must declare a box `local c<N> = {{}}`:\n{body}");
    assert!(body.contains(".v = "), "a mutable capture stores through `.v`:\n{body}");
    assert!(
        body.contains("setmetatable({ __fn = capture_box__lambda"),
        "the box is threaded into an env record:\n{body}"
    );
    assert!(!body.contains("return function"), "a boxed capture gets no by-value snapshot IIFE:\n{body}");
}
