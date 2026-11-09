//! LSP positions are UTF-16 code units by default, so an astral-plane
//! character (two units, one scalar) before the cursor shifts every column on
//! its line. Each request is checked with an emoji ahead of the name it touches.

use crate::label_asserts::assert_offers;
use bone_query::{BoneWorkspace, LineIndex, offset_at_line_col, offset_to_position};
use lsp_types::{DiagnosticSeverity, Location, Position, Range};

#[cfg(not(windows))]
const PATH: &str = "/tmp/rock_lsp_utf16_columns.rock";
#[cfg(windows)]
const PATH: &str = r"C:\scratch\rock_lsp_utf16_columns.rock";

// `"\u{1F980}\u{1D4B3}"` is two scalars but four UTF-16 units, so anything past it on the
// line differs between the two counts.
const SRC: &str = "\
fn main() {
  let s = \"\u{1F980}\u{1D4B3}\"; let target = 1
  print(\"\u{1D4B3}\" + s + str(target))
}
";

fn workspace(src: &str) -> BoneWorkspace {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    bone
}

/// Position `extra` bytes into occurrence `nth` of `needle`, with the column
/// counted in UTF-16 units independently of the code under test.
fn at(src: &str, needle: &str, nth: usize, extra: usize) -> Position {
    let idx = src.match_indices(needle).nth(nth).expect(needle).0 + extra;
    let before = &src[..idx];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Position::new(
        before.matches('\n').count() as u32,
        src[line_start..idx].encode_utf16().count() as u32,
    )
}

/// The text a UTF-16 range covers on one line.
fn covered(src: &str, range: Range) -> String {
    assert_eq!(range.start.line, range.end.line);
    let line = src.lines().nth(range.start.line as usize).unwrap();
    let units: Vec<u16> = line.encode_utf16().collect();
    String::from_utf16(&units[range.start.character as usize..range.end.character as usize])
        .expect("range splits no surrogate pair")
}

fn covered_at(src: &str, location: &Location) -> String {
    covered(src, location.range)
}

#[test]
fn hover_resolves_past_an_astral_character() {
    let bone = workspace(SRC);
    assert!(
        bone.hover(PATH, at(SRC, "target", 0, 2)).is_some(),
        "hover on the binding"
    );
}

#[test]
fn goto_and_references_report_utf16_columns() {
    let bone = workspace(SRC);
    let use_pos = at(SRC, "target", 1, 2);
    let def = bone.definition(PATH, use_pos).expect("definition");
    assert_eq!(def.range.start, at(SRC, "target", 0, 0));
    assert_eq!(covered_at(SRC, &def), "target");
    let refs = bone.references(PATH, use_pos);
    assert_eq!(refs.len(), 2);
    for loc in &refs {
        assert_eq!(covered_at(SRC, loc), "target");
    }
    assert_eq!(bone.document_highlight(PATH, use_pos).len(), 2);
}

#[test]
fn rename_edits_land_on_the_name_after_an_astral_character() {
    let bone = workspace(SRC);
    let pos = at(SRC, "target", 0, 0);
    let prepared = bone
        .prepare_rename(PATH, pos)
        .expect("allowed")
        .expect("renameable");
    assert_eq!(covered(SRC, prepared), "target");
    let edit = bone
        .rename(PATH, pos, "goal")
        .expect("allowed")
        .expect("edits");
    let edits: Vec<_> = edit.changes.unwrap().into_values().flatten().collect();
    assert_eq!(edits.len(), 2);
    for e in edits {
        assert_eq!(covered(SRC, e.range), "target");
    }
}

#[test]
fn completion_reads_the_prefix_after_an_astral_character() {
    let src = "fn main() {\n  let s = \"\u{1F980}\u{1D4B3}\"; let targetable = 1; targ\n}\n";
    let mut bone = workspace(src);
    let items = bone.completion(PATH, at(src, "; targ\n", 0, 6));
    assert!(
        items.iter().any(|i| i.label == "targetable"),
        "{:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
}

#[test]
fn signature_help_resolves_after_an_astral_character() {
    let src = "fn f(a: int) -> int { a }\nfn main() {\n  let s = \"\u{1F980}\u{1D4B3}\"; f(1)\n}\n";
    let bone = workspace(src);
    assert!(bone.signature_help(PATH, at(src, "f(1", 0, 2)).is_some());
}

#[test]
fn inlay_hints_sit_after_the_binding_past_an_astral_character() {
    let bone = workspace(SRC);
    let hints = bone.inlay_hints(PATH, Range::new(Position::new(0, 0), Position::new(100, 0)));
    let hint = hints
        .iter()
        .find(|h| h.position.line == 1 && format!("{:?}", h.label).contains("int"))
        .expect("the `target` hint");
    assert_eq!(hint.position, at(SRC, "target", 0, "target".len()));
}

#[test]
fn semantic_tokens_count_utf16_units() {
    let bone = workspace(SRC);
    let tokens = bone.semantic_tokens(PATH).data;
    let (mut line, mut col) = (0u32, 0u32);
    let mut seen = Vec::new();
    for t in &tokens {
        line += t.delta_line;
        col = if t.delta_line == 0 {
            col + t.delta_start
        } else {
            t.delta_start
        };
        let range = Range::new(
            Position::new(line, col),
            Position::new(line, col + t.length),
        );
        seen.push(covered(SRC, range));
    }
    assert_offers(&seen, &["target", "\"\u{1F980}\u{1D4B3}\""]);
}

#[test]
fn a_multiline_token_counts_utf16_units_on_each_line() {
    let src = "fn main() {\n  let s = \"\"\"\u{1F980} one\n\u{1F980} two\"\"\"\n  print(s)\n}\n";
    let bone = workspace(src);
    let tokens = bone.semantic_tokens(PATH).data;
    let (mut line, mut col) = (0u32, 0u32);
    let mut seen = Vec::new();
    for t in &tokens {
        line += t.delta_line;
        col = if t.delta_line == 0 {
            col + t.delta_start
        } else {
            t.delta_start
        };
        let range = Range::new(
            Position::new(line, col),
            Position::new(line, col + t.length),
        );
        seen.push(covered(src, range));
    }
    assert_offers(&seen, &["\"\"\"\u{1F980} one", "\u{1F980} two\"\"\""]);
}

#[test]
fn interpolation_holes_count_utf16_units() {
    let src = "fn main() {\n  let x = 1\n  print(\"\u{1F980} {x} \u{1F980}\")\n}\n";
    let bone = workspace(src);
    let tokens = bone.semantic_tokens(PATH).data;
    let (mut line, mut col) = (0u32, 0u32);
    let mut seen = Vec::new();
    for t in &tokens {
        line += t.delta_line;
        col = if t.delta_line == 0 {
            col + t.delta_start
        } else {
            t.delta_start
        };
        let range = Range::new(
            Position::new(line, col),
            Position::new(line, col + t.length),
        );
        seen.push(covered(src, range));
    }
    assert_offers(&seen, &["x", "} \u{1F980}\""]);
}

#[test]
fn diagnostics_report_utf16_columns() {
    let src = "fn main() {\n  let s = \"\u{1F980}\u{1D4B3}\"; print(not_defined + 1)\n}\n";
    let mut bone = BoneWorkspace::new();
    let published = bone.update(PATH, src.to_string());
    let diag = published
        .iter()
        .flat_map(|(_, d)| d)
        .find(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .expect("an error");
    assert_eq!(covered(src, diag.range), "not_defined");
    assert_eq!(diag.range.start, at(src, "not_defined", 0, 0));
    let pulled = bone.diagnostics_for(PATH);
    assert_eq!(
        pulled
            .iter()
            .find(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .expect("an error")
            .range,
        diag.range
    );
}

#[test]
fn lint_diagnostics_report_utf16_columns() {
    let src =
        "fn main() {\n  let s = \"\u{1F980}\u{1D4B3}\"; let unused_thing = 1\n  print(s)\n}\n";
    let bone = workspace(src);
    let lint = bone
        .lint_diagnostics(PATH)
        .into_iter()
        .find(|d| d.message.contains("unused_thing"))
        .expect("the unused-var lint");
    assert_eq!(covered(src, lint.range), "unused_thing");
}

#[test]
fn conversions_count_utf16_units_both_ways() {
    let src = "a\u{1F980}b\n\u{1D4B3}\u{1D4B3}c\r\u{e9}\nd";
    let idx = LineIndex::new(src);
    for (byte, _) in src.char_indices().chain([(src.len(), ' ')]) {
        let pos = offset_to_position(src, byte);
        assert_eq!(pos, idx.position(src, byte), "LineIndex at {byte}");
        assert_eq!(
            offset_at_line_col(src, pos.line, pos.character),
            byte,
            "round trip at {byte}"
        );
    }
    let crab_end = "a\u{1F980}".len();
    assert_eq!(offset_to_position(src, crab_end), Position::new(0, 3));
    assert_eq!(offset_at_line_col(src, 0, 3), crab_end);
}

#[test]
fn a_column_inside_a_surrogate_pair_snaps_to_the_characters_start() {
    let src = "a\u{1F980}b\n";
    assert_eq!(offset_at_line_col(src, 0, 2), 1);
}

#[test]
fn a_column_past_the_line_clamps_to_its_end_not_the_next_line() {
    let src = "ab\ncd\r\nef";
    assert_eq!(offset_at_line_col(src, 0, 99), 2);
    assert_eq!(offset_at_line_col(src, 1, 99), 5, "before the CRLF");
    assert_eq!(offset_at_line_col(src, 2, 99), src.len());
    assert_eq!(offset_at_line_col(src, 9, 0), src.len());
}
