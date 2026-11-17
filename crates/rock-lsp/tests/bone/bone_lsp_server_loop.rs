//! End-to-end pin of the `LspServer` message loop in `rock-lsp/src/lib.rs`.
//!
//! Drives the transport-free server (`new` / `initialize` / `handle` /
//! `exited`) through initialize, didOpen/didChange diagnostics, hover,
//! definition, shutdown/exit and unknown messages, asserting the exact
//! outgoing messages.

use lsp_server::{Message, Notification, Request, RequestId};
use rock_lsp::LspServer;
use serde_json::{Value, json};
use std::str::FromStr;

fn init_params() -> Value {
    json!({ "capabilities": {}, "rootUri": Value::Null })
}

fn started() -> LspServer {
    LspServer::new()
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
fn duplicate_decorator_diagnostic_is_unique_and_clears_after_an_edit() {
    let dir = tempfile::tempdir().unwrap();
    let uri = uri_of(&dir.path().join("main.rock"));
    let mut server = started();
    let source = "deco init reg {}\ndeco init reg {}\nfn main() {}\n";
    let out = roundtrip(&mut server, did_open(&uri, source));
    let diagnostics = diagnostics_for(&out, &uri);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .unwrap()
            .contains("duplicate definition of decorator `reg`"),
        "{diagnostics:?}"
    );
    assert_eq!(diagnostics[0]["range"]["start"]["line"], 1);

    let out = roundtrip(
        &mut server,
        did_change(&uri, "deco init reg {}\nfn main() {}\n"),
    );
    let diagnostics = diagnostics_for(&out, &uri);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn standalone_duplicate_diagnostics_do_not_expose_package_identity() {
    for source in [
        "struct Item {}\nstruct Item {}\n",
        "fn item() {}\nglobal item: int = 0\n",
        "struct item {}\nmod item {}\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let uri = uri_of(&dir.path().join("main.rock"));
        let out = roundtrip(&mut started(), did_open(&uri, source));
        let diagnostics = diagnostics_for(&out, &uri);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let message = diagnostics[0]["message"].as_str().unwrap();
        assert!(message.starts_with("duplicate definition"), "{message}");
        assert!(
            !message.contains("<root>") && !message.contains("@/"),
            "{message}"
        );
    }
}

#[test]
fn standalone_root_module_diagnostic_uses_the_source_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let uri = uri_of(&dir.path().join("main.rock"));
    let out = roundtrip(
        &mut started(),
        did_open(&uri, "mod child {}\nuse self as child;\n"),
    );
    let diagnostics = diagnostics_for(&out, &uri);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    let message = diagnostics[0]["message"].as_str().unwrap();
    assert!(message.contains("module `::`"), "{message}");
    assert!(!message.contains("@/"), "{message}");
}

#[test]
fn initialize_replies_with_the_advertised_capabilities() {
    let id = RequestId::from(1);
    let request = Request {
        id: id.clone(),
        method: "initialize".to_string(),
        params: init_params(),
    };
    let (_server, reply) = LspServer::initialize(request);
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
fn initialize_ignores_client_capabilities_it_does_not_read() {
    // Required sub-fields are missing, as real clients sometimes send.
    let params = json!({
        "capabilities": {
            "general": { "staleRequestSupport": { "cancel": true } },
            "textDocument": { "codeAction": { "codeActionLiteralSupport": {} } }
        },
        "rootUri": Value::Null
    });
    let request = Request {
        id: RequestId::from(1),
        method: "initialize".to_string(),
        params,
    };
    let (_server, reply) = LspServer::initialize(request);
    let Message::Response(resp) = reply else {
        panic!("initialize must answer with a response");
    };
    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
}

#[test]
fn capability_set_stays_identical() {
    let caps = serde_json::to_value(rock_lsp::server_capabilities()).unwrap();
    assert!(caps["semanticTokensProvider"].is_object());
    // The key set is exact; pin the provider options editors depend on.
    assert_eq!(
        caps,
        json!({
            "positionEncoding": caps["positionEncoding"],
            "inlayHintProvider": caps["inlayHintProvider"],
            "semanticTokensProvider": caps["semanticTokensProvider"],
            "definitionProvider": true,
            "hoverProvider": true,
            "referencesProvider": true,
            "typeDefinitionProvider": true,
            "documentHighlightProvider": true,
            "documentSymbolProvider": true,
            "workspaceSymbolProvider": true,
            "completionProvider": {
                "completionItem": { "labelDetailsSupport": false },
                "resolveProvider": false,
                "triggerCharacters": [".", ":", " "],
                "workDoneProgress": false,
            },
            "textDocumentSync": {
                "openClose": true, "change": 1, "willSave": false,
                "willSaveWaitUntil": false, "save": { "includeText": false },
            },
            "renameProvider": { "prepareProvider": true },
            "codeActionProvider": { "codeActionKinds": ["refactor.inline"], "resolveProvider": false },
            "signatureHelpProvider": { "triggerCharacters": ["(", ","] },
            "executeCommandProvider": { "commands": ["rock.completionAccepted"] },
        })
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

    let out = roundtrip(&mut server, did_change(&uri, FIXED));
    let diags = diagnostics_for(&out, &uri);
    assert!(
        diags.iter().all(|d| d["severity"] != 1),
        "valid source clears the error, got {diags:?}"
    );
}

#[test]
fn inlay_hint_refresh_is_a_request_with_unique_ids_and_ignored_replies() {
    let (mut server, _) = LspServer::initialize(Request {
        id: RequestId::from(1),
        method: "initialize".to_string(),
        params: json!({ "capabilities": {
            "workspace": { "inlayHint": { "refreshSupport": true } }
        } }),
    });
    let dir = tempfile::tempdir().unwrap();
    let uri = uri_of(&dir.path().join("main.rock"));
    roundtrip(&mut server, did_open(&uri, FIXED));
    let mut ids = Vec::new();
    for _ in 0..2 {
        let out = roundtrip(&mut server, did_change(&uri, FIXED));
        let refreshes: Vec<_> = out
            .iter()
            .filter(|m| match m {
                Message::Request(r) => r.method == "workspace/inlayHint/refresh",
                Message::Notification(n) => n.method == "workspace/inlayHint/refresh",
                _ => false,
            })
            .collect();
        assert_eq!(refreshes.len(), 1, "one refresh per edit: {out:?}");
        let Message::Request(refresh) = refreshes[0] else {
            panic!("refresh must be a request with an id: {refreshes:?}");
        };
        let wire = serde_json::to_value(refresh).unwrap();
        assert!(wire.get("id").is_some(), "refresh carries an id: {wire}");
        assert!(refresh.params.is_null());
        ids.push(refresh.id.clone());
        let reply = lsp_server::Response::new_ok(refresh.id.clone(), Value::Null);
        assert!(roundtrip(&mut server, reply.into()).is_empty());
    }
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn inlay_hint_refresh_requires_advertised_support() {
    let dir = tempfile::tempdir().unwrap();
    let uri = uri_of(&dir.path().join("main.rock"));
    for params in [
        json!({}),
        init_params(),
        json!({ "capabilities": { "workspace": {} } }),
        json!({ "capabilities": { "workspace": { "inlayHint": {} } } }),
        json!({ "capabilities": {
            "workspace": { "inlayHint": { "refreshSupport": false } }
        } }),
    ] {
        let (mut server, _) = LspServer::initialize(Request {
            id: RequestId::from(1),
            method: "initialize".to_string(),
            params,
        });
        roundtrip(&mut server, did_open(&uri, FIXED));
        let out = roundtrip(&mut server, did_change(&uri, FIXED));
        assert!(
            out.iter().all(|m| match m {
                Message::Request(r) => r.method != "workspace/inlayHint/refresh",
                Message::Notification(n) => n.method != "workspace/inlayHint/refresh",
                _ => true,
            }),
            "unsupported refresh must not be sent: {out:?}"
        );
    }
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
fn malformed_request_params_answer_invalid_params() {
    // Every request needs a response, or the client waits on its id forever.
    let mut server = started();
    let out = roundtrip(
        &mut server,
        req(3, "textDocument/hover", json!({ "textDocument": 42 })),
    );
    let resp = response(&out);
    assert_eq!(resp.id, RequestId::from(3));
    assert!(resp.result.is_none(), "{resp:?}");
    let err = resp.error.as_ref().expect("an error response");
    assert_eq!(err.code, lsp_server::ErrorCode::InvalidParams as i32);
}

#[test]
fn closing_buffer_restores_disk_symbols_for_other_open_files() {
    // Returned locations use canonical paths; on macOS the temp dir is
    // behind a symlink, so open the files by their canonical names.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::write(
        dir.join("stick.toml"),
        "[package]\nname = \"close_buffers\"\n",
    )
    .unwrap();
    let a = dir.join("a.rock");
    let b = dir.join("b.rock");
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
    // Returned locations use canonical paths; on macOS the temp dir is
    // behind a symlink, so open the files by their canonical names.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::write(
        dir.join("stick.toml"),
        "[package]\nname = \"renamed_buffers\"\n",
    )
    .unwrap();
    let old = dir.join("old.rock");
    let new = dir.join("new.rock");
    let main = dir.join("main.rock");
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
