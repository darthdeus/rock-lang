//! The "fill struct fields" code action prefers an in-scope binding (local or
//! param) whose NAME matches the field and whose TYPE is assignable: it emits
//! the shorthand `field` instead of a `field: ?` hole. A name-only match (wrong
//! type) or no match falls back to `field: ?`.

use bone_query::BoneWorkspace;
use lsp_types::Position;

const PATH: &str = "/tmp/rock_struct_fill_scope.rock";

fn fill(src: &str, pos: Position) -> String {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    bone.fill_struct_fields(PATH, pos)
        .expect("fill should be offered")
        .new_text
}

#[test]
fn matching_name_and_type_bindings_fill_as_shorthand() {
    // param `x: int` and local `y: int` match both fields exactly.
    let src = "struct P { x: int, y: int }\nfn f(x: int) {\n    let y = 5\n    let p = P {  }\n}\n";
    // cursor inside `P {  }` on line 3.
    let t = fill(src, Position::new(3, 15));
    assert!(
        !t.contains('?'),
        "both fields should be shorthand, no holes: {t:?}"
    );
    assert!(t.contains('x') && t.contains('y'), "names present: {t:?}");
}

#[test]
fn name_match_wrong_type_falls_back_to_hole() {
    // param `x` exists but is a string; field `x` is int -> not shorthand.
    let src = "struct P { x: int }\nfn f(x: string) {\n    let p = P {  }\n}\n";
    let t = fill(src, Position::new(2, 15));
    assert!(
        t.contains("x: ?"),
        "wrong-type binding must not shorthand: {t:?}"
    );
}

#[test]
fn no_binding_falls_back_to_hole() {
    let src = "struct P { x: int }\nfn f() {\n    let p = P {  }\n}\n";
    let t = fill(src, Position::new(2, 15));
    assert!(t.contains("x: ?"), "no binding -> hole: {t:?}");
}

#[test]
fn mixed_shorthands_the_match_and_holes_the_rest() {
    // `x: int` param matches; `y` has no binding.
    let src = "struct P { x: int, y: int }\nfn f(x: int) {\n    let p = P {  }\n}\n";
    let t = fill(src, Position::new(2, 15));
    assert!(!t.contains("x: ?"), "x should be shorthand: {t:?}");
    assert!(t.contains("y: ?"), "y should stay a hole: {t:?}");
}

#[test]
fn fills_a_struct_literal_written_as_a_bare_match_arm_body() {
    // Regression: `=> S { .. }` as a bare arm body used to be a parse error, which
    // masked the missing-field diagnostic and this fill entirely (a struct initializer).
    // With the arm-body parse fixed, the fill offers the missing field `y`.
    let src = "enum K { A, B }\nvalue S { x: float, y: float }\nfn f(k: K) -> S {\n    match k {\n        K::A => S { x: 1.0 }\n        K::B => S { x: 2.0, y: 3.0 }\n    }\n}\n";
    // cursor inside `S { x: 1.0 }` on line 4 (0-indexed).
    let t = fill(src, Position::new(4, 22));
    assert!(
        t.contains('y'),
        "the missing field `y` must be filled: {t:?}"
    );
}

#[test]
fn generic_binding_does_not_shorthand_a_same_named_struct_field() {
    // The param's type is f's generic `T`; the field's is the struct `T`.
    // Both render as `T`, but `P { x }` would not typecheck.
    let src = "struct T { v: int }\nstruct P { x: T }\nfn f<T>(x: T) {\n    let p = P {  }\n}\n";
    let t = fill(src, Position::new(3, 15));
    assert!(
        t.contains("x: ?"),
        "generic binding must not shorthand: {t:?}"
    );
}
