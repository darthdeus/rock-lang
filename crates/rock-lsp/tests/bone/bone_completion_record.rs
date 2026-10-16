//! M6 replay corpus, server half: with recording active, every completion
//! response item carries a `rock.completionAccepted` command
//! `[record_id, index, label]`, and executing that command (what the client
//! does after inserting the item) appends one self-contained JSONL corpus
//! row -- the buffer exactly as the ranker saw it, the full ranked list,
//! and the accepted item. With recording off, items carry no command.

use lsp_types::*;
use rock_lsp::completion_record;
use rock_lsp::{do_completion_workspace, LspWorkspace};
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

fn completion_items(ws: &mut LspWorkspace, path: &std::path::Path) -> Vec<CompletionItem> {
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Uri::from_str(&format!("file://{}", path.display())).unwrap(),
            },
            // cursor after `pg` inside `sfx(...)`.
            position: Position::new(5, 8),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };
    match do_completion_workspace(ws, &params).unwrap().expect("completion response") {
        CompletionResponse::List(list) => list.items,
        CompletionResponse::Array(items) => items,
    }
}

#[test]
fn recording_round_trip_tags_items_and_writes_corpus_row() {
    // The recorder is process-global; isolate the writer under libtest too.
    const CHILD: &str = "ROCK_COMPLETION_RECORD_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                std::thread::current().name().expect("libtest test name"),
                "--nocapture",
            ])
            .env(CHILD, "1")
            .status()
            .expect("run isolated recorder test");
        assert!(status.success(), "isolated recorder test failed: {status}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    std::fs::write(&path, SRC).unwrap();
    let corpus = dir.path().join("corpus.jsonl");
    completion_record::init_recorder_at(corpus.clone());

    let mut ws = LspWorkspace::new();
    ws.bone.update(path.to_str().unwrap(), SRC.to_string());
    let items = completion_items(&mut ws, &path);
    assert!(!items.is_empty(), "fixture must complete something for `pg`");

    // Every item carries the accept command: same record_id, own index+label.
    let first_cmd = items[0].command.as_ref().expect("items are tagged when recording");
    assert_eq!(first_cmd.command, completion_record::ACCEPT_COMMAND);
    let record_id = first_cmd.arguments.as_ref().unwrap()[0]
        .as_u64()
        .expect("record_id arg");
    for (i, it) in items.iter().enumerate() {
        let args = it
            .command
            .as_ref()
            .unwrap_or_else(|| panic!("item {i} ({}) untagged", it.label))
            .arguments
            .as_ref()
            .unwrap();
        assert_eq!(args[0].as_u64(), Some(record_id), "one record_id per response");
        assert_eq!(args[1].as_u64(), Some(i as u64), "index arg is the item's rank");
        assert_eq!(args[2].as_str(), Some(it.label.as_str()), "label arg");
    }

    // Execute the command the way the client does on accept.
    let picked = items.len().min(2) - 1;
    let exec = ExecuteCommandParams {
        command: completion_record::ACCEPT_COMMAND.to_string(),
        arguments: vec![
            serde_json::json!(record_id),
            serde_json::json!(picked),
            serde_json::json!(items[picked].label),
        ],
        work_done_progress_params: Default::default(),
    };
    let result = completion_record::handle_execute_command(&exec).expect("accept handled");
    assert_eq!(result, serde_json::Value::Null);

    let corpus_text = std::fs::read_to_string(&corpus).expect("corpus row written");
    let rows: Vec<serde_json::Value> =
        corpus_text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), 1, "one accept -> one row");
    let row = &rows[0];
    assert_eq!(row["record_id"].as_u64(), Some(record_id));
    assert_eq!(row["buffer_text"].as_str(), Some(SRC), "the exact text the ranker saw");
    assert_eq!(row["prefix"].as_str(), Some("pg"));
    assert_eq!(row["pos"], serde_json::json!({ "line": 5, "character": 8 }));
    assert_eq!(row["accepted"]["index"].as_u64(), Some(picked as u64));
    assert_eq!(row["accepted"]["label"].as_str(), Some(items[picked].label.as_str()));
    let row_items = row["items"].as_array().unwrap();
    assert_eq!(row_items.len(), items.len(), "row carries the FULL ranked list");
    assert_eq!(row_items[0]["label"].as_str(), Some(items[0].label.as_str()));
    assert!(row_items[0]["sortText"].is_string(), "items keep their sortText");

    // Unknown command is a rejection, not a silent Ok.
    let bogus = ExecuteCommandParams {
        command: "rock.nope".to_string(),
        arguments: vec![],
        work_done_progress_params: Default::default(),
    };
    assert!(completion_record::handle_execute_command(&bogus).is_err());
}

#[test]
fn recording_off_leaves_items_untagged() {
    // The recording-on test runs in a child, leaving this process off even
    // when libtest runs both tests concurrently or sequentially.
    assert!(
        std::env::var_os("ROCK_LSP_RECORD").is_none(),
        "test env must not set ROCK_LSP_RECORD"
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    std::fs::write(&path, SRC).unwrap();
    let mut ws = LspWorkspace::new();
    ws.bone.update(path.to_str().unwrap(), SRC.to_string());
    let items = completion_items(&mut ws, &path);
    assert!(!items.is_empty());
    for it in &items {
        assert!(
            it.command.is_none(),
            "item {} tagged while recording is off",
            it.label
        );
    }
}
