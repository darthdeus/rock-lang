//! Checks the C leaf layout used by typed NativeBuffer views.

use skull_mir::lower::{value_struct_all_float_leaf_count, RepArena};

use crate::common::ws_with;

const SRC: &str = r#"
value V2 { x: f32, y: f32 }
value Vertex { pos: V2, uv: V2, r: f32, g: f32, b: f32, a: f32, z: f32, w: f32 }
value Flags { on: bool, n: int, hit: bool, v: f32 }
value Wide { x: float, y: float }
fn main() {}
"#;

#[test]
fn an_all_float_element_plans_exactly_the_flat_slot_scheme() {
    let ws = ws_with(SRC);
    let name = ustr::Ustr::from("Vertex");
    let k = value_struct_all_float_leaf_count(&ws, name).expect("Vertex is all-float");
    let plan = RepArena::default()
        .nominal_leaf_addresses(&ws, bone_semantic::hir::FnBackend::Js, name)
        .expect("Vertex has a leaf plan");
    assert_eq!(plan.len(), k, "one leaf per f32 slot");
    for (i, a) in plan.iter().enumerate() {
        assert_eq!(a.view_field.as_str(), "__vf", "leaf {i} is not a float view");
        assert_eq!(a.stride as usize, k, "leaf {i} stride must be the float count");
        assert_eq!(a.offset as usize, i, "leaf {i} must sit at slot {i}");
    }
}

#[test]
fn double_fields_keep_the_float64_leaf_layout() {
    let ws = ws_with(SRC);
    let name = ustr::Ustr::from("Wide");
    let arena = RepArena::default();
    let (leaves, size, align) = arena
        .nominal_leaf_layout(&ws, bone_semantic::hir::FnBackend::Js, name)
        .expect("Wide has a C leaf layout");
    assert_eq!(
        leaves.iter().map(|l| (l.path.as_str(), l.cty, l.byte_off)).collect::<Vec<_>>(),
        vec![("x", "double", 0), ("y", "double", 8)]
    );
    assert_eq!((size, align), (16, 8));

    let plan = arena
        .nominal_leaf_addresses(&ws, bone_semantic::hir::FnBackend::Js, name)
        .expect("Wide has leaf addresses");
    assert_eq!(
        plan.iter().map(|a| a.view_field.as_str()).collect::<Vec<_>>(),
        vec!["__vd", "__vd"]
    );
    assert_eq!(plan.iter().map(|a| a.stride).collect::<Vec<_>>(), vec![2, 2]);
    assert_eq!(plan.iter().map(|a| a.offset).collect::<Vec<_>>(), vec![0, 1]);
}

#[test]
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
