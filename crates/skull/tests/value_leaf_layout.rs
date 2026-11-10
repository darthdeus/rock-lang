//! The two-walker agreement invariant behind leaf addressing.
//!
//! `value_leaf_paths` (over `ValueShape`) and `value_leaf_layout` (over the C
//! layout) are SEPARATE recursive walks, and leaf values are matched to byte
//! offsets POSITIONALLY. If they ever drift, every emitted `ffi.offsetof`
//! assertion still passes -- each path/offset pair it checks is internally
//! consistent -- while repr leaves land at the wrong addresses. So the runtime
//! layout checks do not cover this; these do.
//!
//!

use crate::common::ws_of;
use skull_mir::lower::{LeafSlot, RepArena};
use ustr::Ustr;

fn layout(ws: &bone_semantic::inference::Workspace, name: &str) -> Vec<LeafSlot> {
    RepArena::default()
        .nominal_leaf_layout(ws, bone_semantic::hir::FnBackend::Any, Ustr::from(name))
        .unwrap_or_else(|e| panic!("{name}: {}", e.message))
        .0
}

const SHAPES: &str = r#"
value Flat { x: f32, y: f32 }
value Nested { pos: Flat, uv: Flat }
value Mixed { a: f32, n: int, b: bool }
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
        // the owned leaf-layout projection returns an error on disagreement, so reaching
        // Ok IS the assertion -- but compare explicitly too, so a future
        // refactor that weakens the check does not silently pass this test.
        let got: Vec<Ustr> = layout(&ws, name).iter().map(|l| l.path).collect();
        let want = RepArena::default()
            .nominal_value_leaf_paths_required(
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
    let err = RepArena::default()
        .nominal_leaf_layout(
            &ws,
            bone_semantic::hir::FnBackend::Any,
            Ustr::from("NotAType"),
        )
        .expect_err("a non-value type must not produce a layout");
    assert!(
        err.message.contains("NotAType"),
        "the rejection must name the type, got: {}",
        err.message
    );
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
    assert_eq!(
        got,
        vec![("a", "float", 0), ("n", "int", 4), ("b", "bool", 8)]
    );

    let bools: Vec<(&str, usize)> = layout(&ws, "Bools")
        .iter()
        .map(|l| (l.path.as_str(), l.byte_off))
        .collect();
    assert_eq!(bools, vec![("p", 0), ("q", 1), ("r", 2)]);
}

/// A nested leaf's offset is its parent's plus its own, and the path is dotted.
#[test]
fn nested_leaf_offsets_compose() {
    let ws = ws_of(SHAPES);
    let got: Vec<(&str, usize)> = layout(&ws, "Nested")
        .iter()
        .map(|l| (l.path.as_str(), l.byte_off))
        .collect();
    assert_eq!(
        got,
        vec![("pos.x", 0), ("pos.y", 4), ("uv.x", 8), ("uv.y", 12)]
    );

    // Depth 3, and a nested aggregate followed by a sibling: the sibling starts
    // after the WHOLE nested struct, not after its first leaf.
    let deep: Vec<(&str, usize)> = layout(&ws, "Deep3")
        .iter()
        .map(|l| (l.path.as_str(), l.byte_off))
        .collect();
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
    let got: Vec<(&str, usize)> = layout(&ws, "DeepMixed")
        .iter()
        .map(|l| (l.path.as_str(), l.byte_off))
        .collect();
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

/// `float` is f64 in a value's C layout; `f32` is the explicit 4-byte leaf.
#[test]
fn float_width_follows_the_declared_type() {
    let ws = ws_of(
        "value Narrow { x: f32, y: f32, n: int }\nvalue Wide { x: float, n: int }\nvalue Spelled { x: f64 }\nfn main() { }\n",
    );
    let arena = RepArena::default();
    let (narrow, size, _) = arena
        .nominal_leaf_layout(
            &ws,
            bone_semantic::hir::FnBackend::Any,
            Ustr::from("Narrow"),
        )
        .expect("Narrow layout");
    let leaves: Vec<(&str, &str, usize)> = narrow
        .iter()
        .map(|l| (l.path.as_str(), l.cty, l.byte_off))
        .collect();
    assert_eq!(
        leaves,
        vec![("x", "float", 0), ("y", "float", 4), ("n", "int", 8)]
    );
    assert_eq!(size, 12);

    let (wide, size, align) = arena
        .nominal_leaf_layout(&ws, bone_semantic::hir::FnBackend::Any, Ustr::from("Wide"))
        .expect("Wide layout");
    let leaves: Vec<(&str, &str, usize)> = wide
        .iter()
        .map(|l| (l.path.as_str(), l.cty, l.byte_off))
        .collect();
    assert_eq!(leaves, vec![("x", "double", 0), ("n", "int", 8)]);
    assert_eq!((size, align), (16, 8));

    let (spelled, _, _) = arena
        .nominal_leaf_layout(
            &ws,
            bone_semantic::hir::FnBackend::Any,
            Ustr::from("Spelled"),
        )
        .expect("Spelled layout");
    assert_eq!(spelled[0].cty, "double");
}

const LAYOUT_PROGRAM: &str = r#"
nobox value Inner { a: float, b: int }
nobox value Outer { inner: Inner, c: float, d: int }
fn main() {
    let o = Outer { inner: Inner { a: 1.0, b: 2 }, c: 3.0, d: 4 }
    print(o.inner.b + o.d)
}
"#;

/// The load-time layout check is one shared helper call per type, and a
/// mismatch still names the drifted size or leaf.
#[test]
fn layout_checks_share_one_failure_path() {
    let lua = crate::common::emit_module(LAYOUT_PROGRAM);
    // The size and leaf messages, both in the shared helper.
    assert_eq!(lua.matches("skull layout drift").count(), 2, "{lua}");
    assert_eq!(crate::common::run_main(&lua), "6");
    let check = lua
        .lines()
        .find(|line| line.starts_with("__rock_layout_check(\"Outer\""))
        .unwrap_or_else(|| panic!("no Outer layout check:\n{lua}"));
    let leaf = check.replace("\"d\", 24,", "\"d\", 25,");
    assert_ne!(leaf, check, "{check}");
    let (ok, out) = crate::common::run_luajit_capturing(&lua.replace(check, &leaf));
    assert!(
        !ok && out.contains("skull layout drift: Outer.d at 24, expected 25"),
        "{out}"
    );
    let size = check.replace("\"Outer\", 32,", "\"Outer\", 40,");
    assert_ne!(size, check, "{check}");
    let (ok, out) = crate::common::run_luajit_capturing(&lua.replace(check, &size));
    assert!(
        !ok && out.contains("skull layout drift: sizeof(Outer) = 32, expected 40"),
        "{out}"
    );
}

/// A nested leaf is also read through its flattened alias, so the alias must
/// sit at the same offset as the leaf it stands for.
#[test]
fn a_nested_leaf_alias_at_another_offset_fails_the_load() {
    let lua = crate::common::emit_module(LAYOUT_PROGRAM);
    let alias = |path: &str| {
        let hex: String = path.bytes().map(|b| format!("{b:02x}")).collect();
        format!("__rock_leaf_{hex}")
    };
    let (a, b) = (alias("inner.a"), alias("inner.b"));
    let flat = format!("double {a}; int {b};");
    assert!(lua.contains(&flat), "no flattened alias `{flat}`:\n{lua}");
    let swapped = lua.replace(&flat, &format!("int {b}; double {a};"));
    let (ok, out) = crate::common::run_luajit_capturing(&swapped);
    assert!(
        !ok && out.contains(&format!("skull leaf alias drift: Outer.{a}")),
        "{out}"
    );
}

fn layout_check_line<'a>(lua: &'a str, sym: &str) -> &'a str {
    let head = format!("__rock_layout_check(\"{sym}\"");
    lua.lines()
        .find(|line| line.starts_with(&head))
        .unwrap_or_else(|| panic!("no {sym} layout check:\n{lua}"))
}

const MUTATION_PROGRAM: &str = r#"
nobox value Inner { a: float, b: int }
nobox value Outer { inner: Inner, c: float, e: float, d: int }
fn main() {
    let o = Outer { inner: Inner { a: 1.0, b: 2 }, c: 3.0, e: 5.0, d: 4 }
    print(o.inner.b + o.d)
}
"#;

/// Corrupt the C side of the emitted program one way at a time -- the cdef
/// LuaJIT lays out, never the compiler's expected offsets -- and require the
/// load to fail with the diagnostic that names what moved. These edits do not
/// depend on how the checks are encoded.
fn assert_load_fails(lua: &str, edits: &[(&str, &str)], message: &str) {
    let mut edited = lua.to_string();
    for (from, to) in edits {
        assert_eq!(edited.matches(from).count(), 1, "`{from}` in:\n{lua}");
        edited = edited.replace(from, to);
    }
    let (ok, out) = crate::common::run_luajit_capturing(&edited);
    assert!(
        !ok && out.contains(message),
        "expected `{message}`, got:\n{out}"
    );
}

#[test]
fn a_layout_mutation_fixture_loads_unmodified() {
    let lua = crate::common::emit_module(MUTATION_PROGRAM);
    assert_eq!(crate::common::run_main(&lua), "6");
}

#[test]
fn a_moved_flat_leaf_fails_the_load() {
    let lua = crate::common::emit_module(MUTATION_PROGRAM);
    assert_load_fails(
        &lua,
        &[("  double c;\n  double e;\n", "  double e;\n  double c;\n")],
        "skull layout drift: Outer.c at 24, expected 16",
    );
}

#[test]
fn a_moved_nested_leaf_alias_fails_the_load() {
    let lua = crate::common::emit_module(MUTATION_PROGRAM);
    let (a, b) = ("__rock_leaf_696e6e65722e61", "__rock_leaf_696e6e65722e62");
    assert_load_fails(
        &lua,
        &[(
            &format!("double {a}; int {b};"),
            &format!("int {b}; double {a};"),
        )],
        &format!("skull leaf alias drift: Outer.{a}"),
    );
}

/// The embedding struct composes offsets through the nested type, so a
/// nested type laid out differently fails the embedding struct's own check
/// even when the nested type's check is gone.
#[test]
fn a_reordered_embedded_type_fails_the_load() {
    let lua = crate::common::emit_module(MUTATION_PROGRAM);
    let reorder = (
        "  double a;\n  int b;\n} Inner;",
        "  int b;\n  double a;\n} Inner;",
    );
    assert_load_fails(
        &lua,
        &[reorder],
        "skull layout drift: Inner.a at 8, expected 0",
    );
    let inner_check = layout_check_line(&lua, "Inner");
    assert_load_fails(
        &lua,
        &[reorder, (&format!("{inner_check}\n"), "")],
        "skull layout drift: Outer.inner.a at 8, expected 0",
    );
}

#[test]
fn a_grown_struct_fails_the_load() {
    let lua = crate::common::emit_module(MUTATION_PROGRAM);
    assert_load_fails(
        &lua,
        &[("  int d;\n} Outer;", "  int d;\n  double pad;\n} Outer;")],
        "skull layout drift: sizeof(Outer) = 48, expected 40",
    );
}
