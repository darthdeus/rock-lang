#![allow(clippy::mutable_key_type)]

/// Open a tracing info-span scoped to the rest of the enclosing block, for
/// whatever tracing subscriber the process installs. rock-lsp is its only user.
#[macro_export]
macro_rules! scope {
    ($name:expr) => {
        let _scope = ::tracing::info_span!($name).entered();
    };
}

use std::str::FromStr;

use anyhow::Result;
use log::error;
use lsp_server::{Connection, Message, Request, RequestId, Response};
use lsp_types::request::{
    CodeActionRequest, Completion, DocumentHighlightRequest, DocumentSymbolRequest, GotoDefinition,
    GotoTypeDefinition, HoverRequest, InlayHintRequest, PrepareRenameRequest, References, Rename,
    Request as _, SemanticTokensFullDeltaRequest, SemanticTokensFullRequest,
    SemanticTokensRangeRequest, SignatureHelpRequest, WorkspaceSymbolRequest,
};
use lsp_types::{
    CompletionList, Hover, InlayHint, Location, SignatureHelp, TextDocumentPositionParams, Uri,
};

#[cfg(not(target_family = "wasm"))]
pub mod cli;
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

/// Opt-in per-request server-side timing (`ROCK_LSP_TIMING=1`). Emitted at
/// `info` through the shared backend, so it lands on stderr / `:LspLog` with the
/// standard `[<elapsed>s ...]` timestamp. Measures dequeue -> response-sent (the
/// server's actual handle time). Compare against the client-observed round-trip
/// (`:RockPanel`): a small handle time under a large round-trip localizes the
/// latency to the queue / transport (head-of-line blocking or write
/// backpressure), NOT to compute.
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

/// Per-method server-side handle-time samples (ms), for the `rock/stats`
/// introspection request. Recorded unconditionally (a cheap capped ring per
/// method) so `rock/stats` has data even without `ROCK_LSP_TIMING`.
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

/// Snapshot for the `rock/stats` request: per-method request counts + totals,
/// rendered generically by the panel's STATS block.
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

/// Snapshot for the `rock/modules` request, shaped for the modules floater:
/// `{ total_modules, contexts: [{ kind, root, module_count, modules: [...] }] }`.
/// `ago_ms` is how long ago each module last (re)inferred (null if never touched
/// this session -- rendered `-`). bone tracks no per-module compile *duration*
/// or dep-staleness, so `last_compile_us`/`stale` stay null/false.
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

/// What this server tells the client it can do, sent back with `initialize`.
///
/// The set below is load-bearing for editors: entries gate whether requests
/// are even issued, so additions or removals change visible behaviour. Keep it
/// in sync with the dispatch in `handle_request`.
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
            // `:` is listed so the second char of `::` pops enum variants for
            // `Foo::|`; the first char alone has no scope yet and simply falls
            // back to plain identifiers. Without it nvim never auto-opens on
            // `::`. `" "` opens the popup on a bare space (`let x = |`) with an
            // `isIncomplete` empty-prefix list that re-scores while typing.
            // See `extract_scope_resolution`.
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
        // Always listed (nvim will not send a command the server left
        // undeclared); completions only bear the command while
        // ROCK_LSP_RECORD records, so this is inert the rest of the time.
        "executeCommandProvider": { "commands": [completion_record::ACCEPT_COMMAND] },
        "semanticTokensProvider": {
            // Delta: on a re-request the client sends the previous result_id
            // and gets just the changed token span instead of the whole array.
            "full": { "delta": true },
            "legend": semantic_tokens::get_semantic_tokens_legend(),
            // nvim requests range for the initial paint / scroll; the handler
            // classifies only viewport bodies.
            "range": true,
        },
        "inlayHintProvider": { "resolveProvider": false },
    })
}

pub fn run_lsp_server() -> Result<()> {
    log::info!("Starting Rock LSP server ...");

    let (link, threads) = Connection::stdio();

    if let Err(broken) = link.initialize(server_capabilities()) {
        if broken.channel_is_disconnected() {
            threads.join()?;
        }
        return Err(anyhow::Error::from(broken));
    }

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| main_loop(link)));

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
            // Staying here to `threads.join()` would wedge: the reader
            // thread waits on stdin for EOF the client never sends while it
            // still believes the server is up, keeping a dead process with
            // its whole compiled session in RAM and dropping every later
            // request. Exit instead so the client notices and restarts.
            std::process::exit(1);
        }
    };

    eprintln!("Shutting down LSP server...");
    threads.join()?;
    log::info!("Shutting down Rock LSP server");

    Ok(())
}

fn main_loop(link: Connection) -> Result<()> {
    let mut server = LspServer::new();
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

/// Server state plus a transport-free message handler: every inbound
/// message appends whatever answers and notifications it causes, letting the
/// stdio loop and an embedded host (a Wasm Worker) run the same dispatch.
pub struct LspServer {
    workspace: LspWorkspace,
    shutdown_mode: bool,
    exited: bool,
}

impl LspServer {
    /// The server reads nothing from the client's `initialize` params.
    pub fn new() -> Self {
        Self {
            workspace: LspWorkspace::new(),
            shutdown_mode: false,
            exited: false,
        }
    }

    /// Reply to `initialize` without lsp-server's blocking handshake, for
    /// hosts that pass JSON-RPC messages through directly.
    pub fn initialize(req: Request) -> (Self, Message) {
        let payload = serde_json::json!({ "capabilities": server_capabilities() });
        (Self::new(), Response::new_ok(req.id, payload).into())
    }

    /// Whether the client has sent `exit`.
    pub fn exited(&self) -> bool {
        self.exited
    }

    pub fn handle(&mut self, msg: Message, out: &mut Vec<Message>) -> Result<()> {
        match msg {
            Message::Request(seen) => self.handle_request(seen, out),
            Message::Response(_) => Ok(()),
            Message::Notification(seen) => self.handle_notification(seen, out),
        }
    }

    fn handle_request(&mut self, req: Request, out: &mut Vec<Message>) -> Result<()> {
        log::debug!("MESSAGE RECEIVED: method={}, id={:?}", req.method, req.id);
        if req.method == "shutdown" {
            out.push(Response::new_ok(req.id, ()).into());
            self.shutdown_mode = true;
            return Ok(());
        }

        // Past shutdown every request fails; only `exit` still works, and it
        // arrives as a notification.
        if self.shutdown_mode {
            out.push(failed_request(
                req.id,
                lsp_server::ErrorCode::InvalidRequest,
                "Server is shutting down".to_string(),
            ));
            return Ok(());
        }

        // Handle timer for this request. It lives until the dispatch arm
        // below returns, so it spans dequeue to response-sent for each
        // request kind. Samples always land in REQUEST_STATS (backing
        // rock/stats); the per-request log line needs ROCK_LSP_TIMING=1.
        let _timing = ReqTiming {
            method: req.method.clone(),
            id: req.id.clone(),
            start: bone_core::prelude::Instant::now(),
        };

        // Fixed panic hook for the wedge-recovery test. Harmless otherwise:
        // it only fires with ROCK_LSP_ALLOW_TEST_PANIC set, so a genuine
        // client can never trip it.
        if req.method == "$/rockDebugPanic"
            && std::env::var_os("ROCK_LSP_ALLOW_TEST_PANIC").is_some()
        {
            panic!("deliberate test panic ($/rockDebugPanic)");
        }

        let ws = &mut self.workspace;
        match req.method.as_str() {
            GotoDefinition::METHOD => {
                answer::<GotoDefinition>(req, out, ws, go_to_definition_workspace)
            }
            GotoTypeDefinition::METHOD => {
                answer::<GotoTypeDefinition>(req, out, ws, go_to_type_definition_workspace)
            }
            References::METHOD => answer::<References>(req, out, ws, find_references_workspace),
            HoverRequest::METHOD => answer::<HoverRequest>(req, out, ws, do_hover_workspace),
            SignatureHelpRequest::METHOD => {
                answer::<SignatureHelpRequest>(req, out, ws, do_signature_help_workspace)
            }
            InlayHintRequest::METHOD => {
                answer::<InlayHintRequest>(req, out, ws, do_inlay_hints_workspace)
            }
            Completion::METHOD => answer::<Completion>(req, out, ws, do_completion_workspace),
            PrepareRenameRequest::METHOD => {
                answer::<PrepareRenameRequest>(req, out, ws, prepare_rename_workspace)
            }
            Rename::METHOD => answer::<Rename>(req, out, ws, do_rename_workspace),
            DocumentSymbolRequest::METHOD => {
                answer::<DocumentSymbolRequest>(req, out, ws, do_document_symbols_workspace)
            }
            WorkspaceSymbolRequest::METHOD => {
                answer::<WorkspaceSymbolRequest>(req, out, ws, do_workspace_symbols_workspace)
            }
            DocumentHighlightRequest::METHOD => {
                answer::<DocumentHighlightRequest>(req, out, ws, do_document_highlight_workspace)
            }
            SemanticTokensFullRequest::METHOD => answer::<SemanticTokensFullRequest>(
                req,
                out,
                ws,
                semantic_tokens::full_semantic_tokens,
            ),
            SemanticTokensFullDeltaRequest::METHOD => answer::<SemanticTokensFullDeltaRequest>(
                req,
                out,
                ws,
                semantic_tokens::delta_semantic_tokens,
            ),
            SemanticTokensRangeRequest::METHOD => answer::<SemanticTokensRangeRequest>(
                req,
                out,
                ws,
                semantic_tokens::range_semantic_tokens,
            ),
            CodeActionRequest::METHOD => {
                answer::<CodeActionRequest>(req, out, ws, compute_code_actions_workspace)
            }
            // Introspection requests for the rock.nvim panel.
            "rock/stats" => {
                let loaded = ws
                    .bone
                    .projects_summary()
                    .iter()
                    .map(|(_, m)| m.len())
                    .sum();
                send_response(out, req.id, Ok(request_stats_json(loaded)));
            }
            "rock/modules" => {
                let snapshot = modules_json(ws.bone.projects_summary());
                send_response(out, req.id, Ok(snapshot));
            }
            // `rock.completionAccepted`: the corpus recorder's accept signal,
            // run by the client after inserting a tagged completion item
            // (ROCK_LSP_RECORD).
            "workspace/executeCommand" => {
                let parsed = serde_json::from_value::<lsp_types::ExecuteCommandParams>(req.params)
                    .map_err(|e| format!("malformed executeCommand params: {e}"))
                    .and_then(|p| completion_record::handle_execute_command(&p));
                match parsed {
                    Ok(v) => send_response(out, req.id, Ok(v)),
                    Err(detail) => {
                        log::warn!("workspace/executeCommand rejected: {detail}");
                        out.push(failed_request(
                            req.id,
                            lsp_server::ErrorCode::InvalidParams,
                            detail,
                        ));
                    }
                }
            }
            method => {
                log::warn!("Unhandled request method: {method}");
                let message = format!("Method '{method}' not found");
                out.push(failed_request(
                    req.id,
                    lsp_server::ErrorCode::MethodNotFound,
                    message,
                ));
            }
        }
        Ok(())
    }

    fn handle_notification(
        &mut self,
        not: lsp_server::Notification,
        out: &mut Vec<Message>,
    ) -> Result<()> {
        // Handle exit notification
        if not.method == "exit" {
            // Exit the main loop when we receive the exit notification
            self.exited = true;
            return Ok(());
        }

        // In shutdown mode, ignore all other notifications
        if self.shutdown_mode {
            return Ok(());
        }

        let method = not.method.as_str();
        if method == "textDocument/didSave" {
            self.on_save(&not)?;
        } else if method == "textDocument/didOpen" {
            self.on_open(&not, out)?;
        } else if method == "textDocument/didChange" {
            self.on_change(&not, out)?;
        } else if method == "textDocument/didClose" {
            self.on_close(&not, out)?;
        } else if method == "textDocument/didChangeConfiguration"
            || method == "workspace/didChangeConfiguration"
        {
            // No settings to apply.
        }
        Ok(())
    }

    /// A save only refreshes `.rock.tags`: with FULL sync the buffer text we
    /// already hold (from didChange) is newer than the file, so reloading
    /// from disk here could clobber it with a stale read.
    fn on_save(&self, note: &lsp_server::Notification) -> Result<()> {
        let _span = tracing::debug_span!("lsp_did_save_notification").entered();
        let at = doc_uri(note)?;
        log::debug!("got textDocument/didSave notification for {}", at.as_str());
        regenerate_all_ctags(&self.workspace, "save");
        Ok(())
    }

    /// Track a newly opened document, compile it, and write tags: the first
    /// open batch-loads the whole compilation unit, which is also what a
    /// restart replays buffer by buffer, so each root gets fresh tags here
    /// rather than waiting for the first save.
    fn on_open(&mut self, note: &lsp_server::Notification, out: &mut Vec<Message>) -> Result<()> {
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

    /// Apply an edit: bump the version, drop cached inlay hints (and nudge
    /// the client to re-ask via `workspace/inlayHint/refresh`), recompile,
    /// and publish whatever diagnostics moved.
    fn on_change(&mut self, note: &lsp_server::Notification, out: &mut Vec<Message>) -> Result<()> {
        let at = doc_uri(note)?;
        let Some(path) = notification_path(out, &at, "didChange")? else {
            return Ok(());
        };
        // FULL sync sends the whole buffer as the first content change.
        let text = doc_text(note, "/contentChanges/0/text")?;
        self.workspace
            .update_document_version(&at.to_string(), doc_version(note));
        // The refresh makes the client re-request hints for the edited text.
        notify(out, "workspace/inlayHint/refresh", serde_json::Value::Null);
        bone_reload_and_publish(out, &path, text, &mut self.workspace)?;
        Ok(())
    }

    /// Closing an editor buffer returns authority to disk; it does not delete
    /// an existing source from the project.
    fn on_close(&mut self, note: &lsp_server::Notification, out: &mut Vec<Message>) -> Result<()> {
        let _span = tracing::debug_span!("lsp_did_close_notification").entered();
        let at = doc_uri(note)?;
        log::debug!("got textDocument/didClose for {}", at.as_str());
        let Some(path) = notification_path(out, &at, "didClose")? else {
            return Ok(());
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => bone_reload_and_publish(out, &path, text, &mut self.workspace)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Remove the declarations through the normal edit path so
                // dependents get fresh diagnostics before dropping the entry.
                bone_reload_and_publish(out, &path, String::new(), &mut self.workspace)?;
                self.workspace.bone.remove(&path);
            }
            Err(error) => log::error!("bone LSP: cannot reload closed document `{path}`: {error}"),
        }
        self.workspace.close_document(at.as_str());
        Ok(())
    }
}

/// Regenerate the `.rock.tags` for the project root containing `uri` from the
/// live bone session. Used on didOpen: the first open batch-loads that root's
/// whole compilation unit, so its tags are complete after this one write, and a
/// restart (which re-opens every buffer) refreshes each open root exactly once
/// via its own didOpen -- no cross-root fan-out here, which would rewrite every
/// root once per reopened buffer. Best-effort: a failure is logged, never
/// propagated -- ctags must not break editing.
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

/// Regenerate `.rock.tags` for EVERY open project root, not just the edited
/// file's -- an editor with several packages open (e.g. an application and its library) must
/// keep all their tag files fresh so a jump from one resolves the other, even
/// while editing only one. Used on didSave: the save is the point a sibling
/// root's stale tags should catch up. `write_tags_file` skips roots whose bytes
/// are unchanged, so the untouched siblings cost a compare, not a rewrite.
/// Best-effort: a failure is logged per root, never propagated.
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

/// bone-backend equivalent of `reload_sources_on_change_workspace`: feed the
/// edit into the per-root bone session and publish diagnostics for every file
/// the session reports on (so cross-file errors update and now-clean files get a
/// clearing publish). Used on didOpen/didChange when `backend == Bone`.
fn bone_reload_and_publish(
    out: &mut Vec<Message>,
    file_path: &str,
    contents: String,
    workspace: &mut LspWorkspace,
) -> Result<()> {
    // Compile-on-open/edit cost (ROCK_LSP_TIMING=1). This is the notification
    // path (didOpen/didChange) that head-of-line-blocks every queued request
    // behind it, so it's the number to watch when first-paint feels slow.
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
        // `bone.update` returns every tracked file each keystroke; only publish
        // files whose diagnostics actually changed (skip the ~130 unchanged
        // empty clears for dependency files the editor never opened).
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
        let notice = lsp_types::PublishDiagnosticsParams {
            uri: file_uri,
            diagnostics,
        };
        notify(
            out,
            "textDocument/publishDiagnostics",
            serde_json::to_value(notice)?,
        );
    }
    Ok(())
}

/// Answer an `R` request with `handler`. The handler's answer must have the
/// request's declared result type, so it cannot send a shape the protocol
/// does not allow.
fn answer<R: lsp_types::request::Request>(
    req: Request,
    out: &mut Vec<Message>,
    workspace: &mut LspWorkspace,
    handler: fn(&mut LspWorkspace, &R::Params) -> Result<R::Result>,
) {
    let _span =
        tracing::info_span!("lsp_request", method = R::METHOD, request_id = %req.id).entered();
    match serde_json::from_value::<R::Params>(req.params) {
        Ok(params) => send_response(out, req.id, handler(workspace, &params)),
        Err(error) => {
            let detail = format!("invalid {} params: {error}", R::METHOD);
            log::error!("{detail}");
            out.push(failed_request(
                req.id,
                lsp_server::ErrorCode::InvalidParams,
                detail,
            ));
        }
    }
}

fn notify(out: &mut Vec<Message>, method: &str, params: serde_json::Value) {
    out.push(Message::Notification(lsp_server::Notification {
        method: method.to_string(),
        params,
    }));
}

/// An error response message for a request that cannot be answered.
fn failed_request(id: RequestId, code: lsp_server::ErrorCode, text: String) -> Message {
    Response::new_err(id, code as i32, text).into()
}

/// The document a notification talks about, read from its `textDocument.uri`.
fn doc_uri(note: &lsp_server::Notification) -> Result<Uri> {
    note.params["textDocument"]["uri"]
        .as_str()
        .and_then(|text| Uri::from_str(text).ok())
        .ok_or_else(|| anyhow::anyhow!("{} has no textDocument.uri", note.method))
}

fn doc_version(note: &lsp_server::Notification) -> Option<i32> {
    note.params["textDocument"]["version"]
        .as_i64()
        .map(|v| v as i32)
}

/// The full text a notification carries at `text`, a JSON pointer.
fn doc_text(note: &lsp_server::Notification, text: &str) -> Result<String> {
    note.params
        .pointer(text)
        .and_then(|raw| raw.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("{} has no {text}", note.method))
}

/// The file a document notification is about. A URI that names no local file
/// (an unsaved `untitled:` buffer, a remote scheme) is not an error that should
/// stop the server: the notification is skipped and the client is told why.
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
            notify(out, "window/showMessage", serde_json::to_value(params)?);
            Ok(None)
        }
    }
}

/// Answer a request: serialize the handler's value into a response, or the
/// handler's failure into a `RequestFailed` error. A value that cannot
/// serialize becomes `InternalError`.
fn send_response<T: serde::Serialize>(out: &mut Vec<Message>, id: RequestId, t: Result<T>) {
    let answer = match t.map(|done| serde_json::to_value(&done)) {
        Ok(Ok(payload)) => Message::Response(Response {
            id,
            result: Some(payload),
            error: None,
        }),
        Ok(Err(e)) => {
            let detail = format!("failed to serialize response: {e}");
            error!("{detail}");
            failed_request(id, lsp_server::ErrorCode::InternalError, detail)
        }
        Err(fault) => {
            log::error!("error: {fault}");
            failed_request(id, lsp_server::ErrorCode::RequestFailed, fault.to_string())
        }
    };
    out.push(answer);
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
        let mut out = Vec::new();
        send_response(&mut out, RequestId::from(7), Ok(Unserializable));
        let Some(Message::Response(resp)) = out.pop() else {
            panic!("expected a response");
        };
        assert_eq!(resp.id, RequestId::from(7));
        assert!(resp.result.is_none(), "{:?}", resp.result);
        let err = resp
            .error
            .expect("serialization failure must be an error response");
        assert_eq!(err.code, lsp_server::ErrorCode::InternalError as i32);
        assert!(err.message.contains("refused by test"), "{}", err.message);
    }
}
