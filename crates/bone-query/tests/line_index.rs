//! `LineIndex` is the batched O(log n + col) replacement for the O(offset)
//! `offset_to_position` on hot paths (inlay hints convert ~one offset per hint).
//! It MUST produce byte-identical positions or every hint misplaces, so pin
//! equivalence across line shapes, multibyte glyphs, boundary + mid-char + past-
//! EOF offsets.

use bone_query::{offset_to_position, LineIndex};

#[test]
fn line_index_matches_offset_to_position() {
    let cases = [
        "",
        "abc",
        "abc\n",
        "a\nbb\nccc",
        "a\nbb\nccc\n",
        "\n\n\n",
        "héllo\nwörld\n", // multibyte scalars
        "tab\tend\nx",
        "x\r\ny", // lone CR is not a line break here; only \n is
    ];
    for src in cases {
        let idx = LineIndex::new(src);
        // Include a couple of offsets past EOF (both implementations clamp).
        for offset in 0..=src.len() + 2 {
            assert_eq!(
                offset_to_position(src, offset),
                idx.position(src, offset),
                "LineIndex disagreed with offset_to_position at offset {offset} in {src:?}"
            );
        }
    }
}
