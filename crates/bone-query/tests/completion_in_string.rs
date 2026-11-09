//! String-literal text is not code: completion offers nothing inside it. An
//! interpolation hole IS code, so completion keeps working between the braces
//! -- the split is per-position within one literal, not per-literal.

use crate::label_asserts::assert_offers;
use bone_query::BoneWorkspace;
use lsp_types::Position;

const PATH: &str = "/tmp/rock_lsp_in_string.rock";

fn labels(src: &str, line: u32, ch: u32) -> Vec<String> {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    bone.completion(PATH, Position::new(line, ch))
        .into_iter()
        .map(|c| c.label)
        .collect()
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
fn string_text_offers_nothing() {
    let cases: &[&str] = &[
        // plain string text offers nothing
        "    print(\"some hel$0\")",
        // text after a hole offers nothing
        "    print(\"{mob.health} hel$0\")",
        // text between two holes offers nothing
        "    print(\"{mob.health} hel$0 {mob.health}\")",
        // `{{` emits a literal `{` -- what follows is text, not an expression.
        "    print(\"{{hel$0}}\")",
        // Raw strings never interpolate, so every position in one is text.
        "    print(r\"raw hel$0\")",
        // raw string braces are text not a hole
        "    print(r\"raw {hel$0}\")",
        // triple quoted text offers nothing
        "    let t = \"\"\"triple hel$0\"\"\"",
        // The common keystroke state: the closing quote isn't typed yet.
        "    print(\"some hel$0",
        // A hole may hold its own literal (unescaped -- the lexer tracks nesting);
        // that inner literal's text is still text.
        "    print(\"{join(v, \"hel$0\")}\")",
    ];
    for body in cases {
        assert_eq!(labels_at(&src(body)), Vec::<String>::new(), "{body}");
    }
}

// == interpolation holes: code ================================================

#[test]
fn holes_and_code_outside_strings_still_complete() {
    let cases: &[(&str, &str)] = &[
        ("    print(\"x={hel$0}\")", "helper_thing"),
        ("    print(\"hp={mob.hea$0}\")", "health: int"),
        ("    print(\"a {mob.health} b {hel$0}\")", "helper_thing"),
        ("    let t = \"\"\"t {hel$0} u\"\"\"", "helper_thing"),
        ("    print(\"x={hel$0", "helper_thing"),
        ("    let s = \"txt\"\n    hel$0", "helper_thing"),
    ];
    for (body, label) in cases {
        assert_offers(&labels_at(&src(body)), &[*label]);
    }
}

// == code around strings keeps working ========================================
