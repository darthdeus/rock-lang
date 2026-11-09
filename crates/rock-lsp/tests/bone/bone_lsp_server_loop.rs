//! End-to-end pin of the `LspServer` message loop in `rock-lsp/src/lib.rs`.
//!
//! Drives the transport-free server (`new` / `initialize` / `handle` /
//! `exited`) through initialize, didOpen/didChange diagnostics, hover,
//! definition, shutdown/exit and unknown messages, asserting the exact
//! outgoing messages. The rewrite of the server plumbing must keep every
//! expectation here green without touching this file.

use lsp_server::{Message, Notification, Request, RequestId};
use rock_lsp::LspServer;
use serde_json::{Value, json};
use std::str::FromStr;

fn init_params() -> Value {
    json!({ "capabilities": {}, "rootUri": Value::Null })
}

fn started() -> LspServer {
    LspServer::new(init_params()).expect("init params parse")
}

fn req(id: i32, method: &str, params: Value) -> Message {
    Message::Request(Request {
        id: RequestId::from(id),
        method: method.to_string(),
        params,
    })
}

fn not(method: &str, params: Value) -> Message {
    Message::Notification(Notification {
        method: method.to_string(),
        params,
    })
}

fn roundtrip(server: &mut LspServer, msg: Message) -> Vec<Message> {
    let mut out = Vec::new();
    server.handle(msg, &mut out).expect("handle must not fail");
    out
}

fn response(out: &[Message]) -> &lsp_server::Response {
    assert_eq!(out.len(), 1, "exactly one outgoing message, got {out:?}");
    match &out[0] {
        Message::Response(resp) => resp,
        other => panic!("expected a response, got {other:?}"),
    }
}

fn uri_of(path: &std::path::Path) -> String {
    lsp_types::Uri::from_file_path(path.to_str().unwrap())
        .unwrap()
        .to_string()
}

const BROKEN: &str = "fn main() {\n    let n: int = \"text\"\n    print(\"{n}\")\n}\n";
const FIXED: &str = "fn main() {\n    let bad = is_nan(1.5)\n    print(\"{bad}\")\n}\n";

fn did_open(uri: &str, text: &str) -> Message {
    not(
        "textDocument/didOpen",
        json!({ "textDocument": { "uri": uri, "languageId": "rock", "version": 1, "text": text } }),
    )
}

fn did_change(uri: &str, text: &str) -> Message {
    not(
        "textDocument/didChange",
        json!({ "textDocument": { "uri": uri, "version": 2 }, "contentChanges": [{ "text": text }] }),
    )
}

fn diagnostics_for(out: &[Message], uri: &str) -> Vec<Value> {
    out.iter()
        .filter_map(|m| match m {
            Message::Notification(n) if n.method == "textDocument/publishDiagnostics" => {
                Some(&n.params)
            }
            _ => None,
        })
        .filter(|p| p["uri"].as_str() == Some(uri))
        .flat_map(|p| p["diagnostics"].as_array().cloned().unwrap_or_default())
        .collect()
}

#[test]
fn initialize_replies_with_the_advertised_capabilities() {
    let id = RequestId::from(1);
    let request = Request {
        id: id.clone(),
        method: "initialize".to_string(),
        params: init_params(),
    };
    let (_server, reply) = LspServer::initialize(request).expect("initialize must succeed");
    let Message::Response(resp) = reply else {
        panic!("initialize must answer with a response");
    };
    assert_eq!(resp.id, id);
    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let caps = resp.result.as_ref().expect("result")["capabilities"].clone();
    assert_eq!(
        caps,
        serde_json::to_value(rock_lsp::server_capabilities()).unwrap(),
        "the initialize reply must carry exactly server_capabilities()"
    );
}

#[test]
fn capability_set_stays_identical() {
    let caps = serde_json::to_value(rock_lsp::server_capabilities()).unwrap();
    let map = caps.as_object().expect("capabilities is an object");
    // Pin the full key set: adding or dropping a provider changes editors'
    // behaviour, so the rewrite must reproduce this list exactly.
    let mut keys: Vec<&str> = map.keys().map(|k| k.as_str()).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "codeActionProvider",
            "completionProvider",
            "definitionProvider",
            "documentHighlightProvider",
            "documentSymbolProvider",
            "executeCommandProvider",
            "hoverProvider",
            "inlayHintProvider",
            "positionEncoding",
            "referencesProvider",
            "renameProvider",
            "semanticTokensProvider",
            "signatureHelpProvider",
            "textDocumentSync",
            "typeDefinitionProvider",
            "workspace",
            "workspaceSymbolProvider",
        ],
        "capability keys: {keys:?}"
    );
    assert_eq!(caps["definitionProvider"], json!(true));
    assert_eq!(caps["hoverProvider"], json!(true));
    assert_eq!(caps["referencesProvider"], json!(true));
    assert_eq!(caps["typeDefinitionProvider"], json!(true));
    assert_eq!(caps["documentHighlightProvider"], json!(true));
    assert_eq!(caps["documentSymbolProvider"], json!(true));
    assert_eq!(caps["workspaceSymbolProvider"], json!(true));
    assert_eq!(caps["completionProvider"]["resolveProvider"], json!(false));
    assert_eq!(
        caps["completionProvider"]["triggerCharacters"],
        json!([".", ":", " "])
    );
    assert_eq!(
        caps["textDocumentSync"],
        json!({ "openClose": true, "change": 1, "willSave": false,
                 "willSaveWaitUntil": false, "save": { "includeText": false } })
    );
    assert_eq!(caps["renameProvider"]["prepareProvider"], json!(true));
    assert_eq!(
        caps["codeActionProvider"]["codeActionKinds"],
        json!(["refactor.inline"])
    );
    assert_eq!(
        caps["signatureHelpProvider"]["triggerCharacters"],
        json!(["(", ","])
    );
    assert_eq!(
        caps["executeCommandProvider"]["commands"],
        json!(["rock.completionAccepted"])
    );
    assert!(
        caps["semanticTokensProvider"].is_object(),
        "semantic tokens stay advertised"
    );
    assert_eq!(
        caps["workspace"]["workspaceFolders"],
        json!({ "supported": true, "changeNotifications": true })
    );
}

#[test]
fn did_open_publishes_the_type_error_and_change_clears_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let uri = uri_of(&path);
    let mut server = started();

    let out = roundtrip(&mut server, did_open(&uri, BROKEN));
    let diags = diagnostics_for(&out, &uri);
    let errors: Vec<&Value> = diags.iter().filter(|d| d["severity"] == 1).collect();
    assert!(
        !errors.is_empty(),
        "didOpen with a type error must publish it, got {diags:?}"
    );
    assert!(
        errors.iter().any(|d| d["range"]["start"]["line"] == 1),
        "the error is on line 2 (0-based 1): {errors:?}"
    );

    // didChange also asks the client to refresh inlay hints.
    let out = roundtrip(&mut server, did_change(&uri, FIXED));
    assert!(
        out.iter().any(|m| matches!(m,
            Message::Notification(n) if n.method == "workspace/inlayHint/refresh")),
        "didChange must emit workspace/inlayHint/refresh, got {out:?}"
    );
    let diags = diagnostics_for(&out, &uri);
    assert!(
        diags.iter().all(|d| d["severity"] != 1),
        "valid source clears the error, got {diags:?}"
    );
}

#[test]
fn hover_and_definition_answer_through_handle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let uri = uri_of(&path);
    let mut server = started();
    roundtrip(&mut server, did_open(&uri, FIXED));

    let at = json!({ "textDocument": { "uri": uri },
                     "position": { "line": 1, "character": 16 } });
    let out = roundtrip(&mut server, req(10, "textDocument/hover", at.clone()));
    let resp = response(&out);
    assert_eq!(resp.id, RequestId::from(10));
    assert!(resp.error.is_none(), "hover error: {:?}", resp.error);
    let text = serde_json::to_string(&resp.result).unwrap();
    assert!(
        text.contains("is_nan"),
        "hover names the stdlib call: {text}"
    );

    let out = roundtrip(&mut server, req(11, "textDocument/definition", at));
    let resp = response(&out);
    assert_eq!(resp.id, RequestId::from(11));
    assert!(resp.error.is_none(), "definition error: {:?}", resp.error);
    let result = resp.result.clone().expect("definition result");
    let loc = if result.is_array() {
        result[0].clone()
    } else {
        result
    };
    let target = loc["uri"]
        .as_str()
        .or_else(|| loc["targetUri"].as_str())
        .expect("a location uri");
    lsp_types::Uri::from_str(target).expect("definition uri parses");
    assert!(
        target.ends_with("/stdlib/unified.rock"),
        "definition lands in the stdlib copy, got {target}"
    );
}

#[test]
fn shutdown_answers_then_exit_ends_the_loop() {
    let mut server = started();

    let out = roundtrip(&mut server, req(1, "shutdown", Value::Null));
    let resp = response(&out);
    assert!(
        resp.result == Some(Value::Null),
        "shutdown answers null: {resp:?}"
    );
    assert!(resp.error.is_none());
    assert!(!server.exited(), "shutdown alone does not exit");

    // After shutdown every request is rejected, but the loop keeps running.
    let out = roundtrip(
        &mut server,
        req(
            2,
            "textDocument/hover",
            json!({ "textDocument": { "uri": "file:///x.rock" },
            "position": { "line": 0, "character": 0 } }),
        ),
    );
    let resp = response(&out);
    assert!(resp.result.is_none(), "{resp:?}");
    let err = resp.error.as_ref().expect("an error response");
    assert_eq!(err.code, lsp_server::ErrorCode::InvalidRequest as i32);
    assert!(!server.exited());

    // Notifications after shutdown are swallowed silently.
    let out = roundtrip(&mut server, did_open("file:///x.rock", FIXED));
    assert!(
        out.is_empty(),
        "post-shutdown notification emits nothing: {out:?}"
    );

    roundtrip(&mut server, not("exit", Value::Null));
    assert!(server.exited(), "exit ends the loop");
}

#[test]
fn unknown_request_is_not_found_and_unknown_notification_is_silent() {
    let mut server = started();

    let out = roundtrip(&mut server, req(9, "frobnicate/do", json!({})));
    let resp = response(&out);
    assert_eq!(resp.id, RequestId::from(9));
    assert!(resp.result.is_none(), "{resp:?}");
    let err = resp.error.as_ref().expect("an error response");
    assert_eq!(err.code, lsp_server::ErrorCode::MethodNotFound as i32);
    assert_eq!(err.message, "Method 'frobnicate/do' not found");

    let out = roundtrip(&mut server, not("frobnicate/event", json!({})));
    assert!(
        out.is_empty(),
        "unknown notification emits nothing: {out:?}"
    );
    assert!(!server.exited());
}

#[test]
fn malformed_request_params_do_not_answer() {
    // A JSON body that cannot parse as the method's params is logged and
    // dropped: the server emits no response rather than a wrong one.
    let mut server = started();
    let out = roundtrip(
        &mut server,
        req(3, "textDocument/hover", json!({ "textDocument": 42 })),
    );
    assert!(
        out.is_empty(),
        "JSON-mismatched request emits nothing: {out:?}"
    );
}

#[test]
fn closing_buffer_restores_disk_symbols_for_other_open_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("stick.toml"),
        "[package]\nname = \"close_buffers\"\n",
    )
    .unwrap();
    let a = dir.path().join("a.rock");
    let b = dir.path().join("b.rock");
    std::fs::write(&a, "struct Foo { x: int }\n").unwrap();
    let source = "fn use_it() -> Foo { Foo { x: 1 } }\n";
    std::fs::write(&b, source).unwrap();
    let mut server = started();
    roundtrip(
        &mut server,
        did_open(&uri_of(&a), "struct Draft { x: int }\n"),
    );
    roundtrip(&mut server, did_open(&uri_of(&b), source));
    roundtrip(
        &mut server,
        not(
            "textDocument/didClose",
            json!({"textDocument": {"uri": uri_of(&a)}}),
        ),
    );
    let out = roundtrip(&mut server, did_change(&uri_of(&b), &format!("{source}\n")));
    let diags = diagnostics_for(&out, &uri_of(&b));
    assert!(diags.iter().all(|d| d["severity"] != 1), "{diags:?}");
    let out = roundtrip(
        &mut server,
        req(
            901,
            "textDocument/definition",
            json!({
                "textDocument": {"uri": uri_of(&b)}, "position": {"line": 0, "character": 15}
            }),
        ),
    );
    let result = response(&out).result.as_ref().unwrap();
    assert!(
        result.to_string().contains(&uri_of(&a)),
        "disk symbol should remain navigable: {result}"
    );
}

#[test]
fn closing_renamed_buffer_refreshes_definition_and_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("stick.toml"),
        "[package]\nname = \"renamed_buffers\"\n",
    )
    .unwrap();
    let old = dir.path().join("old.rock");
    let new = dir.path().join("new.rock");
    let main = dir.path().join("main.rock");
    let declaration = "struct Foo { x: int }\n";
    let source = "fn use_it() -> Foo { Foo { x: 1 } }\n";
    std::fs::write(&old, declaration).unwrap();
    std::fs::write(&main, source).unwrap();
    let mut server = started();
    roundtrip(&mut server, did_open(&uri_of(&old), declaration));
    roundtrip(&mut server, did_open(&uri_of(&main), source));
    std::fs::rename(&old, &new).unwrap();
    roundtrip(
        &mut server,
        not(
            "textDocument/didClose",
            json!({"textDocument": {"uri": uri_of(&old)}}),
        ),
    );
    roundtrip(&mut server, did_open(&uri_of(&new), declaration));
    let out = roundtrip(
        &mut server,
        req(
            902,
            "textDocument/definition",
            json!({
                "textDocument": {"uri": uri_of(&main)}, "position": {"line": 0, "character": 15}
            }),
        ),
    );
    assert!(
        response(&out)
            .result
            .as_ref()
            .unwrap()
            .to_string()
            .contains(&uri_of(&new)),
        "{out:?}"
    );
    std::fs::remove_file(&new).unwrap();
    let out = roundtrip(
        &mut server,
        not(
            "textDocument/didClose",
            json!({"textDocument": {"uri": uri_of(&new)}}),
        ),
    );
    let diags = diagnostics_for(&out, &uri_of(&main));
    assert!(
        diags
            .iter()
            .any(|d| d["severity"] == 1 && d["message"].as_str().unwrap().contains("Foo")),
        "{out:?}"
    );
}
