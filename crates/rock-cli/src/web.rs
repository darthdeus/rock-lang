//! `rock web <project> [--out <dir>]` -- emit a static browser build
//! (`index.html` + `game.js`) for a Rock project via the JS backend (stone-web
//! Arc 2, `docs/stone-web-plan.md`).
//!
//! `game.js` is ONE classic script (the JS path is already classic/ES5-ish, no
//! ESM): the same runtime prelude set the node path uses (`build_run_program`),
//! but with `JS_WEB_RUNTIME` in place of the node RNG/env glue (a browser
//! `print` -> console.log, a config object instead of `process.env`), the
//! emitted program, then a boot line. `index.html` loads it. Serve the out dir
//! with any static file server -- no COOP/COEP headers (no SharedArrayBuffer in
//! the design).

use crate::{FileWatcher, Result};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

#[derive(Clone)]
struct WebAsset {
    key: String,
    source: PathBuf,
}

pub fn run_web(project: &str, out: Option<&str>, watch: bool, port: u16) -> Result<()> {
    if watch {
        watch_web(project, out, port)
    } else {
        build_web(project, out)
    }
}

pub fn build_web(project: &str, out: Option<&str>) -> Result<()> {
    let out_dir = web_out_dir(project, out);
    build_web_bundle(project, &out_dir, true, false).map(|_| ())
}

fn web_out_dir(project: &str, out: Option<&str>) -> PathBuf {
    out.map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(project).join("web"))
}

fn build_web_bundle(
    project: &str,
    out_dir: &Path,
    copy_assets: bool,
    watch: bool,
) -> Result<Vec<WebAsset>> {
    let (emitted, ws) = crate::js_test::try_emit_js_program_project(project)?;
    let register_lines = shader_register_lines_js(&ws)?;
    let game_dir = web_game_dir(project);
    let assets = collect_web_assets(Path::new(project), &game_dir)?;
    let preload: Vec<String> = assets.iter().map(|asset| asset.key.clone()).collect();
    let game_js = assemble_web_program(&emitted, &register_lines, &preload, &game_dir);
    let title = Path::new(project)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("rock");
    let index_html = index_html(title, watch);

    std::fs::create_dir_all(out_dir)?;
    if copy_assets {
        for asset in &assets {
            let dst = out_dir.join(&asset.key);
            std::fs::create_dir_all(dst.parent().expect("asset parent"))?;
            std::fs::copy(&asset.source, dst)?;
        }
    }

    let game_js_path = out_dir.join("game.js");
    let index_path = out_dir.join("index.html");
    rock_host::project::write_atomic(&game_js_path, &game_js)?;
    rock_host::project::write_atomic(&index_path, &index_html)?;
    log::info!(target: "WEB", "{} -> {} ({} + game.js)", project, out_dir.display(), index_path.display());
    Ok(assets)
}

fn web_game_dir(project: &str) -> String {
    let path = Path::new(project);
    let relative = if path.is_absolute() {
        rock_host::project::find_workspace_root(&std::env::current_dir().unwrap_or_default())
            .and_then(|root| path.strip_prefix(root).ok().map(Path::to_path_buf))
            .or_else(|| path.file_name().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("game"))
    } else {
        path.to_path_buf()
    };
    relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn collect_web_assets(project: &Path, game_dir: &str) -> Result<Vec<WebAsset>> {
    let mut assets = Vec::new();
    for rel in release_assets(project)? {
        let src = project.join(&rel);
        if !src.exists() {
            log::warn!(target: "WEB", "[release].assets: {} does not exist, skipping", src.display());
            continue;
        }
        if src.is_dir() {
            for entry in walkdir::WalkDir::new(&src).sort_by_file_name() {
                let entry = entry?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let rel_to_game = entry.path().strip_prefix(project).expect("under project");
                let key = format!(
                    "{}/{}",
                    game_dir,
                    rel_to_game.to_string_lossy().replace('\\', "/")
                );
                assets.push(WebAsset {
                    key,
                    source: entry.path().to_path_buf(),
                });
            }
        } else {
            let key = format!("{}/{}", game_dir, rel.replace('\\', "/"));
            assets.push(WebAsset { key, source: src });
        }
    }

    let fallback_key = format!("{}/assets/fonts/DroidSans.ttf", game_dir);
    if !assets.iter().any(|asset| asset.key == fallback_key) {
        let src = repo_relative("stone-rs/fonts/DroidSans.ttf");
        if src.is_file() {
            assets.push(WebAsset {
                key: fallback_key,
                source: src,
            });
        } else {
            log::warn!(target: "WEB", "engine fallback font missing at {}; text will not render", src.display());
        }
    }
    assets.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(assets)
}

/// The `[release].assets` staging list from a project's stick.toml. Defaults to
/// `["assets"]` (plus `level.txt` when present) when the table is absent,
/// mirroring `tools/release/collect_assets.py`. A malformed stick.toml
/// propagates as an error rather than silently defaulting.
fn release_assets(project: &Path) -> Result<Vec<String>> {
    let path = project.join("stick.toml");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Ok(default_release_assets(project)),
    };
    let value: serde_json::Value = basic_toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("web: failed to parse {}: {}", path.display(), e))?;
    Ok(
        match value
            .get("release")
            .and_then(|r| r.get("assets"))
            .and_then(|a| a.as_array())
        {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
            None => default_release_assets(project),
        },
    )
}

fn default_release_assets(project: &Path) -> Vec<String> {
    let mut items = vec!["assets".to_string()];
    if project.join("level.txt").exists() {
        items.push("level.txt".to_string());
    }
    items
}

/// A path relative to the workspace root (the compile runs from anywhere).
fn repo_relative(rel: &str) -> PathBuf {
    rock_host::project::find_workspace_root(&std::env::current_dir().unwrap_or_default())
        .map(|r| r.join(rel))
        .unwrap_or_else(|| PathBuf::from(rel))
}

struct ReloadHub {
    generation: AtomicU64,
    subscribers: Mutex<Vec<std::sync::mpsc::Sender<u64>>>,
}

impl ReloadHub {
    fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            subscribers: Mutex::new(Vec::new()),
        }
    }

    fn subscribe(&self) -> std::sync::mpsc::Receiver<u64> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    fn reload(&self) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.subscribers
            .lock()
            .unwrap()
            .retain(|subscriber| subscriber.send(generation).is_ok());
    }
}

struct WebServerState {
    out_dir: PathBuf,
    assets: RwLock<HashMap<String, PathBuf>>,
    reload: ReloadHub,
}

impl WebServerState {
    fn new(out_dir: PathBuf, assets: Vec<WebAsset>) -> Self {
        Self {
            out_dir,
            assets: RwLock::new(asset_map(assets)),
            reload: ReloadHub::new(),
        }
    }

    fn publish_assets(&self, assets: Vec<WebAsset>) {
        *self.assets.write().unwrap() = asset_map(assets);
    }
}

fn asset_map(assets: Vec<WebAsset>) -> HashMap<String, PathBuf> {
    assets
        .into_iter()
        .map(|asset| (asset.key, asset.source))
        .collect()
}

fn watch_web(project: &str, out: Option<&str>, port: u16) -> Result<()> {
    rock_host::project::ensure_workspace_root()?;
    let out_dir = web_out_dir(project, out);
    let initial_assets = match build_web_bundle(project, &out_dir, false, true) {
        Ok(assets) => assets,
        Err(e) => {
            log::error!(target: "WEB", "{e}");
            log::info!(target: "WEB", "watching for changes; the server will publish the first successful build");
            Vec::new()
        }
    };
    std::fs::create_dir_all(&out_dir)?;

    let state = Arc::new(WebServerState::new(out_dir.clone(), initial_assets));
    let bound_port = start_web_server(port, state.clone())?;
    log::info!(target: "WEB", "Serving http://127.0.0.1:{bound_port}");

    let mut watch_paths = crate::watch::project_engine_watch_paths(project);
    watch_paths.extend(crate::watch::compiler_watch_paths());
    let (tx, rx) = std::sync::mpsc::channel();
    let _watcher = FileWatcher::default_for_rock_sources()
        .with_all_files()
        .with_ignored_roots(vec![out_dir])
        .watch(watch_paths, tx)?;
    log::info!(target: "WEB", "Watching {project}, stdlib/, stone/, and compiler sources");

    for event in rx {
        if event.paths.iter().any(crate::watch::is_compiler_source) {
            log::info!(target: "WEB", "Compiler changed; rebuilding rock");
            match crate::watch::build_compiler() {
                Ok(()) => {
                    log::info!(target: "WEB", "Compiler rebuilt; re-exec'ing web watch");
                    if let Err(e) = crate::watch::reexec_current_binary() {
                        log::error!(target: "WEB", "{e}; continuing with the previous compiler");
                    }
                }
                Err(e) => {
                    log::error!(target: "WEB", "{e}; continuing with the previous compiler");
                }
            }
            continue;
        }

        match build_web_bundle(project, &state.out_dir, false, true) {
            Ok(assets) => {
                state.publish_assets(assets);
                state.reload.reload();
                log::info!(target: "WEB", "Browser build updated");
            }
            Err(e) => {
                eprintln!("{e}");
                log::error!(target: "WEB", "Build failed; serving last green browser build");
            }
        }
    }
    Ok(())
}

fn start_web_server(port: u16, state: Arc<WebServerState>) -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let state = state.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = serve_connection(stream, &state) {
                            log::debug!(target: "WEB", "HTTP connection ended: {e}");
                        }
                    });
                }
                Err(e) => log::error!(target: "WEB", "HTTP accept failed: {e}"),
            }
        }
    });
    Ok(port)
}

fn serve_connection(mut stream: TcpStream, state: &WebServerState) -> Result<()> {
    let mut request_line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut request_line)?;
    let mut request = request_line.split_whitespace();
    if request.next() != Some("GET") {
        return write_response(&mut stream, 405, "text/plain", b"method not allowed");
    }
    let raw_path = request.next().unwrap_or("/");
    let path = raw_path.split('?').next().unwrap_or("/");
    if path == "/__rock_reload" {
        let reloads = state.reload.subscribe();
        stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n",
        )?;
        stream.flush()?;
        for generation in reloads {
            write!(stream, "data: {generation}\n\n")?;
            stream.flush()?;
        }
        return Ok(());
    }

    let decoded = percent_decode(path).ok_or_else(|| anyhow::anyhow!("invalid URL escape"))?;
    let key = decoded.trim_start_matches('/');
    if key.split('/').any(|part| part == "..") {
        return write_response(&mut stream, 400, "text/plain", b"invalid path");
    }
    let source = state.assets.read().unwrap().get(key).cloned().or_else(|| {
        let relative = if key.is_empty() { "index.html" } else { key };
        let path = state.out_dir.join(relative);
        path.is_file().then_some(path)
    });
    let Some(source) = source else {
        return write_response(&mut stream, 404, "text/plain", b"not found");
    };
    let bytes = std::fs::read(&source)?;
    write_response(&mut stream, 200, content_type(&source), &bytes)
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    Ok(())
}

fn percent_decode(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(bytes.get(index + 1).copied()?)?;
            let low = hex(bytes.get(index + 2).copied()?)?;
            decoded.push(high * 16 + low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("ogg") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("ttf") => "font/ttf",
        Some("txt" | "rock" | "glsl") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// One `__rock_shader_register("<key>", "<glsl>")` line per distinct
/// `rock_shader` pair -- the JS mirror of skull's Lua `shader_registration_lines`
/// (same collector, same Core330 profile: the profile is a property of the GL
/// context, and the web `stone_create_shader_from_combined` shim does the ES
/// rewrite at shader-compile time, like the native GLES path). Emitted after
/// the program (module load), before the boot line, so registrations queue in
/// PENDING_SHADER_SRCS pre-GL exactly as on a native cold start.
fn shader_register_lines_js(ws: &bone_semantic::inference::Workspace) -> Result<String> {
    use std::fmt::Write as _;
    let pairs =
        skull_mir::lower::collect_shader_pairs(ws).map_err(|e| anyhow::anyhow!(e.message))?;
    let mut out = String::new();
    let profile = skull_codegen_glsl::GlslProfile::Core330;
    for pair in pairs {
        let fname = ws.module(pair.frag.module).functions[pair.frag.func].name;
        let glsl = match pair.vert {
            Some(vert) => {
                let vname = ws.module(vert.module).functions[vert.func].name;
                skull_codegen_glsl::emit_shader_pipeline(
                    ws,
                    vert.module,
                    vname.as_str(),
                    fname.as_str(),
                    profile,
                )
            }
            None => {
                skull_codegen_glsl::emit_shader_entry(ws, pair.frag.module, fname.as_str(), profile)
            }
        }
        .map_err(|e| anyhow::anyhow!(e.message))?;
        let _ = writeln!(
            out,
            "__rock_shader_register({}, {});",
            crate::js_test::js_str(&pair.key),
            crate::js_test::js_str(&glsl)
        );
    }
    Ok(out)
}

/// Assemble `game.js`: the shared runtime preludes (same set/order as the node
/// path) + `JS_WEB_RUNTIME` (browser print/config, in place of the node RNG/env
/// glue) + a scoped `var __G = {}` + the emitted program + the shader
/// registration lines, then the boot line: `__rock_web_boot(main)` fetches the
/// preload manifest (the TTF) into the in-memory registry and registers each
/// font as a FontFace BEFORE `main()` runs, so the sync file/font reads stay
/// sync (spec boot step 2).
fn assemble_web_program(
    emitted: &skull_codegen_js::emit::JsProgram,
    register_lines: &str,
    preload: &[String],
    game_dir: &str,
) -> String {
    let mut p = String::from(skull_codegen_js::JS_NB_CHECK_SHIMS);
    p.push_str(skull_codegen_js::JS_VEC_CHECK_SHIMS);
    p.push_str(skull_codegen_js::JS_TABLE_PRIM);
    p.push_str(skull_codegen_js::JS_MAP_ITER_RUNTIME);
    p.push_str(skull_codegen_js::JS_STRUCTURAL_EQ);
    p.push_str(skull_codegen_js::JS_ENUM_RUNTIME);
    p.push_str(skull_codegen_js::JS_CALL_HELPER);
    p.push_str(skull_codegen_js::JS_CORO_RUNTIME);
    p.push_str(skull_codegen_js::JS_STRING_RUNTIME);
    p.push_str(skull_codegen_js::JS_INSPECT_RUNTIME);
    p.push_str(skull_codegen_js::JS_DEEPCOPY_RUNTIME);
    p.push_str(skull_codegen_js::JS_LOG_RUNTIME);
    p.push_str(skull_codegen_js::JS_RNG_RUNTIME);
    p.push_str(skull_codegen_js::JS_WEB_RUNTIME);
    // GAME_SOURCE_DIR on web = the repo-relative game dir, so asset_path-routed
    // loads key as "{game_dir}/{rel}" -- the same key space stage_assets writes
    // and file_read literals ("games/chess/book.txt") already use.
    p.push_str(&format!(
        "\nvar __ROCK_WEB_GAME_DIR = {};\n",
        crate::js_test::js_str(game_dir)
    ));
    p.push_str("\nvar __G = {};\n");
    // Deterministic default seed (a game re-seeds via random_seed() itself);
    // Date.now() matches the node path's non-deterministic default.
    p.push_str("__rock_rng_seed(Date.now());\n");
    p.push_str(&emitted.program);
    p.push_str(register_lines);
    let manifest: Vec<String> = preload.iter().map(|s| crate::js_test::js_str(s)).collect();
    p.push_str(&format!(
        "\nvar __ROCK_WEB_PRELOAD = [{}];\n",
        manifest.join(", ")
    ));
    match &emitted.main_js_name {
        Some(main) => p.push_str(&format!("__rock_web_boot({main});\n")),
        None => p.push_str("__rock_web_boot(null);\n"),
    }
    p
}

/// The loader page: a full-viewport canvas (`#stone-canvas`, the id the WebGL2
/// context boot in `JS_WEB_RUNTIME` will bind in MB) and the classic
/// `<script src="game.js">`.
fn index_html(title: &str, watch: bool) -> String {
    let reload = if watch {
        "<script>new EventSource('/__rock_reload').onmessage = function () { location.reload(); };</script>\n"
    } else {
        ""
    };
    format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
  html, body {{ margin: 0; height: 100%; background: #111; }}
  #stone-canvas {{ display: block; width: 100vw; height: 100vh; }}
</style>
</head>
<body>
<canvas id="stone-canvas"></canvas>
{reload}\
<script src="game.js"></script>
</body>
</html>
"#
    )
}
