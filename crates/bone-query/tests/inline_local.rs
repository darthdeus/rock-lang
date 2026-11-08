//! The "inline local" code action: replace every use of a `let` binding with
//! its initializer (parenthesized when the initializer isn't atomic) and delete
//! the `let`. Offered on the binding or any of its uses.

use bone_query::BoneWorkspace;
use lsp_types::{Position, TextEdit};

const PATH: &str = "/tmp/rock_inline_local.rock";

/// Byte offset of an LSP position (ASCII test sources: char == byte).
fn pos_to_off(src: &str, p: Position) -> usize {
    let mut off = 0;
    for (i, line) in src.split_inclusive('\n').enumerate() {
        if i as u32 == p.line {
            return off + p.character as usize;
        }
        off += line.len();
    }
    off + p.character as usize
}

/// Apply inline_local's edits (descending, so earlier offsets stay valid).
fn apply(src: &str, edits: Vec<TextEdit>) -> String {
    let mut spans: Vec<(usize, usize, String)> = edits
        .into_iter()
        .map(|e| {
            (
                pos_to_off(src, e.range.start),
                pos_to_off(src, e.range.end),
                e.new_text,
            )
        })
        .collect();
    spans.sort_by_key(|x| std::cmp::Reverse(x.0));
    let mut s = src.to_string();
    for (start, end, text) in spans {
        s.replace_range(start..end, &text);
    }
    s
}

fn inlined(src: &str, pos: Position) -> String {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let il = bone
        .inline_local(PATH, pos)
        .expect("inline should be offered");
    apply(src, il.edits)
}

#[test]
fn inlines_a_binary_initializer_parenthesized_at_every_use() {
    let src = "fn f() -> int {\n    let x = 1 + 2\n    return x * x\n}\n";
    // cursor on the binding `x` (line 1, char 8).
    let out = inlined(src, Position::new(1, 8));
    assert_eq!(out, "fn f() -> int {\n    return (1 + 2) * (1 + 2)\n}\n");
}

#[test]
fn offered_on_a_use_not_only_the_binding() {
    let src = "fn f() -> int {\n    let x = 1 + 2\n    return x * x\n}\n";
    // cursor on a USE of `x` (line 2, char 11).
    let out = inlined(src, Position::new(2, 11));
    assert_eq!(out, "fn f() -> int {\n    return (1 + 2) * (1 + 2)\n}\n");
}

#[test]
fn atomic_initializer_is_not_parenthesized() {
    let src =
        "fn f(v: int) -> int {\n    let y = g(v)\n    return y + 1\n}\nfn g(a: int) -> int { a }\n";
    let out = inlined(src, Position::new(1, 8));
    assert_eq!(
        out,
        "fn f(v: int) -> int {\n    return g(v) + 1\n}\nfn g(a: int) -> int { a }\n"
    );
}

fn offered(src: &str, pos: Position) -> bool {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    bone.inline_local(PATH, pos).is_some()
}

/// Inlining copies the initializer into every use, so an initializer with
/// side effects would run once per use (or per iteration, or per closure
/// call) instead of once.
#[test]
fn not_offered_for_an_impure_initializer_used_twice() {
    let call =
        "fn f(v: int) -> int {\n    let y = g(v)\n    return y + y\n}\nfn g(a: int) -> int { a }\n";
    assert!(!offered(call, Position::new(1, 8)), "call");
    let method = "fn f() -> int {\n    let y = (1 - 2).abs()\n    return y + y\n}\n";
    assert!(!offered(method, Position::new(1, 8)), "method call");
    let index = "fn f(xs: [int]) -> int {\n    let y = xs[0]\n    return y + y\n}\n";
    assert!(!offered(index, Position::new(1, 8)), "indexing");
    let alloc = "fn f() -> int {\n    let y = [1, 2]\n    return y.len() + y.len()\n}\n";
    assert!(!offered(alloc, Position::new(1, 8)), "allocation");
}

#[test]
fn not_offered_for_an_impure_initializer_with_no_use() {
    let src = "fn f(v: int) {\n    let y = g(v)\n}\nfn g(a: int) -> int { a }\n";
    assert!(!offered(src, Position::new(1, 8)));
}

#[test]
fn not_offered_for_an_impure_initializer_used_once_in_a_loop_or_closure() {
    let looped = "fn f(v: int) {\n    let y = g(v)\n    for i in 0..3 {\n        print(y + i)\n    }\n}\nfn g(a: int) -> int { a }\n";
    assert!(!offered(looped, Position::new(1, 8)), "for body");
    let whiled = "fn f(v: int) {\n    let y = g(v)\n    while v > 0 {\n        print(y)\n    }\n}\nfn g(a: int) -> int { a }\n";
    assert!(!offered(whiled, Position::new(1, 8)), "while body");
    let closure = "fn f(v: int) -> int {\n    let y = g(v)\n    let h = () => y\n    return h()\n}\nfn g(a: int) -> int { a }\n";
    assert!(!offered(closure, Position::new(1, 8)), "closure");
}

/// A loop or closure that also holds the `let` re-runs the initializer
/// anyway, so a single use inside it may still be inlined.
#[test]
fn offered_for_an_impure_initializer_declared_inside_the_same_loop() {
    let src = "fn f(v: int) {\n    for i in 0..3 {\n        let y = g(v)\n        print(y + i)\n    }\n}\nfn g(a: int) -> int { a }\n";
    let out = inlined(src, Position::new(2, 12));
    assert_eq!(
        out,
        "fn f(v: int) {\n    for i in 0..3 {\n        print(g(v) + i)\n    }\n}\nfn g(a: int) -> int { a }\n"
    );
}

#[test]
fn offered_for_a_pure_initializer_used_twice() {
    let src = "struct P {\n    a: int\n}\nfn f(p: P, n: int) -> int {\n    let y = -p.a * n\n    return y + y\n}\n";
    let out = inlined(src, Position::new(4, 8));
    assert_eq!(
        out,
        "struct P {\n    a: int\n}\nfn f(p: P, n: int) -> int {\n    return (-p.a * n) + (-p.a * n)\n}\n"
    );
}

/// A use that is a whole call argument is delimited by the call's `(`/`,`/`)`,
/// so it keeps its own precedence without parens; a use inside an operator
/// still gets them.
#[test]
fn a_use_that_is_a_whole_call_argument_is_not_parenthesized() {
    let src = "fn f() -> int {\n    let x = 1 + 2\n    print(x)\n    g(x, x * 2)\n    return x\n}\nfn g(a: int, b: int) {}\n";
    let out = inlined(src, Position::new(1, 8));
    assert_eq!(
        out,
        "fn f() -> int {\n    print(1 + 2)\n    g(1 + 2, (1 + 2) * 2)\n    return (1 + 2)\n}\nfn g(a: int, b: int) {}\n"
    );
}

/// A method receiver is not delimited: `(1 + 2).abs()` needs its parens.
#[test]
fn a_use_that_is_a_method_receiver_stays_parenthesized() {
    let src = "fn f() -> int {\n    let x = 1 - 2\n    return x.abs()\n}\n";
    let out = inlined(src, Position::new(1, 8));
    assert_eq!(out, "fn f() -> int {\n    return (1 - 2).abs()\n}\n");
}
