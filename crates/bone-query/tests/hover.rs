//! LSP hover token resolution. Two concerns:
//!  - Braces: hovering a real `{` / `}` resolves to the enclosing Block node
//!    (the only lapper entries a Block emits -- see alloc_block_expr in
//!    bone-lower). A silent miss means the block was allocated through plain
//!    `alloc_expr` (no markers) instead of `alloc_block_expr`.
//!  - Tokens: keywords and punctuation resolve to the enclosing construct;
//!    whitespace resolves to nothing. See `docs/lsp-hover-token-resolution.md`.

use bone_query::BoneWorkspace;
use lsp_types::{HoverContents, Position};

const PATH: &str = "/tmp/rock_hover.rock";

// == Brace hover: every braced-block form resolves at both `{` and `}` ========

fn pos_of(src: &str, needle: &str) -> Position {
    let idx = src.find(needle).expect("needle") + needle.len() - 1;
    let before = &src[..idx];
    let line = before.matches('\n').count() as u32;
    let col = (idx - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    Position::new(line, col)
}

fn hover_at(bone: &BoneWorkspace, pos: Position) -> String {
    match bone.hover(PATH, pos).map(|h| h.contents) {
        Some(HoverContents::Markup(m)) => m.value,
        _ => String::new(),
    }
}

#[test]
fn brace_hover_resolves_the_block_in_every_braced_form() {
    // Each block's tail is a MarkerS literal, so the BLOCK hovers "MarkerS"
    // while any enclosing node that might shadow the brace offset hovers
    // something else (or nothing). (name, source, text ending at the open
    // brace, text ending at the close brace).
    let pre = "struct MarkerS { f: int }\n";
    let cases: &[(&str, &str, &str, &str)] = &[
        (
            "fn body",
            "fn f() -> MarkerS {\n  MarkerS { f: 1 }\n}\n",
            "-> MarkerS {",
            "\n}",
        ),
        (
            "block expression",
            "fn f() {\n  let x = {\n    MarkerS { f: 1 }\n  }\n}\n",
            "x = {",
            "}\n  }",
        ),
        (
            "if branch",
            "fn f() {\n  let x = if true {\n    MarkerS { f: 1 }\n  } else {\n    MarkerS { f: 2 }\n  }\n}\n",
            "if true {",
            "f: 1 }\n  }",
        ),
        (
            "if-let then",
            "fn f(a: int?) {\n  if let v = a {\n    MarkerS { f: v }\n  }\n}\n",
            "a {",
            "}\n  }",
        ),
        (
            "if-let else",
            "fn f(a: int?) {\n  if let v = a {\n    MarkerS { f: v }\n  } else {\n    MarkerS { f: 0 }\n  }\n}\n",
            "else {",
            "f: 0 }\n  }",
        ),
        (
            "lifecycle hook body",
            "fn f() {\n  on_killed {\n    MarkerS { f: 1 }\n  }\n}\n",
            "on_killed {",
            "}\n  }",
        ),
        (
            "decorator body",
            "deco init d {\n  MarkerS { f: 1 }\n}\nfn f() {}\n",
            "deco init d {",
            "\n}",
        ),
    ];

    let mut fails = Vec::new();
    for (name, src, open, close) in cases {
        let full = format!("{pre}{src}");
        let mut bone = BoneWorkspace::new();
        bone.update(PATH, full.to_string());
        let o = hover_at(&bone, pos_of(&full, open));
        let c = hover_at(&bone, pos_of(&full, close));
        if !o.contains("MarkerS") || !c.contains("MarkerS") || o != c {
            fails.push(format!("  [{name}] open={o:?} close={c:?}"));
        }
    }
    assert!(
        fails.is_empty(),
        "braces not resolving their block (allocated via alloc_expr instead of alloc_block_expr?):\n{}",
        fails.join("\n")
    );
}

// == Token hover: keywords/punct resolve to their construct; whitespace to none

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
        HoverContents::Markup(m) => m.value,
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
