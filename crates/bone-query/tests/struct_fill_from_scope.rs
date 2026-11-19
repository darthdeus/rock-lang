//! "Fill struct fields": shorthand for a matching in-scope binding, `field: ?` otherwise.

use crate::fixture::{PATH, workspace};
use lsp_types::Position;

#[test]
fn fills_from_matching_bindings_and_holes_the_rest() {
    // (case, source, cursor, required fragments, forbidden fragments)
    let cases: &[(&str, &str, (u32, u32), &[&str], &[&str])] = &[
        (
            "name and type match",
            "struct P { x: int, y: int }\nfn f(x: int) {\n    let y = 5\n    let p = P {  }\n}\n",
            (3, 15),
            &["x", "y"],
            &["?"],
        ),
        (
            "name matches, type does not",
            "struct P { x: int }\nfn f(x: string) {\n    let p = P {  }\n}\n",
            (2, 15),
            &["x: ?"],
            &[],
        ),
        (
            "no binding",
            "struct P { x: int }\nfn f() {\n    let p = P {  }\n}\n",
            (2, 15),
            &["x: ?"],
            &[],
        ),
        (
            "one match, one hole",
            "struct P { x: int, y: int }\nfn f(x: int) {\n    let p = P {  }\n}\n",
            (2, 15),
            &["y: ?"],
            &["x: ?"],
        ),
        (
            "literal as a bare match-arm body",
            "enum K { A, B }\nvalue S { x: float, y: float }\nfn f(k: K) -> S {\n    match k {\n        K::A => S { x: 1.0 }\n        K::B => S { x: 2.0, y: 3.0 }\n    }\n}\n",
            (4, 22),
            &["y"],
            &[],
        ),
        // Both types render as `T`, but f's generic is not the struct `T`.
        (
            "generic binding named like a struct field type",
            "struct T { v: int }\nstruct P { x: T }\nfn f<T>(x: T) {\n    let p = P {  }\n}\n",
            (3, 15),
            &["x: ?"],
            &[],
        ),
    ];
    for (case, src, (line, col), required, forbidden) in cases {
        let t = workspace(src)
            .fill_struct_fields(PATH, Position::new(*line, *col))
            .unwrap_or_else(|| panic!("{case}: fill not offered"))
            .new_text;
        for f in *required {
            assert!(t.contains(f), "{case}: missing {f:?} in {t:?}");
        }
        for f in *forbidden {
            assert!(!t.contains(f), "{case}: unexpected {f:?} in {t:?}");
        }
    }
}
