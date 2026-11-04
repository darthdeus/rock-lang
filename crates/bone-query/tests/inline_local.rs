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
        "fn f(v: int) -> int {\n    let y = g(v)\n    return y + y\n}\nfn g(a: int) -> int { a }\n";
    let out = inlined(src, Position::new(1, 8));
    assert_eq!(
        out,
        "fn f(v: int) -> int {\n    return g(v) + g(v)\n}\nfn g(a: int) -> int { a }\n"
    );
}
