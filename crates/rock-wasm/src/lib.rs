//! The Rock compiler as a WebAssembly module, for a browser Worker.
//!
//! A raw `cdylib` with a JSON-over-linear-memory ABI rather than
//! wasm-bindgen: the Worker needs strings in and strings out, and this keeps
//! the build to `cargo build --target wasm32-unknown-unknown` with no
//! generated JS glue or CLI version to keep in lockstep with the crate.
//!
//! ABI: the host writes a UTF-8 JSON request into memory from
//! [`rock_alloc`], calls [`rock_request`] (which takes ownership of that
//! buffer), and reads the JSON response at [`rock_output_ptr`] /
//! [`rock_output_len`]. A compiler panic traps; the panic hook leaves
//! `{"ok":false,"panic":...}` in the output first, and the host should
//! instantiate a fresh module after a trap.
//!
//! Requests (`op`):
//! - `install_stdlib` `{files: {name: text}}`: once, before any compile.
//! - `compile` `{file, source, target?: "gl"|"wgpu", files?: {path: text}}`:
//!   `files` are what `embed_string` may read. Returns `lua`, `shaders` and
//!   `diagnostics`, or `stage` and `diagnostics` on failure.
//! - `format` `{source}`: returns `source`.
//! - `info`: returns `nesting_limit`.
//! - `lsp` `{message}`: one JSON-RPC message from the editor (after
//!   `install_stdlib`); returns `messages`, the responses and notifications
//!   the language server sends back. `initialize` starts a server and `exit`
//!   drops it; stdlib locations are `file:///stdlib/<name>`.

use std::sync::Arc;

use bone_core::comptime_files::{EmbedFiles, MemoryComptimeFiles};
use bone_core::modules::NsAliases;
use bone_semantic::inference::{ComptimeInputs, Workspace};
use bone_semantic::prelude::{FxHashMap, Ustr};
use rock_core::CompilerError;
use rock_core::errors::CompilerErrorLevel;
use lsp_server::{ErrorCode, Message, Response};
use serde_json::{Value, json};
use skull::mir_emit::{RendererTarget, ShaderLanguage};
use skull::pipeline::{CompileHost, EmitTarget, Stage};

/// Directory the playground's stdlib files are displayed under.
pub const STDLIB_DISPLAY_DIR: &str = "/stdlib";

/// Handle one JSON request. Errors in the request itself come back as
/// `{"ok":false,"error":...}`.
pub fn handle(request: &str) -> Value {
    match serde_json::from_str::<Value>(request) {
        Ok(request) => dispatch(&request).unwrap_or_else(|error| json!({"ok": false, "error": error})),
        Err(e) => json!({"ok": false, "error": format!("request is not JSON: {e}")}),
    }
}

fn dispatch(request: &Value) -> Result<Value, String> {
    match str_field(request, "op")? {
        "install_stdlib" => {
            let files = string_map(request, "files")?.unwrap_or_default();
            rock_stdlib::install_stdlib(STDLIB_DISPLAY_DIR, files).map_err(|e| e.to_string())?;
            STDLIB_INSTALLED.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(json!({"ok": true}))
        }
        "compile" => compile(request),
        "format" => {
            let source = str_field(request, "source")?;
            match skull_fmt::try_format(source, &skull_fmt::FormatConfig::default()) {
                Ok(source) => Ok(json!({"ok": true, "source": source})),
                Err(error) => Ok(json!({"ok": false, "error": error})),
            }
        }
        "info" => Ok(json!({"ok": true, "nesting_limit": rock_core::nesting::MAX_NESTING_DEPTH})),
        "lsp" => lsp(request),
        op => Err(format!("unknown op `{op}`")),
    }
}

fn compile(request: &Value) -> Result<Value, String> {
    if !stdlib_installed() {
        return Err("install_stdlib must run before compile".to_string());
    }
    let file = str_field(request, "file")?;
    let source = str_field(request, "source")?;
    let renderer = match request.get("target").and_then(Value::as_str) {
        None | Some("gl") => RendererTarget::Gl,
        Some("wgpu") => RendererTarget::Wgpu,
        Some(other) => return Err(format!("unknown target `{other}` (gl, wgpu)")),
    };
    let files = string_map(request, "files")?.unwrap_or_default();
    let mut host = PlaygroundHost {
        file: Ustr::from(file),
        source,
        embed_files: Some(EmbedFiles(Arc::new(MemoryComptimeFiles::new(files)))),
        soft: Vec::new(),
    };
    let target = EmitTarget { project_root: None, renderer };
    let result = skull::pipeline::compile_source(file, source, target, &mut host);
    let mut diagnostics: Vec<Value> = host.soft.iter().map(|f| diagnostic(&f.error, Some(f.lint))).collect();
    Ok(match result {
        Ok(compiled) => {
            let shaders: Vec<Value> = compiled
                .emission
                .shaders
                .iter()
                .map(|s| {
                    let language = match s.language {
                        ShaderLanguage::Glsl => "glsl",
                        ShaderLanguage::Wgsl => "wgsl",
                    };
                    json!({"key": s.key, "language": language, "source": s.source})
                })
                .collect();
            json!({
                "ok": true,
                "lua": compiled.emission.lua,
                "shaders": shaders,
                "diagnostics": diagnostics,
            })
        }
        Err(failure) => {
            let stage = match failure.stage {
                Stage::Parse => "parse",
                Stage::Check => "check",
                Stage::Comptime => "comptime",
                Stage::Lint => "lint",
                Stage::Emit => "emit",
            };
            for e in &failure.errors {
                diagnostics.push(diagnostic(e, None));
                diagnostics.extend(e.siblings.iter().map(|s| diagnostic(s, None)));
            }
            json!({"ok": false, "stage": stage, "diagnostics": diagnostics})
        }
    })
}

thread_local! {
    static LSP: std::cell::RefCell<Option<rock_lsp::LspServer>> = const { std::cell::RefCell::new(None) };
}

fn lsp(request: &Value) -> Result<Value, String> {
    if !stdlib_installed() {
        return Err("install_stdlib must run before lsp".to_string());
    }
    let message = request.get("message").ok_or("missing field `message`")?;
    let message: Message =
        serde_json::from_value(message.clone()).map_err(|e| format!("`message` is not JSON-RPC: {e}"))?;
    let mut out = Vec::new();
    let handled = LSP.with_borrow_mut(|server| -> anyhow::Result<()> {
        match (server.as_mut(), message) {
            (None, Message::Request(req)) if req.method == "initialize" => {
                let (started, response) = rock_lsp::LspServer::initialize(rock_lsp::CompilerPolicy::default(), req)?;
                *server = Some(started);
                out.push(response);
            }
            (Some(running), message) => {
                let handled = running.handle(message, &mut out);
                if running.exited() {
                    *server = None;
                }
                handled?;
            }
            (None, Message::Request(req)) => out.push(
                Response::new_err(req.id, ErrorCode::ServerNotInitialized as i32, "initialize first".to_string()).into(),
            ),
            (None, _) => {}
        }
        Ok(())
    });
    let messages = serde_json::to_value(&out).map_err(|e| e.to_string())?;
    Ok(match handled {
        Ok(()) => json!({"ok": true, "messages": messages}),
        Err(e) => json!({"ok": false, "error": format!("{e:#}"), "messages": messages}),
    })
}

/// The playground's [`CompileHost`]: one source, no manifests (so no alias
/// tables), build inputs only as supplied `embed_string` files, and lint
/// findings for that source returned with the response.
struct PlaygroundHost<'a> {
    file: Ustr,
    source: &'a str,
    embed_files: Option<EmbedFiles>,
    soft: Vec<bone_lints::Finding>,
}

impl CompileHost for PlaygroundHost<'_> {
    fn aliases(&mut self, _modules: &[skull_parser::Ast]) -> Result<NsAliases, CompilerError> {
        Ok(NsAliases::default())
    }

    fn comptime_inputs(&mut self) -> Result<ComptimeInputs, CompilerError> {
        Ok(ComptimeInputs { embed_files: self.embed_files.take().unwrap_or_default(), ..Default::default() })
    }

    fn lint_findings(&mut self, ws: &Workspace) -> Vec<bone_lints::Finding> {
        let sources: FxHashMap<Ustr, &str> = [(self.file, self.source)].into_iter().collect();
        let warm = skull::pipeline::warm_module_ids(ws, &sources);
        let options = bone_lints::LintOptions::default().with_action_policy(None);
        bone_lints::lint_workspace(ws, &warm, false, &sources, &options)
    }

    fn report_soft_findings(&mut self, soft: Vec<bone_lints::Finding>) {
        self.soft = soft;
    }
}

fn diagnostic(e: &CompilerError, lint: Option<&str>) -> Value {
    let severity = match e.level {
        CompilerErrorLevel::Error => "error",
        CompilerErrorLevel::Warning => "warning",
        CompilerErrorLevel::Advice => "advice",
    };
    let labels: Vec<Value> = e
        .labels
        .iter()
        .map(|l| {
            json!({
                "message": l.message,
                "file": l.span.file.as_str(),
                "start": {"line": l.span.line_range.0, "column": l.span.col_range.0},
                "end": {"line": l.span.line_range.1, "column": l.span.col_range.1},
            })
        })
        .collect();
    json!({
        "severity": severity,
        "message": e.message,
        "lint": lint,
        "file": e.location.file.as_str(),
        "line": e.location.line,
        "column": e.location.col,
        "labels": labels,
    })
}

fn str_field<'a>(request: &'a Value, name: &str) -> Result<&'a str, String> {
    request.get(name).and_then(Value::as_str).ok_or_else(|| format!("missing string field `{name}`"))
}

fn string_map(request: &Value, name: &str) -> Result<Option<Vec<(String, String)>>, String> {
    let Some(value) = request.get(name) else { return Ok(None) };
    let object = value.as_object().ok_or_else(|| format!("`{name}` must be an object of strings"))?;
    object
        .iter()
        .map(|(k, v)| {
            v.as_str()
                .map(|v| (k.clone(), v.to_string()))
                .ok_or_else(|| format!("`{name}.{k}` must be a string"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn stdlib_installed() -> bool {
    STDLIB_INSTALLED.load(std::sync::atomic::Ordering::Relaxed)
}

static STDLIB_INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(target_family = "wasm")]
mod abi {
    use std::sync::Mutex;

    static OUTPUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

    fn set_output(bytes: Vec<u8>) {
        *OUTPUT.lock().unwrap_or_else(|p| p.into_inner()) = bytes;
    }

    /// A buffer of `len` bytes for the host to write a request into.
    #[unsafe(no_mangle)]
    pub extern "C" fn rock_alloc(len: usize) -> *mut u8 {
        let mut buffer = Vec::<u8>::with_capacity(len);
        let ptr = buffer.as_mut_ptr();
        std::mem::forget(buffer);
        ptr
    }

    /// Handle the request in a [`rock_alloc`] buffer, taking ownership of it.
    /// Returns 0 when the response has `"ok": true`, 1 otherwise.
    ///
    /// # Safety
    /// `ptr` must come from `rock_alloc(len)` and hold `len` written bytes.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn rock_request(ptr: *mut u8, len: usize) -> u32 {
        static HOOK: std::sync::Once = std::sync::Once::new();
        HOOK.call_once(|| {
            std::panic::set_hook(Box::new(|info| {
                set_output(serde_json::json!({"ok": false, "panic": info.to_string()}).to_string().into_bytes());
            }))
        });
        // SAFETY: the caller passes a buffer `rock_alloc(len)` returned, now filled.
        let bytes = unsafe { Vec::from_raw_parts(ptr, len, len) };
        let response = match String::from_utf8(bytes) {
            Ok(request) => super::handle(&request),
            Err(_) => serde_json::json!({"ok": false, "error": "request is not UTF-8"}),
        };
        let ok = response["ok"] == true;
        set_output(response.to_string().into_bytes());
        u32::from(!ok)
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn rock_output_ptr() -> *const u8 {
        OUTPUT.lock().unwrap_or_else(|p| p.into_inner()).as_ptr()
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn rock_output_len() -> usize {
        OUTPUT.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}
