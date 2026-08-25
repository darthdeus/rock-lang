//! unused-var lint over bone HIR. Harness mirrors bone-incremental's
//! error_recovery tests: `semantic_pass(&mut [parse(src)])` -> `Workspace`.

use bone_lints::{filter_suppressed, unused_vars, UnusedVar};
use bone_semantic::prelude::*;

fn parse(src: &str) -> skull_parser::Ast {
    skull_parser::parse_module_clean(src, Ustr::from("<unused_var>")).expect("parse")
}

fn findings(src: &str) -> Vec<UnusedVar> {
    let ws = semantic_pass(&mut [parse(src)]).expect("clean program typechecks");
    unused_vars(&ws)
}

fn names_after_suppression(src: &str) -> Vec<String> {
    let mut ns: Vec<String> = filter_suppressed(findings(src), src)
        .iter()
        .map(|f| f.name.to_string())
        .collect();
    ns.sort();
    ns
}

fn names(src: &str) -> Vec<String> {
    let mut ns: Vec<String> = findings(src).iter().map(|f| f.name.to_string()).collect();
    ns.sort();
    ns
}

#[test]
fn flags_an_unused_let() {
    let src = "fn f() -> int {\n    let x = 5\n    3\n}";
    assert_eq!(names(src), vec!["x".to_string()]);
}

#[test]
fn a_read_let_is_not_flagged() {
    let src = "fn f() -> int {\n    let x = 5\n    x\n}";
    assert!(names(src).is_empty(), "x is read, must not be flagged");
}

#[test]
fn underscore_prefix_is_silenced() {
    let src = "fn f() -> int {\n    let _x = 5\n    3\n}";
    assert!(names(src).is_empty(), "`_x` opts out of the warning");
}

#[test]
fn write_only_local_is_flagged() {
    // `y` is assigned but never read as a value -> still unused.
    let src = "fn f() -> int {\n    let y = 5\n    y = 6\n    3\n}";
    assert_eq!(names(src), vec!["y".to_string()]);
}

#[test]
fn compound_assign_counts_as_read() {
    // `x += 1` reads x, so x is used.
    let src = "fn f() -> int {\n    let x = 0\n    x += 1\n    3\n}";
    assert!(names(src).is_empty(), "compound assign reads the target");
}

#[test]
fn unused_for_loop_var_is_flagged() {
    let src = "fn f() -> int {\n    for i in 0..3 {\n    }\n    3\n}";
    assert_eq!(names(src), vec!["i".to_string()]);
}

#[test]
fn destructure_flags_only_the_unused_leg() {
    let src = "fn f() -> int {\n    let (a, b) = (1, 2)\n    a\n}";
    assert_eq!(names(src), vec!["b".to_string()]);
}

#[test]
fn allow_comment_above_suppresses() {
    let src = "fn f() -> int {\n    // rock: allow(unused_var)\n    let x = 5\n    3\n}";
    assert_eq!(names(src), vec!["x".to_string()], "raw finding still exists");
    assert!(names_after_suppression(src).is_empty(), "comment above silences it");
}

#[test]
fn trailing_allow_comment_suppresses() {
    let src = "fn f() -> int {\n    let x = 5 // rock: allow(unused_var)\n    3\n}";
    assert!(names_after_suppression(src).is_empty(), "trailing comment silences it");
}

#[test]
fn allow_of_a_different_lint_does_not_suppress() {
    let src = "fn f() -> int {\n    // rock: allow(pure)\n    let x = 5\n    3\n}";
    assert_eq!(names_after_suppression(src), vec!["x".to_string()], "wrong lint name -> still warns");
}

#[test]
fn unused_param_is_flagged() {
    let src = "fn f(a: int, b: int) -> int { a }";
    assert_eq!(names(src), vec!["b".to_string()], "b is never read");
}

#[test]
fn artifact_definition_key_param_is_metadata_used() {
    let src = r#"
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
"#;
    assert!(names(src).is_empty(), "artifact-definition metadata consumes `key`");
}

#[test]
fn lifecycle_stage_param_is_metadata_used() {
    let src = r#"
@lifecycle(phase="update", owner="session", stage_param="stage")
deco init update(stage: int = 100) {}
"#;
    assert!(names(src).is_empty(), "lifecycle metadata consumes `stage`");
}

#[test]
fn ordinary_decorator_param_still_requires_a_body_read() {
    let src = "deco init tagged(value: int) {}";
    assert_eq!(names(src), vec!["value".to_string()]);
}

#[test]
fn read_param_is_not_flagged() {
    let src = "fn f(a: int) -> int { a }";
    assert!(names(src).is_empty(), "a is read");
}

#[test]
fn underscore_param_is_silenced() {
    let src = "fn f(_a: int, b: int) -> int { b }";
    assert!(names(src).is_empty(), "`_a` opts out");
}

#[test]
fn inline_lua_fn_params_are_not_flagged() {
    // The Lua body uses a/b invisibly (no HIR body -> root is None). Flagging
    // these would false-positive on every FFI/inline(lua) shim.
    let src = "inline(lua) fn add(a: int, b: int) -> int = \"\"\"return a + b\"\"\"";
    assert!(names(src).is_empty(), "inline(lua) params are used by the Lua body: {:?}", names(src));
}

#[test]
fn extern_fn_params_are_not_flagged() {
    let src = "extern fn c_add(a: int, b: int) -> int";
    assert!(names(src).is_empty(), "extern params are consumed by C");
}

#[test]
fn lambda_param_is_not_flagged() {
    // `a` is a lambda param (Param-kind but outside the fn's param_bindings).
    // Only the enclosing fn's OWN params are candidates.
    let src = "fn f() -> int {\n    let g = (a: int) => 1\n    g(5)\n}";
    assert!(names(src).is_empty(), "lambda params are exempt: {:?}", names(src));
}

#[test]
fn unused_match_payload_bind_is_flagged() {
    // `n` binds the `Some` payload but is never read -> unused.
    let src = "enum Opt {\n    Some(int),\n    None,\n}\n\
               fn f(o: Opt) -> int {\n    \
               match o {\n        Some(n) => 5,\n        None => 0,\n    }\n}";
    assert_eq!(names(src), vec!["n".to_string()]);
}

#[test]
fn read_match_payload_bind_is_not_flagged() {
    let src = "enum Opt {\n    Some(int),\n    None,\n}\n\
               fn f(o: Opt) -> int {\n    \
               match o {\n        Some(n) => n,\n        None => 0,\n    }\n}";
    assert!(names(src).is_empty(), "n is read in the arm body");
}

#[test]
fn unused_if_let_bind_is_flagged() {
    let src = "fn f(o: int?) -> int {\n    if let x = o {\n        7\n    } else {\n        0\n    }\n}";
    assert_eq!(names(src), vec!["x".to_string()], "if-let bind never read");
}

#[test]
fn match_arm_payload_less_variant_is_not_flagged() {
    // bone lowers the bare `None` arm to a Let binding; it must NOT be
    // reported as an unused variable.
    let src = "enum Opt {\n    Some(int),\n    None,\n}\n\
               fn f(o: Opt) -> int {\n    \
               match o {\n        Some(n) => n,\n        None => 0,\n    }\n}";
    assert!(
        names(src).is_empty(),
        "the `None` variant arm must not warn as unused: got {:?}",
        names(src)
    );
}
