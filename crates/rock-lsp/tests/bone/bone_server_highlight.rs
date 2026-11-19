use crate::common::{at, bone_ws};
use rock_lsp::do_document_highlight_workspace;

#[test]
fn highlights_all_occurrences_of_a_local() {
    let src = "fn main() {\n    let count = 0\n    let bumped = count + 1\n    print(count)\n}\n";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, src);
    for (line, column) in [(1, 8), (3, 10)] {
        let highlights = do_document_highlight_workspace(&mut ws, &at(&path, line, column))
            .unwrap()
            .unwrap_or_default();
        let mut lines: Vec<_> = highlights.iter().map(|h| h.range.start.line).collect();
        lines.sort();
        assert_eq!(lines, [1, 2, 3], "cursor at {line}:{column}");
        for h in &highlights {
            assert_eq!(h.range.end.character - h.range.start.character, 5, "{h:?}");
        }
    }
}
