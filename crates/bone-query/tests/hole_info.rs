//! Typed-hole reports: the inferred or context-coerced type, and the bindings in scope.

use crate::fixture::{PATH, workspace};
use lsp_types::Position;

#[test]
fn hole_reports() {
    // (case, source, cursor, report prefix, a scope entry or "")
    let cases = [
        (
            "type hole infers from its initializer",
            "fn main() {\n  let count = 5\n  let x: ? = count + 1\n}\n",
            (2, 9),
            "Type hole: int",
            "count: int",
        ),
        (
            "expression hole takes the annotation",
            "fn main() {\n  let a = 1\n  let x: int = ?\n}\n",
            (2, 15),
            "Expression hole expecting: int",
            "a: int",
        ),
        (
            "expression hole takes the parameter type",
            "fn takes(n: int) {}\nfn main() {\n  takes(?)\n}\n",
            (2, 8),
            "Expression hole expecting: int",
            "",
        ),
    ];
    for (case, src, (line, col), prefix, scope) in cases {
        let report = workspace(src)
            .hole_info(PATH, Position::new(line, col))
            .unwrap_or_else(|| panic!("{case}: no report"));
        assert!(report.starts_with(prefix), "{case}: {report:?}");
        assert!(report.contains(scope), "{case}: {report:?}");
    }
}

#[test]
fn none_when_cursor_is_not_on_a_hole() {
    let bone = workspace("fn main() {\n  let count = 5\n}\n");
    assert!(bone.hole_info(PATH, Position::new(1, 6)).is_none());
}
