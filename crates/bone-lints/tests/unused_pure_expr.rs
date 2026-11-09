//! unused-pure-expr lint over bone HIR. Harness mirrors unused_var.rs:
//! `semantic_pass(&mut [parse(src)])` -> `Workspace`.

use bone_lints::{UnusedPureExpr, filter_suppressed, unused_pure_exprs};
use bone_semantic::prelude::*;

fn parse(src: &str) -> skull_parser::Ast {
    skull_parser::parse_module_clean(src, Ustr::from("<unused_pure_expr>")).expect("parse")
}

fn findings(src: &str) -> Vec<UnusedPureExpr> {
    let ws = semantic_pass(&mut [parse(src)]).expect("clean program typechecks");
    unused_pure_exprs(&ws)
}

fn count(src: &str) -> usize {
    findings(src).len()
}

fn count_after_suppression(src: &str) -> usize {
    filter_suppressed(findings(src), src).len()
}

#[test]
fn flags_a_comparison_statement() {
    // The canonical bug: `==` where `=` was meant, result discarded.
    let src = "fn f() -> int {\n    let x = 1\n    x == 1\n    x\n}";
    let fs = findings(src);
    assert_eq!(fs.len(), 1, "one discarded comparison, got {:?}", fs.len());
    assert!(fs[0].is_comparison, "the `==` root is a comparison");
}

#[test]
fn flags_the_canonical_enum_comparison_bug() {
    // The real shipped bug: `mob.ai_state == MobAiState::Wander` in statement
    // position. `==` lowers to BinOp even for enums; the EnumVariant RHS is a
    // pure constructor, so the whole expr is side-effect-free.
    let src = "enum S {\n    Wander,\n    Flee,\n}\n\
               struct Mob {\n    ai_state: S,\n}\n\
               fn tick(mob: Mob) -> int {\n    mob.ai_state == S::Wander\n    5\n}";
    let fs = findings(src);
    assert_eq!(
        fs.len(),
        1,
        "the enum comparison is flagged, got {:?}",
        fs.len()
    );
    assert!(fs[0].is_comparison);
}

#[test]
fn discarded_pure_expressions_are_counted() {
    let cases: &[(&str, &str, usize)] = &[
        // `x == 1` is the block tail (VALUE position) -- the fn returns it.
        (
            "comparison_as_block_tail_is_not_flagged",
            "fn f() -> bool {\n    let x = 1\n    x == 1\n}",
            0,
        ),
        // The critical trap: the branch block's tail is a value position, NOT a
        // Statement::Expr -- even though the enclosing `if` is in statement position
        // and the value is ultimately discarded. Conservative v1 does not flag it.
        (
            "comparison_in_statement_if_branch_tail_is_not_flagged",
            "fn f() {\n    let x = 1\n    if x == 1 {\n        x == 2\n    }\n}",
            0,
        ),
        // bare literal `5` is a discarded pure expr
        ("flags_a_literal", "fn f() -> int {\n    5\n    3\n}", 1),
        // field access `s.a` is pure and discarded
        (
            "flags_field_access_chain",
            "struct S {\n    a: int,\n}\nfn f(s: S) -> int {\n    s.a\n    3\n}",
            1,
        ),
        // bare tuple literal is pure and discarded
        (
            "flags_a_tuple_literal",
            "fn f() -> int {\n    let x = 1\n    (x, x)\n    x\n}",
            1,
        ),
        // calls may have effects
        (
            "call_is_not_flagged",
            "fn g() {}\nfn f() -> int {\n    g()\n    3\n}",
            0,
        ),
        // assignment is a write, not a pure discard
        (
            "assignment_is_not_flagged",
            "fn f() -> int {\n    let x = 1\n    x = 2\n    x\n}",
            0,
        ),
        (
            "compound_assign_is_not_flagged",
            "fn f() -> int {\n    let x = 0\n    x += 1\n    x\n}",
            0,
        ),
        // index may OOB-error; conservatively skipped
        (
            "index_is_not_flagged",
            "fn f() -> int {\n    let xs = [1, 2, 3]\n    xs[0]\n    3\n}",
            0,
        ),
        // UFCS/method calls may have effects
        (
            "method_call_is_not_flagged",
            "fn f() -> int {\n    let xs = [1, 2, 3]\n    xs.push(4)\n    3\n}",
            0,
        ),
        // `g()` inside the comparison has effects, so the statement is not
        // side-effect-free by construction -- don't flag.
        (
            "comparison_with_call_operand_is_not_flagged",
            "fn g() -> int { 1 }\nfn f() -> int {\n    let x = 1\n    g() == x\n    x\n}",
            0,
        ),
    ];
    for (name, src, expected) in cases {
        assert_eq!(count(src), *expected, "{name}");
    }
}

#[test]
fn flags_bare_variable_read() {
    let src = "fn f() -> int {\n    let x = 1\n    x\n    3\n}";
    let fs = findings(src);
    assert_eq!(fs.len(), 1, "bare `x` read is a discarded pure expr");
    assert!(!fs[0].is_comparison, "a var read is not a comparison");
}

#[test]
fn flags_logical_binop() {
    let src = "fn f() -> int {\n    let x = 1\n    x > 0 && x < 5\n    x\n}";
    let fs = findings(src);
    assert_eq!(fs.len(), 1, "`&&` root is a discarded pure expr");
    assert!(
        !fs[0].is_comparison,
        "a logical `&&` root is generic, not a comparison"
    );
}

#[test]
fn allow_comment_above_suppresses() {
    let src = "fn f() -> int {\n    let x = 1\n    // rock: allow(unused_pure_expr)\n    x == 1\n    x\n}";
    assert_eq!(count(src), 1, "raw finding still exists");
    assert_eq!(count_after_suppression(src), 0, "comment above silences it");
}

#[test]
fn trailing_allow_comment_suppresses() {
    let src =
        "fn f() -> int {\n    let x = 1\n    x == 1 // rock: allow(unused_pure_expr)\n    x\n}";
    assert_eq!(
        count_after_suppression(src),
        0,
        "trailing comment silences it"
    );
}

#[test]
fn allow_of_a_different_lint_does_not_suppress() {
    let src =
        "fn f() -> int {\n    let x = 1\n    // rock: allow(unused_var)\n    x == 1\n    x\n}";
    assert_eq!(
        count_after_suppression(src),
        1,
        "wrong lint name -> still warns"
    );
}
