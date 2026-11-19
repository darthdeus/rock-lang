use super::*;

const LOCAL: &str = "fn main() {\n    let count = 42\n    print(\"\u{1f319}\") print(count)\n    print(count + 1)\n}";

fn range(start: (u32, u32), end: (u32, u32)) -> Value {
    json!({
        "start": {"line": start.0, "character": start.1},
        "end": {"line": end.0, "character": end.1}
    })
}

#[test]
fn code_action_inlines_a_use_after_non_ascii_text() {
    let source = "fn main() {\n    let x = 42\n    print(\"\u{00e9}\") print(x)\n}";
    let mut doc = Document::single(source);
    let at = range((1, 8), (1, 8));
    let reply = doc.request(
        "textDocument/codeAction",
        json!({"range": at, "context": {"diagnostics": []}}),
    );
    assert_eq!(
        doc.apply_action(&reply, "Inline local `x`"),
        source
            .replace("    let x = 42\n", "")
            .replace("print(x)", "print(42)")
    );
}

#[test]
fn prepare_rename_and_document_highlight_use_utf16_columns() {
    let mut doc = Document::single(LOCAL);
    let prepared = doc.at("textDocument/prepareRename", 2, 23);
    assert_eq!(prepared, range((2, 22), (2, 27)));
    let highlights = doc.at("textDocument/documentHighlight", 1, 8);
    reference_starts("highlight", &highlights, &[(1, 8), (2, 22), (3, 10)]);
}

#[test]
fn semantic_token_range_and_delta_agree_with_full() {
    let mut doc = Document::single(LOCAL);
    let full = doc.request("textDocument/semanticTokens/full", json!({}));
    let whole = doc.request(
        "textDocument/semanticTokens/range",
        json!({"range": range((0, 0), (5, 0))}),
    );
    assert_eq!(whole["data"], full["data"]);
    let viewport = doc.request(
        "textDocument/semanticTokens/range",
        json!({"range": range((1, 0), (2, 0))}),
    );
    let viewport = array(&viewport["data"]).len();
    assert!(0 < viewport && viewport < array(&full["data"]).len());

    doc.replace(&LOCAL.replace("count + 1", "count + 1 + count"));
    let delta = doc.request(
        "textDocument/semanticTokens/full/delta",
        json!({"previousResultId": full["resultId"]}),
    );
    let mut data = array(&full["data"]).to_vec();
    let mut edits = array(&delta["edits"]).to_vec();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit["start"].as_u64()));
    for edit in edits {
        let start = edit["start"].as_u64().unwrap() as usize;
        let deleted = edit["deleteCount"].as_u64().unwrap() as usize;
        let inserted = edit["data"].as_array().cloned().unwrap_or_default();
        data.splice(start..start + deleted, inserted);
    }
    assert_eq!(Value::Array(data), doc.tokens());
}

#[test]
fn execute_command_accepts_a_completion_record() {
    let mut doc = Document::single(LOCAL);
    let reply = doc.request(
        "workspace/executeCommand",
        json!({"command": "rock.completionAccepted", "arguments": [1, 0, "count"]}),
    );
    assert!(reply.is_null(), "{reply}");
}
