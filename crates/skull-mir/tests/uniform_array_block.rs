//! std140 layout for a stage's `UniformArray<T>` block: an array's element
//! stride is its element's base alignment rounded up to 16. Every expected
//! number is a literal read off the std140 rules.

use crate::common::{key, ws_of};
use skull_mir::gpu_layout::{ArrayMemberDecl, WireKind, uniform_array_block, uniform_block};
use ustr::Ustr;

fn decl(name: &str, elem: WireKind, capacity: u32) -> ArrayMemberDecl {
    ArrayMemberDecl {
        name: Ustr::from(name),
        elem,
        capacity,
        span: bone_core::prelude::Span::unknown(),
    }
}

fn layout(p: &skull_mir::gpu_layout::UniformArrayBlockPlan) -> Vec<(&str, u32, u32, u32)> {
    p.members
        .iter()
        .map(|m| (m.name.as_str(), m.offset, m.stride, m.capacity))
        .collect()
}

#[test]
fn outline_detects_three_vec4_arrays() {
    let p = uniform_array_block(
        Ustr::from("outline_detect_frag_arrays"),
        &[
            decl("outline_colors", WireKind::F32x4, 256),
            decl("outline_colors_occluded", WireKind::F32x4, 256),
            decl("outline_params", WireKind::F32x4, 256),
        ],
    )
    .expect("12288 bytes is well inside the budget");
    assert_eq!(
        layout(&p),
        vec![
            ("outline_colors", 0, 16, 256),
            ("outline_colors_occluded", 4096, 16, 256),
            ("outline_params", 8192, 16, 256),
        ]
    );
    assert_eq!(p.size, 12288);
}

#[test]
fn scalar_and_vector_arrays_all_pay_stride_16() {
    let p = uniform_array_block(
        Ustr::from("sprite_batch_frag_arrays"),
        &[
            decl("uLightTypes", WireKind::I32, 16),
            decl("uLightPositions", WireKind::F32x2, 16),
            decl("uLightColors", WireKind::F32x4, 16),
            decl("uLightRadii", WireKind::F32, 16),
            decl("uLightStrengths", WireKind::F32, 16),
            decl("uLightDirections", WireKind::F32x2, 16),
            decl("uLightAngles", WireKind::F32, 16),
        ],
    )
    .expect("1792 bytes");
    assert_eq!(
        p.members.iter().map(|m| m.offset).collect::<Vec<_>>(),
        vec![0, 256, 512, 768, 1024, 1280, 1536]
    );
    assert!(p.members.iter().all(|m| m.stride == 16));
    assert_eq!(p.size, 1792);
}

#[test]
fn three_vec4_arrays_straddle_the_portable_limit() {
    for (capacity, expected_size) in [(341, Some(16368)), (342, None)] {
        let members = ["a", "b", "c"].map(|name| decl(name, WireKind::F32x4, capacity));
        let result = uniform_array_block(Ustr::from("arrays"), &members);
        if let Some(size) = expected_size {
            assert_eq!(result.expect("48 * 341 = 16368 fits").size, size);
        } else {
            let message = result
                .expect_err("48 * 342 = 16416 exceeds the limit")
                .message;
            assert!(
                message.contains("16416") && message.contains("16384"),
                "must name the size and the budget: {message}"
            );
        }
    }
}

#[test]
fn a_fourth_vec4_256_array_fits_exactly() {
    let p = uniform_array_block(
        Ustr::from("arrays"),
        &[
            decl("a", WireKind::F32x4, 256),
            decl("b", WireKind::F32x4, 256),
            decl("c", WireKind::F32x4, 256),
            decl("d", WireKind::F32x4, 256),
        ],
    )
    .expect("4 * 256 * 16 = 16384, exactly the guarantee");
    assert_eq!(p.size, 16384);
}

#[test]
fn array_rejections_keep_the_specific_reason() {
    let cases = [
        ("empty", vec![], &["no members"][..]),
        (
            "zero capacity",
            vec![decl("a", WireKind::F32x4, 0)],
            &["capacity"][..],
        ),
        (
            "Vec3",
            vec![decl("a", WireKind::F32x3, 4)],
            &["element type `Vec3`", "allowed: float, int, Vec2, Vec4"][..],
        ),
        (
            "Mat4",
            vec![decl("a", WireKind::Mat4, 4)],
            &["element type `Mat4`", "allowed: float, int, Vec2, Vec4"][..],
        ),
        (
            "uint",
            vec![decl("a", WireKind::U32, 4)],
            &["element type `uint`", "allowed: float, int, Vec2, Vec4"][..],
        ),
        (
            "bool",
            vec![decl("a", WireKind::Bool32, 4)],
            &["element type `bool`", "allowed: float, int, Vec2, Vec4"][..],
        ),
    ];
    for (case, members, fragments) in cases {
        let message = uniform_array_block(Ustr::from("arrays"), &members)
            .expect_err(case)
            .message;
        for fragment in fragments {
            assert!(
                message.contains(fragment),
                "{case}: missing `{fragment}`: {message}"
            );
        }
    }
}

/// The driver resolves members by name, so name and kind are both identity.
#[test]
fn the_fingerprint_separates_same_size_layouts() {
    let a = uniform_array_block(Ustr::from("arrays"), &[decl("a", WireKind::F32, 16)]).unwrap();
    let b = uniform_array_block(Ustr::from("arrays"), &[decl("a", WireKind::I32, 16)]).unwrap();
    let c = uniform_array_block(Ustr::from("arrays"), &[decl("b", WireKind::F32, 16)]).unwrap();
    assert_eq!(a.size, b.size, "same size is the premise of this test");
    assert_eq!(a.size, c.size);
    assert_ne!(
        a.fingerprint(),
        b.fingerprint(),
        "element kind must be hashed"
    );
    assert_ne!(
        a.fingerprint(),
        c.fingerprint(),
        "member name must be hashed"
    );

    assert!(a.fingerprint() < (1u64 << 53));
}

/// Array and scalar blocks are separate bindings; a bind must not accept one
/// for the other even when every keyed fact agrees.
#[test]
fn an_array_block_fingerprint_differs_from_a_scalar_blocks() {
    let ws = ws_of("value Shared { m0: Vec4 }");
    let scalar = uniform_block(&ws, key(&ws, "Shared")).expect("scalar plan");
    let arrays =
        uniform_array_block(key(&ws, "Shared"), &[decl("m0", WireKind::F32x4, 1)]).unwrap();

    assert_eq!(scalar.size, arrays.size, "both 16 bytes");
    assert_eq!(scalar.struct_name, arrays.block_name, "both named Shared");
    assert_eq!(
        scalar.members[0].name, arrays.members[0].name,
        "both member m0"
    );
    assert_eq!(
        scalar.members[0].offset, arrays.members[0].offset,
        "both at 0"
    );
    assert_eq!(scalar.members[0].kind, arrays.members[0].elem, "both Vec4");

    assert_ne!(
        scalar.fingerprint(),
        arrays.fingerprint(),
        "an array block and a scalar block of identical shape must not hash alike"
    );
    assert!(arrays.fingerprint() < (1u64 << 53));
}

/// At stride 16 a u32 product wraps at capacity 2^28, onto a size that would
/// pass the budget.
#[test]
fn overflowing_capacities_are_rejected() {
    let big = (1u32 << 27) + 1;
    for (case, capacities) in [
        ("u32 wrap point", vec![1 << 28]),
        ("largest capacity", vec![u32::MAX]),
        ("sum of two members", vec![big, big]),
    ] {
        let members: Vec<_> = capacities
            .into_iter()
            .enumerate()
            .map(|(i, capacity)| decl(&format!("m{i}"), WireKind::F32x4, capacity))
            .collect();
        let message = uniform_array_block(Ustr::from("arrays"), &members)
            .expect_err(case)
            .message;
        assert!(message.contains("16384"), "{case}: {message}");
    }
}
