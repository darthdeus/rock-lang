use crate::support::lint_only;
use bone_lints::Finding;

fn findings(src: &str) -> Vec<Finding> {
    lint_only(&[("<unused_pure_expr>", src)], "unused_pure_expr")
}

fn count(src: &str) -> usize {
    findings(src).len()
}

fn is_comparison(finding: &Finding) -> bool {
    finding.error.message.contains("comparison")
}

#[test]
fn flagged_roots_are_classified() {
    let cases: &[(&str, bool)] = &[
        ("fn f() -> int {\n    let x = 1\n    x == 1\n    x\n}", true),
        (
            "enum S {\n    Wander,\n    Flee,\n}\n\
             struct Mob {\n    ai_state: S,\n}\n\
             fn tick(mob: Mob) -> int {\n    mob.ai_state == S::Wander\n    5\n}",
            true,
        ),
        ("fn f() -> int {\n    let x = 1\n    x\n    3\n}", false),
        (
            "fn f() -> int {\n    let x = 1\n    x > 0 && x < 5\n    x\n}",
            false,
        ),
    ];
    for (src, comparison) in cases {
        let fs = findings(src);
        assert_eq!(fs.len(), 1, "{src}: {fs:?}");
        assert_eq!(is_comparison(&fs[0]), *comparison, "{src}");
    }
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
fn macro_diagnostics_share_the_invocation_span() {
    let src = "macro declare() -> int { 1; 2; 0 }\nfn main() { let _a = declare() }";
    let fs = findings(src);
    assert_eq!(fs.len(), 2);
    assert_eq!(fs[0].error.message, fs[1].error.message);
    let start = src.rfind("declare()").unwrap();
    for f in fs {
        assert_eq!(
            f.error.labels[0].span.offset_range(),
            (start, start + "declare()".len())
        );
    }
}
