//! A comment is not code: completion inside `//`, `///`, `#` and `/* */`
//! offers nothing. The `// rock: ...` directive is the one exception, covered
//! by `completion_magic_comment.rs`.

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
fn line_comment_offers_nothing() {
    let src = format!("{HELPER}fn main() {{\n    // plain hel\n}}\n");
    assert_eq!(labels(&src, 2, 16), Vec::<String>::new());
}

#[test]
fn doc_comment_offers_nothing() {
    let src = format!("{HELPER}fn main() {{\n    /// docs hel\n}}\n");
    assert_eq!(labels(&src, 2, 16), Vec::<String>::new());
}

#[test]
fn trailing_comment_offers_nothing() {
    let src = format!("{HELPER}fn main() {{\n    let x = 1 // hel\n}}\n");
    assert_eq!(labels(&src, 2, 20), Vec::<String>::new());
}

#[test]
fn hash_comment_offers_nothing() {
    let src = format!("{HELPER}fn main() {{\n    # hash hel\n}}\n");
    assert_eq!(labels(&src, 2, 14), Vec::<String>::new());
}

#[test]
fn block_comment_offers_nothing() {
    let src = format!("{HELPER}fn main() {{\n    /* block hel */\n}}\n");
    assert_eq!(labels(&src, 2, 16), Vec::<String>::new());
}

#[test]
fn multi_line_block_comment_offers_nothing() {
    let src = format!("{HELPER}fn main() {{\n    /* one\n       hel\n     */\n}}\n");
    assert_eq!(labels(&src, 3, 10), Vec::<String>::new());
}

#[test]
fn code_after_a_string_holding_slashes_still_completes() {
    // `//` inside a string literal does not open a comment -- the completion
    // after it is real code and must still resolve.
    let src = format!("{HELPER}fn main() {{\n    let u = \"http://x\"\n    hel\n}}\n");
    let out = labels(&src, 3, 7);
    assert!(
        out.iter().any(|l| l == "helper_thing"),
        "expected `helper_thing`, got {out:?}"
    );
}

#[test]
fn code_immediately_before_a_comment_still_completes() {
    let src = format!("{HELPER}fn main() {{\n    hel// note\n}}\n");
    let out = labels(&src, 2, 7);
    assert!(
        out.iter().any(|l| l == "helper_thing"),
        "expected `helper_thing`, got {out:?}"
    );
}

#[test]
fn conditional_directive_is_not_a_comment() {
    // `#if` lexes as a directive, not a `#` comment -- code after it completes.
    let src = format!("{HELPER}fn main() {{\n    #if debug\n    hel\n    #end\n}}\n");
    let out = labels(&src, 3, 7);
    assert!(
        out.iter().any(|l| l == "helper_thing"),
        "expected `helper_thing`, got {out:?}"
    );
}

#[test]
fn non_ascii_earlier_in_the_comment_keeps_the_offset_aligned() {
    // LSP characters are UTF-16 units, comment scanning is byte offsets.
    let src = format!("{HELPER}fn main() {{\n    // caf\u{e9} hel\n}}\n");
    assert_eq!(labels(&src, 2, 15), Vec::<String>::new());
}

#[test]
fn code_after_a_closed_block_comment_still_completes() {
    let src = format!("{HELPER}fn main() {{\n    /* note */ hel\n}}\n");
    let out = labels(&src, 2, 18);
    assert!(
        out.iter().any(|l| l == "helper_thing"),
        "expected `helper_thing`, got {out:?}"
    );
}
