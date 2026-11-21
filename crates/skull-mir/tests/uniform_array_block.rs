//! std140 layout for a stage's `UniformArray<T>` block: an array's element
//! stride is its element's base alignment rounded up to 16. Every expected
//! number is a literal read off the std140 rules.

use skull_mir::gpu_layout::{ArrayMemberDecl, WireKind, uniform_array_block};
use ustr::Ustr;

fn decl(name: &str, elem: WireKind, capacity: u32) -> ArrayMemberDecl {
    ArrayMemberDecl {
        name: Ustr::from(name),
        elem,
        capacity,
        span: bone_core::prelude::Span::unknown(),
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
