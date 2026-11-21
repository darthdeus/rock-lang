//! std140 layout for an `@uniform` binding group (OpenGL 4.5 core, 7.6.2.2).
//! Every expected offset is a literal read off the std140 rules, never computed
//! from the layout function. A block bool is 32-bit on the wire, while Rock's C
//! bool is one byte.

use crate::common::{checked_ws_of, key};

use skull_mir::gpu_layout::{UniformBlockPlan, uniform_block};
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

const BLOOM_BLUR: &str = "value BloomBlurU { radius: float, direction: Vec2, resolution: float }";

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
