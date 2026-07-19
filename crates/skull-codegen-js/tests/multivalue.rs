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
use common::{emit_user, emit_user_err, ws_with};

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

// A closure capturing an iteration-local `let` copy gets a per-iteration `let`
// INSIDE the loop body (not a shared fn-top declaration).
#[test]
fn loop_capture_declares_per_iteration() {
    let ws = ws_with(
        "fn f() {\n\
             let fns: [fn() -> int] = []\n\
             for i in 0..3 { let a = i; fns.push(() => a) }\n\
         }",
    );
    let js = emit_closure(&ws, "f").expect("emits");
    // A `let c<N>;` appears INSIDE the `for` body (per-iteration), after `for (`.
    let for_pos = js.find("for (").expect("a for loop");
    assert!(
        js[for_pos..].contains("{\n    let c"),
        "captured cell declared inside the loop body:\n{js}"
    );
}

// A closure capturing the loop COUNTER directly cannot be block-scoped without
// rewriting the closure reference -> reject (skip), never the shared final value.
#[test]
fn loop_counter_capture_rejects() {
    let ws = ws_with(
        "fn f() {\n\
             let fns: [fn() -> int] = []\n\
             for i in 0..3 { fns.push(() => i) }\n\
         }",
    );
    assert!(
        emit_user_err(&ws, "f").contains("loop counter"),
        "counter capture rejects: {}",
        emit_user_err(&ws, "f")
    );
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
