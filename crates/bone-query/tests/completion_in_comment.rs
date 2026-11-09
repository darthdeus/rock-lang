//! A comment is not code: completion inside `//`, `///`, `#` and `/* */`
//! offers nothing. The `// rock: ...` directive is the one exception, covered
//! by `completion_magic_comment.rs`.

use crate::label_asserts::assert_offers;
use bone_query::BoneWorkspace;
use lsp_types::Position;

const PATH: &str = "/tmp/rock_lsp_in_comment.rock";

fn labels(src: &str, line: u32, ch: u32) -> Vec<String> {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    bone.completion(PATH, Position::new(line, ch))
        .into_iter()
        .map(|c| c.label)
        .collect()
}

const HELPER: &str = "fn helper_thing() {}\n";

#[test]
fn comment_text_offers_nothing() {
    let cases: &[(String, u32, u32)] = &[
        (
            format!("{HELPER}fn main() {{\n    // plain hel\n}}\n"),
            2,
            16,
        ),
        (
            format!("{HELPER}fn main() {{\n    /// docs hel\n}}\n"),
            2,
            16,
        ),
        (
            format!("{HELPER}fn main() {{\n    let x = 1 // hel\n}}\n"),
            2,
            20,
        ),
        (format!("{HELPER}fn main() {{\n    # hash hel\n}}\n"), 2, 14),
        (
            format!("{HELPER}fn main() {{\n    /* block hel */\n}}\n"),
            2,
            16,
        ),
        (
            format!("{HELPER}fn main() {{\n    /* one\n       hel\n     */\n}}\n"),
            3,
            10,
        ),
        // LSP characters are UTF-16 units, comment scanning is byte offsets.
        (
            format!("{HELPER}fn main() {{\n    // caf\u{e9} hel\n}}\n"),
            2,
            15,
        ),
    ];
    for (src, line, ch) in cases {
        assert_eq!(labels(src, *line, *ch), Vec::<String>::new(), "{src}");
    }
}

#[test]
fn code_outside_comments_still_completes() {
    let cases: &[(String, u32, u32, &str)] = &[
        // `//` inside a string literal does not open a comment -- the completion
        // after it is real code and must still resolve.
        (
            format!("{HELPER}fn main() {{\n    let u = \"http://x\"\n    hel\n}}\n"),
            3,
            7,
            "helper_thing",
        ),
        (
            format!("{HELPER}fn main() {{\n    hel// note\n}}\n"),
            2,
            7,
            "helper_thing",
        ),
        // `#if` lexes as a directive, not a `#` comment -- code after it completes.
        (
            format!("{HELPER}fn main() {{\n    #if debug\n    hel\n    #end\n}}\n"),
            3,
            7,
            "helper_thing",
        ),
        (
            format!("{HELPER}fn main() {{\n    /* note */ hel\n}}\n"),
            2,
            18,
            "helper_thing",
        ),
    ];
    for (src, line, ch, label) in cases {
        assert_offers(&labels(src, *line, *ch), &[*label]);
    }
}
