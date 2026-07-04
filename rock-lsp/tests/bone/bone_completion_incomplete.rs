//! The interactive-server completion handler on the bone backend must mark its
//! result `isIncomplete = true`. Bone's completion does prefix-sensitive
//! FILTERING and match-quality RANKING, so a popup-open (often empty-prefix)
//! response is not a superset the client can correctly re-filter -- the client
//! must re-request per keystroke. Returning a plain `CompletionResponse::Array`
//! (isIncomplete = false) made nvim-cmp cache the empty-prefix list and
//! client-filter it, freezing the empty-prefix order (`tile_center(pg|` showed
//! dist-1 `prop.image_dims()` above dist-2 `player.movable.grid`). Driven
//! through the adapter the same way the server dispatches a request.

use lsp_types::*;
use rock_lsp::{do_completion_workspace, LspBackend, LspWorkspace};
use std::str::FromStr;

const SRC: &str = "\
struct IVec2 { x: int }
struct Movable { grid: IVec2 }
struct Player { movable: Movable }
fn sfx(g: IVec2) {}
fn r(player: Player) {
  sfx(pg)
}
";

#[test]
fn bone_completion_is_marked_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    std::fs::write(&path, SRC).unwrap();
    let mut ws = LspWorkspace::new();
    ws.backend = LspBackend::Bone;
    ws.bone.update(path.to_str().unwrap(), SRC.to_string());

    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Uri::from_str(&format!("file://{}", path.display())).unwrap(),
            },
            // cursor after `pg` inside `sfx(...)` (line 5, char 8).
            position: Position::new(5, 8),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };

    let resp = do_completion_workspace(&mut ws, &params).unwrap().expect("completion response");
    match resp {
        CompletionResponse::List(list) => assert!(
            list.is_incomplete,
            "bone completion list must be incomplete so the client re-requests per keystroke"
        ),
        CompletionResponse::Array(_) => {
            panic!("bone completion must return a List (isIncomplete=true), not a plain Array")
        }
    }
}
