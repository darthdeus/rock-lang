//! Multi-value ABI + loop-capture emission (T7). A scalarized tuple/multi return
//! packs into a JS array (`return [a, b]`) the `CallMulti` caller destructures
//! (`[d0, d1] = f()`); dest cells hoist-declare at the fn top. A closure created
//! in a loop that captures an iteration-local cell gets a per-iteration `let`
//! inside the loop body. Boundaries that this raw-lowered path can't emit
//! correctly REJECT (skip): a closure capturing a loop counter/cursor, a value
//! struct (deferred to a future arc), a scalar-ABI arg/param count mismatch.

use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;

mod common;
use common::{emit_user, ws_with};

/// Co-emit `root` + its plain-Rock callees (needed for `push`), returning the JS.
fn emit_closure(ws: &bone_semantic::inference::Workspace, root: &str) -> Result<String, String> {
    let scx = ScalarCtx::new(ws);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let mut r = None;
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == root {
            r = Some(FuncRef { module: mid, func: fid });
        }
    }
    skull_codegen_js::emit::emit_with_closure(ws, &scx, &[(r.unwrap(), root.to_string())])
        .map_err(|e| e.message)
}

// A tuple return scalarizes to N leaves packed into a JS array.
#[test]
fn tuple_return_packs_array() {
    let ws = ws_with("fn mk(a: int, b: int) -> (int, int) { (a, b) }");
    let js = emit_user(&ws, "mk");
    assert!(js.contains("return [v"), "multi-return packs an array:\n{js}");
}

// A closure capturing an iteration-local `let` copy snapshots THAT iteration's
// value into a fresh env record built inside the loop body (`convert_closures`
// classifies the copy as a by-value capture). Per-iteration correctness is pinned
// behaviorally in tests/js_multivalue.rock on both backends.
#[test]
fn loop_capture_snapshots_per_iteration() {
    let ws = ws_with(
        "fn f() {\n\
             let fns: [fn() -> int] = []\n\
             for i in 0..3 { let a = i; fns.push(() => a) }\n\
         }",
    );
    let js = emit_closure(&ws, "f").expect("emits");
    assert!(js.contains("__fn: f__lambda0"), "per-iteration capture record:\n{js}");
    assert!(js.contains("function f__lambda0("), "hoisted body:\n{js}");
}

// A closure capturing the loop COUNTER directly now EMITS -- the shared
// closure-conversion pass classifies the counter as a per-iteration by-value
// capture (the fresh record each iteration snapshots the counter's value),
// where the deleted T7 path rejected it as out of subset. Per-iteration
// correctness is pinned in tests/closure_capture_semantics.rock on both backends.
#[test]
fn loop_counter_capture_now_emits() {
    let ws = ws_with(
        "fn f() {\n\
             let fns: [fn() -> int] = []\n\
             for i in 0..3 { fns.push(() => i) }\n\
         }",
    );
    let js = emit_closure(&ws, "f").expect("counter capture now emits");
    assert!(js.contains("__fn: f__lambda0"), "counter snapshotted into a record:\n{js}");
}

// A reassigned param captured in a loop stays function-scoped (shared box), NOT
// per-iteration -- it emits (does not reject) and keeps a single binding.
#[test]
fn reassigned_param_stays_shared() {
    let ws = ws_with(
        "fn f(p: int) {\n\
             let fns: [fn() -> int] = []\n\
             for i in 0..3 { fns.push(() => p); p = p + 10 }\n\
         }",
    );
    let js = emit_closure(&ws, "f").expect("emits");
    // `p` is the fn param `c0`; it is not relocated into the loop body.
    assert!(js.starts_with("function f(c0)"), "p stays a param:\n{js}");
}

// Value-struct emission (all-scalar emit, GC-carrying reject) is covered by
// tests/value_structs.rs (T8b); the T7-era whole-value reject pin retired there.
