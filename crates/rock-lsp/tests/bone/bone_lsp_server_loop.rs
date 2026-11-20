//! The transport-free `LspServer` message loop, asserting the exact outgoing
//! messages.

use bone_query::FileUriExt;
use lsp_server::{Message, Notification, Request, RequestId};
use rock_lsp::LspServer;
use serde_json::{Value, json};
use std::str::FromStr;

fn init_params() -> Value {
    json!({ "capabilities": {}, "rootUri": Value::Null })
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

/// Error and clearing diagnostics are published per file, without synthetic
/// package identities (`<root>`, `@/`) in standalone messages.
#[test]
fn did_open_publishes_errors_and_a_valid_change_clears_them() {
    // (opened source, message fragment, line)
    for (source, message, line) in [
        (BROKEN, "", 1),
        (
            "deco init reg {}\ndeco init reg {}\nfn main() {}\n",
            "duplicate definition of decorator `reg`",
            1,
        ),
        (
            "struct Item {}\nstruct Item {}\n",
            "duplicate definition",
            1,
        ),
        (
            "fn item() {}\nglobal item: int = 0\n",
            "duplicate definition",
            1,
        ),
        ("struct item {}\nmod item {}\n", "duplicate definition", 1),
        ("mod child {}\nuse self as child;\n", "module `::`", 1),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let uri = uri_of(&dir.path().join("main.rock"));
        let mut server = LspServer::new();
        let out = roundtrip(&mut server, did_open(&uri, source));
        let diagnostics = diagnostics_for(&out, &uri);
        let errors: Vec<_> = diagnostics.iter().filter(|d| d["severity"] == 1).collect();
        assert_eq!(errors.len(), 1, "{source}: {diagnostics:?}");
        let text = errors[0]["message"].as_str().unwrap();
        assert!(text.contains(message), "{text}");
        assert!(!text.contains("<root>") && !text.contains("@/"), "{text}");
        assert_eq!(errors[0]["range"]["start"]["line"], line, "{source}");

        let out = roundtrip(&mut server, did_change(&uri, FIXED));
        let diagnostics = diagnostics_for(&out, &uri);
        assert!(
            diagnostics.iter().all(|d| d["severity"] != 1),
            "{diagnostics:?}"
        );
    }
}

/// Clients send capabilities with required sub-fields missing; the reply
/// carries exactly `server_capabilities()`, pinned key by key.
#[test]
fn initialize_replies_with_the_advertised_capabilities() {
    let params = json!({
        "capabilities": {
            "general": { "staleRequestSupport": { "cancel": true } },
            "textDocument": { "codeAction": { "codeActionLiteralSupport": {} } }
        },
        "rootUri": Value::Null
    });
    let id = RequestId::from(1);
    let (_server, reply) = LspServer::initialize(Request {
        id: id.clone(),
        method: "initialize".to_string(),
        params,
    });
    let Message::Response(resp) = reply else {
        panic!("initialize must answer with a response");
    };
    assert_eq!(resp.id, id);
    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let caps = resp.result.as_ref().expect("result")["capabilities"].clone();
    assert_eq!(
        caps,
        serde_json::to_value(rock_lsp::server_capabilities()).unwrap()
    );
    assert!(caps["semanticTokensProvider"].is_object());
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

/// The refresh is a request with a fresh id, sent only to clients that
/// advertise `refreshSupport`; the client's reply produces no output.
#[test]
fn inlay_hint_refresh_follows_client_support() {
    let dir = tempfile::tempdir().unwrap();
    let uri = uri_of(&dir.path().join("main.rock"));
    let supported =
        json!({ "capabilities": { "workspace": { "inlayHint": { "refreshSupport": true } } } });
    for (params, refreshes) in [
        (supported, true),
        (json!({}), false),
        (init_params(), false),
        (json!({ "capabilities": { "workspace": {} } }), false),
        (
            json!({ "capabilities": { "workspace": { "inlayHint": {} } } }),
            false,
        ),
        (
            json!({ "capabilities": { "workspace": { "inlayHint": { "refreshSupport": false } } } }),
            false,
        ),
    ] {
        let (mut server, _) = LspServer::initialize(Request {
            id: RequestId::from(1),
            method: "initialize".to_string(),
            params: params.clone(),
        });
        roundtrip(&mut server, did_open(&uri, FIXED));
        let mut ids = Vec::new();
        for _ in 0..2 {
            let out = roundtrip(&mut server, did_change(&uri, FIXED));
            let sent: Vec<_> = out
                .iter()
                .filter(|m| match m {
                    Message::Request(r) => r.method == "workspace/inlayHint/refresh",
                    Message::Notification(n) => n.method == "workspace/inlayHint/refresh",
                    _ => false,
                })
                .collect();
            if !refreshes {
                assert!(sent.is_empty(), "{params}: {out:?}");
                continue;
            }
            assert_eq!(sent.len(), 1, "one refresh per edit: {out:?}");
            let Message::Request(refresh) = sent[0] else {
                panic!("refresh must be a request with an id: {sent:?}");
            };
            assert!(refresh.params.is_null());
            ids.push(refresh.id.clone());
            let reply = lsp_server::Response::new_ok(refresh.id.clone(), Value::Null);
            assert!(roundtrip(&mut server, reply.into()).is_empty());
        }
        if refreshes {
            assert_ne!(ids[0], ids[1]);
        }
    }
}

#[test]
fn hover_definition_and_references_answer_through_handle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let uri = uri_of(&path);
    let mut server = LspServer::new();
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

    let out = roundtrip(
        &mut server,
        req(
            12,
            "textDocument/references",
            json!({ "textDocument": { "uri": uri }, "position": { "line": 1, "character": 8 } }),
        ),
    );
    let resp = response(&out);
    assert!(resp.error.is_none(), "references error: {:?}", resp.error);
    assert!(resp.result.as_ref().is_some_and(Value::is_array));
}

#[test]
fn shutdown_answers_then_exit_ends_the_loop() {
    let mut server = LspServer::new();

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

/// Every request needs a response, or the client waits on its id forever.
#[test]
fn unknown_or_malformed_requests_answer_errors_and_unknown_notifications_are_silent() {
    let mut server = LspServer::new();
    for (id, method, params, code) in [
        (
            9,
            "frobnicate/do",
            json!({}),
            lsp_server::ErrorCode::MethodNotFound,
        ),
        (
            3,
            "textDocument/hover",
            json!({ "textDocument": 42 }),
            lsp_server::ErrorCode::InvalidParams,
        ),
    ] {
        let out = roundtrip(&mut server, req(id, method, params));
        let resp = response(&out);
        assert_eq!(resp.id, RequestId::from(id));
        assert!(resp.result.is_none(), "{resp:?}");
        assert_eq!(
            resp.error.as_ref().expect("an error response").code,
            code as i32
        );
    }

    let out = roundtrip(&mut server, not("frobnicate/event", json!({})));
    assert!(
        out.is_empty(),
        "unknown notification emits nothing: {out:?}"
    );
    assert!(!server.exited());
}

#[test]
fn stats_count_every_request_by_method() {
    let mut server = LspServer::new();
    for id in 0..250 {
        roundtrip(&mut server, req(id, "frobnicate/do", json!({})));
    }
    let out = roundtrip(&mut server, req(250, "rock/stats", json!({})));
    let stats = response(&out).result.clone().expect("stats");
    assert_eq!(stats["method_counts"]["frobnicate/do"], 250);
    assert_eq!(stats["requests_total"], 251);
}

/// A package directory under its canonical path: returned locations use
/// canonical paths, and the macOS temporary directory is behind a symlink.
fn package_dir(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::write(
        dir.join("stick.toml"),
        format!("[package]\nname = \"{name}\"\n"),
    )
    .unwrap();
    (tmp, dir)
}

fn did_close(path: &std::path::Path) -> Message {
    not(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri_of(path)}}),
    )
}

fn definition(server: &mut LspServer, path: &std::path::Path, line: u32, ch: u32) -> String {
    let out = roundtrip(
        server,
        req(
            900,
            "textDocument/definition",
            json!({
                "textDocument": {"uri": uri_of(path)}, "position": {"line": line, "character": ch}
            }),
        ),
    );
    response(&out).result.as_ref().unwrap().to_string()
}

const USES_FOO: &str = "fn use_it() -> Foo { Foo { x: 1 } }\n";
const DECLARES_FOO: &str = "struct Foo { x: int }\n";

#[test]
fn closing_buffer_restores_disk_symbols_for_other_open_files() {
    let (_tmp, dir) = package_dir("close_buffers");
    let a = dir.join("a.rock");
    let b = dir.join("b.rock");
    std::fs::write(&a, DECLARES_FOO).unwrap();
    std::fs::write(&b, USES_FOO).unwrap();
    let mut server = LspServer::new();
    roundtrip(
        &mut server,
        did_open(&uri_of(&a), "struct Draft { x: int }\n"),
    );
    roundtrip(&mut server, did_open(&uri_of(&b), USES_FOO));
    roundtrip(&mut server, did_close(&a));
    let out = roundtrip(
        &mut server,
        did_change(&uri_of(&b), &format!("{USES_FOO}\n")),
    );
    let diags = diagnostics_for(&out, &uri_of(&b));
    assert!(diags.iter().all(|d| d["severity"] != 1), "{diags:?}");
    let target = definition(&mut server, &b, 0, 15);
    assert!(target.contains(&uri_of(&a)), "{target}");
}

#[test]
fn closing_renamed_buffer_refreshes_definition_and_diagnostics() {
    let (_tmp, dir) = package_dir("renamed_buffers");
    let old = dir.join("old.rock");
    let new = dir.join("new.rock");
    let main = dir.join("main.rock");
    std::fs::write(&old, DECLARES_FOO).unwrap();
    std::fs::write(&main, USES_FOO).unwrap();
    let mut server = LspServer::new();
    roundtrip(&mut server, did_open(&uri_of(&old), DECLARES_FOO));
    roundtrip(&mut server, did_open(&uri_of(&main), USES_FOO));
    std::fs::rename(&old, &new).unwrap();
    roundtrip(&mut server, did_close(&old));
    roundtrip(&mut server, did_open(&uri_of(&new), DECLARES_FOO));
    let target = definition(&mut server, &main, 0, 15);
    assert!(target.contains(&uri_of(&new)), "{target}");

    std::fs::remove_file(&new).unwrap();
    let out = roundtrip(&mut server, did_close(&new));
    let diags = diagnostics_for(&out, &uri_of(&main));
    assert!(
        diags
            .iter()
            .any(|d| d["severity"] == 1 && d["message"].as_str().unwrap().contains("Foo")),
        "{out:?}"
    );
}
