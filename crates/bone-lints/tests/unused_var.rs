//! unused-var lint over bone HIR. Harness mirrors bone-incremental's
//! error_recovery tests: `semantic_pass(&mut [parse(src)])` -> `Workspace`.

use bone_lints::{filter_suppressed, unused_vars, UnusedVar};
use bone_semantic::prelude::*;

fn parse(src: &str) -> skull_parser::Ast {
    skull_parser::parse_module(src, Ustr::from("<unused_var>")).expect("parse")
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
