//! The bone "Fill struct fields" code action (`compute_code_actions_workspace`).
//!
//! When the cursor is inside a struct literal `Name { ... }` that's missing
//! required fields (declared, no default, not already provided), the bone
//! backend offers an action whose edit inserts `field: ?` for each missing
//! field. Fields with a default are not added (omitting them is legal). The
//! generated `?` is a Rock expression hole, so the result type-checks and the
//! user can hover each `?` for its expected type.

use crate::common::*;
use lsp_types::*;

const POINT: &str = "struct Point { x: int, y: int }";
const BAR: &str = "struct Bar { a: int, b: int = 5 }";

/// The single action whose title mentions struct fields, or None.
fn fill_action(actions: &[CodeAction]) -> Option<&CodeAction> {
    actions.iter().find(|a| a.title.contains("struct field"))
}

fn source(decl: &str, literal: &str) -> String {
    format!("{decl}\n\nfn main() {{\n    let p = {literal}\n}}\n")
}

#[test]
fn struct_field_fill_cases() {
    for (decl, literal, (line, character), inserted, absent) in [
        (POINT, "Point {}", (3, 16), Some("Point { x: ?, y: ? }"), ""),
        (
            POINT,
            "Point { x: 1 }",
            (3, 20),
            Some("Point { x: 1, y: ? }"),
            "x: ?",
        ),
        (
            POINT,
            "Point { x: 1, }",
            (3, 20),
            Some("Point { x: 1, y: ? }"),
            "",
        ),
        (BAR, "Bar { a: 1 }", (3, 18), None, ""),
        (BAR, "Bar {}", (3, 17), Some("Bar { a: ? }"), "b: ?"),
        (POINT, "Point { x: 1, y: 2 }", (3, 20), None, ""),
        (POINT, "Point { x: 1, y: 2 }", (0, 8), None, ""),
        (
            POINT,
            "Point {\n        x: 1\n    }",
            (4, 8),
            Some("        x: 1,\n        y: ?,\n"),
            "",
        ),
    ] {
        let src = source(decl, literal);
        let actions = run(&src, line, character);
        match (inserted, fill_action(&actions)) {
            (Some(expected), Some(action)) => {
                let out = apply(&src, action);
                assert!(
                    out.contains(expected),
                    "{literal} at {line}:{character}: {out:?}"
                );
                assert!(
                    absent.is_empty() || !out.contains(absent),
                    "{literal} at {line}:{character}: {out:?}"
                );
            }
            (None, None) => {}
            (expected, action) => {
                panic!("{literal} at {line}:{character}: expected {expected:?}, got {action:?}")
            }
        }
    }
}
