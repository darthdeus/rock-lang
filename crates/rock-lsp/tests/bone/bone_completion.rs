//! Completion through the server adapter, the same way the server dispatches
//! a request.

use crate::common::bone_ws;
use lsp_types::*;
use rock_lsp::do_completion_workspace;

fn complete(src: &str, line: u32, ch: u32) -> CompletionList {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, src);
    let params = CompletionParams {
        text_document: TextDocumentIdentifier {
            uri: Uri::from_file_path(path.to_str().unwrap()).unwrap(),
        },
        position: Position::new(line, ch),
        context: None,
    };
    do_completion_workspace(&mut ws, &params)
        .unwrap()
        .expect("completion response")
}

/// Bone's completion filters and ranks by the typed prefix, so a popup-open
/// (often empty-prefix) response is not a superset the client can re-filter.
/// A complete list made nvim-cmp cache the empty-prefix order and freeze it;
/// the client must re-request per keystroke.
#[test]
fn bone_completion_is_marked_incomplete() {
    let src = "\
struct IVec2 { x: int }
struct Movable { grid: IVec2 }
struct Player { movable: Movable }
fn sfx(g: IVec2) {}
fn r(player: Player) {
  sfx(pg)
}
";
    // cursor after `pg` inside `sfx(...)`.
    assert!(complete(src, 5, 8).is_incomplete);
}

/// Named-arg completion at a call with a `..spread` operand skips the params
/// the spread already supplies.
#[test]
fn spread_covered_params_are_skipped() {
    let src = "\
struct Dims { width: int, height: int }
fn configure(width: int, height: int, title: string) {}
fn r() {
  let d = Dims { width: 10, height: 20 }
  configure(..d, )
}
";
    // cursor after `..d, ` inside configure's arg list.
    let labels: Vec<String> = complete(src, 4, 17)
        .items
        .into_iter()
        .map(|i| i.label)
        .collect();
    assert!(labels.iter().any(|l| l == "title:"), "{labels:?}");
    assert!(
        !labels.iter().any(|l| l == "width:" || l == "height:"),
        "spread-supplied params offered: {labels:?}"
    );
}
