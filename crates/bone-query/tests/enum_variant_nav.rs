//! Hover and goto-definition on enum variants: patterns and construction take different paths.

use crate::fixture::{PATH, assert_no_errors, hover_markup, pos, workspace};
use bone_query::BoneWorkspace;
use bone_query::FileUriExt;
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

#[test]
fn hover_and_definition_on_variants_and_enum_qualifiers() {
    let (variant, qualifier) = (Some((2, 4)), Some((0, 5)));
    let cases: &[(&str, usize, &str, Option<(u32, u32)>)] = &[
        // (marker, offset into it, hover, definition start); `(a` is the
        // pattern, `(SpellKind` the construction
        ("SpellState::Casting(a", 12, VARIANT_CASTING, variant),
        ("SpellState::Casting(a", 0, ENUM_BODY, qualifier),
        (
            "SpellState::Casting(SpellKind",
            12,
            VARIANT_CASTING,
            variant,
        ),
        ("SpellState::Casting(SpellKind", 0, ENUM_BODY, qualifier),
        ("enum SpellState {", 5, ENUM_BODY, None),
        ("Casting(SpellKind, float", 0, VARIANT_CASTING, None),
    ];
    let b = workspace(SRC);
    for (marker, into, expected, def) in cases {
        let at = pos(SRC, marker, *into);
        assert_eq!(hover(&b, at), *expected, "{marker}");
        if let Some((line, ch)) = def {
            let loc = b.definition(PATH, at).expect("definition");
            assert_eq!(loc.range.start, Position::new(*line, *ch), "{marker}");
        }
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
            let HoverContents::Markup(markup) = h.contents else {
                panic!("expected markup hover")
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
            panic!("expected markup hover")
        };
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

    #[test]
    fn explicit_record_pattern_names_follow_the_scrutinee_type() {
        use crate::fixture::apply_edits;

        let src = "\
struct Point { x: int }
enum Shape { Point { x: float } }
fn main() {
    let x = 3
    let point = Point { x }
    let shape = Shape::Point { x: 1.0 }
    match point {
        Point { x: count } => print(count)
    }
    match shape {
        Point { x: size } => print(size)
    }
}
";
        let mut b = BoneWorkspace::new();
        assert_no_errors(&b.update(PATH, src.to_owned()));
        let field = |marker, into| Range::new(pos(src, marker, into), pos(src, marker, into + 1));
        for (decl, literal, pattern, doc, replacements) in [
            (
                "x: int",
                field("Point { x }", 8),
                "x: count",
                "x: int (field of Point)",
                [
                    ("x: int", "coord: int"),
                    ("Point { x }", "Point { coord: x }"),
                    ("x: count", "coord: count"),
                ],
            ),
            (
                "x: float",
                field("x: 1.0", 0),
                "x: size",
                "x: float (field of Shape::Point)",
                [
                    ("x: float", "coord: float"),
                    ("x: 1.0", "coord: 1.0"),
                    ("x: size", "coord: size"),
                ],
            ),
        ] {
            let (decl, pattern) = (field(decl, 0), field(pattern, 0));
            let at = pattern.start;
            assert_eq!(b.definition(PATH, at).unwrap().range, decl);
            assert_eq!(hover(&b, at), doc);
            assert_eq!(b.hover(PATH, at).unwrap().range, Some(pattern));
            let references = b.references(PATH, at);
            assert_eq!(references.len(), 3, "{references:?}");
            for range in [decl, literal, pattern] {
                assert!(references.iter().any(|location| location.range == range));
            }
            let edit = b.rename(PATH, at, "coord").unwrap().unwrap();
            let mut changes = edit.changes.unwrap().into_iter();
            let (uri, edits) = changes.next().unwrap();
            assert_eq!(uri.to_file_path().unwrap(), PATH);
            assert!(changes.next().is_none());
            let expected = replacements
                .into_iter()
                .fold(src.to_owned(), |text, (from, to)| text.replace(from, to));
            assert_eq!(apply_edits(src, edits), expected);
        }
    }

    /// The range of `name`, found `into` bytes past `marker`.
    fn token(marker: &str, into: usize, name: &str) -> Range {
        Range::new(pos(SRC, marker, into), pos(SRC, marker, into + name.len()))
    }

    #[test]
    fn every_byte_of_a_record_reference_uses_the_same_token() {
        let b = workspace(SRC);
        let circle = token("Circle { radius: float", 0, "Circle");
        let radius = token("Circle { radius: float", 9, "radius");
        let circle_doc = "```rock\nShape::Circle { radius: float }\n```";
        let circle_field_doc = "radius: float (field of Shape::Circle)";
        let enum_doc = hover(&b, pos(SRC, "enum Shape", 5));
        for (marker, into, name, def, doc) in [
            ("Circle { radius: 1", 0, "Circle", circle, circle_doc),
            ("Shape::Circle", 7, "Circle", circle, circle_doc),
            (
                "Shape::Circle",
                0,
                "Shape",
                token("enum Shape", 5, "Shape"),
                &enum_doc,
            ),
            ("Circle { radius: 1", 9, "radius", radius, circle_field_doc),
            (
                "Shape::Circle { radius: 2",
                16,
                "radius",
                radius,
                circle_field_doc,
            ),
            (
                "Point { radius: 3",
                8,
                "radius",
                token("Point { radius: float", 8, "radius"),
                "radius: float (field of Point)",
            ),
        ] {
            for i in 0..name.len() {
                let p = pos(SRC, marker, into + i);
                let loc = b
                    .definition(PATH, p)
                    .unwrap_or_else(|| panic!("no definition at {p:?}"));
                assert_eq!(loc.uri.to_file_path().unwrap(), PATH);
                assert_eq!(loc.range, def, "{marker}: definition at {p:?}");
                let h = b
                    .hover(PATH, p)
                    .unwrap_or_else(|| panic!("no hover at {p:?}"));
                let HoverContents::Markup(m) = h.contents else {
                    panic!("expected markup hover");
                };
                assert_eq!(m.value, doc, "{marker}: hover at {p:?}");
                assert_eq!(h.range, Some(token(marker, into, name)), "{marker}");
            }
        }

        for i in 0.."Point".len() {
            let p = pos(SRC, "Point { radius: 3", i);
            assert_eq!(
                b.definition(PATH, p).unwrap().range,
                token("struct Point", 7, "Point")
            );
        }
        for i in 0.."Circle".len() {
            let p = pos(SRC, "Circle { radius: float", i);
            let h = b.hover(PATH, p).unwrap();
            let HoverContents::Markup(markup) = h.contents else {
                panic!("expected markup hover");
            };
            assert_eq!(markup.value, circle_doc);
            assert_eq!(h.range, Some(circle));
        }
        assert_eq!(
            hover(&b, pos(SRC, "Point { radius: 3", 0)),
            hover(&b, pos(SRC, "struct Point", 7))
        );
    }
}
