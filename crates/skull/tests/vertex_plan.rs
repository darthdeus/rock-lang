//! The CPU-side buffer plan for a vertex/instance struct: attribute byte
//! offsets and element stride.
//!
//! The defect class these guard is DRIFT, not arithmetic. Every attribute kind
//! the artifact admits (`float`/`int`/`Vec2`/`Vec3`/`Vec4`) is 4-byte aligned,
//! so `4 * cumulative components` and the real C layout agree for every struct
//! expressible today -- a wrong-but-plausible model passes any single-struct
//! assertion. What it cannot pass is a plan that has to FOLLOW the struct when
//! the struct changes, which is what a hand-written `stride, [components..]`
//! literal at a call site fails to do.
//!
//! So the load-bearing tests here are differential (reorder, add a field), and
//! `grass_instance_shape_pins_the_known_values` is deliberately the WEAK one --
//! it passes against a hardcoded table and is kept only to pin the numbers the
//! bitgun call site uses.

use crate::common::ws_of;
use skull_mir::gpu_layout::AttrKind;
use skull_mir::lower::RepArena;
use skull_mir::vertex_plan::{vertex_plan, AttrSlot, VertexPlan};
use ustr::Ustr;

// `ws_of` is a bare single-module workspace (no stdlib), and `attr_kind`
// recognizes vectors by nominal NAME -- so the fixture declares its own, the
// same way `value_leaf_layout.rs` declares its shapes.
const SHAPES: &str = r#"
value Vec2 { x: f32, y: f32 }
value Vec3 { x: f32, y: f32, z: f32 }
value Vec4 { x: f32, y: f32, z: f32, w: f32 }
value Grass { pos: Vec2, rot: f32, scale: f32, height: f32 }
value GrassReordered { rot: f32, pos: Vec2, scale: f32, height: f32 }
value GrassPlusOne { pos: Vec2, rot: f32, scale: f32, height: f32, tint: f32 }
value WithInt { pos: Vec2, id: int }
value Wide { a: Vec4, b: Vec3, c: f32 }
fn main() { }
"#;

fn plan(ws: &bone_semantic::inference::Workspace, name: &str) -> Vec<(String, u32, usize)> {
    vertex_plan(ws, &RepArena::default(), Ustr::from(name))
        .unwrap_or_else(|e| panic!("{name}: {e:?}"))
        .slots
        .iter()
        .map(|s| (s.name.to_string(), s.location, s.byte_offset))
        .collect()
}

fn stride(ws: &bone_semantic::inference::Workspace, name: &str) -> usize {
    vertex_plan(ws, &RepArena::default(), Ustr::from(name))
        .unwrap_or_else(|e| panic!("{name}: {e:?}"))
        .byte_stride
}

/// The numbers bitgun's `gpu_mesh_set_instances` call site passes by hand
/// (stride 5 floats = 20 bytes, components [2,1,1,1]).
///
/// WEAK ON PURPOSE: a hardcoded table passes this. It pins the values, it does
/// not demonstrate they were derived -- the differential tests below do that.
#[test]
fn grass_instance_shape_pins_the_known_values() {
    let ws = ws_of(SHAPES);
    assert_eq!(
        plan(&ws, "Grass"),
        vec![
            ("pos".to_string(), 0, 0),
            ("rot".to_string(), 1, 8),
            ("scale".to_string(), 2, 12),
            ("height".to_string(), 3, 16),
        ]
    );
    assert_eq!(stride(&ws, "Grass"), 20);
}

/// Reordering the struct MOVES every offset after the moved field. This is the
/// drift the call-site literal cannot see: `5, [2,1,1,1]` is unchanged by a
/// reorder while every attribute silently rebinds to a neighbour's bytes.
#[test]
fn reordering_fields_moves_the_offsets() {
    let ws = ws_of(SHAPES);
    assert_eq!(
        plan(&ws, "GrassReordered"),
        vec![
            ("rot".to_string(), 0, 0),
            ("pos".to_string(), 1, 4),
            ("scale".to_string(), 2, 12),
            ("height".to_string(), 3, 16),
        ]
    );
    // Same fields, same stride -- only the offsets and locations move, which is
    // exactly why a stride-only cross-check would not catch it.
    assert_eq!(stride(&ws, "GrassReordered"), 20);
}

/// Adding a field grows the stride and appends a location.
#[test]
fn adding_a_field_grows_the_stride() {
    let ws = ws_of(SHAPES);
    assert_eq!(stride(&ws, "GrassPlusOne"), 24);
    let got = plan(&ws, "GrassPlusOne");
    assert_eq!(got.len(), 5);
    assert_eq!(got[4], ("tint".to_string(), 4, 20));
}

/// Offsets follow the C layout for wider vectors, and the plan covers the whole
/// struct: the last attribute's offset plus its own size is the stride.
#[test]
fn wide_vectors_pack_by_c_rules() {
    let ws = ws_of(SHAPES);
    assert_eq!(
        plan(&ws, "Wide"),
        vec![
            ("a".to_string(), 0, 0),
            ("b".to_string(), 1, 16),
            ("c".to_string(), 2, 28),
        ]
    );
    assert_eq!(stride(&ws, "Wide"), 32);
}

/// An integer attribute is representable in the plan -- it carries its kind --
/// but REJECTS the float-unit form the `gpu_mesh_*` API takes today, which
/// casts the buffer to `float*` and binds every attribute through
/// `glVertexAttribPointer`. Reading an int through that path yields the float
/// reinterpretation of its bits: a silent wrong-value class, not an error.
#[test]
fn an_integer_attribute_rejects_the_float_unit_form() {
    let ws = ws_of(SHAPES);
    let p = vertex_plan(&ws, &RepArena::default(), Ustr::from("WithInt"))
        .expect("int is a valid attribute kind");
    assert_eq!(p.slots[1].kind, AttrKind::I32);

    let err = p.float_unit_sizes().expect_err("the float path must not accept an int attribute");
    assert!(err.contains("id"), "the rejection must name the offending field, got: {err}");

    // The all-float sibling still passes, so the rejection is about the int and
    // not about the form itself.
    let g = vertex_plan(&ws, &RepArena::default(), Ustr::from("Grass")).expect("Grass");
    assert_eq!(g.float_unit_sizes().expect("all-float"), vec![2, 1, 1, 1]);
    assert_eq!(g.float_unit_stride().expect("all-float"), 5);
}

/// The float-unit view (`gpu_mesh_*`'s component-counts-plus-stride) DROPS
/// `byte_offset`, and `stone-rs`'s `validate_layout` reconstructs offsets
/// cumulatively -- so the two agree only while every attribute sits exactly at
/// `4 * cumulative components`. That holds for every struct expressible today
/// (all admitted kinds are 4-byte-aligned floats) and expires at the first
/// `Mat4` or packed-u8 attribute.
///
/// Which is why this is pinned against a HAND-BUILT plan: no source struct can
/// currently violate it, so a call-level RED is unreachable. Building the plan
/// directly is what makes the check falsifiable at all -- without it the
/// assertion would be untested code guarding a case nothing can produce yet.
#[test]
fn a_slot_off_its_cumulative_position_rejects_the_float_unit_view() {
    let synthetic = VertexPlan {
        struct_name: Ustr::from("Synthetic"),
        slots: vec![
            AttrSlot {
                name: Ustr::from("pos"),
                location: 0,
                kind: AttrKind::F32x2,
                byte_offset: 0,
            },
            // Cumulative position is 8; 12 is where a Vec3-shaped hole would put
            // it. The float-unit view cannot express the difference.
            AttrSlot {
                name: Ustr::from("rot"),
                location: 1,
                kind: AttrKind::F32,
                byte_offset: 12,
            },
        ],
        byte_stride: 16,
    };

    let err = synthetic
        .float_unit_sizes()
        .expect_err("a slot off its cumulative position must not yield a float-unit view");
    assert!(err.contains("rot"), "the rejection must name the field, got: {err}");
    assert!(err.contains("Synthetic"), "the rejection must name the struct, got: {err}");
    assert!(
        synthetic.float_unit_stride().is_err(),
        "the stride half of the same view must reject too, or a caller taking only \
         the stride would proceed on a layout the view cannot describe"
    );

    // Positive control: the real derivation satisfies the invariant, so the
    // check is not simply rejecting everything.
    let ws = ws_of(SHAPES);
    let real = vertex_plan(&ws, &RepArena::default(), Ustr::from("Grass")).expect("Grass");
    assert!(real.float_unit_sizes().is_ok());
    assert!(real.float_unit_stride().is_ok());
}

/// TRAILING padding is the other half of the same representability hole. The
/// float-unit view states component counts and a stride; `stone-rs`'s
/// `validate_layout` then checks `sum(sizes) == stride`. So a struct whose
/// stride exceeds its attributes' extent is not describable by the view either
/// -- the per-slot start check above passes it, and the failure lands at mesh
/// creation as a runtime reject rather than at compile time.
///
/// Hand-built for the same reason as the sibling: every admitted kind is
/// 4-byte aligned, so no source struct has trailing padding today.
#[test]
fn trailing_padding_rejects_the_float_unit_view() {
    let padded = VertexPlan {
        struct_name: Ustr::from("Padded"),
        slots: vec![AttrSlot {
            name: Ustr::from("a"),
            location: 0,
            kind: AttrKind::F32,
            byte_offset: 0,
        }],
        // One float of attributes, two floats of stride.
        byte_stride: 8,
    };

    let err = padded
        .float_unit_sizes()
        .expect_err("attributes that do not span the stride must not yield a float-unit view");
    assert!(err.contains("Padded"), "the rejection must name the struct, got: {err}");
    assert!(
        padded.float_unit_stride().is_err(),
        "the stride half must reject too -- it is the half that would be believed"
    );

    // Positive control: a plan whose attributes exactly span its stride.
    let ws = ws_of(SHAPES);
    assert!(vertex_plan(&ws, &RepArena::default(), Ustr::from("Grass"))
        .expect("Grass")
        .float_unit_sizes()
        .is_ok());
}
