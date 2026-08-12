//! The two-walker agreement invariant behind leaf addressing.
//!
//! `value_leaf_paths` (over `ValueShape`) and `value_leaf_layout` (over the C
//! layout) are SEPARATE recursive walks, and leaf values are matched to byte
//! offsets POSITIONALLY. If they ever drift, every emitted `ffi.offsetof`
//! assertion still passes -- each path/offset pair it checks is internally
//! consistent -- while repr leaves land at the wrong addresses. So the runtime
//! layout checks do not cover this; these do.
//!
//! Design: docs/native-buffer-leaf-addressing-plan.md.

use crate::common::ws_of;
use skull_mir::lower::{value_leaf_layout_checked, LeafSlot, RepArena};
use ustr::Ustr;

fn layout(ws: &bone_semantic::inference::Workspace, name: &str) -> Vec<LeafSlot> {
    value_leaf_layout_checked(ws, Ustr::from(name))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .0
}

const SHAPES: &str = r#"
value Flat { x: float, y: float }
value Nested { pos: Flat, uv: Flat }
value Mixed { a: float, n: int, b: bool }
value DeepMixed { lo: Mixed, hi: Mixed, tag: int }
value Bools { p: bool, q: bool, r: bool }
value Deep3 { inner: Nested, tail: Flat }
fn main() { }
"#;

/// The invariant itself, over every shape family the layout walk admits:
/// primitives, nesting, mixed widths, bool packing, depth 3.
#[test]
fn leaf_walkers_agree_on_path_order() {
    let ws = ws_of(SHAPES);
    for name in ["Flat", "Nested", "Mixed", "DeepMixed", "Bools", "Deep3"] {
        // `value_leaf_layout_checked` returns Err on disagreement, so reaching
        // Ok IS the assertion -- but compare explicitly too, so a future
        // refactor that weakens the check does not silently pass this test.
        let got: Vec<Ustr> = layout(&ws, name).iter().map(|l| l.path).collect();
        let want = RepArena::default()
            .nominal_value_leaf_paths(
                &ws,
                bone_semantic::hir::FnBackend::Any,
                Ustr::from(name),
            )
            .expect("value shape");
        assert_eq!(want, got, "leaf order disagreement for `{name}`");
    }
}

/// A type with no C layout REJECTS naming itself, rather than returning a
/// skippable `None`. The emission site turns this into a CompilerError: every
/// name it passes is already in the FFI-clean set, so a missing layout is an
/// invariant failure, and skipping it would emit a type whose offsets nothing
/// validated.
#[test]
fn a_type_without_a_c_layout_rejects_by_name() {
    let ws = ws_of(SHAPES);
    let err = value_leaf_layout_checked(&ws, Ustr::from("NotAType"))
        .expect_err("a non-value type must not produce a layout");
    assert!(err.contains("NotAType"), "the rejection must name the type, got: {err}");
}

/// Offsets follow C rules, not declaration index: `Mixed` packs float@0, int@4,
/// bool@8; `Bools` packs three 1-byte leaves at 0/1/2. Pinning the VALUES (not
/// just agreement) is what catches a walk that is self-consistent but wrong.
#[test]
fn mixed_width_offsets_follow_c_layout() {
    let ws = ws_of(SHAPES);
    let got: Vec<(&str, &str, usize)> = layout(&ws, "Mixed")
        .iter()
        .map(|l| (l.path.as_str(), l.cty, l.byte_off))
        .collect();
    assert_eq!(got, vec![("a", "float", 0), ("n", "int", 4), ("b", "bool", 8)]);

    let bools: Vec<(&str, usize)> =
        layout(&ws, "Bools").iter().map(|l| (l.path.as_str(), l.byte_off)).collect();
    assert_eq!(bools, vec![("p", 0), ("q", 1), ("r", 2)]);
}

/// A nested leaf's offset is its parent's plus its own, and the path is dotted.
#[test]
fn nested_leaf_offsets_compose() {
    let ws = ws_of(SHAPES);
    let got: Vec<(&str, usize)> =
        layout(&ws, "Nested").iter().map(|l| (l.path.as_str(), l.byte_off)).collect();
    assert_eq!(got, vec![("pos.x", 0), ("pos.y", 4), ("uv.x", 8), ("uv.y", 12)]);

    // Depth 3, and a nested aggregate followed by a sibling: the sibling starts
    // after the WHOLE nested struct, not after its first leaf.
    let deep: Vec<(&str, usize)> =
        layout(&ws, "Deep3").iter().map(|l| (l.path.as_str(), l.byte_off)).collect();
    assert_eq!(
        deep,
        vec![
            ("inner.pos.x", 0),
            ("inner.pos.y", 4),
            ("inner.uv.x", 8),
            ("inner.uv.y", 12),
            ("tail.x", 16),
            ("tail.y", 20),
        ]
    );
}

/// Trailing padding: `Mixed` is float+int+bool = 9 bytes of members, rounded up
/// to align 4 = 12. `DeepMixed` therefore starts `hi` at 12, not 9 -- the exact
/// case that makes a FLATTENED cdef unsound and leaf addressing sound.
#[test]
fn nested_struct_trailing_padding_is_respected() {
    let ws = ws_of(SHAPES);
    let got: Vec<(&str, usize)> =
        layout(&ws, "DeepMixed").iter().map(|l| (l.path.as_str(), l.byte_off)).collect();
    assert_eq!(
        got,
        vec![
            ("lo.a", 0),
            ("lo.n", 4),
            ("lo.b", 8),
            ("hi.a", 12),
            ("hi.n", 16),
            ("hi.b", 20),
            ("tag", 24),
        ]
    );
}
