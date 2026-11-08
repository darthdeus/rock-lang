//! Multi-value ABI + loop-capture emission (T7). A scalarized tuple/multi return
//! sends leaf 0 as the ordinary return value and leaves 1..N through module-scope
//! scratch slots (`__rock_ret1 = b; return a;`), which the `CallMulti` caller
//! captures inseparably (`d0 = f(); d1 = __rock_ret1;`); dest cells hoist-declare
//! at the fn top. An array would allocate. A closure created
//! in a loop that captures an iteration-local cell gets a per-iteration `let`
//! inside the loop body. Boundaries that this raw-lowered path can't emit
//! correctly REJECT (skip): a closure capturing a loop counter/cursor, a value
//! struct (deferred to a future arc), a scalar-ABI arg/param count mismatch.

use crate::common::{emit_user, ws_with};
use skull_mir::mir::FuncRef;

/// Co-emit `root` + its plain-Rock callees (needed for `push`), returning the JS.
fn emit_closure(ws: &bone_semantic::inference::Workspace, root: &str) -> Result<String, String> {
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let mut r = None;
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == root {
            r = Some(FuncRef {
                module: mid,
                func: fid,
            });
        }
    }
    skull_codegen_js::emit::emit_with_closure(ws, &[(r.unwrap(), root.to_string())])
        .map_err(|e| e.message)
}

// A tuple return scalarizes to N leaves: one returned, the rest published to the
// scratch bank. No array anywhere on the wire.
#[test]
fn tuple_return_uses_scratch_slots() {
    let ws = ws_with("fn mk(a: int, b: int) -> (int, int) { (a, b) }");
    let js = emit_user(&ws, "mk");
    assert!(
        js.contains("__rock_ret1 = "),
        "leaf 1 publishes to the bank:\n{js}"
    );
    assert!(
        !js.contains("return ["),
        "multi-return must not allocate an array:\n{js}"
    );
}

// A closure capturing an iteration-local `let` copy snapshots THAT iteration's
// value into a fresh env record built inside the loop body (`convert_closures`
// classifies the copy as a by-value capture). Per-iteration correctness is pinned
// behaviorally in tests/functions/js_multivalue.rock on both backends.
#[test]
fn loop_capture_snapshots_per_iteration() {
    let ws = ws_with(
        "fn f() {\n\
             let fns: [fn() -> int] = []\n\
             for i in 0..3 { let a = i; fns.push(() => a) }\n\
         }",
    );
    let js = emit_closure(&ws, "f").expect("emits");
    assert!(
        js.contains("__slot: __rock_code_slot_ref("),
        "per-iteration capture record:\n{js}"
    );
    assert!(js.contains("function f__lambda0("), "hoisted body:\n{js}");
}

// A closure capturing the loop COUNTER directly now EMITS -- the shared
// closure-conversion pass classifies the counter as a per-iteration by-value
// capture (the fresh record each iteration snapshots the counter's value),
// where the deleted T7 path rejected it as out of subset. Per-iteration
// correctness is pinned in tests/closures/capture_semantics.rock on both backends.
#[test]
fn loop_counter_capture_now_emits() {
    let ws = ws_with(
        "fn f() {\n\
             let fns: [fn() -> int] = []\n\
             for i in 0..3 { fns.push(() => i) }\n\
         }",
    );
    let js = emit_closure(&ws, "f").expect("counter capture now emits");
    assert!(
        js.contains("__slot: __rock_code_slot_ref("),
        "counter snapshotted into a record:\n{js}"
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

// A `nobox` element of a tuple return carries its LEAVES into the return array,
// so no `NV2` object crosses the boundary; an UNMARKED value keeps its single
// boxed slot (the asymmetry guard). JS gets this from the shared MIR shape, not
// a backend-local rule.
#[test]
fn nobox_tuple_element_splats_into_the_return_array() {
    let ws = ws_with(
        "nobox value NV2 { x: float, y: float }\n\
         fn mk(v: NV2) -> (bool, NV2) { (true, v) }",
    );
    let js = emit_user(&ws, "mk");
    assert!(
        !js.contains("\"NV2\""),
        "nobox element materialized an object:\n{js}"
    );
    assert_eq!(
        return_wire_arity(&js),
        3,
        "the flag plus two leaves must come back as three values:\n{js}"
    );
}

#[test]
fn unmarked_value_tuple_element_keeps_its_box() {
    let ws = ws_with(
        "value BV2 { x: float, y: float }\n\
         fn mk(v: BV2) -> (bool, BV2) { (true, v) }",
    );
    let js = emit_user(&ws, "mk");
    // Assert the WIRE, not the presence of a `BV2` literal: a splatting return
    // that packs and re-splits still builds one, so only the wire's ARITY
    // distinguishes the two ABIs.
    assert_eq!(
        return_wire_arity(&js),
        2,
        "unmarked element lost its single boxed slot:\n{js}"
    );
}

/// The multi-value wire's arity: leaf 0 rides the return value, so it is the
/// highest published scratch slot plus one. Zero published slots means a
/// single-value return, which is not a multi-value wire at all.
fn return_wire_arity(js: &str) -> usize {
    let top = js
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("__rock_ret"))
        .filter_map(|r| r.split(' ').next())
        .filter_map(|n| n.parse::<usize>().ok())
        .max()
        .unwrap_or_else(|| panic!("no scratch-slot publish in:\n{js}"));
    top + 1
}

// A bound zero-leaf result (`Storage`) lowers to a multi-value call with no
// destinations; it is still called, for its effects.
#[test]
fn bound_zero_leaf_call_result_is_a_bare_call() {
    let src = "fn dest(points: [int]) -> Storage {\n\
                   points.push(5)\n\
                   return points.storage\n\
               }\n\
               fn root() -> int {\n\
                   let points = [1]\n\
                   let s = dest(points)\n\
                   return points.len()\n\
               }";
    let js = emit_closure(&ws_with(src), "root").expect("zero-leaf call emits");
    assert!(js.contains("  dest_arr_int(v1);"), "{js}");
}
