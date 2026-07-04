//! Typed-hole reports on the bone facade (`BoneWorkspace::hole_info`).
//!
//! Type holes (`let x: ? = expr`) report their inferred type; expression holes
//! (`let x: int = ?`, `f(?)`) report the type coerced from the surrounding
//! context. Both list the bindings in scope.

use bone_query::BoneWorkspace;
use lsp_types::Position;

const PATH: &str = "/tmp/rock_lsp_bone_hole_info_main.rock";

fn fresh(src: &str) -> BoneWorkspace {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    bone
}

#[test]
fn type_hole_reports_inferred_type_and_scope() {
    // line 0: fn main() {
    // line 1:   let count = 5
    // line 2:   let x: ? = count + 1   <- `?` at char 9, infers to int
    let src = "fn main() {\n  let count = 5\n  let x: ? = count + 1\n}\n";
    let bone = fresh(src);

    let report = bone
        .hole_info(PATH, Position::new(2, 9))
        .expect("cursor on the `?` type hole should produce a report");

    assert!(
        report.starts_with("Type hole: int"),
        "expected the hole to infer to int, got: {report:?}"
    );
    assert!(
        report.contains("count: int"),
        "scope dump should list the in-scope `count: int`, got: {report:?}"
    );
}

#[test]
fn expression_hole_reports_type_coerced_from_annotation() {
    // line 2:   let x: int = ?   <- `?` (an expr hole) at char 15, coerced to int
    let src = "fn main() {\n  let a = 1\n  let x: int = ?\n}\n";
    let bone = fresh(src);

    let report = bone
        .hole_info(PATH, Position::new(2, 15))
        .expect("cursor on the `?` expression hole should produce a report");

    assert!(
        report.starts_with("Expression hole expecting: int"),
        "expr hole should take the annotation's int, got: {report:?}"
    );
    assert!(
        report.contains("a: int"),
        "scope dump should list the in-scope `a: int`, got: {report:?}"
    );
}

#[test]
fn expression_hole_reports_type_coerced_from_call_arg() {
    // The `?` argument to `takes(n: int)` coerces to the parameter type.
    let src = "fn takes(n: int) {}\nfn main() {\n  takes(?)\n}\n";
    let bone = fresh(src);
    // line 2:   takes(?)  -> `?` at char 8
    let report = bone
        .hole_info(PATH, Position::new(2, 8))
        .expect("cursor on the `?` call-arg hole should produce a report");
    assert!(
        report.starts_with("Expression hole expecting: int"),
        "call-arg hole should take the param's int, got: {report:?}"
    );
}

#[test]
fn none_when_cursor_is_not_on_a_hole() {
    let src = "fn main() {\n  let count = 5\n}\n";
    let bone = fresh(src);
    // Cursor on `count` (line 1, char 6) -- an ordinary binding, not a hole.
    assert!(bone.hole_info(PATH, Position::new(1, 6)).is_none());
}
