//! Checks the C leaf layout used by typed NativeBuffer views.

use skull_mir::lower::{value_struct_all_float_leaf_count, RepArena};

use crate::common::ws_with;

const SRC: &str = r#"
value V2 { x: float, y: float }
value Vertex { pos: V2, uv: V2, r: float, g: float, b: float, a: float, z: float, w: float }
value Flags { on: bool, n: int, hit: bool, v: float }
fn main() {}
"#;

#[test]
#[ignore = "known red (os-m3-js-pure-rock): float leaf view is __vd instead of __vf"]
fn an_all_float_element_plans_exactly_the_flat_slot_scheme() {
    let ws = ws_with(SRC);
    let name = ustr::Ustr::from("Vertex");
    let k = value_struct_all_float_leaf_count(&ws, name).expect("Vertex is all-float");
    let plan = RepArena::default()
        .nominal_leaf_addresses(&ws, bone_semantic::hir::FnBackend::Js, name)
        .expect("Vertex has a leaf plan");
    assert_eq!(plan.len(), k, "one leaf per float slot");
    for (i, a) in plan.iter().enumerate() {
        assert_eq!(a.view_field.as_str(), "__vf", "leaf {i} is not a float view");
        assert_eq!(a.stride as usize, k, "leaf {i} stride must be the float count");
        assert_eq!(a.offset as usize, i, "leaf {i} must sit at slot {i}");
    }
}

#[test]
#[ignore = "known red (os-m3-js-pure-rock): float leaf view is __vd instead of __vf"]
fn a_mixed_width_element_strides_each_view_in_its_own_units() {
    let ws = ws_with(SRC);
    let plan = RepArena::default()
        .nominal_leaf_addresses(
            &ws,
            bone_semantic::hir::FnBackend::Js,
            ustr::Ustr::from("Flags"),
        )
        .expect("Flags has a leaf plan");
    let fields: Vec<&str> = plan.iter().map(|a| a.view_field.as_str()).collect();
    assert_eq!(fields, vec!["__vb", "__vi", "__vb", "__vf"], "one view per leaf C type");
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
