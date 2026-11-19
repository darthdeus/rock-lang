//! With recording on, each completion item carries a `rock.completionAccepted`
//! command, and executing it appends one self-contained JSONL corpus row.

use crate::common::{bone_ws, completion};
use lsp_types::*;
use rock_lsp::completion_record;

const SRC: &str = "\
struct IVec2 { x: int }
struct Movable { grid: IVec2 }
struct Player { movable: Movable }
fn sfx(g: IVec2) {}
fn r(player: Player) {
  sfx(pg)
}
";

/// Completion after `pg` inside `sfx(...)`.
fn complete_pg() -> CompletionList {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    completion(&mut bone_ws(&path, SRC), &path, 5, 8)
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
    let corpus = dir.path().join("corpus.jsonl");
    completion_record::init_recorder_at(corpus.clone());

    let items = complete_pg().items;
    assert!(
        !items.is_empty(),
        "fixture must complete something for `pg`"
    );

    let first_cmd = items[0]
        .command
        .as_ref()
        .expect("items are tagged when recording");
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
        assert_eq!(
            args[0].as_u64(),
            Some(record_id),
            "one record_id per response"
        );
        assert_eq!(
            args[1].as_u64(),
            Some(i as u64),
            "index arg is the item's rank"
        );
        assert_eq!(args[2].as_str(), Some(it.label.as_str()), "label arg");
    }

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
    let rows: Vec<serde_json::Value> = corpus_text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 1, "one accept -> one row");
    let row = &rows[0];
    assert_eq!(row["record_id"].as_u64(), Some(record_id));
    assert_eq!(
        row["buffer_text"].as_str(),
        Some(SRC),
        "the exact text the ranker saw"
    );
    assert_eq!(row["prefix"].as_str(), Some("pg"));
    assert_eq!(row["pos"], serde_json::json!({ "line": 5, "character": 8 }));
    assert_eq!(row["accepted"]["index"].as_u64(), Some(picked as u64));
    assert_eq!(
        row["accepted"]["label"].as_str(),
        Some(items[picked].label.as_str())
    );
    let row_items = row["items"].as_array().unwrap();
    assert_eq!(
        row_items.len(),
        items.len(),
        "row carries the FULL ranked list"
    );
    assert_eq!(
        row_items[0]["label"].as_str(),
        Some(items[0].label.as_str())
    );
    assert!(
        row_items[0]["sortText"].is_string(),
        "items keep their sortText"
    );

    // Unknown command is a rejection, not a silent Ok.
    let bogus = ExecuteCommandParams {
        command: "rock.nope".to_string(),
        arguments: vec![],
        work_done_progress_params: Default::default(),
    };
    assert!(completion_record::handle_execute_command(&bogus).is_err());
}

/// Bone filters and ranks by the typed prefix, so a response is not a
/// superset the client can re-filter: a complete list made nvim-cmp freeze the
/// empty-prefix order. The recording-on test runs in a child process, so this
/// one sees recording off.
#[test]
fn completion_is_incomplete_and_untagged_when_recording_is_off() {
    assert!(
        std::env::var_os("ROCK_LSP_RECORD").is_none(),
        "test env must not set ROCK_LSP_RECORD"
    );
    let list = complete_pg();
    assert!(list.is_incomplete);
    assert!(!list.items.is_empty());
    for it in &list.items {
        assert!(it.command.is_none(), "item {} tagged", it.label);
    }
}
