//! String-literal text is not code: completion offers nothing inside it. An
//! interpolation hole IS code, so completion keeps working between the braces
//! -- the split is per-position within one literal, not per-literal.

use bone_query::BoneWorkspace;
use lsp_types::Position;

const PATH: &str = "/tmp/rock_lsp_in_string.rock";

fn labels(src: &str, line: u32, ch: u32) -> Vec<String> {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    bone.completion(PATH, Position::new(line, ch)).into_iter().map(|c| c.label).collect()
}

/// Column of the cursor placed right after the `$0` marker, which is stripped.
fn labels_at(src: &str) -> Vec<String> {
    let off = src.find("$0").expect("marker");
    let line = src[..off].matches('\n').count() as u32;
    let line_start = src[..off].rfind('\n').map_or(0, |i| i + 1);
    let ch = src[line_start..off].chars().count() as u32;
    labels(&src.replace("$0", ""), line, ch)
}

const PRELUDE: &str = "struct Mob { health: int }\nfn helper_thing() {}\n";

fn src(body: &str) -> String {
    format!("{PRELUDE}fn main() {{\n    let mob = Mob {{ health: 1 }}\n{body}\n}}\n")
}

// == literal text: nothing ====================================================

#[test]
fn plain_string_text_offers_nothing() {
    assert_eq!(labels_at(&src("    print(\"some hel$0\")")), Vec::<String>::new());
}

#[test]
fn text_after_a_hole_offers_nothing() {
    assert_eq!(labels_at(&src("    print(\"{mob.health} hel$0\")")), Vec::<String>::new());
}

#[test]
fn text_between_two_holes_offers_nothing() {
    let s = src("    print(\"{mob.health} hel$0 {mob.health}\")");
    assert_eq!(labels_at(&s), Vec::<String>::new());
}

#[test]
fn escaped_braces_are_text_not_a_hole() {
    // `{{` emits a literal `{` -- what follows is text, not an expression.
    assert_eq!(labels_at(&src("    print(\"{{hel$0}}\")")), Vec::<String>::new());
}

#[test]
fn raw_string_offers_nothing() {
    // Raw strings never interpolate, so every position in one is text.
    assert_eq!(labels_at(&src("    print(r\"raw hel$0\")")), Vec::<String>::new());
}

#[test]
fn raw_string_braces_are_text_not_a_hole() {
    assert_eq!(labels_at(&src("    print(r\"raw {hel$0}\")")), Vec::<String>::new());
}

#[test]
fn triple_quoted_text_offers_nothing() {
    assert_eq!(labels_at(&src("    let t = \"\"\"triple hel$0\"\"\"")), Vec::<String>::new());
}

#[test]
fn unterminated_string_text_offers_nothing() {
    // The common keystroke state: the closing quote isn't typed yet.
    assert_eq!(labels_at(&src("    print(\"some hel$0")), Vec::<String>::new());
}

// == interpolation holes: code ================================================

#[test]
fn hole_offers_scope_symbols() {
    let out = labels_at(&src("    print(\"x={hel$0}\")"));
    assert!(out.iter().any(|l| l == "helper_thing"), "expected `helper_thing`, got {out:?}");
}

#[test]
fn hole_offers_member_completion() {
    let out = labels_at(&src("    print(\"hp={mob.hea$0}\")"));
    assert!(out.iter().any(|l| l == "health: int"), "expected `health: int`, got {out:?}");
}

#[test]
fn second_hole_after_literal_text_offers_scope_symbols() {
    let out = labels_at(&src("    print(\"a {mob.health} b {hel$0}\")"));
    assert!(out.iter().any(|l| l == "helper_thing"), "expected `helper_thing`, got {out:?}");
}

#[test]
fn triple_quoted_hole_offers_scope_symbols() {
    let out = labels_at(&src("    let t = \"\"\"t {hel$0} u\"\"\""));
    assert!(out.iter().any(|l| l == "helper_thing"), "expected `helper_thing`, got {out:?}");
}

#[test]
fn unterminated_hole_offers_scope_symbols() {
    let out = labels_at(&src("    print(\"x={hel$0"));
    assert!(out.iter().any(|l| l == "helper_thing"), "expected `helper_thing`, got {out:?}");
}

#[test]
fn nested_string_inside_a_hole_is_text() {
    // A hole may hold its own literal (unescaped -- the lexer tracks nesting);
    // that inner literal's text is still text.
    assert_eq!(labels_at(&src("    print(\"{join(v, \"hel$0\")}\")")), Vec::<String>::new());
}

// == code around strings keeps working ========================================

#[test]
fn code_after_a_closed_string_still_completes() {
    let out = labels_at(&src("    let s = \"txt\"\n    hel$0"));
    assert!(out.iter().any(|l| l == "helper_thing"), "expected `helper_thing`, got {out:?}");
}
