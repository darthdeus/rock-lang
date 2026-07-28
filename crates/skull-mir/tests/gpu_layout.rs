//! Vertex layout computed from a value struct's fields -- the first slice of
//! the target-neutral interface artifact (docs/shader-binding-layout-plan.md
//! P1). One computation feeds the cdata declaration, glVertexAttribPointer,
//! and the shader's attribute locations, so the three cannot disagree.
//!
//! The live lockstep this replaces: bitgun/grass.rock passes stride `5` and
//! attr sizes `[2, 1, 1, 1]` by hand, both derived from GrassInstance's
//! fields, with grass_shader.rock carrying a comment asserting the agreement.

use std::sync::LazyLock;
use ustr::Ustr;

use skull_mir::gpu_layout::{vertex_layout, AttrKind, VertexLayout};

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

/// Type-check `src` over the real stdlib and hand back the workspace.
fn ws_of(src: &str) -> bone_semantic::inference::Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let errs = skull_mir::verdict::body_errors(&ws);
    assert!(errs.is_empty(), "fixture must type-check: {errs:?}");
    ws
}

fn layout_of(src: &str, struct_name: &str) -> VertexLayout {
    let ws = ws_of(src);
    vertex_layout(&ws, Ustr::from(struct_name)).expect("layout must compute")
}

/// The live shape: bitgun's GrassInstance. Field order is attribute order,
/// tightly packed, no padding -- vertex buffers are not std140.
#[test]
fn grass_instance_layout_matches_the_hand_written_constants() {
    let l = layout_of(
        "value GrassInstance { pos: Vec2, rot: float, scale: float, height: float }",
        "GrassInstance",
    );

    // The `5` in gpu_mesh_set_instances(.., 5, ..)
    assert_eq!(l.stride_floats(), 5, "stride in floats");
    assert_eq!(l.stride_bytes, 20, "stride in bytes");

    // The `[2, 1, 1, 1]` in the same call.
    assert_eq!(l.attr_sizes(), vec![2, 1, 1, 1], "per-attribute component counts");

    let names: Vec<&str> = l.attrs.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, vec!["pos", "rot", "scale", "height"]);

    // Locations are RECORDED, not an enumerate() index materialized during
    // printing (the emitter's current shape, lib.rs:2255).
    let locs: Vec<u32> = l.attrs.iter().map(|a| a.location).collect();
    assert_eq!(locs, vec![0, 1, 2, 3]);

    let offsets: Vec<u32> = l.attrs.iter().map(|a| a.offset_bytes).collect();
    assert_eq!(offsets, vec![0, 8, 12, 16]);

    assert_eq!(l.attrs[0].kind, AttrKind::F32x2);
    assert_eq!(l.attrs[1].kind, AttrKind::F32);
}

/// The engine's sprite vertex: pos(2)+uv(2)+color(4)+pivotY(1)+entityId(1),
/// stride 10 floats. These are the MESH_*_OFFSET constants in
/// stone-rs/src/sprite.rs:585-586 that the layout must reproduce exactly.
#[test]
fn sprite_vertex_layout_matches_the_mesh_offset_constants() {
    let l = layout_of(
        "value SpriteVert { aPos: Vec2, aTexCoord: Vec2, aColor: Vec4, aPivotY: float, aEntityId: float }",
        "SpriteVert",
    );

    assert_eq!(l.stride_floats(), 10, "the hard-coded stride at every draw site");
    assert_eq!(l.attr_sizes(), vec![2, 2, 4, 1, 1]);

    // MESH_POS_OFFSET 0, MESH_UV_OFFSET 2, MESH_COLOR_OFFSET 4,
    // MESH_PIVOT_Y_OFFSET 8, MESH_ENTITY_ID_OFFSET 9 -- in floats.
    let float_offsets: Vec<u32> = l.attrs.iter().map(|a| a.offset_bytes / 4).collect();
    assert_eq!(float_offsets, vec![0, 2, 4, 8, 9]);
}

/// An int field is a distinct attribute kind: GL needs glVertexAttribIPointer,
/// and a float-path upload of an int attr is a silent wrong-value class.
#[test]
fn int_fields_are_a_distinct_attr_kind() {
    let l = layout_of("value WithId { pos: Vec2, id: int }", "WithId");
    assert_eq!(l.attrs[0].kind, AttrKind::F32x2);
    assert_eq!(l.attrs[1].kind, AttrKind::I32);
    assert_eq!(l.stride_bytes, 12);
}

/// Vertex layout is tightly packed. A Vec3 does NOT round up to 16 bytes the
/// way it would under std140 -- that rule belongs to the uniform family, and
/// applying it here would silently corrupt every buffer.
#[test]
fn vec3_is_tightly_packed_not_std140_padded() {
    let l = layout_of("value V3Vert { a: Vec3, b: float }", "V3Vert");
    assert_eq!(l.attrs[0].offset_bytes, 0);
    assert_eq!(l.attrs[1].offset_bytes, 12, "b follows a with no padding");
    assert_eq!(l.stride_bytes, 16);
}

/// A non-tier-0 field cannot live in a vertex buffer. Reject with a span
/// rather than emitting a layout that silently disagrees with the GPU.
#[test]
fn non_numeric_field_rejects() {
    let ws = ws_of("value Bad { pos: Vec2, name: string }");
    let e = vertex_layout(&ws, Ustr::from("Bad")).expect_err("a string field must reject");
    let msg = format!("{e:?}");
    assert!(msg.contains("name"), "the error must name the offending field: {msg}");
}
