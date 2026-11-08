//! Interpolation-hole spans must land on the hole's real source position.
//!
//! The tokenizer resolves `\n`/`\t`/`\"` escapes before the parser sees the
//! string, so anything deriving hole offsets from the token's CONTENT drifts one
//! byte left per preceding escape. Every position-shaped feature reads those
//! spans, so this pins goto-definition, hover and semantic tokens on holes that
//! sit behind escapes, doubled braces and non-ASCII text.

use bone_query::BoneWorkspace;
use lsp_types::Position;

#[cfg(not(windows))]
const PATH: &str = "/tmp/rock_interp_hole_spans.rock";
#[cfg(windows)]
const PATH: &str = r"C:\tmp\rock_interp_hole_spans.rock";

/// Goto-definition at the `x` of the (single) hole in `line`, which is placed in
/// a `fn` after a `let x`. Returns the target line, or `None` for no target.
fn def_line_at_hole(line: &str) -> Option<u32> {
    let src = format!("fn main() {{\n    let x = 5\n    {line}\n}}\n");
    // LSP columns are characters, `find` gives bytes; +1 steps over the `{`.
    let text = src.lines().nth(2).unwrap();
    let col = text[..text.find("{x}").expect("hole")].chars().count() + 1;
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.clone());
    bone.definition(PATH, Position::new(2, col as u32))
        .map(|l| l.range.start.line)
}

/// The control: no escape before the hole, so nothing can have shifted.
#[test]
fn plain_hole_resolves() {
    assert_eq!(def_line_at_hole("print(\"ab{x} c\")"), Some(1));
}

#[test]
fn hole_after_a_newline_escape_resolves() {
    assert_eq!(def_line_at_hole("print(\"a\\nb{x} c\")"), Some(1));
}

#[test]
fn hole_after_several_escapes_resolves() {
    assert_eq!(def_line_at_hole("print(\"a\\n\\t\\\\b{x} c\")"), Some(1));
}

#[test]
fn hole_after_an_escaped_quote_resolves() {
    assert_eq!(def_line_at_hole("print(\"a\\\"b{x} c\")"), Some(1));
}

#[test]
fn hole_after_doubled_braces_resolves() {
    assert_eq!(def_line_at_hole("print(\"a{{b}}c{x} d\")"), Some(1));
}

#[test]
fn hole_after_non_ascii_text_resolves() {
    assert_eq!(def_line_at_hole("print(\"caf\u{e9} {x} c\")"), Some(1));
}

#[test]
fn second_hole_after_an_escape_resolves() {
    assert_eq!(def_line_at_hole("print(\"a\\nb{x} c {x} d\")"), Some(1));
}

#[test]
fn plain_hole_in_a_triple_quoted_string_resolves() {
    // `"""` is three delimiter bytes, not one.
    assert_eq!(def_line_at_hole("print(\"\"\"ab{x} c\"\"\")"), Some(1));
}

#[test]
fn hole_in_a_triple_quoted_string_resolves() {
    assert_eq!(def_line_at_hole("print(\"\"\"a\\nb{x} c\"\"\")"), Some(1));
}

/// A hole may carry the enclosing literal's escapes (`"{f(\"a\")}"`, real corpus
/// shape). Each escape costs a source byte, so the sub-expression's spans are
/// mapped back rather than laid out linearly.
#[test]
fn hole_with_escaped_quotes_resolves_its_identifiers() {
    let src = "fn pick(sep: string, n: int) -> string { sep }\n\
               fn main() {\n\
               \x20   let count = 2\n\
               \x20   print(\"[{pick(\\\",\\\", count)}]\")\n\
               }\n";
    let line = src.lines().nth(3).unwrap();
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    // The callee sits before the escapes -- it resolves either way, so it is
    // the control.
    let col = line[..line.find("pick(").unwrap()].chars().count();
    let d = bone.definition(PATH, Position::new(3, col as u32));
    assert_eq!(
        d.map(|l| l.range.start.line),
        Some(0),
        "callee before the escapes"
    );
    // `count` sits after two escaped bytes, so an unmapped span covers
    // `", cou` instead -- probe its LAST character, which only the mapped span
    // reaches.
    let col = line[..line.find("count)").unwrap()].chars().count() + 4;
    let d = bone.definition(PATH, Position::new(3, col as u32));
    assert_eq!(
        d.map(|l| l.range.start.line),
        Some(2),
        "arg after the escapes"
    );
}
