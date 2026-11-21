//! Hover and goto-definition on enum variants: patterns and construction take different paths.

use crate::fixture::{PATH, assert_no_errors, hover_markup, pos, workspace};
use bone_query::BoneWorkspace;
use bone_query::FileUriExt;
use lsp_types::{HoverContents, Position, Range};

#[track_caller]
fn hover(bone: &BoneWorkspace, p: Position) -> String {
    hover_markup(bone, PATH, p).unwrap_or_else(|| panic!("no hover at {p:?}"))
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
