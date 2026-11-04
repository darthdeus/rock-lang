//! Named-arg completion at a call with a `..spread` operand must skip the
//! params the spread already supplies. `configure(..d, |)` where `d: Dims`
//! (fields width, height) offers only `title:` -- not `width:`/`height:`.

use lsp_types::*;
use rock_lsp::{LspWorkspace, do_completion_workspace};
use std::str::FromStr;

const SRC: &str = "\
struct Dims { width: int, height: int }
fn configure(width: int, height: int, title: string) {}
fn r() {
  let d = Dims { width: 10, height: 20 }
  configure(..d, )
}
";

fn labels_at(line: u32, ch: u32) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    std::fs::write(&path, SRC).unwrap();
    let mut ws = LspWorkspace::new();
    ws.bone.update(path.to_str().unwrap(), SRC.to_string());

    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Uri::from_str(&format!("file://{}", path.display())).unwrap(),
            },
            position: Position::new(line, ch),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };

    let resp = do_completion_workspace(&mut ws, &params)
        .unwrap()
        .expect("completion response");
    let items = match resp {
        CompletionResponse::List(list) => list.items,
        CompletionResponse::Array(items) => items,
    };
    items.into_iter().map(|i| i.label).collect()
}

#[test]
fn spread_covered_params_are_skipped() {
    // cursor after `..d, ` inside configure's arg list.
    let labels = labels_at(4, 17);
    assert!(
        labels.iter().any(|l| l == "title:"),
        "the uncovered `title:` must still be offered, got: {labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l == "width:"),
        "`width:` is supplied by the spread `..d` -- must not be offered, got: {labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l == "height:"),
        "`height:` is supplied by the spread `..d` -- must not be offered, got: {labels:?}"
    );
}
