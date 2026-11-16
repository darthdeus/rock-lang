//! The multi-value ABI: a scalarized tuple/multi return sends leaf 0 as the
//! ordinary return value and leaves 1..N through module-scope scratch slots
//! (`__rock_ret1 = b; return a;`), which the caller reads back right after the
//! call. An array or an object on that wire would allocate per call.

use crate::common::{emit_user, ws_with};

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

/// A `nobox` return crosses the JS call boundary as LEAVES, not as anything the
/// guarantee forbids. This replaced a blanket rejection, so it asserts the
/// property that rejection was protecting rather than the rejection itself: the
/// value must not materialize as an object OR as a transport array (which is an
/// allocation just the same, and does not vanish under V8's escape analysis).
#[test]
fn nobox_value_return_crosses_as_leaves_not_a_box() {
    let src = "nobox value V { x: int, y: int }\n\
               fn mk() -> V { V { x: 1, y: 2 } }\n\
               fn root() -> int {\n\
                   let v = mk()\n\
                   return v.x * 10 + v.y\n\
               }";
    let ws = ws_with(src);
    let js = crate::common::emit_closure(&ws, "root")
        .expect("a nobox return emits under the scratch ABI");
    assert!(
        !js.contains("\"V\""),
        "nobox value materialized an object:\n{js}"
    );
    assert!(
        !js.contains("return ["),
        "nobox return allocated a transport array:\n{js}"
    );
    assert!(
        js.contains("__rock_ret1 = "),
        "leaf 1 must ride the scratch bank:\n{js}"
    );
}

// A `nobox` element of a tuple return carries its LEAVES onto the return wire,
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
    let js = crate::common::emit_closure(&ws_with(src), "root").expect("zero-leaf call emits");
    assert!(js.contains("  dest_arr_int(v1);"), "{js}");
}
