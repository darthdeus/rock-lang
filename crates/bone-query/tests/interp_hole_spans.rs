//! The tokenizer resolves escapes before parsing, so hole spans must map back to source.

use crate::fixture::{PATH, pos, workspace};

/// Escapes inside a hole cost source bytes, so its sub-expression spans are mapped back.
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

#[test]
fn dynamic_format_width_after_an_escape_keeps_its_source_span() {
    let src = "fn main() {\n    let digits = 4\n    print(\"{12:\\u{0023}{digits}x}\")\n}\n";
    let bone = workspace(src);
    let found = bone
        .definition(PATH, pos(src, "digits}", 5))
        .expect("width expression reaches its binding");
    assert_eq!(found.range.start, pos(src, "digits =", 0));
}
