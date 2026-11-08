//! Hover + goto-definition on enum variant *references* (match patterns and
//! construction) and on the enum/variant declarations themselves.
//!
//! The variant name resolves to the variant (with its payload types); the enum
//! qualifier resolves to the enum (its full body). Both patterns and
//! construction go through different code paths (AST-based pattern walk vs the
//! HIR construction nodes), so each case is pinned separately.

use bone_query::BoneWorkspace;
use lsp_types::{HoverContents, Position};

#[cfg(not(windows))]
const PATH: &str = "/tmp/rock_enum_variant_nav.rock";
#[cfg(windows)]
const PATH: &str = r"C:\tmp\rock_enum_variant_nav.rock";

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
    let idx = SRC
        .find(marker)
        .unwrap_or_else(|| panic!("marker not found: {marker:?}"))
        + into;
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
    let loc = bone
        .definition(PATH, p)
        .unwrap_or_else(|| panic!("no definition at {p:?}"));
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
    assert_eq!(
        h, VARIANT_CASTING,
        "construction variant -> variant + payload"
    );
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

#[test]
fn qualified_record_paths_keep_canonical_identity_and_token_spans() {
    use crate::modules_v1::{V1Project, pos_of};
    use lsp_types::Range;

    let project = V1Project::new();
    let main = "\
use shapes::Shape as Figure;
fn main() {
    let a = shapes::Shape::Circle { radius: 1.0 }
    let b = Figure::Circle { radius: 2.0 }
    let c = other::Shape::Circle { side: 3 }
}
";
    let shapes = "pub mod shapes;\npub enum Shape { Circle { radius: float } }\n";
    let other = "pub mod other;\npub enum Shape { Circle { side: int } }\n";
    for (name, src) in [
        ("main.rock", main),
        ("shapes.rock", shapes),
        ("other.rock", other),
    ] {
        std::fs::write(project.dir.path().join(name), src).unwrap();
    }
    let path = project.path("main.rock");
    let mut b = BoneWorkspace::new();
    let diagnostics = b.update(&path, main.to_string());
    assert!(
        diagnostics
            .iter()
            .flat_map(|(_, ds)| ds)
            .all(|d| d.severity != Some(lsp_types::DiagnosticSeverity::ERROR)),
        "valid qualified constructors: {diagnostics:#?}"
    );
    for (prefix, file, src, payload) in [
        ("shapes::Shape::", "shapes.rock", shapes, "radius: float"),
        ("Figure::", "shapes.rock", shapes, "radius: float"),
        ("other::Shape::", "other.rock", other, "side: int"),
    ] {
        let expected = Range::new(pos_of(src, "Circle", 0, 0), pos_of(src, "Circle", 0, 6));
        for i in 0.."Circle".len() {
            let p = pos_of(main, prefix, 0, prefix.len() + i);
            let location = b.definition(&path, p).expect("variant definition");
            assert_eq!(location.uri.to_file_path().unwrap(), project.path(file));
            assert_eq!(location.range, expected);
            let h = b.hover(&path, p).expect("variant hover");
            let HoverContents::Markup(markup) = h.contents else {
                panic!("markup hover")
            };
            assert!(
                markup.value.contains(&format!("::Circle {{ {payload} }}")),
                "{markup:?}"
            );
            assert_eq!(
                h.range,
                Some(Range::new(
                    pos_of(main, prefix, 0, prefix.len()),
                    pos_of(main, prefix, 0, prefix.len() + 6),
                ))
            );
        }
        let qualifier = prefix.trim_end_matches("::");
        let into = qualifier.rfind("::").map_or(0, |i| i + 2);
        let p = pos_of(main, prefix, 0, into);
        let location = b.definition(&path, p).expect("enum definition");
        assert_eq!(location.uri.to_file_path().unwrap(), project.path(file));
        assert_eq!(location.range.start, pos_of(src, "enum Shape", 0, 5));
        let h = b.hover(&path, p).expect("enum hover");
        let HoverContents::Markup(markup) = h.contents else {
            panic!("markup hover")
        };
        assert!(markup.value.contains("enum "), "{markup:?}");
    }
}

mod record {
    use super::*;
    use lsp_types::Range;

    const SRC: &str = "\
enum Shape {
    Circle { radius: float },
    Rect { width: float },
    Point { radius: int },
}
struct Point { radius: float }
fn f() {
    let bare = Circle { radius: 1.0 }
    let qualified = Shape::Circle { radius: 2.0 }
    let point = Point { radius: 3.0 }
}
";

    fn fresh() -> BoneWorkspace {
        let mut bone = BoneWorkspace::new();
        bone.update(PATH, SRC.to_string());
        bone
    }

    fn pos(marker: &str, into: usize) -> Position {
        let idx = SRC.find(marker).expect(marker) + into;
        let before = &SRC[..idx];
        Position::new(
            before.matches('\n').count() as u32,
            (idx - before.rfind('\n').map_or(0, |i| i + 1)) as u32,
        )
    }

    fn token(marker: &str, into: usize, name: &str) -> Range {
        Range::new(pos(marker, into), pos(marker, into + name.len()))
    }

    fn assert_definition(b: &BoneWorkspace, marker: &str, into: usize, name: &str, target: Range) {
        for i in 0..name.len() {
            let p = pos(marker, into + i);
            let loc = b
                .definition(PATH, p)
                .unwrap_or_else(|| panic!("no definition at {p:?}"));
            assert_eq!(loc.uri.to_file_path().unwrap(), PATH);
            assert_eq!(loc.range, target, "definition at {p:?}");
        }
    }

    fn assert_hover(b: &BoneWorkspace, marker: &str, into: usize, name: &str, expected: &str) {
        for i in 0..name.len() {
            let p = pos(marker, into + i);
            assert_eq!(hover(b, p), expected, "hover at {p:?}");
            assert_eq!(
                b.hover(PATH, p).unwrap().range,
                Some(token(marker, into, name))
            );
        }
    }

    #[test]
    fn bare_record_head_definition_is_the_variant() {
        let b = fresh();
        assert_definition(
            &b,
            "Circle { radius: 1",
            0,
            "Circle",
            token("Circle { radius: float", 0, "Circle"),
        );
    }

    #[test]
    fn qualified_record_head_definition_is_the_variant() {
        let b = fresh();
        assert_definition(
            &b,
            "Shape::Circle",
            7,
            "Circle",
            token("Circle { radius: float", 0, "Circle"),
        );
    }

    #[test]
    fn bare_record_head_hover_shows_variant_payload() {
        let b = fresh();
        assert_hover(
            &b,
            "Circle { radius: 1",
            0,
            "Circle",
            "```rock\nShape::Circle { radius: float }\n```",
        );
    }

    #[test]
    fn qualified_record_head_hover_shows_variant_payload() {
        let b = fresh();
        assert_hover(
            &b,
            "Shape::Circle",
            7,
            "Circle",
            "```rock\nShape::Circle { radius: float }\n```",
        );
    }

    #[test]
    fn record_variant_declaration_hover_shows_variant_payload() {
        let b = fresh();
        assert_hover(
            &b,
            "Circle { radius: float",
            0,
            "Circle",
            "```rock\nShape::Circle { radius: float }\n```",
        );
    }

    #[test]
    fn record_qualifier_keeps_enum_target_and_hover() {
        let b = fresh();
        assert_definition(
            &b,
            "Shape::Circle",
            0,
            "Shape",
            token("enum Shape", 5, "Shape"),
        );
        assert_hover(
            &b,
            "Shape::Circle",
            0,
            "Shape",
            &hover(&b, pos("enum Shape", 5)),
        );
    }

    #[test]
    fn record_payload_fields_keep_field_targets_and_hovers() {
        let b = fresh();
        for marker in ["Circle { radius: 1", "Shape::Circle { radius: 2"] {
            let into = marker.find("radius").unwrap();
            assert_definition(
                &b,
                marker,
                into,
                "radius",
                token("Circle { radius: float", 9, "radius"),
            );
            assert_hover(
                &b,
                marker,
                into,
                "radius",
                "radius: float (field of Shape::Circle)",
            );
        }
    }

    #[test]
    fn ordinary_struct_heads_and_fields_keep_struct_targets() {
        let b = fresh();
        assert_definition(
            &b,
            "Point { radius: 3",
            0,
            "Point",
            token("struct Point", 7, "Point"),
        );
        assert_eq!(
            hover(&b, pos("Point { radius: 3", 0)),
            hover(&b, pos("struct Point", 7))
        );
        assert_definition(
            &b,
            "Point { radius: 3",
            8,
            "radius",
            token("Point { radius: float", 8, "radius"),
        );
        assert_hover(
            &b,
            "Point { radius: 3",
            8,
            "radius",
            "radius: float (field of Point)",
        );
    }
}
