#![allow(clippy::mutable_key_type)]

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
use log::{error, info};
use lsp_server::{Connection, ExtractError, Message, Request, RequestId, Response};
use lsp_types::{
    notification::Notification,
    request::{
        CodeActionRequest, Completion, DocumentHighlightRequest, DocumentSymbolRequest,
        GotoDefinition, GotoTypeDefinition, GotoTypeDefinitionParams, HoverRequest, InlayHintRequest,
        PrepareRenameRequest, References, Rename, SemanticTokensFullDeltaRequest,
        SemanticTokensFullRequest, SemanticTokensRangeRequest, SignatureHelpRequest,
        WorkspaceSymbolRequest,
    },
    CodeActionKind, CodeActionOptions, CodeActionProviderCapability, CompletionOptions,
    CompletionResponse, GotoDefinitionResponse, Hover, HoverProviderCapability, InitializeParams,
    InlayHint, InlayHintServerCapabilities, OneOf, RenameOptions, SaveOptions, ServerCapabilities,
    SignatureHelp, SignatureHelpOptions, TextDocumentPositionParams, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions, Uri,
    WorkDoneProgressOptions,
};

pub mod cli;
mod code_actions;
pub mod semantic_tokens;
pub mod server_cli;
pub mod workspace;
mod workspace_adapters;

pub use server_cli::run_cli;

pub use code_actions::*;
pub use workspace::*;
pub use workspace_adapters::*;

pub fn debug_write(msg: String) {
    log::trace!("{}", msg);
}

#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        log::trace!($($arg)*);
    };
}

/// Collapse a standard LSP request dispatch arm into one line.
///
/// Handles the common `cast::<T>` → `send_response` → continue pattern.
/// Leaves complex handlers (SemanticTokens caching, SignatureHelp shutdown
/// guard) as explicit `match cast::<T>` blocks.
macro_rules! dispatch_request {
    ($req:ident, $conn:expr, $ws:expr, $ReqType:ty, $span:literal, $handler:expr) => {
        let $req = match cast::<$ReqType>($req) {
            Ok((id, params)) => {
                let _span = tracing::info_span!($span, request_id = %id).entered();
                let result = $handler(&mut $ws, &params);
                send_response(&$conn, id, result)?;
                continue;
            }
            Err(err @ ExtractError::JsonError { .. }) => {
                log::error!(
                    concat!("JSON error in ", stringify!($ReqType), " request: {:?}"),
                    err
                );
                continue;
            }
            Err(ExtractError::MethodMismatch(req)) => req,
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
    start: std::time::Instant,
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
/// bone doesn't track per-module compile timing/staleness, so those fields are
/// null/false (the floater renders them as `-`).
fn modules_json(summary: Vec<(String, Vec<String>)>) -> serde_json::Value {
    let total: usize = summary.iter().map(|(_, ids)| ids.len()).sum();
    let contexts: Vec<serde_json::Value> = summary
        .into_iter()
        .map(|(root, ids)| {
            let module_count = ids.len();
            let modules: Vec<serde_json::Value> = ids
                .into_iter()
                .map(|id| {
                    serde_json::json!({
                        "id": id,
                        "last_compile_us": serde_json::Value::Null,
                        "ago_ms": serde_json::Value::Null,
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


pub fn run_lsp_server() -> Result<()> {
    info!("Starting Rock LSP server ...");
    debug_log!("LSP_SERVER_STARTING: Debug logging enabled");

    let (connection, io_threads) = Connection::stdio();

    let server_capabilities = serde_json::to_value(&ServerCapabilities {
        definition_provider: Some(OneOf::Left(true)),
        type_definition_provider: Some(lsp_types::TypeDefinitionProviderCapability::Simple(true)),
        references_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),

        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
            retrigger_characters: None,
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),

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

        completion_provider: Some(CompletionOptions {
            resolve_provider: Some(false),
            // `:` fires on every char of `::` — the first one is harmless
            // (no scope context yet, falls through to identifier completion);
            // the second one is what we actually want, surfacing
            // enum-variant completions for `Foo::|`. Without `:` in this
            // list, nvim's built-in completion never auto-triggers on
            // `::` and the popup stays empty until the user manually
            // invokes omnifunc. See `extract_scope_resolution`.
            // `" "` opens the popup on a bare space (empty prefix), so the
            // namespace / arg-filtered set is visible before the first letter
            // is typed (`let x = |`, `return |`); the empty-prefix result is
            // served `isIncomplete`, so it re-scores as the user keeps typing.
            trigger_characters: Some(vec![".".to_string(), ":".to_string(), " ".to_string()]),
            all_commit_characters: None,
            work_done_progress_options: WorkDoneProgressOptions {
                work_done_progress: Some(false),
            },
            completion_item: Some(lsp_types::CompletionOptionsCompletionItem {
                label_details_support: Some(false),
            }),
        }),

        text_document_sync: Some(TextDocumentSyncCapability::Options(
            text_sync_options(),
        )),
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
    })?;

    let initialization_params = match connection.initialize(server_capabilities) {
        Ok(it) => it,
        Err(e) => {
            if e.channel_is_disconnected() {
                io_threads.join()?;
            }

            return Err(e.into());
        }
    };

    let main_loop_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        main_loop(connection, initialization_params)
    }));

    match main_loop_result {
        Ok(Ok(_)) => {
            eprintln!("LSP main loop exited successfully");
        }
        Ok(Err(err)) => {
            eprintln!("LSP main loop exited with error: {err:?}");
            notify(format!("{err:?}"));
        }
        Err(panic_payload) => {
            eprintln!("=== LSP MAIN LOOP PANICKED ===");
            if let Some(s) = panic_payload.downcast_ref::<&str>() {
                eprintln!("Panic message (str): {}", s);
            } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                eprintln!("Panic message (String): {}", s);
            } else {
                eprintln!("Panic with unknown payload type: {:?}", panic_payload);
            }
            eprintln!("=== END PANIC INFO ===");
            // Do NOT fall through to `io_threads.join()`: the reader thread is
            // blocked on stdin, so join() waits for EOF the client won't send
            // while it still thinks the server is alive. That leaves a wedged
            // process holding its compiled session in RAM, silently swallowing
            // every subsequent request. Exit so the client sees the crash and
            // restarts cleanly.
            std::process::exit(1);
        }
    };

    eprintln!("Shutting down LSP server...");
    io_threads.join()?;
    info!("Shutting down Rock LSP server");
    // notify("Shutting down Rock LSP server");

    Ok(())
}

fn main_loop(connection: Connection, params: serde_json::Value) -> Result<()> {
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
        log::info!("[semtok-caps] client semanticTokens.requests = {}",
            requests.map(|r| r.to_string()).unwrap_or_else(|| "<absent>".to_string()));
        if let Some(obj) = semantic_tokens.as_object_mut() {
            if obj.get("tokenTypes").is_none() {
                log::warn!("Client sent non-compliant semantic token capabilities. Patching missing 'tokenTypes'.");
                obj.insert("tokenTypes".to_string(), serde_json::json!([]));
            }
            if obj.get("tokenModifiers").is_none() {
                log::warn!("Client sent non-compliant semantic token capabilities. Patching missing 'tokenModifiers'.");
                obj.insert("tokenModifiers".to_string(), serde_json::json!([]));
            }
            // Per LSP spec, `relative` is the only format currently defined.
            // It's a safe default to assume if the client supports semantic tokens at all.
            if obj.get("formats").is_none() {
                log::warn!("Client sent non-compliant semantic token capabilities. Patching missing 'formats'.");
                obj.insert("formats".to_string(), serde_json::json!(["relative"]));
            }
            // The 'requests' field is also required by the spec
            if obj.get("requests").is_none() {
                log::warn!("Client sent non-compliant semantic token capabilities. Patching missing 'requests'.");
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
    info!("Starting example main loop");

    // let _lsp_context = LspContext::new();
    let mut workspace = LspWorkspace::new();
    let mut shutdown_mode = false;

    // debug_log!("LSP_SERVER_INITIALIZED: Workspace created, entering main loop");

    for msg in &connection.receiver {
        // Only log non-verbose message types to avoid spam
        match &msg {
            Message::Request(_req) => {
                // debug_log!("REQUEST_RECEIVED: method={}, id={:?}", req.method, req.id);
            }
            Message::Notification(_not) => {
                // debug_log!("NOTIFICATION_RECEIVED: method={}", not.method);
            }
            _ => {} // Skip logging responses
        }

        match msg {
            Message::Request(req) => {
                log::debug!("MESSAGE RECEIVED: method={}, id={:?}", req.method, req.id);
                if connection.handle_shutdown(&req)? {
                    // debug_log!("SHUTDOWN_HANDLED");
                    shutdown_mode = true;
                    // Continue processing to handle the exit notification
                    continue; // Prevent double-processing of shutdown request
                }

                // In shutdown mode, ignore all requests except exit
                if shutdown_mode {
                    // debug_log!("IGNORING_REQUEST_AFTER_SHUTDOWN: method={}", req.method);
                    // Send an error response for any request after shutdown
                    let resp = Response {
                        id: req.id,
                        result: None,
                        error: Some(lsp_server::ResponseError {
                            code: lsp_server::ErrorCode::InvalidRequest as i32,
                            message: "Server is shutting down".to_string(),
                            data: None,
                        }),
                    };
                    connection.sender.send(Message::Response(resp))?;
                    continue;
                }

                // Server-side handle timer. Dropped at the `continue` of
                // whichever dispatch arm answers this request, so it captures
                // dequeue -> response-sent for every request type. Always
                // records into REQUEST_STATS (feeds rock/stats); only logs the
                // per-request line when ROCK_LSP_TIMING=1.
                let _timing = ReqTiming {
                    method: req.method.clone(),
                    id: req.id.clone(),
                    start: std::time::Instant::now(),
                };

                // Deterministic panic trigger for the wedge-recovery test. Inert
                // unless ROCK_LSP_ALLOW_TEST_PANIC is set, so a real client can
                // never crash the server with it.
                if req.method == "$/rockDebugPanic"
                    && std::env::var_os("ROCK_LSP_ALLOW_TEST_PANIC").is_some()
                {
                    panic!("deliberate test panic ($/rockDebugPanic)");
                }

                // debug!("got request: {req:?}");

                dispatch_request!(req, connection, workspace, GotoDefinition, "lsp_goto_definition", go_to_definition_workspace);
                dispatch_request!(req, connection, workspace, GotoTypeDefinition, "lsp_goto_type_definition", go_to_type_definition_workspace);
                dispatch_request!(req, connection, workspace, References, "lsp_references", find_references_workspace);
                dispatch_request!(req, connection, workspace, HoverRequest, "lsp_hover", do_hover_workspace);

                let req = match cast::<SignatureHelpRequest>(req) {
                    Ok((id, params)) => {
                        let _span =
                            tracing::info_span!("lsp_signature_help", request_id = %id).entered();

                        // In shutdown mode, respond with error instead of processing
                        if shutdown_mode {
                            let resp = Response::new_err(
                                id,
                                lsp_server::ErrorCode::InvalidRequest as i32,
                                "Server is shutting down".to_string(),
                            );
                            connection.sender.send(Message::Response(resp))?;
                            continue;
                        }

                        let result = do_signature_help_workspace(&mut workspace, &params);
                        send_response(&connection, id, result)?;
                        continue;
                    }
                    Err(err @ ExtractError::JsonError { .. }) => {
                        log::error!("JSON error in SignatureHelp request: {err:?}");
                        continue;
                    }
                    Err(ExtractError::MethodMismatch(req)) => req,
                };

                dispatch_request!(req, connection, workspace, InlayHintRequest, "lsp_inlay_hints", do_inlay_hints_workspace);
                dispatch_request!(req, connection, workspace, Completion, "lsp_completion", do_completion_workspace);
                dispatch_request!(req, connection, workspace, PrepareRenameRequest, "lsp_prepare_rename", prepare_rename_workspace);
                dispatch_request!(req, connection, workspace, Rename, "lsp_rename", do_rename_workspace);
                dispatch_request!(req, connection, workspace, lsp_types::request::References, "lsp_references_alt", find_references_workspace);
                dispatch_request!(req, connection, workspace, DocumentSymbolRequest, "lsp_document_symbols", do_document_symbols_workspace);
                dispatch_request!(req, connection, workspace, WorkspaceSymbolRequest, "lsp_workspace_symbols", do_workspace_symbols_workspace);
                dispatch_request!(req, connection, workspace, DocumentHighlightRequest, "lsp_document_highlight", do_document_highlight_workspace);

                let req = match cast::<SemanticTokensFullRequest>(req) {
                    Ok((id, params)) => {
                        let _span =
                            tracing::info_span!("lsp_semantic_tokens", request_id = %id).entered();

                        let uri = params.text_document.uri.to_string();
                        let current_version = workspace.get_document_version(&uri);

                        // debug_log!("SEMANTIC_TOKENS_REQUEST: id={}, uri={}, version={:?}", id, uri, current_version);

                        // Check if we have valid cached tokens and clone the data we need
                        let (has_cache, cached_is_current, cached_tokens, cached_result_id) = {
                            let _span =
                                tracing::debug_span!("check_semantic_tokens_cache").entered();
                            if let Some(cached) =
                                workspace.get_cached_semantic_tokens(&uri, current_version)
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
                            if let lsp_types::SemanticTokensResult::Tokens(ref mut tokens) = result
                            {
                                tokens.result_id = cached_result_id;
                            }
                            Ok(Some(result))
                        } else {
                            // Try to compute new tokens
                            // debug_log!("SEMANTIC_TOKENS_CACHE_MISS or STALE: id={}, uri={} - computing new tokens", id, uri);

                            let res = {
                                let _span =
                                    tracing::debug_span!("compute_semantic_tokens_workspace")
                                        .entered();
                                semantic_tokens::compute_semantic_tokens_workspace(
                                    &mut workspace,
                                    params.clone(),
                                )
                            }; match res {
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
                                        if let lsp_types::SemanticTokensResult::Tokens(
                                            ref mut t,
                                        ) = result
                                        {
                                            t.result_id = cached_result_id;
                                        }
                                        Ok(Some(result))
                                    } else {
                                        // Successfully computed new tokens, cache them
                                        let result_id = workspace.cache_semantic_tokens(
                                            &uri,
                                            tokens.clone(),
                                            current_version,
                                        );
                                        let mut result = tokens;
                                        // Update the result ID
                                        if let lsp_types::SemanticTokensResult::Tokens(
                                            ref mut t,
                                        ) = result
                                        {
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
                                        if let lsp_types::SemanticTokensResult::Tokens(
                                            ref mut tokens,
                                        ) = result
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
                                        if let lsp_types::SemanticTokensResult::Tokens(
                                            ref mut tokens,
                                        ) = result
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

                        send_response(&connection, id, result)?;
                        continue;
                    }
                    Err(err @ ExtractError::JsonError { .. }) => {
                        log::error!("JSON error in SemanticTokens request: {err:?}");
                        continue;
                    }
                    Err(ExtractError::MethodMismatch(req)) => req,
                };

                let req = match cast::<SemanticTokensFullDeltaRequest>(req) {
                    Ok((id, params)) => {
                        let _span = tracing::info_span!("lsp_semantic_tokens_delta", request_id = %id).entered();
                        let uri = params.text_document.uri.to_string();
                        let current_version = workspace.get_document_version(&uri);
                        let prev_id = params.previous_result_id.clone();

                        // Snapshot what the client currently holds (the array it
                        // got under `prev_id`), then compute the new tokens.
                        let prev = workspace
                            .get_cached_semantic_tokens(&uri, current_version)
                            .map(|c| (c.result_id.clone(), c.tokens.clone()));

                        let full_params = lsp_types::SemanticTokensParams {
                            work_done_progress_params: params.work_done_progress_params.clone(),
                            partial_result_params: params.partial_result_params.clone(),
                            text_document: params.text_document.clone(),
                        };
                        let computed = semantic_tokens::compute_semantic_tokens_workspace(
                            &mut workspace,
                            full_params,
                        );

                        let result = match computed {
                            Ok(Some(new_result)) if !semantic_tokens_result_is_empty(&new_result) => {
                                let new_id = workspace.cache_semantic_tokens(
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
                                    Some((id, lsp_types::SemanticTokensResult::Tokens(mut t))) => {
                                        t.result_id = Some(id);
                                        Ok(Some(lsp_types::SemanticTokensFullDeltaResult::Tokens(t)))
                                    }
                                    _ => Ok(None),
                                }
                            }
                        };

                        send_response(&connection, id, result)?;
                        continue;
                    }
                    Err(err @ ExtractError::JsonError { .. }) => {
                        log::error!("JSON error in SemanticTokens delta request: {err:?}");
                        continue;
                    }
                    Err(ExtractError::MethodMismatch(req)) => req,
                };

                let req = match cast::<SemanticTokensRangeRequest>(req) {
                    Ok((id, params)) => {
                        let _span = tracing::info_span!("lsp_semantic_tokens_range", request_id = %id).entered();
                        let result = semantic_tokens::compute_semantic_tokens_range_workspace(
                            &mut workspace,
                            params,
                        );
                        send_response(&connection, id, result)?;
                        continue;
                    }
                    Err(err @ ExtractError::JsonError { .. }) => {
                        log::error!("JSON error in SemanticTokens range request: {err:?}");
                        continue;
                    }
                    Err(ExtractError::MethodMismatch(req)) => req,
                };

                dispatch_request!(req, connection, workspace, CodeActionRequest, "lsp_code_actions", compute_code_actions_workspace);

                // Custom introspection requests for the rock.nvim panel. Bone
                // ports of the old rock-semantic endpoints (rock/consistency was
                // dropped in the port -- it had no bone equivalent worth the
                // surface).
                if req.method == "rock/stats" {
                    let loaded = workspace.bone.projects_summary().iter().map(|(_, m)| m.len()).sum();
                    send_response(&connection, req.id, Ok(request_stats_json(loaded)))?;
                    continue;
                }
                if req.method == "rock/modules" {
                    let snapshot = modules_json(workspace.bone.projects_summary());
                    send_response(&connection, req.id, Ok(snapshot))?;
                    continue;
                }

                // If we reach here, the request was not handled
                log::warn!("Unhandled request method: {}", req.method);
                let resp = Response {
                    id: req.id,
                    result: None,
                    error: Some(lsp_server::ResponseError {
                        code: lsp_server::ErrorCode::MethodNotFound as i32,
                        message: format!("Method '{}' not found", req.method),
                        data: None,
                    }),
                };
                connection.sender.send(Message::Response(resp))?;
            }

            Message::Response(_resp) => {
                // debug_log!("RESPONSE_RECEIVED: id={:?}", resp.id);
            }

            Message::Notification(not) => {
                // debug_log!("NOTIFICATION_PROCESSING: method={}", not.method);

                // Handle exit notification
                if not.method == "exit" {
                    // debug_log!("EXIT_NOTIFICATION_RECEIVED");
                    // Exit the main loop when we receive the exit notification
                    return Ok(());
                }

                // In shutdown mode, ignore all other notifications
                if shutdown_mode {
                    // debug_log!("IGNORING_NOTIFICATION_AFTER_SHUTDOWN: method={}", not.method);
                    continue;
                }

                match not.method.as_str() {
                    "textDocument/didSave" => {
                        let _span = tracing::debug_span!("lsp_did_save_notification").entered();

                        let uri = notification_uri(&not)?;

                        // debug_log!(
                        //     "DID_SAVE_RECEIVED: uri={}, current_version={:?}",
                        //     uri.to_string(), workspace.get_document_version(&uri.to_string())
                        // );

                        // With TextDocumentSyncKind::FULL, we should NOT reload from disk on save
                        // because we already have the correct content from didChange notifications.
                        // Reloading from disk can cause race conditions and content mismatches.
                        //
                        // The editor sends:
                        // 1. didChange with in-memory content
                        // 2. didSave immediately after
                        //
                        // If we reload from disk on didSave, we might get stale content.

                        // Just log that we received the save notification
                        log::debug!(
                            "got textDocument/didSave notification for {}",
                            uri.path().as_str()
                        );

                        // Regenerate the project's `.rock.tags` on save (the
                        // conventional ctags cadence -- no per-keystroke cost).
                        // Sourced from the live bone session, so it stays fresh
                        // even in pure-edit sessions that never run `rock compile`.
                        regenerate_ctags(&workspace, &uri, "save");
                    }

                    "textDocument/didOpen" => {
                        let _span = tracing::debug_span!("lsp_did_open_notification").entered();

                        // Extract content from didOpen notification (like didChange does)
                        let uri = not
                            .params
                            .as_object()
                            .and_then(|o| o.get("textDocument"))
                            .and_then(|o| o.get("uri"))
                            .and_then(|v| v.as_str())
                            .and_then(|s| Uri::from_str(s).ok())
                            .ok_or_else(|| {
                                anyhow::anyhow!("Failed to get uri from didOpen notification")
                            })?;

                        let version = not
                            .params
                            .as_object()
                            .and_then(|o| o.get("textDocument"))
                            .and_then(|o| o.get("version"))
                            .and_then(|v| v.as_i64())
                            .map(|v| v as i32);

                        let contents = not
                            .params
                            .as_object()
                            .and_then(|o| o.get("textDocument"))
                            .and_then(|o| o.get("text"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .ok_or_else(|| {
                                anyhow::anyhow!("Failed to get contents from didOpen notification")
                            })?;

                        // debug_log!(
                        //     "DID_OPEN_RECEIVED: uri={}, version={:?}, content_len={}, line_count={}",
                        //     uri.to_string(), version, contents.len(), contents.lines().count()
                        // );

                        // Set initial document version
                        workspace.update_document_version(&uri.to_string(), version);

                        bone_reload_and_publish(&connection, &uri, contents, &mut workspace)?;
                        // Opening a project file is the LSP's "full build": the
                        // first open batch-loads the whole compilation unit, so
                        // regenerate the workspace-wide `.rock.tags` now (and on
                        // every restart, which re-opens the buffers). Without
                        // this a freshly opened / restarted project had NO tags
                        // until the first save.
                        regenerate_ctags(&workspace, &uri, "open");
                    }

                    "textDocument/didChange" => {
                        let uri = notification_uri(&not)?;

                        // Extract document version and contents
                        let params = not.params.as_object().ok_or_else(|| {
                            anyhow::anyhow!(
                                "Invalid didChange notification: params is not an object"
                            )
                        })?;
                        let version = params
                            .get("textDocument")
                            .and_then(|td| td.get("version"))
                            .and_then(|v| v.as_i64())
                            .map(|v| v as i32);

                        let contents = params
                            .get("contentChanges")
                            .and_then(|v| v.as_array())
                            .and_then(|a| a.first())
                            .and_then(|v| v.get("text"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .ok_or_else(|| {
                                anyhow::anyhow!("Failed to get contents from notification")
                            })?;

                        // Enhanced debug logging for document changes
                        // debug_log!(
                        //     "DID_CHANGE_RECEIVED: uri={}, version={:?}, content_len={}, first_50_chars={:?}",
                        //     uri.to_string(), version, contents.len(),
                        //     contents.chars().take(50).collect::<String>()
                        // );

                        // Log line count and some line positions for debugging
                        let _line_count = contents.lines().count();
                        // debug_log!("DID_CHANGE_LINES: uri={}, line_count={}", uri.to_string(), line_count);

                        // Update document version in workspace (this invalidates semantic token cache)
                        let _old_version = workspace.get_document_version(&uri.to_string());
                        workspace.update_document_version(&uri.to_string(), version);

                        // IMPORTANT: Force invalidate inlay hints cache to ensure proper editor refresh
                        // This forces the editor to see empty hints first, then proper hints on next request
                        workspace.force_invalidate_inlay_hints(&uri.to_string());

                        // Force the editor to refresh inlay hints by sending workspace/inlayHint/refresh
                        // This should tell the editor to re-request inlay hints for all documents
                        let inlay_hint_refresh_notification = lsp_server::Notification {
                            method: "workspace/inlayHint/refresh".to_string(),
                            params: serde_json::Value::Null,
                        };

                        if let Err(_e) = connection.sender.send(lsp_server::Message::Notification(
                            inlay_hint_refresh_notification,
                        )) {
                            // debug_log!("Failed to send inlay hint refresh notification: {:?}", _e);
                        } else {
                            // debug_log!("INLAY_HINTS_REFRESH_SENT: uri={}", uri.to_string());
                        }

                        // debug_log!(
                        //     "VERSION_UPDATE: uri={}, old_version={:?}, new_version={:?}, inlay_hints_cleared",
                        //     uri.to_string(), old_version, version
                        // );

                        bone_reload_and_publish(&connection, &uri, contents, &mut workspace)?;
                    }

                    "textDocument/didClose" => {
                        let _span = tracing::debug_span!("lsp_did_close_notification").entered();
                        let uri = notification_uri(&not)?;
                        log::debug!("got textDocument/didClose for {}", uri.as_str());
                        let uri_s = uri.as_str();
                        let path = uri_s.strip_prefix("file://").unwrap_or(uri_s);
                        workspace.bone.remove(path);
                        workspace.close_document(uri.as_str());
                    }

                    "textDocument/didChangeConfiguration" => {}
                    "workspace/didChangeConfiguration" => {}

                    _ => {
                        // debug_log!("UNEXPECTED_NOTIFICATION: method={}", not.method);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Regenerate the project's workspace-wide `.rock.tags` from the live bone
/// session. Best-effort: a write failure is logged, never propagated -- ctags
/// must not break editing. Called on didOpen (full build / restart) and didSave
/// (refresh).
fn regenerate_ctags(workspace: &LspWorkspace, uri: &Uri, when: &str) {
    let uri_s = uri.as_str();
    let file_path = uri_s.strip_prefix("file://").unwrap_or(uri_s);
    match workspace.bone.write_ctags(file_path) {
        Ok(n) => log::debug!("wrote .rock.tags on {when} ({n} symbols)"),
        Err(e) => log::warn!("ctags: failed to write .rock.tags: {e}"),
    }
}

/// bone-backend equivalent of `reload_sources_on_change_workspace`: feed the
/// edit into the per-root bone session and publish diagnostics for every file
/// the session reports on (so cross-file errors update and now-clean files get a
/// clearing publish). Used on didOpen/didChange when `backend == Bone`.
fn bone_reload_and_publish(
    connection: &Connection,
    uri: &Uri,
    contents: String,
    workspace: &mut LspWorkspace,
) -> Result<()> {
    let uri_s = uri.as_str();
    let file_path = uri_s.strip_prefix("file://").unwrap_or(uri_s).to_string();

    // Compile-on-open/edit cost (ROCK_LSP_TIMING=1). This is the notification
    // path (didOpen/didChange) that head-of-line-blocks every queued request
    // behind it, so it's the number to watch when first-paint feels slow.
    let _t = std::time::Instant::now();
    let per_file = workspace.bone.update(&file_path, contents);
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
        let Ok(file_uri) = Uri::from_str(&format!("file://{path}")) else {
            log::error!("bone LSP: cannot build URI for diagnostics: {path}");
            continue;
        };
        let params = lsp_types::PublishDiagnosticsParams {
            uri: file_uri,
            version: None,
            diagnostics,
        };
        connection.sender.send(lsp_server::Message::Notification(lsp_server::Notification {
            method: lsp_types::notification::PublishDiagnostics::METHOD.to_string(),
            params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
        }))?;
    }
    Ok(())
}

/// Pull the typed params out of a request when its method matches.
///
/// Returns the untouched request on a method mismatch so dispatch can try
/// the next arm; a JSON shape error is reported to the caller instead.
fn cast<R>(seen: Request) -> Result<(RequestId, R::Params), ExtractError<Request>>
where
    R: lsp_types::request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    seen.extract(R::METHOD)
}

fn notification_uri(not: &lsp_server::Notification) -> Result<Uri> {
    let text = not.params.get("textDocument")
        .and_then(|document| document.get("uri"))
        .and_then(serde_json::Value::as_str);
    match text {
        Some(value) => Uri::from_str(value).map_err(|_| anyhow::anyhow!("Failed to get uri from notification")),
        None => Err(anyhow::anyhow!("Failed to get uri from notification")),
    }
}

fn send_response<T>(connection: &Connection, id: RequestId, t: Result<T, anyhow::Error>) -> Result<(), anyhow::Error>
where
    T: serde::Serialize,
{
    let (result, error) = match t {
        Ok(value) => (Some(serde_json::to_value(&value).unwrap_or_else(|e| {
                log::error!("Failed to serialize response: {}", e);
                serde_json::Value::Null
            })), None),
        Err(failure) => {
            error!("error: {}", failure);
            let error = lsp_server::ResponseError {
                code: lsp_server::ErrorCode::RequestFailed as i32,
                message: failure.to_string(),
                data: None,
            };
            (None, Some(error))
        },
    };
    let reply = Response { id, result, error };
    connection.sender.send(Message::Response(reply))?;
    Ok(())
}

// print_symbol_table(
//     &context
//         .compiled_module
//         .as_ref()
//         .unwrap()
//         .semantic
//         .storage,
//     sources,
//     None,
// );

fn notify(_msg: impl AsRef<str>) {
    // TODO: neat, but we don't really like/use this much
    // if let Err(e) = notify_rust::Notification::new()
    //     .summary(&format!("[rock-lsp]: {}", msg.as_ref()))
    //     .show()
    // {
    //     log::error!("Failed to show notification: {}", e);
    // }
}


fn text_sync_options() -> TextDocumentSyncOptions {
    let mut options = TextDocumentSyncOptions::default();
    options.open_close = Some(true);
    options.change = Some(TextDocumentSyncKind::FULL);
    options.will_save = Some(false);
    options.will_save_wait_until = Some(false);
    options.save = Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
        include_text: Some(false),
    }));
    options
}
