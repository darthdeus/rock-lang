//! LSP positions are UTF-16 code units by default, so an astral-plane
//! character (two units, one scalar) before the cursor shifts every column on
//! its line. Each request is checked with an emoji ahead of the name it touches.

use crate::fixture::{PATH, decode_tokens, position_of_offset, workspace};
use crate::label_asserts::assert_offers;
use bone_query::{LineIndex, offset_at_line_col, offset_to_position};
use lsp_types::{DiagnosticSeverity, Position, Range};

// `"\u{1F980}\u{1D4B3}"` is two scalars but four UTF-16 units, so anything past it on the
// line differs between the two counts.
const SRC: &str = "\
fn main() {
  let s = \"\u{1F980}\u{1D4B3}\"; let target = 1
  print(\"\u{1D4B3}\" + s + str(target))
}
";

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
fn hover_completion_and_signature_help_resolve_past_an_astral_character() {
    assert!(
        workspace(SRC)
            .hover(PATH, at(SRC, "target", 0, 2))
            .is_some()
    );

    let src = "fn main() {\n  let s = \"\u{1F980}\u{1D4B3}\"; let targetable = 1; targ\n}\n";
    let labels: Vec<String> = workspace(src)
        .completion(PATH, at(src, "; targ\n", 0, 6))
        .into_iter()
        .map(|i| i.label)
        .collect();
    assert_offers(&labels, &["targetable"]);

    let src = "fn f(a: int) -> int { a }\nfn main() {\n  let s = \"\u{1F980}\u{1D4B3}\"; f(1)\n}\n";
    assert!(
        workspace(src)
            .signature_help(PATH, at(src, "f(1", 0, 2))
            .is_some()
    );
}

#[test]
fn goto_and_references_report_utf16_columns() {
    let bone = workspace(SRC);
    let use_pos = at(SRC, "target", 1, 2);
    let def = bone.definition(PATH, use_pos).expect("definition");
    assert_eq!(def.range.start, at(SRC, "target", 0, 0));
    assert_eq!(covered(SRC, def.range), "target");
    let refs = bone.references(PATH, use_pos);
    assert_eq!(refs.len(), 2);
    for loc in &refs {
        assert_eq!(covered(SRC, loc.range), "target");
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
    // (case, source, texts that some token must cover exactly)
    let cases: &[(&str, &str, &[&str])] = &[
        (
            "name after astral text",
            SRC,
            &["target", "\"\u{1F980}\u{1D4B3}\""],
        ),
        (
            "each line of a multi-line string",
            "fn main() {\n  let s = \"\"\"\u{1F980} one\n\u{1F980} two\"\"\"\n  print(s)\n}\n",
            &["\"\"\"\u{1F980} one", "\u{1F980} two\"\"\""],
        ),
        (
            "interpolation holes",
            "fn main() {\n  let x = 1\n  print(\"\u{1F980} {x} \u{1F980}\")\n}\n",
            &["x", "} \u{1F980}\""],
        ),
    ];
    for (case, src, wanted) in cases {
        let seen: Vec<String> = decode_tokens(&workspace(src).semantic_tokens(PATH).data)
            .into_iter()
            .map(|(line, col, t)| {
                let range = Range::new(
                    Position::new(line, col),
                    Position::new(line, col + t.length),
                );
                covered(src, range)
            })
            .collect();
        for w in *wanted {
            assert!(
                seen.iter().any(|s| s == w),
                "{case}: no token covers {w:?}: {seen:?}"
            );
        }
    }
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
fn out_of_range_columns_snap_within_the_line() {
    // Inside a surrogate pair: the character's start.
    assert_eq!(offset_at_line_col("a\u{1F980}b\n", 0, 2), 1);
    // Past the line: its end, not the next line.
    let src = "ab\ncd\r\nef";
    assert_eq!(offset_at_line_col(src, 0, 99), 2);
    assert_eq!(offset_at_line_col(src, 1, 99), 5, "before the CRLF");
    assert_eq!(offset_at_line_col(src, 2, 99), src.len());
    assert_eq!(offset_at_line_col(src, 9, 0), src.len());
}
