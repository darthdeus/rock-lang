//! The "inline local" code action: replace every use of a `let` binding with
//! its initializer (parenthesized when the initializer isn't atomic) and delete
//! the `let`. Offered on the binding or any of its uses.

use crate::fixture::{PATH, apply_edits, workspace};
use lsp_types::Position;

fn inlined(src: &str, pos: Position) -> String {
    let il = workspace(src)
        .inline_local(PATH, pos)
        .expect("inline should be offered");
    apply_edits(src, il.edits)
}

#[test]
fn inlines_at_every_use() {
    let cases: &[(&str, &str, (u32, u32), &str)] = &[
        // cursor on the binding `x` (line 1, char 8).
        (
            "inlines_a_binary_initializer_parenthesized_at_every_use",
            "fn f() -> int {\n    let x = 1 + 2\n    return x * x\n}\n",
            (1, 8),
            "fn f() -> int {\n    return (1 + 2) * (1 + 2)\n}\n",
        ),
        // cursor on a USE of `x` (line 2, char 11).
        (
            "offered_on_a_use_not_only_the_binding",
            "fn f() -> int {\n    let x = 1 + 2\n    return x * x\n}\n",
            (2, 11),
            "fn f() -> int {\n    return (1 + 2) * (1 + 2)\n}\n",
        ),
        (
            "atomic_initializer_is_not_parenthesized",
            "fn f(v: int) -> int {\n    let y = g(v)\n    return y + 1\n}\nfn g(a: int) -> int { a }\n",
            (1, 8),
            "fn f(v: int) -> int {\n    return g(v) + 1\n}\nfn g(a: int) -> int { a }\n",
        ),
        // A loop or closure that also holds the `let` re-runs the initializer
        // anyway, so a single use inside it may still be inlined.
        (
            "offered_for_an_impure_initializer_declared_inside_the_same_loop",
            "fn f(v: int) {\n    for i in 0..3 {\n        let y = g(v)\n        print(y + i)\n    }\n}\nfn g(a: int) -> int { a }\n",
            (2, 12),
            "fn f(v: int) {\n    for i in 0..3 {\n        print(g(v) + i)\n    }\n}\nfn g(a: int) -> int { a }\n",
        ),
        (
            "offered_for_a_pure_initializer_used_twice",
            "struct P {\n    a: int\n}\nfn f(p: P, n: int) -> int {\n    let y = -p.a * n\n    return y + y\n}\n",
            (4, 8),
            "struct P {\n    a: int\n}\nfn f(p: P, n: int) -> int {\n    return (-p.a * n) + (-p.a * n)\n}\n",
        ),
        // A use that is a whole call argument is delimited by the call's `(`/`,`/`)`,
        // so it keeps its own precedence without parens; a use inside an operator
        // still gets them.
        (
            "a_use_that_is_a_whole_call_argument_is_not_parenthesized",
            "fn f() -> int {\n    let x = 1 + 2\n    print(x)\n    g(x, x * 2)\n    return x\n}\nfn g(a: int, b: int) {}\n",
            (1, 8),
            "fn f() -> int {\n    print(1 + 2)\n    g(1 + 2, (1 + 2) * 2)\n    return (1 + 2)\n}\nfn g(a: int, b: int) {}\n",
        ),
        // A method receiver is not delimited: `(1 + 2).abs()` needs its parens.
        (
            "a_use_that_is_a_method_receiver_stays_parenthesized",
            "fn f() -> int {\n    let x = 1 - 2\n    return x.abs()\n}\n",
            (1, 8),
            "fn f() -> int {\n    return (1 - 2).abs()\n}\n",
        ),
    ];
    for (name, src, (line, ch), expected) in cases {
        assert_eq!(inlined(src, Position::new(*line, *ch)), *expected, "{name}");
    }
}

/// Inlining copies the initializer into every use, so an initializer with
/// side effects would run once per use (or per iteration, or per closure
/// call) instead of once, or not at all without a use.
#[test]
fn not_offered_for_an_impure_initializer_that_would_run_a_different_number_of_times() {
    // (case, source with the binding at 1:8)
    let cases = [
        (
            "call used twice",
            "fn f(v: int) -> int {\n    let y = g(v)\n    return y + y\n}\nfn g(a: int) -> int { a }\n",
        ),
        (
            "method call used twice",
            "fn f() -> int {\n    let y = (1 - 2).abs()\n    return y + y\n}\n",
        ),
        (
            "indexing used twice",
            "fn f(xs: [int]) -> int {\n    let y = xs[0]\n    return y + y\n}\n",
        ),
        (
            "allocation used twice",
            "fn f() -> int {\n    let y = [1, 2]\n    return y.len() + y.len()\n}\n",
        ),
        (
            "no use",
            "fn f(v: int) {\n    let y = g(v)\n}\nfn g(a: int) -> int { a }\n",
        ),
        (
            "once in a for body",
            "fn f(v: int) {\n    let y = g(v)\n    for i in 0..3 {\n        print(y + i)\n    }\n}\nfn g(a: int) -> int { a }\n",
        ),
        (
            "once in a while body",
            "fn f(v: int) {\n    let y = g(v)\n    while v > 0 {\n        print(y)\n    }\n}\nfn g(a: int) -> int { a }\n",
        ),
        (
            "once in a closure",
            "fn f(v: int) -> int {\n    let y = g(v)\n    let h = () => y\n    return h()\n}\nfn g(a: int) -> int { a }\n",
        ),
    ];
    for (case, src) in cases {
        let offered = workspace(src)
            .inline_local(PATH, Position::new(1, 8))
            .is_some();
        assert!(!offered, "{case}");
    }
}
