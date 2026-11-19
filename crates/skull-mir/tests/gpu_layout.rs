//! Vertex attributes and texture slots derived from struct fields. Only kinds,
//! locations and component counts are asserted here: byte offsets and stride
//! are the struct's C layout and must not get a second model.

use crate::common::{checked_ws_of, key};
use skull_mir::gpu_layout::{
    AttrKind, SamplerKind, TextureSlots, VertexAttrs, texture_slots, vertex_attrs,
};

fn attrs_of(src: &str, struct_name: &str) -> VertexAttrs {
    let ws = checked_ws_of(src);
    vertex_attrs(&ws, key(&ws, struct_name)).expect("attributes must compute")
}

fn slots_of(src: &str, struct_name: &str) -> TextureSlots {
    let ws = checked_ws_of(src);
    texture_slots(&ws, key(&ws, struct_name)).expect("slots must compute")
}

#[test]
fn vertex_attrs_follow_field_order_and_component_count() {
    let a = attrs_of(
        "value MeshInstance { pos: Vec2, rot: float, scale: float, height: float }",
        "MeshInstance",
    );
    assert_eq!(a.attr_sizes(), vec![2, 1, 1, 1]);
    let names: Vec<&str> = a.attrs.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, vec!["pos", "rot", "scale", "height"]);
    let locs: Vec<u32> = a.attrs.iter().map(|x| x.location).collect();
    assert_eq!(locs, vec![0, 1, 2, 3]);

    for (src, sizes, kinds) in [
        (
            "value V { a: Vec2, b: Vec2, c: Vec4, d: float, e: float }",
            vec![2, 2, 4, 1, 1],
            vec![
                AttrKind::F32x2,
                AttrKind::F32x2,
                AttrKind::F32x4,
                AttrKind::F32,
                AttrKind::F32,
            ],
        ),
        (
            "value V { a: Vec3, b: float }",
            vec![3, 1],
            vec![AttrKind::F32x3, AttrKind::F32],
        ),
        (
            "value V { pos: Vec2, id: int }",
            vec![2, 1],
            vec![AttrKind::F32x2, AttrKind::I32],
        ),
    ] {
        let a = attrs_of(src, "V");
        assert_eq!(a.attr_sizes(), sizes, "{src}");
        assert_eq!(a.attrs.iter().map(|x| x.kind).collect::<Vec<_>>(), kinds);
    }
    assert!(AttrKind::I32.is_integer());
    assert!(!AttrKind::F32x2.is_integer());
}

#[test]
fn texture_slots_follow_field_order_and_sampler_kind() {
    let t = slots_of(
        "struct MeshTex { uTexture: Sampler2D, mask_tex: Sampler2D, ids: ISampler2D }",
        "MeshTex",
    );
    let got: Vec<_> = t
        .slots
        .iter()
        .map(|s| (s.name.as_str(), s.slot, s.kind))
        .collect();
    assert_eq!(
        got,
        vec![
            ("uTexture", 0, SamplerKind::Float),
            ("mask_tex", 1, SamplerKind::Float),
            ("ids", 2, SamplerKind::Int),
        ]
    );
}

#[test]
fn unsupported_fields_and_structs_are_rejected_by_name() {
    for (src, name, fragment) in [
        ("value Bad { pos: Vec2, name: string }", "Bad", "name"),
        ("struct Boxed { pos: Vec2 }", "Boxed", "value"),
    ] {
        let ws = checked_ws_of(src);
        let e = vertex_attrs(&ws, key(&ws, name)).expect_err(src);
        assert!(format!("{e:?}").contains(fragment), "{src}: {e:?}");
    }
    for (src, name, fragment) in [
        (
            "struct Bad { tex: Sampler2D, scale: float }",
            "Bad",
            "scale",
        ),
        ("value Bad { tex: Sampler2D }", "Bad", "value"),
        ("struct Empty { }", "Empty", ""),
    ] {
        let ws = checked_ws_of(src);
        let e = texture_slots(&ws, key(&ws, name)).expect_err(src);
        assert!(format!("{e:?}").contains(fragment), "{src}: {e:?}");
    }
}
