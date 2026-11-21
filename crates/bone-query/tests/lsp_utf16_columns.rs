//! LSP positions are UTF-16 code units by default, so an astral-plane
//! character (two units, one scalar) before the cursor shifts every column on
//! its line. Each request is checked with an emoji ahead of the name it touches.

use crate::fixture::{PATH, position_of_offset, workspace};
use lsp_types::{DiagnosticSeverity, Position, Range};

// `"\u{1F980}\u{1D4B3}"` is two scalars but four UTF-16 units, so anything past it on the
// line differs between the two counts.

/// Position `extra` bytes into occurrence `nth` of `needle`, with the column
/// counted in UTF-16 units independently of the code under test.
fn at(src: &str, needle: &str, nth: usize, extra: usize) -> Position {
    position_of_offset(
        src,
        src.match_indices(needle).nth(nth).expect(needle).0 + extra,
    )
}

/// The text a UTF-16 range covers on one line; panics on a split surrogate.
fn covered(src: &str, range: Range) -> String {
    assert_eq!(range.start.line, range.end.line);
    let line = src.lines().nth(range.start.line as usize).unwrap();
    let units: Vec<u16> = line.encode_utf16().collect();
    String::from_utf16(&units[range.start.character as usize..range.end.character as usize])
        .expect("range splits no surrogate pair")
}

#[test]
fn diagnostics_report_utf16_columns() {
    let src = "fn main() {\n  let s = \"\u{1F980}\u{1D4B3}\"; print(not_defined + 1)\n}\n";
    let mut bone = bone_query::BoneWorkspace::new();
    let published = bone.update(PATH, src.to_string());
    let diag = published
        .iter()
        .flat_map(|(_, d)| d)
        .find(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .expect("an error");
    assert_eq!(covered(src, diag.range), "not_defined");
    assert_eq!(diag.range.start, at(src, "not_defined", 0, 0));
    let pulled = bone.diagnostics_for(PATH);
    let pulled = pulled
        .iter()
        .find(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .expect("an error");
    assert_eq!(pulled.range, diag.range);

    let src =
        "fn main() {\n  let s = \"\u{1F980}\u{1D4B3}\"; let unused_thing = 1\n  print(s)\n}\n";
    let lint = workspace(src)
        .lint_diagnostics(PATH)
        .into_iter()
        .find(|d| d.message.contains("unused_thing"))
        .expect("the unused-var lint");
    assert_eq!(covered(src, lint.range), "unused_thing");
}
