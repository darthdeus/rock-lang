#![allow(clippy::mutable_key_type)]

use std::str::FromStr;

use anyhow::Result;
use bone_query::{BoneWorkspace, FileUriExt};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument, Exit,
    Notification as LspNotification, PublishDiagnostics, ShowMessage,
};
use lsp_types::request::{
    CodeActionRequest, Completion, DocumentHighlightRequest, DocumentSymbolRequest, ExecuteCommand,
    GotoDefinition, GotoTypeDefinition, HoverRequest, InlayHintRefreshRequest, InlayHintRequest,
    PrepareRenameRequest, References, Rename, Request as LspRequest,
    SemanticTokensFullDeltaRequest, SemanticTokensFullRequest, SemanticTokensRangeRequest,
    Shutdown, SignatureHelpRequest, WorkspaceSymbolRequest,
};
use lsp_types::{
    CompletionList, ExecuteCommandParams, InlayHint, Location, TextDocumentPositionParams, Uri,
};

mod code_actions;
pub mod semantic_tokens;
#[cfg(not(target_family = "wasm"))]
pub mod server_cli;
pub mod workspace;
mod workspace_adapters;

#[cfg(not(target_family = "wasm"))]
pub use server_cli::run_cli;

pub use code_actions::*;
pub use workspace::*;
pub use workspace_adapters::*;

pub mod completion_record;

fn default_debug_file(name: &str) -> std::path::PathBuf {
    #[cfg(windows)]
    return std::env::temp_dir().join(name);
    #[cfg(not(windows))]
    std::path::Path::new("/tmp").join(name)
}

#[cfg(all(test, windows))]
mod debug_path_tests {
    #[test]
    fn default_debug_files_use_the_system_temp_directory() {
        for name in ["lsp_debug.log", "rock-completion-corpus.jsonl"] {
            let path = super::default_debug_file(name);
            assert_eq!(path.parent(), Some(std::env::temp_dir().as_path()));
        }
    }
}

fn lsp_timing_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("ROCK_LSP_TIMING").is_some())
}

struct ReqTiming {
    method: String,
    id: lsp_server::RequestId,
    start: bone_core::prelude::Instant,
}

impl Drop for ReqTiming {
    fn drop(&mut self) {
        let ms = self.start.elapsed().as_secs_f64() as f32 * 1000.0;
        record_request_time(&self.method, ms);
        if lsp_timing_enabled() {
            log::info!(
                target: "lsp_timing",
                "req #{} {} handled in {:.1}ms",
                self.id,
                self.method,
                ms
            );
        }
    }
}

static REQUEST_STATS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::collections::VecDeque<f32>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

const REQUEST_STATS_CAP: usize = 200;

fn record_request_time(method: &str, ms: f32) {
    if let Ok(mut map) = REQUEST_STATS.lock() {
        let q = map.entry(method.to_string()).or_default();
        if q.len() == REQUEST_STATS_CAP {
            q.pop_front();
        }
        q.push_back(ms);
    }
}

/// Snapshot for `rock/stats`, rendered generically by the panel's STATS block.
fn request_stats_json(loaded_modules: usize) -> serde_json::Value {
    let mut method_counts = serde_json::Map::new();
    let mut total = 0usize;
    if let Ok(map) = REQUEST_STATS.lock() {
        for (method, samples) in map.iter() {
            method_counts.insert(method.clone(), serde_json::json!(samples.len()));
            total += samples.len();
        }
    }
    serde_json::json!({
        "loaded_modules": loaded_modules,
        "requests_total": total,
        "distinct_methods": method_counts.len(),
        "method_counts": method_counts,
    })
}

/// Populate module recency for `rock/modules`. Duration and staleness are unavailable.
fn modules_json(summary: Vec<(String, Vec<bone_query::ModuleRecency>)>) -> serde_json::Value {
    let total: usize = summary.iter().map(|(_, ids)| ids.len()).sum();
    let contexts: Vec<serde_json::Value> = summary
        .into_iter()
        .map(|(root, ids)| {
            let module_count = ids.len();
            let modules: Vec<serde_json::Value> = ids
                .into_iter()
                .map(|m| {
                    serde_json::json!({
                        "id": m.id,
                        "last_compile_us": serde_json::Value::Null,
                        "ago_ms": m.ago_ms,
                        "stale": false,
                        "is_stdlib": false,
                        "is_builtin": false,
                    })
                })
                .collect();
            serde_json::json!({
                "kind": "project",
                "root": root,
                "module_count": module_count,
                "modules": modules,
            })
        })
        .collect();
    serde_json::json!({ "total_modules": total, "contexts": contexts })
}

pub fn server_capabilities() -> serde_json::Value {
    serde_json::json!({
        "positionEncoding": "utf-16",
        // Whole-document sync: the client sends full text on open and change.
        "textDocumentSync": {
            "openClose": true,
            "change": 1,
            "willSave": false,
            "willSaveWaitUntil": false,
            "save": { "includeText": false },
        },
        "hoverProvider": true,
        "completionProvider": {
            "completionItem": { "labelDetailsSupport": false },
            "resolveProvider": false,
            "triggerCharacters": [".", ":", " "],
            "workDoneProgress": false,
        },
        "signatureHelpProvider": { "triggerCharacters": ["(", ","] },
        "definitionProvider": true,
        "typeDefinitionProvider": true,
        "referencesProvider": true,
        "documentHighlightProvider": true,
        "documentSymbolProvider": true,
        "workspaceSymbolProvider": true,
        "codeActionProvider": {
            "codeActionKinds": [lsp_types::CodeActionKind::REFACTOR_INLINE],
            "resolveProvider": false,
        },
        "renameProvider": { "prepareProvider": true },
        // nvim will not send a command the server left undeclared; completions only
        // bear it while ROCK_LSP_RECORD records.
        "executeCommandProvider": { "commands": [completion_record::ACCEPT_COMMAND] },
        "semanticTokensProvider": {
            "full": { "delta": true },
            "legend": semantic_tokens::get_semantic_tokens_legend(),
            // nvim requests range for the initial paint and scroll.
            "range": true,
        },
        "inlayHintProvider": { "resolveProvider": false },
    })
}

pub fn run_lsp_server() -> Result<()> {
    log::info!("Starting Rock LSP server ...");

    let (link, threads) = Connection::stdio();

    let params = match link.initialize(server_capabilities()) {
        Ok(params) => params,
        Err(broken) => {
            if broken.channel_is_disconnected() {
                threads.join()?;
            }
            return Err(anyhow::Error::from(broken));
        }
    };

    let server = LspServer::from_initialize_params(&params);
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| main_loop(link, server)));

    match outcome {
        Ok(Ok(_)) => {
            eprintln!("LSP main loop exited successfully");
        }
        Ok(Err(fault)) => {
            eprintln!("LSP main loop exited with error: {fault:?}");
        }
        Err(payload) => {
            eprintln!("=== LSP MAIN LOOP PANICKED ===");
            if let Some(s) = payload.downcast_ref::<&str>() {
                eprintln!("Panic message (str): {}", s);
            } else if let Some(s) = payload.downcast_ref::<String>() {
                eprintln!("Panic message (String): {}", s);
            } else {
                eprintln!("Panic with unknown payload type: {:?}", payload);
            }
            eprintln!("=== END PANIC INFO ===");
            // The reader blocks on stdin until the client sees the server exit.
            std::process::exit(1);
        }
    };

    eprintln!("Shutting down LSP server...");
    threads.join()?;
    log::info!("Shutting down Rock LSP server");

    Ok(())
}

fn main_loop(link: Connection, mut server: LspServer) -> Result<()> {
    let mut pending: Vec<Message> = Vec::new();
    for incoming in &link.receiver {
        let done = server.handle(incoming, &mut pending);
        for reply in pending.drain(..) {
            link.sender.send(reply)?;
        }
        done?;
        if server.exited() {
            return Ok(());
        }
    }
    Ok(())
}

/// Transport-free message handler shared by the stdio loop and embedded hosts
/// (a Wasm Worker).
pub struct LspServer {
    workspace: LspWorkspace,
    shutdown_mode: bool,
    exited: bool,
    inlay_hint_refresh_support: bool,
    next_request_id: u64,
}

impl LspServer {
    pub fn new() -> Self {
        Self {
            workspace: LspWorkspace::new(),
            shutdown_mode: false,
            exited: false,
            inlay_hint_refresh_support: false,
            next_request_id: 0,
        }
    }

    fn from_initialize_params(params: &serde_json::Value) -> Self {
        Self {
            inlay_hint_refresh_support: params
                .pointer("/capabilities/workspace/inlayHint/refreshSupport")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            ..Self::new()
        }
    }

    /// Reply to `initialize` without lsp-server's blocking handshake, for
    /// hosts that pass JSON-RPC messages through directly.
    pub fn initialize(req: Request) -> (Self, Message) {
        let payload = serde_json::json!({ "capabilities": server_capabilities() });
        (
            Self::from_initialize_params(&req.params),
            Response::new_ok(req.id, payload).into(),
        )
    }

    pub fn exited(&self) -> bool {
        self.exited
    }

    pub fn handle(&mut self, msg: Message, out: &mut Vec<Message>) -> Result<()> {
        match msg {
            Message::Request(seen) => {
                out.push(self.handle_request(seen).into());
                Ok(())
            }
            Message::Response(_) => Ok(()),
            Message::Notification(seen) => self.handle_notification(seen, out),
        }
    }

    fn handle_request(&mut self, req: Request) -> Response {
        log::debug!("MESSAGE RECEIVED: method={}, id={:?}", req.method, req.id);
        if req.method == Shutdown::METHOD {
            self.shutdown_mode = true;
            return Response::new_ok(req.id, ());
        }

        // Past shutdown every request fails; only `exit` still works, and it
        // arrives as a notification.
        if self.shutdown_mode {
            return failed_request(
                req.id,
                ErrorCode::InvalidRequest,
                "Server is shutting down".to_string(),
            );
        }

        let _timing = ReqTiming {
            method: req.method.clone(),
            id: req.id.clone(),
            start: bone_core::prelude::Instant::now(),
        };

        // Only fires with ROCK_LSP_ALLOW_TEST_PANIC set, so a genuine client can never trip it.
        if req.method == "$/rockDebugPanic"
            && std::env::var_os("ROCK_LSP_ALLOW_TEST_PANIC").is_some()
        {
            panic!("deliberate test panic ($/rockDebugPanic)");
        }

        let ws = &mut self.workspace;
        match req.method.as_str() {
            GotoDefinition::METHOD => answer::<GotoDefinition, _>(req, ws, |ws, params| {
                at_position(ws, params, BoneWorkspace::definition)
                    .map(|loc| loc.map(|loc| vec![loc].into()))
            }),
            GotoTypeDefinition::METHOD => answer::<GotoTypeDefinition, _>(req, ws, |ws, params| {
                at_position(ws, params, BoneWorkspace::type_definition)
                    .map(|loc| loc.map(|loc| vec![loc].into()))
            }),
            References::METHOD => answer::<References, _>(req, ws, find_references_workspace),
            HoverRequest::METHOD => answer::<HoverRequest, _>(req, ws, |ws, params| {
                at_position(ws, params, BoneWorkspace::hover)
            }),
            SignatureHelpRequest::METHOD => {
                answer::<SignatureHelpRequest, _>(req, ws, |ws, params| {
                    at_position(ws, params, BoneWorkspace::signature_help)
                })
            }
            InlayHintRequest::METHOD => {
                answer::<InlayHintRequest, _>(req, ws, do_inlay_hints_workspace)
            }
            Completion::METHOD => {
                answer_optional::<Completion, _, _, _>(req, ws, do_completion_workspace)
            }
            PrepareRenameRequest::METHOD => {
                answer::<PrepareRenameRequest, _>(req, ws, |ws, params| {
                    // A refusal becomes the request's error, which editors show as the reason.
                    let range = at_position(ws, params, BoneWorkspace::prepare_rename)??;
                    Ok(range.map(lsp_types::PrepareRenameResponse::Range))
                })
            }
            Rename::METHOD => answer::<Rename, _>(req, ws, do_rename_workspace),
            DocumentSymbolRequest::METHOD => answer_optional::<DocumentSymbolRequest, _, _, _>(
                req,
                ws,
                do_document_symbols_workspace,
            ),
            WorkspaceSymbolRequest::METHOD => {
                answer::<WorkspaceSymbolRequest, _>(req, ws, |workspace, params| {
                    do_workspace_symbols_workspace(workspace, params)
                        .map(|symbols| symbols.map(lsp_types::WorkspaceSymbolResponse::Flat))
                })
            }
            DocumentHighlightRequest::METHOD => {
                answer::<DocumentHighlightRequest, _>(req, ws, do_document_highlight_workspace)
            }
            SemanticTokensFullRequest::METHOD => {
                answer_optional::<SemanticTokensFullRequest, _, _, _>(
                    req,
                    ws,
                    semantic_tokens::full_semantic_tokens,
                )
            }
            SemanticTokensFullDeltaRequest::METHOD => answer::<SemanticTokensFullDeltaRequest, _>(
                req,
                ws,
                semantic_tokens::delta_semantic_tokens,
            ),
            SemanticTokensRangeRequest::METHOD => {
                answer_optional::<SemanticTokensRangeRequest, _, _, _>(
                    req,
                    ws,
                    semantic_tokens::range_semantic_tokens,
                )
            }
            CodeActionRequest::METHOD => {
                answer::<CodeActionRequest, _>(req, ws, |workspace, params| {
                    compute_code_actions_workspace(workspace, params).map(|actions| {
                        actions.map(|actions| actions.into_iter().map(Into::into).collect())
                    })
                })
            }
            // Introspection requests for the rock.nvim panel.
            "rock/stats" => {
                let loaded = ws
                    .bone
                    .projects_summary()
                    .iter()
                    .map(|(_, m)| m.len())
                    .sum();
                serialize_response(req.id, Ok(request_stats_json(loaded)))
            }
            "rock/modules" => {
                serialize_response(req.id, Ok(modules_json(ws.bone.projects_summary())))
            }
            // `rock.completionAccepted`: the corpus recorder's accept signal (ROCK_LSP_RECORD).
            ExecuteCommand::METHOD => {
                let parsed = serde_json::from_value::<ExecuteCommandParams>(req.params)
                    .map_err(|e| format!("malformed executeCommand params: {e}"))
                    .and_then(|p| completion_record::handle_execute_command(&p));
                match parsed {
                    Ok(value) => serialize_response(req.id, Ok(value)),
                    Err(detail) => {
                        log::warn!("workspace/executeCommand rejected: {detail}");
                        failed_request(req.id, ErrorCode::InvalidParams, detail)
                    }
                }
            }
            method => {
                log::warn!("Unhandled request method: {method}");
                failed_request(
                    req.id,
                    ErrorCode::MethodNotFound,
                    format!("Method '{method}' not found"),
                )
            }
        }
    }

    fn handle_notification(&mut self, note: Notification, out: &mut Vec<Message>) -> Result<()> {
        match note.method.as_str() {
            Exit::METHOD => {
                self.exited = true;
                Ok(())
            }
            _ if self.shutdown_mode => Ok(()),
            DidSaveTextDocument::METHOD => self.on_save(&note),
            DidOpenTextDocument::METHOD => self.on_open(&note, out),
            DidChangeTextDocument::METHOD => self.on_change(&note, out),
            DidCloseTextDocument::METHOD => self.on_close(&note, out),
            _ => Ok(()),
        }
    }

    /// With full sync the held buffer is newer than the file, so a save must not
    /// reload from disk.
    fn on_save(&self, note: &Notification) -> Result<()> {
        let _span = tracing::debug_span!("lsp_did_save_notification").entered();
        let at = doc_uri(note)?;
        log::debug!("got textDocument/didSave notification for {}", at.as_str());
        regenerate_all_ctags(&self.workspace, "save");
        Ok(())
    }

    /// Tags are written here, not on first save: a restart replays every buffer
    /// as an open.
    fn on_open(&mut self, note: &Notification, out: &mut Vec<Message>) -> Result<()> {
        let _span = tracing::debug_span!("lsp_did_open_notification").entered();
        let at = doc_uri(note)?;
        let text = doc_text(note, "/textDocument/text")?;
        let Some(path) = notification_path(out, &at, "didOpen")? else {
            return Ok(());
        };
        self.workspace
            .update_document_version(&at.to_string(), doc_version(note));
        bone_reload_and_publish(out, &path, text, &mut self.workspace)?;
        regenerate_ctags(&self.workspace, &path, "open");
        Ok(())
    }

    fn on_change(&mut self, note: &Notification, out: &mut Vec<Message>) -> Result<()> {
        let at = doc_uri(note)?;
        let Some(path) = notification_path(out, &at, "didChange")? else {
            return Ok(());
        };
        // FULL sync sends the whole buffer as the first content change.
        let text = doc_text(note, "/contentChanges/0/text")?;
        self.workspace
            .update_document_version(&at.to_string(), doc_version(note));
        if self.inlay_hint_refresh_support {
            let id = format!("inlay-hint-refresh-{}", self.next_request_id);
            self.next_request_id += 1;
            out.push(
                Request {
                    id: id.into(),
                    method: InlayHintRefreshRequest::METHOD.to_string(),
                    params: serde_json::Value::Null,
                }
                .into(),
            );
        }
        bone_reload_and_publish(out, &path, text, &mut self.workspace)
    }

    /// Closing an editor buffer returns authority to disk; it does not delete
    /// an existing source from the project.
    fn on_close(&mut self, note: &Notification, out: &mut Vec<Message>) -> Result<()> {
        let _span = tracing::debug_span!("lsp_did_close_notification").entered();
        let at = doc_uri(note)?;
        log::debug!("got textDocument/didClose for {}", at.as_str());
        let Some(path) = notification_path(out, &at, "didClose")? else {
            return Ok(());
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => bone_reload_and_publish(out, &path, text, &mut self.workspace)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Remove through the normal edit path so dependents get fresh diagnostics.
                bone_reload_and_publish(out, &path, String::new(), &mut self.workspace)?;
                self.workspace.bone.remove(&path);
            }
            Err(error) => log::error!("bone LSP: cannot reload closed document `{path}`: {error}"),
        }
        self.workspace.close_document(at.as_str());
        Ok(())
    }
}

fn regenerate_ctags(workspace: &LspWorkspace, file_path: &str, when: &str) {
    // A Wasm host has no filesystem to write `.rock.tags` into.
    if cfg!(target_family = "wasm") {
        return;
    }
    match workspace.bone.write_ctags(file_path) {
        Ok(n) => log::debug!("wrote .rock.tags on {when} ({n} symbols)"),
        Err(e) => log::warn!("ctags: failed to write .rock.tags: {e}"),
    }
}

fn regenerate_all_ctags(workspace: &LspWorkspace, when: &str) {
    // A Wasm host has no filesystem to write `.rock.tags` into.
    if cfg!(target_family = "wasm") {
        return;
    }
    for (root, res) in workspace.bone.write_all_ctags() {
        match res {
            Ok(n) => log::debug!(
                "wrote {}/.rock.tags on {when} ({n} symbols)",
                root.display()
            ),
            Err(e) => log::warn!("ctags: failed to write {}/.rock.tags: {e}", root.display()),
        }
    }
}

fn bone_reload_and_publish(
    out: &mut Vec<Message>,
    file_path: &str,
    contents: String,
    workspace: &mut LspWorkspace,
) -> Result<()> {
    let _t = bone_core::prelude::Instant::now();
    let per_file = workspace.bone.update(file_path, contents);
    if lsp_timing_enabled() {
        log::info!(
            target: "lsp_timing",
            "compile(update) {} in {:.1}ms ({} files reported)",
            file_path,
            _t.elapsed().as_secs_f64() * 1000.0,
            per_file.len()
        );
    }

    for (path, diagnostics) in per_file {
        if path == "<autogenerated>" || path == "<unknown>" || path.is_empty() {
            continue;
        }
        // `bone.update` returns every tracked file each keystroke; publish only changes.
        if !workspace.diagnostics_changed(&path, &diagnostics) {
            continue;
        }
        let file_uri = match Uri::from_file_path(&path) {
            Ok(uri) => uri,
            Err(e) => {
                log::error!("bone LSP: cannot publish diagnostics: {e}");
                continue;
            }
        };
        notify::<PublishDiagnostics>(
            out,
            lsp_types::PublishDiagnosticsParams {
                uri: file_uri,
                diagnostics,
                version: None,
            },
        )?;
    }
    Ok(())
}

/// Position-only handlers stay compatible with clients that omit newer context.
fn answer<R: LspRequest, P: serde::de::DeserializeOwned>(
    req: Request,
    workspace: &mut LspWorkspace,
    handler: impl FnOnce(&mut LspWorkspace, &P) -> Result<R::Result>,
) -> Response {
    let _span =
        tracing::info_span!("lsp_request", method = R::METHOD, request_id = %req.id).entered();
    match serde_json::from_value::<P>(req.params) {
        Ok(params) => serialize_response(req.id, handler(workspace, &params)),
        Err(error) => {
            let detail = format!("invalid {} params: {error}", R::METHOD);
            log::error!("{detail}");
            failed_request(req.id, ErrorCode::InvalidParams, detail)
        }
    }
}

fn answer_optional<R, P, T, U>(
    req: Request,
    workspace: &mut LspWorkspace,
    handler: fn(&mut LspWorkspace, &P) -> Result<Option<T>>,
) -> Response
where
    R: LspRequest<Result = Option<U>>,
    P: serde::de::DeserializeOwned,
    T: Into<U>,
{
    answer::<R, P>(req, workspace, |workspace, params| {
        handler(workspace, params).map(|value| value.map(Into::into))
    })
}

fn notify<N: LspNotification>(out: &mut Vec<Message>, params: N::Params) -> Result<()> {
    out.push(
        Notification {
            method: N::METHOD.to_string(),
            params: serde_json::to_value(params)?,
        }
        .into(),
    );
    Ok(())
}

fn failed_request(id: RequestId, code: ErrorCode, message: String) -> Response {
    Response::new_err(id, code as i32, message)
}

fn doc_uri(note: &Notification) -> Result<Uri> {
    note.params["textDocument"]["uri"]
        .as_str()
        .and_then(|text| Uri::from_str(text).ok())
        .ok_or_else(|| anyhow::anyhow!("{} has no textDocument.uri", note.method))
}

fn doc_version(note: &Notification) -> Option<i32> {
    note.params["textDocument"]["version"]
        .as_i64()
        .map(|v| v as i32)
}

fn doc_text(note: &Notification, pointer: &str) -> Result<String> {
    note.params
        .pointer(pointer)
        .and_then(|raw| raw.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("{} has no {pointer}", note.method))
}

/// A URI naming no local file (`untitled:`, a remote scheme) is skipped with a
/// message to the client, not treated as an error.
fn notification_path(out: &mut Vec<Message>, uri: &Uri, method: &str) -> Result<Option<String>> {
    match uri.to_file_path() {
        Ok(path) => Ok(Some(path)),
        Err(e) => {
            let message = format!("rock-lsp: ignoring {method}: {e}");
            log::warn!("{message}");
            let params = lsp_types::ShowMessageParams {
                typ: lsp_types::MessageType::WARNING,
                message,
            };
            notify::<ShowMessage>(out, params)?;
            Ok(None)
        }
    }
}

fn serialize_response<T: serde::Serialize>(id: RequestId, result: Result<T>) -> Response {
    match result.map(serde_json::to_value) {
        Ok(Ok(payload)) => Response {
            id,
            result: Some(payload),
            error: None,
        },
        Ok(Err(error)) => {
            let detail = format!("failed to serialize response: {error}");
            log::error!("{detail}");
            failed_request(id, ErrorCode::InternalError, detail)
        }
        Err(fault) => {
            log::error!("error: {fault}");
            failed_request(id, ErrorCode::RequestFailed, fault.to_string())
        }
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;

    struct Unserializable;

    impl serde::Serialize for Unserializable {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("refused by test"))
        }
    }

    #[test]
    fn unserializable_result_is_an_internal_error_response() {
        let resp = serialize_response(RequestId::from(7), Ok(Unserializable));
        assert_eq!(resp.id, RequestId::from(7));
        assert!(resp.result.is_none(), "{:?}", resp.result);
        let err = resp
            .error
            .expect("serialization failure must be an error response");
        assert_eq!(err.code, ErrorCode::InternalError as i32);
        assert!(err.message.contains("refused by test"), "{}", err.message);
    }
}
