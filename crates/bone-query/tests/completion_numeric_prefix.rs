//! A numeric-literal token under the cursor is never an identifier prefix
//! (Rock identifiers can't start with a digit), so completion must not
//! subsequence-match it against symbols. Regression: typing the `0` in
//! `global DRAG_COMMIT: float = 0` offered `remap_to_01` / `remap_01_range`.

use crate::label_asserts::assert_offers;
use bone_query::BoneWorkspace;
use lsp_types::Position;

const PATH: &str = "/tmp/rock_lsp_numeric_prefix.rock";

fn labels(src: &str, line: u32, ch: u32) -> Vec<String> {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    bone.completion(PATH, Position::new(line, ch))
        .into_iter()
        .map(|c| c.label)
        .collect()
}

#[test]
fn numeric_literal_offers_no_identifier_completions() {
    let src = "fn remap_to_01(value: float, min1: float, max1: float) -> float {\n  value\n}\n\
               global DRAG_COMMIT: float = 0\n";
    // Cursor right after the `0` on the last line.
    let out = labels(src, 3, 29);
    assert!(
        out.is_empty(),
        "numeric-literal prefix `0` should offer nothing, got {out:?}"
    );
}

#[test]
fn identifier_prefix_in_same_slot_still_completes() {
    // Positive control: the guard keys off a digit-led token, so a real
    // identifier prefix at the very same value slot still resolves `remap_to_01`.
    let src = "fn remap_to_01(value: float, min1: float, max1: float) -> float {\n  value\n}\n\
               global DRAG_COMMIT: float = remap\n";
    // Cursor right after `remap`.
    let out = labels(src, 3, 33);
    // identifier prefix `remap` should offer remap_to_01, got
    assert_offers(&out, &["remap_to_01"]);
}
