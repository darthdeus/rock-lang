//! unused-var lint over bone HIR. Harness mirrors bone-incremental's
//! error_recovery tests: `semantic_pass(&mut [parse(src)])` -> `Workspace`.

use bone_lints::{UnusedVar, filter_suppressed, unused_vars};
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
fn unread_bindings_are_flagged() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "flags_an_unused_let",
            "fn f() -> int {\n    let x = 5\n    3\n}",
            "x",
        ),
        // `y` is assigned but never read as a value -> still unused.
        (
            "write_only_local_is_flagged",
            "fn f() -> int {\n    let y = 5\n    y = 6\n    3\n}",
            "y",
        ),
        (
            "unused_for_loop_var_is_flagged",
            "fn f() -> int {\n    for i in 0..3 {\n    }\n    3\n}",
            "i",
        ),
        (
            "destructure_flags_only_the_unused_leg",
            "fn f() -> int {\n    let (a, b) = (1, 2)\n    a\n}",
            "b",
        ),
        // b is never read
        (
            "unused_param_is_flagged",
            "fn f(a: int, b: int) -> int { a }",
            "b",
        ),
        (
            "ordinary_decorator_param_still_requires_a_body_read",
            "deco init tagged(value: int) {}",
            "value",
        ),
        // `n` binds the `Some` payload but is never read -> unused.
        (
            "unused_match_payload_bind_is_flagged",
            "enum Opt {\n    Some(int),\n    None,\n}\n\
               fn f(o: Opt) -> int {\n    \
               match o {\n        Some(n) => 5,\n        None => 0,\n    }\n}",
            "n",
        ),
        // if-let bind never read
        (
            "unused_if_let_bind_is_flagged",
            "fn f(o: int?) -> int {\n    if let x = o {\n        7\n    } else {\n        0\n    }\n}",
            "x",
        ),
    ];
    for (name, src, unused) in cases {
        assert_eq!(names(src), [*unused], "{name}");
    }
}

#[test]
fn used_or_exempt_bindings_are_not_flagged() {
    let cases: &[&str] = &[
        // x is read, must not be flagged
        "fn f() -> int {\n    let x = 5\n    x\n}",
        // `_x` opts out of the warning
        "fn f() -> int {\n    let _x = 5\n    3\n}",
        // `x += 1` reads x, so x is used.
        "fn f() -> int {\n    let x = 0\n    x += 1\n    3\n}",
        // artifact-definition metadata consumes `key`
        r#"
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
"#,
        // lifecycle metadata consumes `stage`
        r#"
@lifecycle(phase="update", owner="process", stage_param="stage")
deco init update(stage: int = 100) {}
"#,
        // a is read
        "fn f(a: int) -> int { a }",
        // `_a` opts out
        "fn f(_a: int, b: int) -> int { b }",
        // The Lua body uses a/b invisibly (no HIR body -> root is None). Flagging
        // these would false-positive on every FFI/inline(lua) shim.
        "inline(lua) fn add(a: int, b: int) -> int = \"\"\"return a + b\"\"\"",
        // extern params are consumed by C
        "extern(lua) fn c_add(a: int, b: int) -> int",
        // `a` is a lambda param (Param-kind but outside the fn's param_bindings).
        // Only the enclosing fn's OWN params are candidates.
        "fn f() -> int {\n    let g = (a: int) => 1\n    g(5)\n}",
        // bone lowers the bare `None` arm to a Let binding; it must NOT be
        // reported as an unused variable.
        "enum Opt {\n    Some(int),\n    None,\n}\n\
               fn f(o: Opt) -> int {\n    \
               match o {\n        Some(n) => n,\n        None => 0,\n    }\n}",
    ];
    for src in cases {
        assert!(names(src).is_empty(), "{src}: {:?}", names(src));
    }
}

#[test]
fn allow_comment_above_suppresses() {
    let src = "fn f() -> int {\n    // rock: allow(unused_var)\n    let x = 5\n    3\n}";
    assert_eq!(
        names(src),
        vec!["x".to_string()],
        "raw finding still exists"
    );
    assert!(
        names_after_suppression(src).is_empty(),
        "comment above silences it"
    );
}

#[test]
fn trailing_allow_comment_suppresses() {
    let src = "fn f() -> int {\n    let x = 5 // rock: allow(unused_var)\n    3\n}";
    assert!(
        names_after_suppression(src).is_empty(),
        "trailing comment silences it"
    );
}

#[test]
fn allow_of_a_different_lint_does_not_suppress() {
    let src = "fn f() -> int {\n    // rock: allow(pure)\n    let x = 5\n    3\n}";
    assert_eq!(
        names_after_suppression(src),
        vec!["x".to_string()],
        "wrong lint name -> still warns"
    );
}
