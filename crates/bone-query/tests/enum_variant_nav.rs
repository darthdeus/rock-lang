//! Hover + goto-definition on enum variant *references* (match patterns and
//! construction) and on the enum/variant declarations themselves.
//!
//! The variant name resolves to the variant (with its payload types); the enum
//! qualifier resolves to the enum (its full body). Both patterns and
//! construction go through different code paths (AST-based pattern walk vs the
//! HIR `Expr::EnumVariant` node), so each case is pinned separately.

use bone_query::BoneWorkspace;
use lsp_types::{HoverContents, Position};

const PATH: &str = "/tmp/rock_enum_variant_nav.rock";

const SRC: &str = "\
enum SpellState {
    Idle,
    Casting(SpellKind, float, float),
    Loaded(SpellKind),
}

enum SpellKind {
    Fire,
    Ice,
}

fn f(s: SpellState) {
    match s {
        SpellState::Idle => {}
        SpellState::Casting(a, b, c) => {}
        SpellState::Loaded(k) => {}
    }
    let x = SpellState::Casting(SpellKind::Fire, 1.0, 2.0)
}
";

const ENUM_BODY: &str = "```rock\n\
enum SpellState {\n\
\x20 Idle,\n\
\x20 Casting(SpellKind, float, float),\n\
\x20 Loaded(SpellKind),\n\
}\n\
```";

const VARIANT_CASTING: &str = "```rock\nSpellState::Casting(SpellKind, float, float)\n```";

fn fresh() -> BoneWorkspace {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, SRC.to_string());
    bone
}

/// Position of `SRC[find(marker) + into]` (0-based line/char).
fn pos(marker: &str, into: usize) -> Position {
    let idx = SRC.find(marker).unwrap_or_else(|| panic!("marker not found: {marker:?}")) + into;
    let before = &SRC[..idx];
    let line = before.matches('\n').count() as u32;
    let col = (idx - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    Position::new(line, col)
}

fn hover(bone: &BoneWorkspace, p: Position) -> String {
    match bone.hover(PATH, p).map(|h| h.contents) {
        Some(HoverContents::Markup(m)) => m.value,
        other => panic!("expected markup hover at {p:?}, got {other:?}"),
    }
}

fn def(bone: &BoneWorkspace, p: Position) -> (u32, u32) {
    let loc = bone.definition(PATH, p).unwrap_or_else(|| panic!("no definition at {p:?}"));
    (loc.range.start.line, loc.range.start.character)
}

// == Match-pattern references ================================================

#[test]
fn hover_pattern_variant_shows_the_variant_with_payload() {
    let b = fresh();
    // `SpellState::Casting(a, b, c)` pattern -- cursor on `Casting` (col 12).
    let h = hover(&b, pos("SpellState::Casting(a", 12));
    assert_eq!(h, VARIANT_CASTING, "pattern variant -> variant + payload");
}

#[test]
fn hover_pattern_enum_qualifier_shows_the_enum_body() {
    let b = fresh();
    // Same pattern -- cursor on the `SpellState` qualifier (col 0).
    let h = hover(&b, pos("SpellState::Casting(a", 0));
    assert_eq!(h, ENUM_BODY, "pattern qualifier -> full enum body");
}

#[test]
fn def_pattern_variant_jumps_to_the_variant_decl() {
    let b = fresh();
    // `Casting(SpellKind, float, float)` decl is on line 2, char 4.
    assert_eq!(def(&b, pos("SpellState::Casting(a", 12)), (2, 4));
}

#[test]
fn def_pattern_enum_qualifier_jumps_to_the_enum_decl() {
    let b = fresh();
    // `enum SpellState` name is on line 0, char 5.
    assert_eq!(def(&b, pos("SpellState::Casting(a", 0)), (0, 5));
}

// == Construction references =================================================

#[test]
fn hover_construction_variant_shows_the_variant_with_payload() {
    let b = fresh();
    let h = hover(&b, pos("SpellState::Casting(SpellKind", 12));
    assert_eq!(h, VARIANT_CASTING, "construction variant -> variant + payload");
}

#[test]
fn hover_construction_enum_qualifier_shows_the_enum_body() {
    let b = fresh();
    let h = hover(&b, pos("SpellState::Casting(SpellKind", 0));
    assert_eq!(h, ENUM_BODY, "construction qualifier -> full enum body");
}

#[test]
fn def_construction_variant_jumps_to_the_variant_decl() {
    let b = fresh();
    assert_eq!(def(&b, pos("SpellState::Casting(SpellKind", 12)), (2, 4));
}

#[test]
fn def_construction_enum_qualifier_jumps_to_the_enum_decl() {
    let b = fresh();
    assert_eq!(def(&b, pos("SpellState::Casting(SpellKind", 0)), (0, 5));
}

// == Declarations ============================================================

#[test]
fn hover_enum_declaration_shows_the_full_body() {
    let b = fresh();
    // `enum SpellState {` name on line 0, char 5.
    let h = hover(&b, pos("enum SpellState {", 5));
    assert_eq!(h, ENUM_BODY, "enum decl name -> full body with payloads");
}

#[test]
fn hover_variant_declaration_shows_the_variant() {
    let b = fresh();
    // `    Casting(SpellKind, float, float),` variant name on line 2.
    let h = hover(&b, pos("Casting(SpellKind, float", 0));
    assert_eq!(h, VARIANT_CASTING, "variant decl name -> variant + payload");
}
