//! Codegen-shape contract: skull snapshots a closure's captured single-assignment
//! cells by value (an IIFE `(function(snap) return function() .. end end)(snap)`)
//! ONLY when the closure is created inside a loop. A closure created once (not in
//! a loop) capturing a cell that is assigned once never sees the cell change, so
//! the snapshot is pure waste -- an extra closure + UCLO per call on hot
//! deferred-draw paths (`sprite_mesh_submit`, the `DrawList` methods,
//! `render_batches`: all `queue_custom_draw(() => { .. })`). A bare
//! `function() .. end` capturing by reference is correct there.
//!
//! In a loop the snapshot IS needed: the closure is (re)created each iteration and
//! must capture THIS iteration's value, not the shared cell's final value.

mod common;
use common::{emit_module, function_body};

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
fn nonloop_closure_captures_by_reference_no_iife() {
    let lua = emit_module(NONLOOP_CAPTURE);
    let body = function_body(&lua, "submit");
    assert!(
        body.contains("function"),
        "submit should still create the closure:\n{body}"
    );
    assert!(
        !body.contains("return function"),
        "a non-loop closure must NOT be wrapped in a snapshot IIFE:\n{body}"
    );
}

// A closure created INSIDE a loop, capturing a cell assigned inside the loop --
// the snapshot IIFE is REQUIRED so each closure captures its own iteration value.
const LOOP_CAPTURE: &str = r#"
@nomangle inline(lua) fn use_one(a: int) = "_G.__sink = (a or 0)"

fn submit_loop(n: int) -> [fn()] {
    let fns: [fn()] = []
    for i in 0..n {
        let a = i + 1
        fns.push(() => { use_one(a) })
    }
    return fns
}

fn main() { let _ = submit_loop(3) }
"#;

#[test]
fn loop_closure_keeps_snapshot_iife() {
    let lua = emit_module(LOOP_CAPTURE);
    let body = function_body(&lua, "submit_loop");
    assert!(
        body.contains("return function"),
        "a loop-created closure must keep the by-value snapshot IIFE:\n{body}"
    );
}
