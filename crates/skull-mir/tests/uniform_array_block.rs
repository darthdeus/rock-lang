//! std140 layout for a stage's ARRAY block -- the `UniformArray<T>` half of the
//! uniform lane.
//!
//! A separate plan type from [`UniformBlockPlan`] on purpose. That one's contract
//! includes a value-struct producer: `uniform_block_checked` requires its
//! `leaf_paths()` to EQUAL `value_leaf_paths` over the same struct, matched by
//! position. Array members emit leaves no value struct can produce, so folding
//! them in would break that agreement or weaken it. Same std140 authority, two
//! producers.
//!
//! The rule being pinned: **an array's element stride is its element's base
//! alignment rounded up to 16**, so for every kind a uniform array may hold
//! (float, int, Vec2, Vec4) the stride is 16 and the member occupies
//! `capacity * 16`. That is also where the waste comes from -- a `float[16]`
//! costs 256 bytes to carry 64.
//!
//! Every expected number here is a LITERAL read off the std140 rules, never
//! computed from the function under test.

use crate::common::{key, ws_of};
use skull_mir::gpu_layout::{ArrayMemberDecl, WireKind, uniform_array_block, uniform_block};
use ustr::Ustr;

fn decl(name: &str, elem: WireKind, capacity: u32) -> ArrayMemberDecl {
    // A synthetic span: these cases exercise the LAYOUT, and the real `@size(N)`
    // site is supplied by the GLSL classifier. That the rejections land on the
    // offending param is pinned there, against real source.
    ArrayMemberDecl {
        name: Ustr::from(name),
        elem,
        capacity,
        span: bone_core::prelude::Span::unknown(),
    }
}

/// `(name, offset, stride, capacity)` per member.
fn layout(p: &skull_mir::gpu_layout::UniformArrayBlockPlan) -> Vec<(&str, u32, u32, u32)> {
    p.members
        .iter()
        .map(|m| (m.name.as_str(), m.offset, m.stride, m.capacity))
        .collect()
}

/// `outline_detect`'s three `@size(256)` `Vec4` arrays -- the tight consumer.
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

/// `sprite_batch_frag`'s seven light arrays, and the scalar-array waste that
/// backlog 18 decided to accept: four of the seven are scalar arrays costing
/// 256 bytes each to carry 64.
#[test]
fn sprite_batchs_light_arrays_pay_stride_16_for_scalars() {
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
    // Every member 256 bytes regardless of element kind -- that IS the rule.
    assert_eq!(
        p.members.iter().map(|m| m.offset).collect::<Vec<_>>(),
        vec![0, 256, 512, 768, 1024, 1280, 1536]
    );
    assert!(p.members.iter().all(|m| m.stride == 16));
    assert_eq!(p.size, 1792);

    // The four scalar arrays spend 1024 bytes carrying 256 bytes of data.
    let scalar_bytes: u32 = p
        .members
        .iter()
        .filter(|m| matches!(m.elem, WireKind::F32 | WireKind::I32))
        .map(|m| m.capacity * m.stride)
        .sum();
    assert_eq!(scalar_bytes, 1024);
}

// -- The portable OpenGL budget, on the array block's OWN size.
//
// The budget is per BLOCK and the arrays get their own, so a stage's scalar
// group is never added in. For outline's shape the block is `48 * N`: one more
// ENTRY adds a Vec4 to each of three arrays.

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

/// A FOURTH `Vec4[256]` array fits, at exactly the guarantee. The limit is a
/// size the block may reach.
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

// -- Rejections. The element kinds are the same set the GLSL emitter admits, so
// the two cannot drift into disagreeing about what an array may hold.

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

/// The fingerprint separates layouts a size comparison cannot. Two blocks with
/// the same total and the same offsets differ only in element KIND or in NAME,
/// and the driver resolves members by name.
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

    // And it must fit the 53-bit wire, for the same reason the scalar block's
    // does: it is emitted as a plain numeric literal.
    assert!(a.fingerprint() < (1u64 << 53));
}

/// An array block's fingerprint must not collide with a SCALAR block's. They are
/// separate bindings uploaded independently, so a bind that compared one against
/// the other would accept a mismatched buffer.
///
/// The two are made as alike as the types allow -- same block name, same single
/// member name, same element kind, same offset 0, same 16-byte size -- so the
/// premise assertions below are what make the inequality mean something.
///
/// This pins the OUTCOME, not a mechanism, and the separation is
/// over-determined: the array hash uses a different seed AND folds in capacity
/// and stride, which the scalar hash has nothing to fold. Measured -- giving both
/// the same seed leaves this test green, so it is not a test of the seed. Two
/// independent reasons is deliberate for a value a bind compares before trusting
/// a buffer, but do not read a pass here as evidence the seed is doing work.
#[test]
fn an_array_block_fingerprint_differs_from_a_scalar_blocks() {
    let ws = ws_of("value Shared { m0: Vec4 }");
    let scalar = uniform_block(&ws, key(&ws, "Shared")).expect("scalar plan");
    let arrays =
        uniform_array_block(key(&ws, "Shared"), &[decl("m0", WireKind::F32x4, 1)]).unwrap();

    // The premise: everything a fingerprint could key on agrees.
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

// -- Capacity arithmetic cannot be trusted to u32.
//
// `capacity * stride` is attacker-shaped input: `@size(N)` is whatever the source
// says. At stride 16 the product reaches u32::MAX at capacity 2^28, so a u32
// multiply either panics (debug overflow checks) or WRAPS -- and a wrap lands on
// a small size that passes the budget check, which is the one thing this
// function exists to enforce. Both are rejections that must arrive as
// diagnostics.

/// The wrap point and maximum input both exceed the budget after checked math.
#[test]
fn overflowing_member_capacities_are_rejected() {
    for (case, capacity) in [("u32 wrap point", 1 << 28), ("largest capacity", u32::MAX)] {
        let message = uniform_array_block(
            Ustr::from("arrays"),
            &[decl("a", WireKind::F32x4, capacity)],
        )
        .expect_err(case)
        .message;
        assert!(message.contains("16384"), "{case}: {message}");
    }
}

/// The sum across members can overflow even when each member alone does not.
#[test]
fn a_capacity_sum_that_would_overflow_is_rejected() {
    let big = (1u32 << 27) + 1;
    let e = uniform_array_block(
        Ustr::from("arrays"),
        &[
            decl("a", WireKind::F32x4, big),
            decl("b", WireKind::F32x4, big),
        ],
    )
    .expect_err("the two together exceed u32 at stride 16");
    assert!(e.message.contains("16384"), "{}", e.message);
}
