//! Vertex attributes derived from a value struct's fields -- the first slice of
//! the target-neutral interface artifact.
//!
//! Scope note: these assert kinds, locations and component counts ONLY. Byte
//! offsets and stride are the struct's C layout, owned by skull-mir's layout
//! walker and cross-checked against LuaJIT -- see the module docs. Nothing here
//! may grow an offset assertion; that would be a second offset model.
//!
//! An instanced mesh may pass attribute sizes `[2, 1, 1, 1]` by hand,
//! derived from its fields. These tests exercise that layout contract.

use std::sync::LazyLock;
use ustr::Ustr;

use skull_mir::gpu_layout::{
    AttrKind, SamplerKind, TextureSlots, VertexAttrs, texture_slots, vertex_attrs,
};

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

/// Type-check `src` over the real stdlib and hand back the workspace.
fn ws_of(src: &str) -> bone_semantic::inference::Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let errs = skull_mir::verdict::body_errors(&ws);
    assert!(errs.is_empty(), "fixture must type-check: {errs:?}");
    ws
}

fn attrs_of(src: &str, struct_name: &str) -> VertexAttrs {
    let ws = ws_of(src);
    vertex_attrs(&ws, Ustr::from(struct_name)).expect("attributes must compute")
}

/// An instanced mesh with one position and three scalar fields.
#[test]
fn mesh_instance_attrs_match_the_hand_written_sizes() {
    let a = attrs_of(
        "value MeshInstance { pos: Vec2, rot: float, scale: float, height: float }",
        "MeshInstance",
    );

    // The `[2, 1, 1, 1]` in gpu_mesh_set_instances(.., 5, [2, 1, 1, 1]).
    assert_eq!(a.attr_sizes(), vec![2, 1, 1, 1]);

    let names: Vec<&str> = a.attrs.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, vec!["pos", "rot", "scale", "height"]);

    // Locations are RECORDED, not an enumerate() index materialized during
    // printing (the emitter's current shape, lib.rs:2255).
    let locs: Vec<u32> = a.attrs.iter().map(|x| x.location).collect();
    assert_eq!(locs, vec![0, 1, 2, 3]);

    assert_eq!(a.attrs[0].kind, AttrKind::F32x2);
    assert_eq!(a.attrs[1].kind, AttrKind::F32);
}

/// The engine's sprite vertex: pos(2)+uv(2)+color(4)+pivotY(1)+entityId(1).
/// Component counts only -- the matching renderer constants are offsets,
/// so they are the C layout's to reproduce, not this module's.
#[test]
fn sprite_vertex_attr_sizes() {
    let a = attrs_of(
        "value SpriteVert { aPos: Vec2, aTexCoord: Vec2, aColor: Vec4, aPivotY: float, aEntityId: float }",
        "SpriteVert",
    );
    assert_eq!(a.attr_sizes(), vec![2, 2, 4, 1, 1]);
    assert_eq!(a.attrs.len(), 5);
    assert_eq!(a.attrs[4].location, 4);
}

/// An int field is a distinct attribute kind: GL needs glVertexAttribIPointer,
/// and a float-path upload of an int attr is a silent wrong-value class.
#[test]
fn int_fields_are_a_distinct_attr_kind() {
    let a = attrs_of("value WithId { pos: Vec2, id: int }", "WithId");
    assert_eq!(a.attrs[0].kind, AttrKind::F32x2);
    assert_eq!(a.attrs[1].kind, AttrKind::I32);
    assert!(a.attrs[1].kind.is_integer());
    assert!(!a.attrs[0].kind.is_integer());
}

/// A Vec3 attribute has THREE components. This is the component count the
/// attribute pointer needs; it makes no claim about the 12-vs-16 byte question,
/// which is the C layout's (and where std140's rounding must never be applied).
#[test]
fn vec3_has_three_components() {
    let a = attrs_of("value V3Vert { a: Vec3, b: float }", "V3Vert");
    assert_eq!(a.attrs[0].kind, AttrKind::F32x3);
    assert_eq!(a.attr_sizes(), vec![3, 1]);
}

/// A non-tier-0 field cannot be a vertex attribute. Reject with a span rather
/// than admitting a field the GPU cannot receive.
#[test]
fn non_numeric_field_rejects() {
    let ws = ws_of("value Bad { pos: Vec2, name: string }");
    let e = vertex_attrs(&ws, Ustr::from("Bad")).expect_err("a string field must reject");
    let msg = format!("{e:?}");
    assert!(
        msg.contains("name"),
        "the error must name the offending field: {msg}"
    );
}

/// A non-`value` struct has no guaranteed C layout, so it cannot back a vertex
/// buffer at all.
#[test]
fn non_value_struct_rejects() {
    let ws = ws_of("struct Boxed { pos: Vec2 }");
    let e = vertex_attrs(&ws, Ustr::from("Boxed")).expect_err("a boxed struct must reject");
    assert!(format!("{e:?}").contains("value"));
}

// --- texture slots (P2): field order is the texture unit --------------------
// The `@textures` group exists so the unit is compiler-assigned. Hand-numbered
// units are the silent-collision class: two `Pass.inputs` entries sharing a
// number bind one texture and drop the other, with nothing to report.

fn slots_of(src: &str, struct_name: &str) -> TextureSlots {
    let ws = ws_of(src);
    texture_slots(&ws, Ustr::from(struct_name)).expect("slots must compute")
}

/// Texture bindings in declaration order.
#[test]
fn mesh_textures_take_slots_in_field_order() {
    let t = slots_of(
        "struct MeshTex { uTexture: Sampler2D, mask_tex: Sampler2D, shadowmap_tex: Sampler2D }",
        "MeshTex",
    );
    assert_eq!(t.slots.len(), 3);
    for (i, want) in ["uTexture", "mask_tex", "shadowmap_tex"].iter().enumerate() {
        assert_eq!(t.slots[i].name.as_str(), *want);
        assert_eq!(t.slots[i].slot, i as u32, "slot is the field index");
        assert_eq!(t.slots[i].kind, SamplerKind::Float);
    }
}

/// Integer samplers are a distinct kind: an isampler2D read through a float
/// sampler is undefined, and the id canvases (r32i) depend on the distinction.
#[test]
fn int_samplers_are_a_distinct_kind() {
    let t = slots_of(
        "struct Picker { color: Sampler2D, ids: ISampler2D }",
        "Picker",
    );
    assert_eq!(t.slots[0].kind, SamplerKind::Float);
    assert_eq!(t.slots[1].kind, SamplerKind::Int);
    assert_eq!(t.slots[1].slot, 1);
}

/// A non-sampler field in the group rejects: it would silently consume no unit
/// and shift every following slot off by one.
#[test]
fn non_sampler_field_rejects() {
    let ws = ws_of("struct Bad { tex: Sampler2D, scale: float }");
    let e = texture_slots(&ws, Ustr::from("Bad")).expect_err("a float field must reject");
    let msg = format!("{e:?}");
    assert!(
        msg.contains("scale"),
        "the error must name the offending field: {msg}"
    );
}

/// Samplers are opaque handles, not uploadable bytes, so the group is a plain
/// struct. A `value` struct would claim a C layout that cannot exist.
#[test]
fn value_struct_rejects() {
    let ws = ws_of("value Bad { tex: Sampler2D }");
    let e = texture_slots(&ws, Ustr::from("Bad")).expect_err("a value struct must reject");
    assert!(format!("{e:?}").contains("value"));
}

/// An empty group is a declaration with no effect -- reject rather than emit a
/// zero-sampler binding group.
#[test]
fn empty_group_rejects() {
    let ws = ws_of("struct Empty { }");
    assert!(texture_slots(&ws, Ustr::from("Empty")).is_err());
}
