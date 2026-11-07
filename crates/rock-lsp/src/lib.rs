#![allow(clippy::mutable_key_type)]

/// Build-supplied compiler conventions shared by one-shot queries and the server.
#[derive(Clone, Copy, Default)]
pub struct CompilerPolicy {
    pub action_layer: Option<&'static bone_lints::ActionLayerPolicy>,
}

impl CompilerPolicy {
    pub fn workspace(self) -> bone_query::BoneWorkspace {
        bone_query::BoneWorkspace::new().with_lint_options(
            bone_lints::LintOptions::default().with_action_policy(self.action_layer),
        )
    }
}

/// Open a tracing info-span scoped to the rest of the enclosing block, output
/// to tracing-flame / tracing-perfetto. rock-lsp is its only user.
#[macro_export]
macro_rules! scope {
    ($name:expr) => {
        let _scope = ::tracing::info_span!($name).entered();
    };
}

use std::str::FromStr;

use anyhow::Result;
use log::error;
use lsp_server::{Connection, ExtractError, Message, Request, RequestId, Response};
use lsp_types::notification::Notification;
use lsp_types::request::{
    CodeActionRequest, Completion, DocumentHighlightRequest, DocumentSymbolRequest, GotoDefinition,
    GotoTypeDefinition, GotoTypeDefinitionParams, HoverRequest, InlayHintRequest,
    PrepareRenameRequest, References, Rename, SemanticTokensFullDeltaRequest,
    SemanticTokensFullRequest, SemanticTokensRangeRequest, SignatureHelpRequest,
    WorkspaceSymbolRequest,
};
use lsp_types::{
    CodeActionKind, CodeActionOptions, CodeActionProviderCapability, CompletionOptions,
    CompletionResponse, GotoDefinitionResponse, Hover, HoverProviderCapability, InitializeParams,
    InlayHint, InlayHintServerCapabilities, OneOf, RenameOptions, SaveOptions, ServerCapabilities,
    SignatureHelp, SignatureHelpOptions, TextDocumentPositionParams, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions, Uri,
    WorkDoneProgressOptions,
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

#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        log::trace!($($arg)*);
    };
}

/// Answer one ordinary LSP request inside `handle_request` and return early.
///
/// Runs the common take-params -> run-workspace-query -> respond flow for a
/// typed request. Handlers with extra steps (the SemanticTokens cache, the
/// SignatureHelp shutdown check) spell out their own `match take::<T>`.
macro_rules! dispatch_request {
    ($req:ident, $out:expr, $ws:expr, $ReqType:ty, $span:literal, $handler:expr) => {
        let $req = match take::<$ReqType>($req) {
            Ok((req_id, args)) => {
                let _span = tracing::info_span!($span, request_id = %req_id).entered();
                let outcome = $handler(&mut $ws, &args);
                send_response($out, req_id, outcome)?;
                return Ok(());
            }
            Err(fault @ ExtractError::JsonError { .. }) => {
                log::error!(
                    concat!("JSON error in ", stringify!($ReqType), " request: {:?}"),
                    fault
                );
                return Ok(());
            }
            Err(ExtractError::MethodMismatch(rest)) => rest,
        };
    };
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
pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        definition_provider: Some(OneOf::Left(true)),
        type_definition_provider: Some(lsp_types::TypeDefinitionProviderCapability::Simple(true)),
        references_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        signature_help_provider: Some(signature_options()),
        inlay_hint_provider: Some(lsp_types::OneOf::Right(
            InlayHintServerCapabilities::Options(lsp_types::InlayHintOptions {
                work_done_progress_options: Default::default(),
                resolve_provider: Some(false),
            }),
        )),
        workspace: Some(lsp_types::WorkspaceServerCapabilities {
            workspace_folders: Some(lsp_types::WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                change_notifications: Some(lsp_types::OneOf::Left(true)),
            }),
            file_operations: None,
        }),
        // Always listed (nvim will not send a command the server left
        // undeclared); completions only bear the command while
        // ROCK_LSP_RECORD records, so this is inert the rest of the time.
        execute_command_provider: Some(lsp_types::ExecuteCommandOptions {
            commands: vec![completion_record::ACCEPT_COMMAND.to_string()],
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        completion_provider: Some(completion_options()),
        text_document_sync: Some(TextDocumentSyncCapability::Options(sync_options())),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![CodeActionKind::REFACTOR_INLINE]),
            work_done_progress_options: WorkDoneProgressOptions::default(),
            resolve_provider: Some(false),
        })),
        semantic_tokens_provider: Some(semantic_tokens::get_semantic_tokens_capabilities()),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        ..Default::default()
    }
}

/// Signature help fires after `(` and after each `,`.
fn signature_options() -> SignatureHelpOptions {
    SignatureHelpOptions {
        trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
        retrigger_characters: None,
        work_done_progress_options: WorkDoneProgressOptions::default(),
    }
}

/// Completion popup configuration.
fn completion_options() -> CompletionOptions {
    CompletionOptions {
        resolve_provider: Some(false),
        // `:` is listed so the second char of `::` pops enum variants for
        // `Foo::|`; the first char alone has no scope yet and simply falls
        // back to plain identifiers. Without it nvim never auto-opens on
        // `::`. `" "` opens the popup on a bare space (`let x = |`) with an
        // `isIncomplete` empty-prefix list that re-scores while typing.
        // See `extract_scope_resolution`.
        trigger_characters: Some(vec![".".to_string(), ":".to_string(), " ".to_string()]),
        work_done_progress_options: WorkDoneProgressOptions {
            work_done_progress: Some(false),
        },
        completion_item: Some(lsp_types::CompletionOptionsCompletionItem {
            label_details_support: Some(false),
        }),
        ..CompletionOptions::default()
    }
}

/// Whole-document sync: the client sends full text on open and change.
fn sync_options() -> TextDocumentSyncOptions {
    TextDocumentSyncOptions {
        open_close: Some(true),
        change: Some(TextDocumentSyncKind::FULL),
        will_save: Some(false),
        will_save_wait_until: Some(false),
        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
            include_text: Some(false),
        })),
    }
}

pub fn run_lsp_server() -> Result<()> {
    run_lsp_server_with_policy(CompilerPolicy::default())
}

pub fn run_lsp_server_with_policy(policy: crate::CompilerPolicy) -> Result<()> {
    log::info!("Starting Rock LSP server ...");
    debug_log!("LSP_SERVER_STARTING: Debug logging enabled");

    let (link, threads) = Connection::stdio();

    let advertised = serde_json::to_value(server_capabilities())?;

    let handshake: serde_json::Value = match link.initialize(advertised) {
        Ok(got) => got,
        Err(broken) => {
            let dead = broken.channel_is_disconnected();
            if dead {
                threads.join()?;
            }

            return Err(anyhow::Error::from(broken));
        }
    };

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        main_loop(link, handshake, policy)
    }));

    match outcome {
        Ok(Ok(_)) => {
            eprintln!("LSP main loop exited successfully");
        }
        Ok(Err(fault)) => {
            eprintln!("LSP main loop exited with error: {fault:?}");
            notify(format!("{fault:?}"));
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

fn main_loop(
    link: Connection,
    params: serde_json::Value,
    policy: crate::CompilerPolicy,
) -> Result<()> {
    let mut server = LspServer::new(policy, params)?;
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
    /// Build the workspace from the client's `initialize` params, once the
    /// handshake (or the direct `initialize` below) has delivered them.
    pub fn new(policy: crate::CompilerPolicy, params: serde_json::Value) -> Result<Self> {
        check_initialize_params(params)?;
        Ok(Self {
            workspace: LspWorkspace::with_policy(policy),
            shutdown_mode: false,
            exited: false,
        })
    }

    /// Reply to `initialize` without lsp-server's blocking handshake, for
    /// hosts that pass JSON-RPC messages through directly.
    pub fn initialize(policy: crate::CompilerPolicy, req: Request) -> Result<(Self, Message)> {
        let made = Self::new(policy, req.params)?;
        let payload = serde_json::json!({ "capabilities": server_capabilities() });
        Ok((made, Response::new_ok(req.id, payload).into()))
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

        // debug!("got request: {req:?}");

        dispatch_request!(
            req,
            out,
            self.workspace,
            GotoDefinition,
            "lsp_goto_definition",
            go_to_definition_workspace
        );
        dispatch_request!(
            req,
            out,
            self.workspace,
            GotoTypeDefinition,
            "lsp_goto_type_definition",
            go_to_type_definition_workspace
        );
        dispatch_request!(
            req,
            out,
            self.workspace,
            References,
            "lsp_references",
            find_references_workspace
        );
        dispatch_request!(
            req,
            out,
            self.workspace,
            HoverRequest,
            "lsp_hover",
            do_hover_workspace
        );

        let req = match take::<SignatureHelpRequest>(req) {
            Ok((req_id, args)) => {
                let _span =
                    tracing::info_span!("lsp_signature_help", request_id = %req_id).entered();

                // Shutting down: fail instead of computing.
                if self.shutdown_mode {
                    out.push(failed_request(
                        req_id,
                        lsp_server::ErrorCode::InvalidRequest,
                        "Server is shutting down".to_string(),
                    ));
                    return Ok(());
                }

                let found = do_signature_help_workspace(&mut self.workspace, &args);
                send_response(out, req_id, found)?;
                return Ok(());
            }
            Err(fault @ ExtractError::JsonError { .. }) => {
                log::error!("JSON error in SignatureHelp request: {fault:?}");
                return Ok(());
            }
            Err(ExtractError::MethodMismatch(rest)) => rest,
        };

        dispatch_request!(
            req,
            out,
            self.workspace,
            InlayHintRequest,
            "lsp_inlay_hints",
            do_inlay_hints_workspace
        );
        dispatch_request!(
            req,
            out,
            self.workspace,
            Completion,
            "lsp_completion",
            do_completion_workspace
        );
        dispatch_request!(
            req,
            out,
            self.workspace,
            PrepareRenameRequest,
            "lsp_prepare_rename",
            prepare_rename_workspace
        );
        dispatch_request!(
            req,
            out,
            self.workspace,
            Rename,
            "lsp_rename",
            do_rename_workspace
        );
        dispatch_request!(
            req,
            out,
            self.workspace,
            lsp_types::request::References,
            "lsp_references_alt",
            find_references_workspace
        );
        dispatch_request!(
            req,
            out,
            self.workspace,
            DocumentSymbolRequest,
            "lsp_document_symbols",
            do_document_symbols_workspace
        );
        dispatch_request!(
            req,
            out,
            self.workspace,
            WorkspaceSymbolRequest,
            "lsp_workspace_symbols",
            do_workspace_symbols_workspace
        );
        dispatch_request!(
            req,
            out,
            self.workspace,
            DocumentHighlightRequest,
            "lsp_document_highlight",
            do_document_highlight_workspace
        );

        let req = match take::<SemanticTokensFullRequest>(req) {
            Ok((req_id, args)) => {
                let _span =
                    tracing::info_span!("lsp_semantic_tokens", request_id = %req_id).entered();

                let uri = args.text_document.uri.to_string();
                let current_version = self.workspace.get_document_version(&uri);

                // debug_log!("SEMANTIC_TOKENS_REQUEST: id={}, uri={}, version={:?}", id, uri, current_version);

                // Check if we have valid cached tokens and clone the data we need
                let (has_cache, cached_is_current, cached_tokens, cached_result_id) = {
                    let _span = tracing::debug_span!("check_semantic_tokens_cache").entered();
                    if let Some(cached) = self
                        .workspace
                        .get_cached_semantic_tokens(&uri, current_version)
                    {
                        let is_current = cached.document_version == current_version;
                        (
                            true,
                            is_current,
                            Some(cached.tokens.clone()),
                            Some(cached.result_id.clone()),
                        )
                    } else {
                        (false, false, None, None)
                    }
                };

                let result = if cached_is_current {
                    // We have current tokens, return them
                    // debug_log!("SEMANTIC_TOKENS_CACHE_HIT (current): id={}, uri={}, result_id={}", id, uri, cached_result_id.as_ref().unwrap());
                    let mut result = cached_tokens.unwrap();
                    // Update the result ID to the current cached one
                    if let lsp_types::SemanticTokensResult::Tokens(ref mut tokens) = result {
                        tokens.result_id = cached_result_id;
                    }
                    Ok(Some(result))
                } else {
                    // Try to compute new tokens
                    // debug_log!("SEMANTIC_TOKENS_CACHE_MISS or STALE: id={}, uri={} - computing new tokens", id, uri);

                    let res = {
                        let _span =
                            tracing::debug_span!("compute_semantic_tokens_workspace").entered();
                        semantic_tokens::compute_semantic_tokens_workspace(
                            &mut self.workspace,
                            args.clone(),
                        )
                    };
                    match res {
                        Ok(Some(tokens)) => {
                            // Defense in depth: if compute_semantic_tokens
                            // returned an empty result but we have a
                            // previously-cached non-empty value, return
                            // the stale cache instead of empty tokens.
                            // Together with the cache_semantic_tokens guard,
                            // this ensures a transient empty result cannot
                            // cause highlighting to disappear in the editor.
                            let new_is_empty = semantic_tokens_result_is_empty(&tokens);
                            let cache_is_nonempty = cached_tokens
                                .as_ref()
                                .map(|c| !semantic_tokens_result_is_empty(c))
                                .unwrap_or(false);

                            if new_is_empty && cache_is_nonempty {
                                log::warn!(
                                    "compute_semantic_tokens returned empty for {}; \
                                     returning stale non-empty cache to preserve highlighting.",
                                    uri
                                );
                                let mut result = cached_tokens.unwrap();
                                if let lsp_types::SemanticTokensResult::Tokens(ref mut t) = result {
                                    t.result_id = cached_result_id;
                                }
                                Ok(Some(result))
                            } else {
                                // Successfully computed new tokens, cache them
                                let result_id = self.workspace.cache_semantic_tokens(
                                    &uri,
                                    tokens.clone(),
                                    current_version,
                                );
                                let mut result = tokens;
                                // Update the result ID
                                if let lsp_types::SemanticTokensResult::Tokens(ref mut t) = result {
                                    t.result_id = Some(result_id.clone());
                                }
                                Ok(Some(result))
                            }
                        }
                        Ok(None) => {
                            // Computation returned no tokens (likely no semantic result in context)
                            // This often happens when switching buffers to a different context
                            // Return stale cache if available to prevent tokens from disappearing
                            if has_cache {
                                // debug_log!("SEMANTIC_TOKENS_NO_RESULT_IN_CONTEXT: returning stale cache for {} to prevent token loss on buffer switch", uri);
                                let mut result = cached_tokens.unwrap();
                                if let lsp_types::SemanticTokensResult::Tokens(ref mut tokens) =
                                    result
                                {
                                    tokens.result_id = cached_result_id;
                                }
                                Ok(Some(result))
                            } else {
                                // debug_log!("SEMANTIC_TOKENS_NO_RESULT_AND_NO_CACHE: {} - likely switching to uncomplied context", uri);
                                Ok(None)
                            }
                        }
                        Err(e) => {
                            // Computation failed, return stale cache if available
                            if has_cache {
                                // debug_log!("SEMANTIC_TOKENS_COMPUTE_ERROR: returning stale cache for {}, error: {:?}", uri, e);
                                let mut result = cached_tokens.unwrap();
                                if let lsp_types::SemanticTokensResult::Tokens(ref mut tokens) =
                                    result
                                {
                                    tokens.result_id = cached_result_id;
                                }
                                Ok(Some(result))
                            } else {
                                Err(e)
                            }
                        }
                    }
                };

                send_response(out, req_id, result)?;
                return Ok(());
            }
            Err(fault @ ExtractError::JsonError { .. }) => {
                log::error!("JSON error in SemanticTokens request: {fault:?}");
                return Ok(());
            }
            Err(ExtractError::MethodMismatch(rest)) => rest,
        };

        let req = match take::<SemanticTokensFullDeltaRequest>(req) {
            Ok((req_id, args)) => {
                let _span = tracing::info_span!("lsp_semantic_tokens_delta", request_id = %req_id)
                    .entered();
                let uri = args.text_document.uri.to_string();
                let current_version = self.workspace.get_document_version(&uri);
                let prev_id = args.previous_result_id.clone();

                // Snapshot what the client currently holds (the array it
                // got under `prev_id`), then compute the new tokens.
                let prev = self
                    .workspace
                    .get_cached_semantic_tokens(&uri, current_version)
                    .map(|c| (c.result_id.clone(), c.tokens.clone()));

                let full_params = lsp_types::SemanticTokensParams {
                    work_done_progress_params: args.work_done_progress_params.clone(),
                    partial_result_params: args.partial_result_params.clone(),
                    text_document: args.text_document.clone(),
                };
                let computed = semantic_tokens::compute_semantic_tokens_workspace(
                    &mut self.workspace,
                    full_params,
                );

                let result = match computed {
                    Ok(Some(new_result)) if !semantic_tokens_result_is_empty(&new_result) => {
                        let new_id = self.workspace.cache_semantic_tokens(
                            &uri,
                            new_result.clone(),
                            current_version,
                        );
                        // Delta only when the client's array matches what
                        // we cached under `prev_id`; otherwise send full.
                        match (&prev, &new_result) {
                            (
                                Some((old_id, lsp_types::SemanticTokensResult::Tokens(old))),
                                lsp_types::SemanticTokensResult::Tokens(new_t),
                            ) if *old_id == prev_id && !old.data.is_empty() => {
                                let edits = semantic_tokens::semantic_tokens_delta_edits(
                                    &old.data,
                                    &new_t.data,
                                );
                                Ok(Some(lsp_types::SemanticTokensFullDeltaResult::TokensDelta(
                                    lsp_types::SemanticTokensDelta {
                                        result_id: Some(new_id),
                                        edits,
                                    },
                                )))
                            }
                            _ => {
                                // Cache miss / id mismatch: full refresh.
                                let mut full = match new_result {
                                    lsp_types::SemanticTokensResult::Tokens(t) => t,
                                    lsp_types::SemanticTokensResult::Partial(_) => {
                                        Default::default()
                                    }
                                };
                                full.result_id = Some(new_id);
                                Ok(Some(lsp_types::SemanticTokensFullDeltaResult::Tokens(full)))
                            }
                        }
                    }
                    // Empty/None/Err: don't wipe highlighting -- resend the
                    // stale cache as a full result if we have one, else nothing.
                    other => {
                        if let Err(e) = other {
                            log::warn!("semantic tokens delta compute failed for {uri}: {e:?}");
                        }
                        match prev {
                            Some((old, lsp_types::SemanticTokensResult::Tokens(mut t))) => {
                                t.result_id = Some(old);
                                Ok(Some(lsp_types::SemanticTokensFullDeltaResult::Tokens(t)))
                            }
                            _ => Ok(None),
                        }
                    }
                };

                send_response(out, req_id, result)?;
                return Ok(());
            }
            Err(fault @ ExtractError::JsonError { .. }) => {
                log::error!("JSON error in SemanticTokens delta request: {fault:?}");
                return Ok(());
            }
            Err(ExtractError::MethodMismatch(rest)) => rest,
        };

        let req = match take::<SemanticTokensRangeRequest>(req) {
            Ok((req_id, args)) => {
                let _span = tracing::info_span!("lsp_semantic_tokens_range", request_id = %req_id)
                    .entered();
                let found = semantic_tokens::compute_semantic_tokens_range_workspace(
                    &mut self.workspace,
                    args,
                );
                send_response(out, req_id, found)?;
                return Ok(());
            }
            Err(fault @ ExtractError::JsonError { .. }) => {
                log::error!("JSON error in SemanticTokens range request: {fault:?}");
                return Ok(());
            }
            Err(ExtractError::MethodMismatch(rest)) => rest,
        };

        dispatch_request!(
            req,
            out,
            self.workspace,
            CodeActionRequest,
            "lsp_code_actions",
            compute_code_actions_workspace
        );

        // Custom introspection requests for the rock.nvim panel. Bone
        // ports of the old rock-semantic endpoints (rock/consistency was
        // dropped in the port -- it had no bone equivalent worth the
        // surface).
        if req.method == "rock/stats" {
            let loaded = self
                .workspace
                .bone
                .projects_summary()
                .iter()
                .map(|(_, m)| m.len())
                .sum();
            send_response(out, req.id, Ok(request_stats_json(loaded)))?;
            return Ok(());
        }
        if req.method == "rock/modules" {
            let snapshot = modules_json(self.workspace.bone.projects_summary());
            send_response(out, req.id, Ok(snapshot))?;
            return Ok(());
        }

        // `rock.completionAccepted`: the corpus recorder's accept signal,
        // run by the client after inserting a tagged completion item
        // (ROCK_LSP_RECORD).
        if req.method == "workspace/executeCommand" {
            let parsed =
                serde_json::from_value::<lsp_types::ExecuteCommandParams>(req.params.clone())
                    .map_err(|e| format!("malformed executeCommand params: {e}"))
                    .and_then(|p| completion_record::handle_execute_command(&p));
            match parsed {
                Ok(v) => send_response(out, req.id, Ok(v))?,
                Err(detail) => {
                    log::warn!("workspace/executeCommand rejected: {detail}");
                    out.push(failed_request(
                        req.id,
                        lsp_server::ErrorCode::InvalidParams,
                        detail,
                    ));
                }
            }
            return Ok(());
        }

        // No arm claimed this request.
        log::warn!("Unhandled request method: {}", req.method);
        let name = req.method.clone();
        out.push(failed_request(
            req.id,
            lsp_server::ErrorCode::MethodNotFound,
            format!("Method '{name}' not found"),
        ));
        Ok(())
    }

    fn handle_notification(
        &mut self,
        not: lsp_server::Notification,
        out: &mut Vec<Message>,
    ) -> Result<()> {
        // debug_log!("NOTIFICATION_PROCESSING: method={}", not.method);

        // Handle exit notification
        if not.method == "exit" {
            // debug_log!("EXIT_NOTIFICATION_RECEIVED");
            // Exit the main loop when we receive the exit notification
            self.exited = true;
            return Ok(());
        }

        // In shutdown mode, ignore all other notifications
        if self.shutdown_mode {
            // debug_log!("IGNORING_NOTIFICATION_AFTER_SHUTDOWN: method={}", not.method);
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
        } else {
            // debug_log!("UNEXPECTED_NOTIFICATION: method={}", not.method);
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
        let opened = opened_doc(note)?;
        let Some(path) = notification_path(out, &opened.uri, "didOpen")? else {
            return Ok(());
        };
        self.workspace
            .update_document_version(&opened.uri.to_string(), opened.version);
        bone_reload_and_publish(out, &path, opened.text, &mut self.workspace)?;
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
        let body = note.params.as_object().ok_or_else(|| {
            anyhow::anyhow!("Invalid didChange notification: params is not an object")
        })?;
        let edited = changed_doc(body)?;
        self.workspace
            .update_document_version(&at.to_string(), edited.version);
        // The refresh makes the client re-request hints for the edited text.
        out.push(lsp_server::Message::Notification(
            lsp_server::Notification {
                method: "workspace/inlayHint/refresh".to_string(),
                params: serde_json::Value::Null,
            },
        ));
        bone_reload_and_publish(out, &path, edited.text, &mut self.workspace)?;
        Ok(())
    }

    /// Forget a closed document on both layers: the bone session entry and
    /// the per-document LSP caches.
    fn on_close(&mut self, note: &lsp_server::Notification, out: &mut Vec<Message>) -> Result<()> {
        let _span = tracing::debug_span!("lsp_did_close_notification").entered();
        let at = doc_uri(note)?;
        log::debug!("got textDocument/didClose for {}", at.as_str());
        let Some(path) = notification_path(out, &at, "didClose")? else {
            return Ok(());
        };
        self.workspace.bone.remove(&path);
        self.workspace.close_document(at.as_str());
        Ok(())
    }
}

fn check_initialize_params(params: serde_json::Value) -> Result<()> {
    let mut params_val = params;

    // It has been observed that some LSP clients (e.g., some versions of Neovim's native LSP)
    // announce semantic token capabilities but omit the required `tokenTypes`, `tokenModifiers`,
    // and `formats` fields, which violates the LSP specification.
    // This non-compliance causes deserialization of `InitializeParams` to fail.
    // To ensure robustness, we defensively patch the `semanticTokens` object here by
    // inserting empty or default values for these missing fields before deserialization.
    if let Some(semantic_tokens) = params_val
        .get_mut("capabilities")
        .and_then(|caps| caps.get_mut("textDocument"))
        .and_then(|td| td.get_mut("semanticTokens"))
    {
        // Log what the CLIENT declares it supports BEFORE we patch defaults in.
        // `requests.range` / `requests.full.delta` decide whether advertising
        // semanticTokens/range or /full/delta is even useful -- a client that
        // doesn't list a request kind won't send it no matter what we advertise.
        let requests = semantic_tokens.get("requests").cloned();
        log::info!(
            "[semtok-caps] client semanticTokens.requests = {}",
            requests
                .map(|r| r.to_string())
                .unwrap_or_else(|| "<absent>".to_string())
        );
        if let Some(obj) = semantic_tokens.as_object_mut() {
            if obj.get("tokenTypes").is_none() {
                log::warn!(
                    "Client sent non-compliant semantic token capabilities. Patching missing 'tokenTypes'."
                );
                obj.insert("tokenTypes".to_string(), serde_json::json!([]));
            }
            if obj.get("tokenModifiers").is_none() {
                log::warn!(
                    "Client sent non-compliant semantic token capabilities. Patching missing 'tokenModifiers'."
                );
                obj.insert("tokenModifiers".to_string(), serde_json::json!([]));
            }
            // Per LSP spec, `relative` is the only format currently defined.
            // It's a safe default to assume if the client supports semantic tokens at all.
            if obj.get("formats").is_none() {
                log::warn!(
                    "Client sent non-compliant semantic token capabilities. Patching missing 'formats'."
                );
                obj.insert("formats".to_string(), serde_json::json!(["relative"]));
            }
            // The 'requests' field is also required by the spec
            if obj.get("requests").is_none() {
                log::warn!(
                    "Client sent non-compliant semantic token capabilities. Patching missing 'requests'."
                );
                obj.insert(
                    "requests".to_string(),
                    serde_json::json!({
                        "full": true,
                        "range": false
                    }),
                );
            }
        }
    }

    // TODO: are these not needed?
    let _params: InitializeParams = serde_json::from_value(params_val.clone()).map_err(|err| {
        // Log the exact payload that failed deserialization (after patching)
        eprintln!(
            "Failed to deserialize InitializeParams after patching: {}. Payload: {}",
            err,
            serde_json::to_string_pretty(&params_val)
                .unwrap_or_else(|_| "INVALID JSON".to_string())
        );
        err
    })?;
    // fs_err::write("/tmp/lsp_debug.log", "MAIN_LOOP_STARTED\n").ok();
    log::info!("Starting example main loop");

    // let _lsp_context = LspContext::new();

    // debug_log!("LSP_SERVER_INITIALIZED: Workspace created, entering main loop");

    Ok(())
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
            version: None,
            diagnostics,
        };
        out.push(lsp_server::Message::Notification(
            lsp_server::Notification {
                method: lsp_types::notification::PublishDiagnostics::METHOD.to_string(),
                params: serde_json::to_value(notice)?,
            },
        ));
    }
    Ok(())
}

/// Pull the typed params out of a request when its method matches.
///
/// Returns the untouched request on a method mismatch so dispatch can try
/// the next arm; a JSON shape error is reported to the caller instead.
fn take<R>(seen: Request) -> Result<(RequestId, R::Params), ExtractError<Request>>
where
    R: lsp_types::request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    seen.extract(R::METHOD)
}

/// An error response message for a request that cannot be answered.
fn failed_request(id: RequestId, code: lsp_server::ErrorCode, text: String) -> Message {
    Message::Response(Response {
        id,
        result: None,
        error: Some(lsp_server::ResponseError {
            code: code as i32,
            message: text,
            data: None,
        }),
    })
}

/// The document a notification talks about, read from its `textDocument.uri`.
fn doc_uri(note: &lsp_server::Notification) -> Result<Uri> {
    note.params
        .get("textDocument")
        .and_then(|doc| doc.get("uri"))
        .and_then(|raw| raw.as_str())
        .and_then(|text| Uri::from_str(text).ok())
        .ok_or_else(|| anyhow::anyhow!("notification has no textDocument.uri"))
}

/// The versioned text a didOpen carries.
struct OpenedDoc {
    uri: Uri,
    version: Option<i32>,
    text: String,
}

fn opened_doc(note: &lsp_server::Notification) -> Result<OpenedDoc> {
    let at = note
        .params
        .get("textDocument")
        .and_then(|doc| doc.get("uri"))
        .and_then(|raw| raw.as_str())
        .and_then(|text| Uri::from_str(text).ok())
        .ok_or_else(|| anyhow::anyhow!("Failed to get uri from didOpen notification"))?;
    let version = note
        .params
        .get("textDocument")
        .and_then(|doc| doc.get("version"))
        .and_then(|raw| raw.as_i64())
        .map(|v| v as i32);
    let text = note
        .params
        .get("textDocument")
        .and_then(|doc| doc.get("text"))
        .and_then(|raw| raw.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Failed to get contents from didOpen notification"))?;
    Ok(OpenedDoc {
        uri: at,
        version,
        text,
    })
}

/// The new version and full text a didChange carries (FULL sync sends the
/// whole buffer as the first content change).
struct ChangedDoc {
    version: Option<i32>,
    text: String,
}

fn changed_doc(body: &serde_json::Map<String, serde_json::Value>) -> Result<ChangedDoc> {
    let version = body
        .get("textDocument")
        .and_then(|doc| doc.get("version"))
        .and_then(|raw| raw.as_i64())
        .map(|v| v as i32);
    let text = body
        .get("contentChanges")
        .and_then(|edits| edits.as_array())
        .and_then(|edits| edits.first())
        .and_then(|edit| edit.get("text"))
        .and_then(|raw| raw.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Failed to get contents from notification"))?;
    Ok(ChangedDoc { version, text })
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
            out.push(lsp_server::Message::Notification(
                lsp_server::Notification {
                    method: lsp_types::notification::ShowMessage::METHOD.to_string(),
                    params: serde_json::to_value(params)?,
                },
            ));
            Ok(None)
        }
    }
}

/// Answer a request: serialize the handler's value into a response, or the
/// handler's failure into a `RequestFailed` error. A value that cannot
/// serialize becomes `InternalError`.
fn send_response<T>(
    out: &mut Vec<Message>,
    id: RequestId,
    t: Result<T, anyhow::Error>,
) -> Result<(), anyhow::Error>
where
    T: serde::Serialize,
{
    let made = match t {
        Ok(done) => serde_json::to_value(&done).map_err(|e| {
            let detail = format!("failed to serialize response: {e}");
            error!("{detail}");
            (lsp_server::ErrorCode::InternalError, detail)
        }),
        Err(fault) => {
            log::error!("error: {fault}");
            Err((lsp_server::ErrorCode::RequestFailed, fault.to_string()))
        }
    };
    let answer = match made {
        Ok(payload) => Response {
            id,
            result: Some(payload),
            error: None,
        },
        Err((code, detail)) => Response {
            id,
            result: None,
            error: Some(lsp_server::ResponseError {
                code: code as i32,
                message: detail,
                data: None,
            }),
        },
    };
    out.push(Message::Response(answer));
    Ok(())
}

fn notify(_msg: impl AsRef<str>) {
    // TODO: neat, but we don't really like/use this much
    // if let Err(e) = notify_rust::Notification::new()
    //     .summary(&format!("[rock-lsp]: {}", msg.as_ref()))
    //     .show()
    // {
    //     log::error!("Failed to show notification: {}", e);
    // }
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
        send_response(&mut out, RequestId::from(7), Ok(Unserializable)).unwrap();
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
