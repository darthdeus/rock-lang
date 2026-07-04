//! Token-level hover: keywords and punctuation resolve to the enclosing
//! construct; whitespace resolves to nothing. See
//! `docs/lsp-hover-token-resolution.md`.

use bone_query::BoneWorkspace;
use lsp_types::Position;

const PATH: &str = "/tmp/rock_lsp_bone_hover_tokens.rock";

const SRC: &str = "\
enum Dir { North, South }
struct Vec2 { x: int, y: int }
fn pick(d: Dir) -> int {
    let v = Vec2 { x: 3, y: 5 }
    let r = if d == Dir::North { 1 } else { 2 }
    match d {
        Dir::North => 1
        Dir::South => 2
    }
    return r
}
";

fn fresh() -> BoneWorkspace {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, SRC.to_string());
    bone
}

/// Hover text at (0-based line, 1-based-ish char computed from a substring).
fn at(bone: &BoneWorkspace, line: u32, ch: u32) -> Option<String> {
    bone.hover(PATH, Position::new(line, ch)).map(|h| match h.contents {
        lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup hover, got {other:?}"),
    })
}

/// Char index (0-based) of `sub` on `line` (0-based), + `nth` char into it.
fn col(line: usize, sub: &str, into: usize) -> u32 {
    let l = SRC.lines().nth(line).unwrap();
    (l.find(sub).unwrap_or_else(|| panic!("{sub:?} not on line {line}: {l:?}")) + into) as u32
}

#[test]
fn match_keyword_and_punct_hover_the_match_value() {
    let b = fresh();
    // `match` keyword (line 5) and the `=>` (line 6) both hover the match's
    // value type (() here -- statement position discards the value).
    let kw = at(&b, 5, col(5, "match", 1)).expect("match kw hovers");
    assert!(kw.contains("Type:"), "match kw -> value type: {kw:?}");
    let arrow = at(&b, 6, col(6, "=>", 0)).expect("`=>` hovers");
    assert!(arrow.contains("Type:"), "`=>` -> match value: {arrow:?}");
}

#[test]
fn double_colon_in_pattern_hovers_the_enum() {
    let b = fresh();
    // The `::` between `Dir` and `North` (line 6).
    let c = SRC.lines().nth(6).unwrap().find("::").unwrap() as u32;
    let h = at(&b, 6, c).expect("`::` hovers");
    assert!(h.contains("enum Dir"), "`::` -> enum: {h:?}");
}

#[test]
fn struct_literal_colon_hovers_the_field() {
    let b = fresh();
    // The `:` in `x: 3` (line 3) -> the field, same as hovering `x`.
    let colon = SRC.lines().nth(3).unwrap().find(": 3").unwrap() as u32;
    let h = at(&b, 3, colon).expect("field `:` hovers");
    assert!(h.contains("x: int (field of Vec2)"), "`:` -> field: {h:?}");
}

#[test]
fn fn_signature_tokens_hover_the_signature() {
    let b = fresh();
    // `fn` keyword, an opening param paren, and the `->` arrow (line 2).
    for (sub, into) in [("fn", 0), ("(", 0), ("->", 0)] {
        let h = at(&b, 2, col(2, sub, into)).unwrap_or_else(|| panic!("{sub} hovers"));
        assert!(h.contains("fn pick("), "`{sub}` -> signature: {h:?}");
    }
}

#[test]
fn return_keyword_hovers_the_returned_value() {
    let b = fresh();
    let h = at(&b, 9, col(9, "return", 0)).expect("return kw hovers");
    assert!(h.contains("Type: int"), "return -> value type: {h:?}");
}

#[test]
fn if_else_hover_the_if_value() {
    let b = fresh();
    let if_h = at(&b, 4, col(4, "if", 0)).expect("if hovers");
    assert!(if_h.contains("Type: int"), "if -> value: {if_h:?}");
    let else_h = at(&b, 4, col(4, "else", 0)).expect("else hovers");
    assert!(else_h.contains("Type: int"), "else -> value: {else_h:?}");
}

#[test]
fn body_keywords_do_not_leak_the_fn_signature() {
    // Regression: the fn-signature hover is bounded to the signature region
    // (before the body brace), so `let`/`match`/`return` keywords inside the
    // body never resolve to the enclosing fn's signature.
    let b = fresh();
    for (line, kw) in [(3u32, "let"), (5, "match"), (9, "return")] {
        if let Some(h) = at(&b, line, col(line as usize, kw, 0)) {
            assert!(!h.contains("fn pick("), "`{kw}` leaked the fn signature: {h:?}");
        }
    }
}

#[test]
fn whitespace_hovers_nothing() {
    let b = fresh();
    // Leading indentation of the `match` line (col 0) and a blank column.
    assert!(at(&b, 5, 0).is_none(), "leading whitespace -> no hover");
    assert!(at(&b, 6, 0).is_none(), "blank inside match -> no hover");
}
