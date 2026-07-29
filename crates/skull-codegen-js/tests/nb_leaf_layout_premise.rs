//! The premise JS leaf addressing is built on: for an ALL-FLOAT element, the C
//! leaf plan is bit-for-bit the flat-slot scheme JS already uses -- stride = the
//! float-leaf count, offset = the leaf's index. That is what lets the vertex pool
//! keep its `Float32Array(n * k)` storage, and `.ptr` keep going to WebGL
//! unchanged, while the emitter switches to the general leaf path.
//!
//! If this ever stops holding, the switch silently reinterprets every vertex
//! buffer in the web renderer.

use skull_mir::lower::{leaf_addr_plan, value_struct_all_float_leaf_count};

mod common;
use common::ws_with;

const SRC: &str = r#"
value V2 { x: float, y: float }
value Vertex { pos: V2, uv: V2, r: float, g: float, b: float, a: float, z: float, w: float }
value Flags { on: bool, n: int, hit: bool, v: float }
fn main() {}
"#;

#[test]
fn an_all_float_element_plans_exactly_the_flat_slot_scheme() {
    let ws = ws_with(SRC);
    let name = ustr::Ustr::from("Vertex");
    let k = value_struct_all_float_leaf_count(&ws, name).expect("Vertex is all-float");
    let plan = leaf_addr_plan(&ws, name).expect("Vertex has a leaf plan");
    assert_eq!(plan.len(), k, "one leaf per float slot");
    for (i, a) in plan.iter().enumerate() {
        assert_eq!(a.view_field.as_str(), "__vf", "leaf {i} is not a float view");
        assert_eq!(a.stride as usize, k, "leaf {i} stride must be the float count");
        assert_eq!(a.offset as usize, i, "leaf {i} must sit at slot {i}");
    }
}

/// The widening case: a mixed element gets one view per C type, each strided in
/// ITS OWN units, and the element size stays a multiple of 4 so a `Float32Array`
/// can still back the storage the other views alias.
#[test]
fn a_mixed_width_element_strides_each_view_in_its_own_units() {
    let ws = ws_with(SRC);
    let plan = leaf_addr_plan(&ws, ustr::Ustr::from("Flags")).expect("Flags has a leaf plan");
    let fields: Vec<&str> = plan.iter().map(|a| a.view_field.as_str()).collect();
    assert_eq!(fields, vec!["__vb", "__vi", "__vb", "__vf"], "one view per leaf C type");
    // C lays `Flags` out as bool(1) pad(3) int(4) bool(1) pad(3) float(4) = 16.
    let byte_strides: Vec<i64> = plan
        .iter()
        .map(|a| a.stride * if a.view_field.as_str() == "__vb" { 1 } else { 4 })
        .collect();
    assert!(
        byte_strides.iter().all(|s| *s == byte_strides[0]),
        "every view must span the same element in bytes: {byte_strides:?}"
    );
    assert_eq!(byte_strides[0] % 4, 0, "element size must stay 4-byte-backable");
}
