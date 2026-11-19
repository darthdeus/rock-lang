//! std140 layout for an `@uniform` binding group (OpenGL 4.5 core, 7.6.2.2).
//! Every expected offset is a literal read off the std140 rules, never computed
//! from the layout function. A block bool is 32-bit on the wire, while Rock's C
//! bool is one byte.

use crate::common::{checked_ws_of, key};

use skull_mir::gpu_layout::{LeafStore, UniformBlockPlan, WireKind, WireView, uniform_block};
use skull_mir::lower::RepArena;

fn plan_of(src: &str, struct_name: &str) -> UniformBlockPlan {
    let ws = checked_ws_of(src);
    uniform_block(&ws, key(&ws, struct_name)).expect("block layout must compute")
}

fn rejection(src: &str, struct_name: &str) -> String {
    let ws = checked_ws_of(src);
    uniform_block(&ws, key(&ws, struct_name))
        .expect_err("must reject")
        .message
}

fn offsets(p: &UniformBlockPlan) -> Vec<(&str, u32)> {
    p.members
        .iter()
        .map(|m| (m.name.as_str(), m.offset))
        .collect()
}

fn stores(p: &UniformBlockPlan) -> Vec<(u32, WireView)> {
    p.leaf_stores().iter().map(|s| (s.offset, s.view)).collect()
}

const BLOOM_BLUR: &str = "value BloomBlurU { radius: float, direction: Vec2, resolution: float }";

#[test]
fn member_offsets_and_block_size_follow_std140() {
    let cases: &[(&str, &[(&str, u32)], u32)] = &[
        // C layout: [0, 4, 12], size 16.
        (
            BLOOM_BLUR,
            &[("radius", 0), ("direction", 8), ("resolution", 16)],
            32,
        ),
        (
            "value V { a: float, b: Vec2, c: float, d: float }",
            &[("a", 0), ("b", 8), ("c", 16), ("d", 20)],
            32,
        ),
        // C layout: [0, 16, 17, 20, 24, 40, 41, 44, 52], size 56.
        (
            "value SpriteFragU { uColor: Vec4, uLightsEnabled: bool, uUnlit: bool, \
             uLightCount: int, uAmbientColor: Vec4, uGlowEnabled: bool, uHasNormalTex: bool, \
             uNormalLightParams: Vec2, uNormalRelief: float }",
            &[
                ("uColor", 0),
                ("uLightsEnabled", 16),
                ("uUnlit", 20),
                ("uLightCount", 24),
                ("uAmbientColor", 32),
                ("uGlowEnabled", 48),
                ("uHasNormalTex", 52),
                ("uNormalLightParams", 56),
                ("uNormalRelief", 64),
            ],
            80,
        ),
        (
            "value V { dir: Vec3, gain: float }",
            &[("dir", 0), ("gain", 12)],
            16,
        ),
        (
            "value V { a: Mat4, b: Mat4, c: Mat4 }",
            &[("a", 0), ("b", 64), ("c", 128)],
            192,
        ),
        (
            "value V { m: Mat4, bounds: Vec2 }",
            &[("m", 0), ("bounds", 64)],
            80,
        ),
        ("value V { strength: float }", &[("strength", 0)], 16),
        // Same offsets as the C layout, but C size is 52.
        (
            "value V { a: Vec2, b: float, c: float, d: Vec4, e: Vec4, f: float }",
            &[
                ("a", 0),
                ("b", 8),
                ("c", 12),
                ("d", 16),
                ("e", 32),
                ("f", 48),
            ],
            64,
        ),
    ];
    for (src, want, size) in cases {
        let name = src.split_whitespace().nth(1).unwrap();
        let p = plan_of(src, name);
        assert_eq!(offsets(&p), *want, "{src}");
        assert_eq!(p.size, *size, "{src}");
    }
}

#[test]
fn member_wire_facts() {
    let p = plan_of("value V { flag: bool, m: Mat4 }", "V");
    assert_eq!(p.members[0].kind, WireKind::Bool32);
    assert_eq!(p.members[1].matrix_stride, Some(16));
}

#[test]
fn unsupported_groups_are_rejected_with_the_reason() {
    for (src, name, fragment) in [
        ("struct NotValue { a: float }", "NotValue", "value"),
        ("value Empty {}", "Empty", "no fields"),
        (
            "value Inner { a: float }\nvalue Outer { i: Inner, b: float }",
            "Outer",
            "Inner",
        ),
    ] {
        let message = rejection(src, name);
        assert!(message.contains(fragment), "{src}: {message}");
    }
}

#[test]
fn leaf_paths_name_every_primitive_in_order() {
    assert_eq!(
        plan_of(BLOOM_BLUR, "BloomBlurU").leaf_paths(),
        vec!["radius", "direction.x", "direction.y", "resolution"]
    );
    let want: Vec<String> = (0..4)
        .flat_map(|c| ["x", "y", "z", "w"].map(move |k| format!("m.c{c}.{k}")))
        .collect();
    assert_eq!(plan_of("value M { m: Mat4 }", "M").leaf_paths(), want);
}

/// Leaves bind to std140 slots by position, so the wire walker and the value
/// walker must produce the same sequence.
#[test]
fn the_wire_and_value_walkers_agree_on_every_shape() {
    for (src, name) in [
        (BLOOM_BLUR, "BloomBlurU"),
        ("value B { m: Mat4, n: Mat4, tail: Vec2 }", "B"),
        ("value C { a: Vec3, b: float, c: Vec4 }", "C"),
        ("value D { flag: bool, count: int, scale: float }", "D"),
    ] {
        let ws = checked_ws_of(src);
        let n = key(&ws, name);
        let plan = skull_mir::lower::uniform_block_checked(&ws, n)
            .unwrap_or_else(|e| panic!("`{name}` must pass the agreement check: {e}"));
        assert_eq!(
            plan.leaf_paths(),
            RepArena::default()
                .nominal_value_leaf_paths_required(&ws, bone_core::hir::FnBackend::Any, n)
                .expect("a value struct has leaf paths")
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>(),
            "`{name}`"
        );
    }
}

/// The two blocks agree on name, member names, size, offsets and leaf count;
/// only the kinds at those offsets differ.
#[test]
fn the_fingerprint_separates_layouts_that_differ_in_kind_or_name() {
    let base = plan_of("value A { x: float, y: Vec2 }", "A");
    let swapped = plan_of("value A { x: Vec2, y: float }", "A");
    assert_eq!(base.struct_name, swapped.struct_name);
    assert_eq!(
        base.members
            .iter()
            .map(|m| (m.name, m.offset))
            .collect::<Vec<_>>(),
        swapped
            .members
            .iter()
            .map(|m| (m.name, m.offset))
            .collect::<Vec<_>>()
    );
    assert_eq!(base.size, swapped.size);
    assert_eq!(base.leaf_stores().len(), swapped.leaf_stores().len());
    assert_ne!(base.fingerprint(), swapped.fingerprint());

    for (src, name) in [
        ("value B { x: float, y: Vec2 }", "B"),
        ("value A { x: float, z: Vec2 }", "A"),
    ] {
        assert_ne!(
            base.fingerprint(),
            plan_of(src, name).fingerprint(),
            "{src}"
        );
    }
    assert_eq!(
        base.fingerprint(),
        plan_of("value A { x: float, y: Vec2 }", "A").fingerprint()
    );
}

/// The fingerprint is emitted as a numeric literal, and LuaJIT and JS hold
/// integers exactly only up to 2^53.
#[test]
fn the_fingerprint_fits_a_host_number() {
    for src in [
        "value A { x: float }",
        "value A { x: float, y: Vec2, z: float }",
        "value A { m: Mat4, n: Mat4, f: float }",
        "value A { a: bool, b: bool, c: int, d: Vec4 }",
        "value A { alpha: Vec3, beta: int, gamma: uint }",
    ] {
        let fp = plan_of(src, "A").fingerprint();
        assert!(fp < 1 << 53, "{src}: fingerprint {fp} exceeds 2^53");
    }
}

#[test]
fn leaf_stores_carry_offsets_and_views() {
    let p = plan_of("value M { a: Mat4, b: Mat4 }", "M");
    let want: Vec<(u32, WireView)> = (0..32).map(|i| (i * 4, WireView::Float)).collect();
    assert_eq!(stores(&p), want);
    assert_eq!(p.members[1].leaf_stores()[0].offset, 64);

    let p = plan_of(
        "value B { flag: bool, count: int, scale: float, packed: uint }",
        "B",
    );
    assert_eq!(
        stores(&p),
        vec![
            (0, WireView::Int),
            (4, WireView::Int),
            (8, WireView::Float),
            (12, WireView::Uint)
        ]
    );

    let p = plan_of(BLOOM_BLUR, "BloomBlurU");
    assert_eq!(
        stores(&p),
        vec![
            (0, WireView::Float),
            (8, WireView::Float),
            (12, WireView::Float),
            (16, WireView::Float)
        ]
    );
    assert_eq!(
        p.members[1].leaf_stores(),
        vec![
            LeafStore {
                offset: 8,
                view: WireView::Float
            },
            LeafStore {
                offset: 12,
                view: WireView::Float
            },
        ]
    );
}

#[test]
fn blocks_are_limited_to_the_portable_opengl_size() {
    let mat4_block = |n: usize| {
        let fields: Vec<String> = (0..n).map(|i| format!("m{i}: Mat4")).collect();
        format!("value BigU {{ {} }}", fields.join(", "))
    };
    assert_eq!(plan_of(&mat4_block(256), "BigU").size, 16384);
    let message = rejection(&mat4_block(257), "BigU");
    assert!(
        message.contains("16448") && message.contains("16384"),
        "{message}"
    );
}

#[test]
fn a_sampler_in_a_uniform_group_names_the_fix() {
    let ws = checked_ws_of("value BadU { tint: Vec4, tex: Sampler2D }\nfn main() {}");
    let msg = skull_mir::gpu_layout::uniform_group_members(&ws, key(&ws, "BadU"))
        .expect_err("a sampler field must reject")
        .message;
    assert!(
        msg.contains("sampler") && msg.contains("fn param"),
        "the rejection does not name the fix: {msg}"
    );

    let ok = checked_ws_of("value GoodU { tint: Vec4 }\nfn main() {}");
    assert_eq!(
        skull_mir::gpu_layout::uniform_group_members(&ok, key(&ok, "GoodU"))
            .expect("a scalar group classifies")
            .len(),
        1
    );
}
