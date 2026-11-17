//! Hover and goto-definition on enum variant references and declarations.
//! Patterns (an AST walk) and construction (HIR nodes) take different paths,
//! so each is pinned.

use crate::fixture::{PATH, assert_no_errors, hover_markup, pos, workspace};
use bone_query::BoneWorkspace;
use lsp_types::{HoverContents, Position, Range};

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

#[track_caller]
fn hover(bone: &BoneWorkspace, p: Position) -> String {
    hover_markup(bone, PATH, p).unwrap_or_else(|| panic!("no hover at {p:?}"))
}

fn def(bone: &BoneWorkspace, p: Position) -> (u32, u32) {
    let loc = bone
        .definition(PATH, p)
        .unwrap_or_else(|| panic!("no definition at {p:?}"));
    (loc.range.start.line, loc.range.start.character)
}

#[test]
fn hover_on_variants_and_enum_qualifiers() {
    let cases: &[(&str, usize, &str)] = &[
        // `SpellState::Casting(a, b, c)` pattern -- cursor on `Casting` (col 12).
        ("SpellState::Casting(a", 12, VARIANT_CASTING),
        // Same pattern -- cursor on the `SpellState` qualifier (col 0).
        ("SpellState::Casting(a", 0, ENUM_BODY),
        // construction variant -> variant + payload
        ("SpellState::Casting(SpellKind", 12, VARIANT_CASTING),
        // construction qualifier -> full enum body
        ("SpellState::Casting(SpellKind", 0, ENUM_BODY),
        // `enum SpellState {` name on line 0, char 5.
        ("enum SpellState {", 5, ENUM_BODY),
        // `    Casting(SpellKind, float, float),` variant name on line 2.
        ("Casting(SpellKind, float", 0, VARIANT_CASTING),
    ];
    let b = workspace(SRC);
    for (marker, into, expected) in cases {
        assert_eq!(hover(&b, pos(SRC, marker, *into)), *expected, "{marker}");
    }
}

#[test]
fn definition_on_variants_and_enum_qualifiers() {
    let cases: &[(&str, usize, (u32, u32))] = &[
        // `Casting(SpellKind, float, float)` decl is on line 2, char 4.
        ("SpellState::Casting(a", 12, (2, 4)),
        // `enum SpellState` name is on line 0, char 5.
        ("SpellState::Casting(a", 0, (0, 5)),
        ("SpellState::Casting(SpellKind", 12, (2, 4)),
        ("SpellState::Casting(SpellKind", 0, (0, 5)),
    ];
    let b = workspace(SRC);
    for (marker, into, expected) in cases {
        assert_eq!(def(&b, pos(SRC, marker, *into)), *expected, "{marker}");
    }
}

#[test]
fn qualified_record_paths_keep_canonical_identity_and_token_spans() {
    use crate::modules_v1::{V1Project, pos_of};

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
    assert_no_errors(&diagnostics);
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
            let HoverContents::Markup(markup) = h.contents;
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
        let HoverContents::Markup(markup) = h.contents;
        assert!(markup.value.contains("enum "), "{markup:?}");
    }
}

mod record {
    use super::*;

    const SRC: &str = "\
enum Shape {
    Circle { radius: float },
    Rect { width: float },
    Point { radius: int },
}
struct Point { radius: float }
fn f() {
    let first = Shape::Circle { radius: 1.0 }
    let qualified = Shape::Circle { radius: 2.0 }
    let point = Point { radius: 3.0 }
}
";

    /// The range of `name`, found `into` bytes past `marker`.
    fn token(marker: &str, into: usize, name: &str) -> Range {
        Range::new(pos(SRC, marker, into), pos(SRC, marker, into + name.len()))
    }

    #[test]
    fn every_byte_of_a_record_reference_navigates_to_its_declaration() {
        let circle = token("Circle { radius: float", 0, "Circle");
        let radius = token("Circle { radius: float", 9, "radius");
        // (reference marker, offset of the name in it, name, declaration)
        let cases = [
            ("Circle { radius: 1", 0, "Circle", circle),
            ("Shape::Circle", 7, "Circle", circle),
            ("Shape::Circle", 0, "Shape", token("enum Shape", 5, "Shape")),
            ("Circle { radius: 1", 9, "radius", radius),
            ("Shape::Circle { radius: 2", 16, "radius", radius),
            (
                "Point { radius: 3",
                0,
                "Point",
                token("struct Point", 7, "Point"),
            ),
            (
                "Point { radius: 3",
                8,
                "radius",
                token("Point { radius: float", 8, "radius"),
            ),
        ];
        let b = workspace(SRC);
        for (marker, into, name, target) in cases {
            for i in 0..name.len() {
                let p = pos(SRC, marker, into + i);
                let loc = b
                    .definition(PATH, p)
                    .unwrap_or_else(|| panic!("no definition at {p:?}"));
                assert_eq!(loc.uri.to_file_path().unwrap(), PATH);
                assert_eq!(loc.range, target, "{marker}: definition at {p:?}");
            }
        }
    }

    #[test]
    fn every_byte_of_a_record_reference_hovers_its_token() {
        let b = workspace(SRC);
        let circle = "```rock\nShape::Circle { radius: float }\n```";
        let enum_hover = hover(&b, pos(SRC, "enum Shape", 5));
        let circle_radius = "radius: float (field of Shape::Circle)";
        // (marker, offset of the name in it, name, hover)
        let cases = [
            ("Circle { radius: 1", 0, "Circle", circle),
            ("Shape::Circle", 7, "Circle", circle),
            ("Circle { radius: float", 0, "Circle", circle),
            ("Shape::Circle", 0, "Shape", &enum_hover),
            ("Circle { radius: 1", 9, "radius", circle_radius),
            ("Shape::Circle { radius: 2", 16, "radius", circle_radius),
            (
                "Point { radius: 3",
                8,
                "radius",
                "radius: float (field of Point)",
            ),
        ];
        for (marker, into, name, expected) in cases {
            for i in 0..name.len() {
                let p = pos(SRC, marker, into + i);
                let h = b
                    .hover(PATH, p)
                    .unwrap_or_else(|| panic!("no hover at {p:?}"));
                let HoverContents::Markup(m) = h.contents;
                assert_eq!(m.value, expected, "{marker}: hover at {p:?}");
                assert_eq!(h.range, Some(token(marker, into, name)), "{marker}");
            }
        }
        assert_eq!(
            hover(&b, pos(SRC, "Point { radius: 3", 0)),
            hover(&b, pos(SRC, "struct Point", 7))
        );
    }
}
