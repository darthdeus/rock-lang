//! Interpolation-hole spans must land on the hole's real source position. The
//! tokenizer resolves escapes before the parser sees the string, so offsets
//! derived from the token's content drift one byte left per preceding escape.

use crate::fixture::{PATH, pos, workspace};

#[test]
fn holes_resolve_past_escapes_and_non_ascii_text() {
    let lines = [
        // The control: no escape before the hole, so nothing can have shifted.
        "print(\"ab{x} c\")",
        "print(\"a\\nb{x} c\")",
        "print(\"a\\n\\t\\\\b{x} c\")",
        "print(\"a\\\"b{x} c\")",
        "print(\"a{{b}}c{x} d\")",
        "print(\"caf\u{e9} {x} c\")",
        "print(\"a\\nb{x} c {x} d\")",
        // `"""` is three delimiter bytes, not one.
        "print(\"\"\"ab{x} c\"\"\")",
        "print(\"\"\"a\\nb{x} c\"\"\")",
    ];
    for line in lines {
        let src = format!("fn main() {{\n    let x = 5\n    {line}\n}}\n");
        let target = workspace(&src).definition(PATH, pos(&src, "{x}", 1));
        assert_eq!(target.map(|l| l.range.start.line), Some(1), "{line}");
    }
}

/// A hole may carry the enclosing literal's escapes (`"{f(\"a\")}"`). Each
/// escape costs a source byte, so the sub-expression's spans are mapped back
/// rather than laid out linearly.
#[test]
fn hole_with_escaped_quotes_resolves_its_identifiers() {
    let src = "fn pick(sep: string, n: int) -> string { sep }\nfn main() {\n    let count = 2\n    print(\"[{pick(\\\",\\\", count)}]\")\n}\n";
    let bone = workspace(src);
    // (case, cursor, definition line). The callee sits before the escapes, so
    // it is the control; an unmapped span for `count` covers `", cou`, so only
    // the mapped span reaches its last character.
    let cases = [
        ("callee before the escapes", pos(src, "pick(\\", 0), 0),
        ("arg after the escapes", pos(src, "count)", 4), 2),
    ];
    for (case, at, line) in cases {
        let def = bone.definition(PATH, at).map(|l| l.range.start.line);
        assert_eq!(def, Some(line), "{case}");
    }
}
