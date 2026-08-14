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

/// Above this the base64 page stops being a good delivery shape (parse time and
/// peak memory, both several times the file). Chosen well above every game that
/// is a real single-file candidate (chesslings 4.4 MB, chess 19.7 MB) and well
/// below one that is not (mag, ~240 MB).
const SINGLE_FILE_WARN_MB: f64 = 32.0;

type WebAsset = rock_host::release::ReleaseAsset;

/// What `rock web` produces. The three are distinct DELIVERY shapes, not
/// formatting options: a directory is what a static host serves, a single file
/// is what runs from `file://` or an upload with no server, and a zip is what
/// itch.io ingests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WebPackage {
    /// `index.html` + `game.js` + loose assets (the default).
    Directory,
    /// One self-contained `.html`, assets base64-embedded.
    SingleFile,
}

pub fn run_web(
    project: &str,
    out: Option<&str>,
    watch: bool,
    port: u16,
    package: WebPackage,
    itch: bool,
) -> Result<()> {
    if watch && (package != WebPackage::Directory || itch) {
        anyhow::bail!(
            "web: --watch serves a live directory over HTTP, so it cannot be \
             combined with --single-file or --itch"
        );
    }
    if watch {
        return watch_web(project, out, port);
    }
    if itch {
        return build_itch_zip(project, out, package);
    }
    match package {
        WebPackage::Directory => build_web(project, out),
        WebPackage::SingleFile => build_single_file(project, out).map(|_| ()),
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

/// The last path component of a project, as a file stem (`games/match3` ->
/// `match3`). Used to name the single-file and zip artifacts.
fn project_stem(project: &str) -> String {
    Path::new(project)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("game")
        .to_string()
}

/// Resolve `--out` for an artifact that is a FILE, not a directory. An `--out`
/// already carrying the right extension is taken verbatim; anything else is
/// treated as a directory to put the default-named artifact in.
fn web_out_file(project: &str, out: Option<&str>, ext: &str) -> PathBuf {
    let default_name = format!("{}.{ext}", project_stem(project));
    match out {
        Some(o) if Path::new(o).extension().and_then(|e| e.to_str()) == Some(ext) => {
            PathBuf::from(o)
        }
        Some(o) => PathBuf::from(o).join(default_name),
        None => PathBuf::from(project).join("web").join(default_name),
    }
}

/// One self-contained `.html`: every asset base64-embedded into
/// `__ROCK_WEB_ASSETS` before an inlined `game.js` runs, so the page issues no
/// network request at all and works from `file://`.
///
/// Returns the written path and the asset count, so a caller can report and a
/// test can assert a denominator.
fn build_single_file(project: &str, out: Option<&str>) -> Result<(PathBuf, usize)> {
    let (emitted, cache, ws) = crate::js_test::try_emit_js_program_project_cached(project)?;
    let register_lines = shader_register_lines_js(&ws, &cache)?;
    let game_dir = web_game_dir(project);
    let assets = collect_web_assets(Path::new(project), &game_dir)?;
    let preload: Vec<String> = assets.iter().map(|asset| asset.key.clone()).collect();
    let game_js = assemble_web_program(&emitted, &register_lines, &preload, &game_dir);
    let html = single_file_html(&project_stem(project), &assets, &game_js)?;

    let path = web_out_file(project, out, "html");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    rock_host::project::write_atomic(&path, &html)?;
    let mb = html.len() as f64 / (1024.0 * 1024.0);
    log::info!(
        target: "WEB",
        "{} -> {} (single file, {} assets embedded, {mb:.1} MB)",
        project,
        path.display(),
        assets.len()
    );
    // Not a rejection -- the caller may well want this for an offline copy --
    // but a single-file build is the wrong shape past a point and the failure is
    // not obvious: the whole page is ONE base64 blob the browser must parse and
    // decode before the first frame, at several times its size in memory, and
    // nothing can be cached between loads. A directory build (or `--itch`) keeps
    // assets separate and incrementally cacheable.
    if mb > SINGLE_FILE_WARN_MB {
        log::warn!(
            target: "WEB",
            "{:.1} MB is large for a single file (over {SINGLE_FILE_WARN_MB:.0} MB); \
             prefer the default directory build or --itch for a game this size",
            mb
        );
    }
    Ok((path, assets.len()))
}

/// A `</script` inside the emitted program would close the wrapping tag and
/// leave the rest of the program as page text -- the HTML tokenizer does not
/// know it is inside a JS string. `<\/script` is the same string to a JS parser
/// and inert to the tokenizer.
fn escape_script_close(js: &str) -> String {
    // Copies unmatched text as SLICES rather than byte-by-byte: `b as char`
    // reinterprets a UTF-8 continuation byte as a codepoint, which turns every
    // non-ASCII string literal in the program into mojibake.
    let mut out = String::with_capacity(js.len() + 16);
    let bytes = js.as_bytes();
    let (mut start, mut i) = (0usize, 0usize);
    while i + 8 <= bytes.len() {
        // Case-insensitive: the tokenizer ends the element on `</ScRiPt` too,
        // and does not require a closing `>`.
        if bytes[i] == b'<'
            && bytes[i + 1] == b'/'
            && bytes[i + 2..i + 8].eq_ignore_ascii_case(b"script")
        {
            // `<` is ASCII, so `i` is always a char boundary here.
            out.push_str(&js[start..i]);
            out.push_str("<\\/");
            i += 2;
            start = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&js[start..]);
    out
}

fn single_file_html(
    title: &str,
    assets: &[WebAsset],
    game_js: &str,
) -> Result<String> {
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::STANDARD;

    let mut embedded = String::new();
    for asset in assets {
        let bytes = std::fs::read(&asset.source).map_err(|e| {
            anyhow::anyhow!("web --single-file: reading {}: {e}", asset.source.display())
        })?;
        embedded.push_str(&format!(
            "{}: \"{}\",\n",
            crate::js_test::js_str(&asset.key),
            engine.encode(&bytes)
        ));
    }

    Ok(format!(
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
<script>
// Every asset, base64-embedded. The prelude's boot prefers an already-present
// __ROCK_WEB_ASSETS entry over a fetch, so this page makes no network request
// and runs from file://.
globalThis.__ROCK_WEB_ASSETS = (function () {{
  var b64 = {{
{embedded}  }};
  var out = {{}};
  for (var k in b64) {{
    var s = atob(b64[k]);
    var u = new Uint8Array(s.length);
    for (var i = 0; i < s.length; i++) u[i] = s.charCodeAt(i);
    out[k] = u;
  }}
  return out;
}})();
</script>
<script>
{game}
</script>
</body>
</html>
"#,
        game = escape_script_close(game_js)
    ))
}

/// A zip with `index.html` at the ROOT, which is what itch.io requires of an
/// HTML5 upload. `package` picks what goes in it: a directory build (assets as
/// separate entries, which the browser caches individually) or the single-file
/// build (one entry).
fn build_itch_zip(project: &str, out: Option<&str>, package: WebPackage) -> Result<()> {
    let staging = tempfile::tempdir()?;
    let mut zip = crate::zip_writer::ZipWriter::new();
    let mut entries = 0usize;

    match package {
        WebPackage::SingleFile => {
            let (path, _) = build_single_file(project, Some(
                staging.path().join("single.html").to_str().expect("utf8 temp path"),
            ))?;
            zip.add("index.html", &std::fs::read(&path)?)?;
            entries += 1;
        }
        WebPackage::Directory => {
            build_web_bundle(project, staging.path(), true, false)?;
            for entry in walkdir::WalkDir::new(staging.path()).sort_by_file_name() {
                let entry = entry?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(staging.path())
                    .expect("under staging")
                    .to_string_lossy()
                    .replace('\\', "/");
                zip.add(&rel, &std::fs::read(entry.path())?)?;
                entries += 1;
            }
        }
    }

    let bytes = zip.finish()?;
    let path = web_out_file(project, out, "zip");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &bytes)?;
    log::info!(
        target: "WEB",
        "{} -> {} (itch.io zip, {} entries, {:.1} MB)",
        project,
        path.display(),
        entries,
        bytes.len() as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}

fn build_web_bundle(
    project: &str,
    out_dir: &Path,
    copy_assets: bool,
    watch: bool,
) -> Result<Vec<WebAsset>> {
    let (emitted, cache, ws) = crate::js_test::try_emit_js_program_project_cached(project)?;
    write_web_bundle(project, out_dir, copy_assets, watch, &emitted, &ws, &cache)
}

fn write_web_bundle(
    project: &str,
    out_dir: &Path,
    copy_assets: bool,
    watch: bool,
    emitted: &skull_codegen_js::emit::JsProgram,
    ws: &bone_semantic::inference::Workspace,
    cache: &skull_codegen_js::emit::JsEmitCache,
) -> Result<Vec<WebAsset>> {
    let register_lines = shader_register_lines_js(ws, cache)?;
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
        let selection = rock_host::release::AssetSelection {
            assets: assets.clone(),
            missing: Vec::new(),
        };
        rock_host::release::copy_assets(&selection, out_dir)?;
    }

    let game_js_path = out_dir.join("game.js");
    let index_path = out_dir.join("index.html");
    rock_host::project::write_atomic(&game_js_path, &game_js)?;
    rock_host::project::write_atomic(&index_path, &index_html)?;
    if watch {
        rock_host::project::write_atomic(&out_dir.join("incremental.js"), "")?;
    }
    log::info!(target: "WEB", "{} -> {} ({} + game.js)", project, out_dir.display(), index_path.display());
    Ok(assets)
}

enum WebCompile {
    NoChange,
    Overlay,
    Reload,
}

struct WebCompiler {
    project: crate::live_project::LiveProject,
    cache: skull_codegen_js::emit::JsEmitCache,
    main_js_name: Option<String>,
    /// The program's `embed_string` inputs (absolute), refreshed per build:
    /// an edit to one is a SOURCE change the extension filter can't know,
    /// exactly as in `rock stone` (docs/comptime-plan.md).
    comptime_files: std::collections::HashSet<PathBuf>,
}

impl WebCompiler {
    fn init(project: &str, out_dir: &Path) -> Result<(Self, Vec<WebAsset>)> {
        let include_tests =
            !rock_host::project::is_game_project(&PathBuf::from(project)).unwrap_or(false);
        let live = crate::live_project::LiveProject::init(project, include_tests)?;
        let (emitted, cache) = skull_codegen_js::emit::emit_program_cached(
            live.workspace(),
            live.entry_module(),
        )
        .map_err(|error| anyhow::anyhow!(
            "{}",
            crate::skull_backend::render_project_err(live.sources(), &error)
        ))?;
        let assets = write_web_bundle(
            project,
            out_dir,
            false,
            true,
            &emitted,
            live.workspace(),
            &cache,
        )?;
        let main_js_name = emitted.main_js_name.clone();
        let mut compiler = Self {
            project: live,
            cache,
            main_js_name,
            comptime_files: Default::default(),
        };
        compiler.refresh_comptime_files();
        Ok((compiler, assets))
    }

    fn refresh_comptime_files(&mut self) {
        self.comptime_files = skull_mir::lower::comptime_embed_files(self.project.workspace())
            .into_iter()
            .map(|p| PathBuf::from(p.as_str()))
            .collect();
    }

    fn recompile(
        &mut self,
        project: &str,
        out_dir: &Path,
        changed: &[PathBuf],
    ) -> Result<WebCompile> {
        let outcome = self.recompile_inner(project, out_dir, changed);
        if outcome.is_ok() {
            self.refresh_comptime_files();
        }
        outcome
    }

    fn recompile_inner(
        &mut self,
        project: &str,
        out_dir: &Path,
        changed: &[PathBuf],
    ) -> Result<WebCompile> {
        match self.project.recompile(changed)? {
            crate::live_project::LiveUpdate::NoChange => Ok(WebCompile::NoChange),
            crate::live_project::LiveUpdate::Full { .. } => {
                log::info!(target: "WEB", "Interface change requires a page reload");
                self.emit_full(project, out_dir)?;
                Ok(WebCompile::Reload)
            }
            crate::live_project::LiveUpdate::BodyOnly { reinferred, .. } => {
                match skull_codegen_js::emit::emit_overlay(
                    self.project.workspace(),
                    self.project.entry_module(),
                    &mut self.cache,
                    &reinferred,
                )
                .map_err(|error| anyhow::anyhow!(
                    "{}",
                    crate::skull_backend::render_project_err(self.project.sources(), &error)
                ))? {
                    skull_codegen_js::emit::JsReload::Overlay(mut overlay) => {
                        overlay.push_str(&shader_register_lines_js(
                            self.project.workspace(),
                            &self.cache,
                        )?);
                        rock_host::project::write_atomic(
                            &out_dir.join("incremental.js"),
                            &overlay,
                        )?;
                        self.write_current_game(project, out_dir)?;
                        Ok(WebCompile::Overlay)
                    }
                    skull_codegen_js::emit::JsReload::FullRequired => {
                        log::info!(target: "WEB", "Module-load JS changed; page reload required");
                        self.emit_full(project, out_dir)?;
                        Ok(WebCompile::Reload)
                    }
                    skull_codegen_js::emit::JsReload::NoChange => Ok(WebCompile::NoChange),
                }
            }
        }
    }

    fn emit_full(&mut self, project: &str, out_dir: &Path) -> Result<Vec<WebAsset>> {
        let (emitted, cache) = skull_codegen_js::emit::emit_program_cached(
            self.project.workspace(),
            self.project.entry_module(),
        )
        .map_err(|error| anyhow::anyhow!(
            "{}",
            crate::skull_backend::render_project_err(self.project.sources(), &error)
        ))?;
        let assets = write_web_bundle(
            project,
            out_dir,
            false,
            true,
            &emitted,
            self.project.workspace(),
            &cache,
        )?;
        self.main_js_name = emitted.main_js_name;
        self.cache = cache;
        Ok(assets)
    }

    fn write_current_game(&self, project: &str, out_dir: &Path) -> Result<()> {
        let game_dir = web_game_dir(project);
        let assets = collect_web_assets(Path::new(project), &game_dir)?;
        let preload: Vec<String> = assets.iter().map(|asset| asset.key.clone()).collect();
        let emitted = skull_codegen_js::emit::JsProgram {
            program: self.cache.current_program().to_string(),
            main_js_name: self.main_js_name.clone(),
        };
        let register_lines = shader_register_lines_js(self.project.workspace(), &self.cache)?;
        let game_js = assemble_web_program(&emitted, &register_lines, &preload, &game_dir);
        rock_host::project::write_atomic(&out_dir.join("game.js"), &game_js)?;
        Ok(())
    }
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
    let config = rock_host::stick::StickConfig::find_from_dir(project)?
        .map(|(_, config)| config);
    let workspace_root = rock_host::project::find_workspace_root(project)
        .or_else(|| rock_host::project::find_workspace_root(&std::env::current_dir().unwrap_or_default()))
        .ok_or_else(|| anyhow::anyhow!("web: cannot find the Rock workspace root for {}", project.display()))?;
    let selection = rock_host::release::select_assets(
        project,
        game_dir,
        config.as_ref(),
        &workspace_root,
        true,
    )?;
    for missing in selection.missing {
        log::warn!(target: "WEB", "release asset {} does not exist, skipping", missing.display());
    }
    Ok(selection.assets)
}

struct ReloadHub {
    generation: AtomicU64,
    subscribers: Mutex<Vec<std::sync::mpsc::Sender<BrowserEvent>>>,
}

#[derive(Clone, Copy)]
enum BrowserEventKind {
    Overlay,
    Reload,
}

#[derive(Clone, Copy)]
struct BrowserEvent {
    generation: u64,
    kind: BrowserEventKind,
}

impl ReloadHub {
    fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            subscribers: Mutex::new(Vec::new()),
        }
    }

    fn subscribe(&self) -> std::sync::mpsc::Receiver<BrowserEvent> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    fn publish(&self, kind: BrowserEventKind) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let event = BrowserEvent { generation, kind };
        self.subscribers
            .lock()
            .unwrap()
            .retain(|subscriber| subscriber.send(event).is_ok());
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

/// Split a watch event into recompile-driving SOURCE paths (`.rock`,
/// `stick.toml`, `embed_string` inputs) and whether any plain asset changed.
/// An embed or stick.toml edit must go THROUGH recompile: classifying it as
/// an asset published a bare browser Reload that served the old bundle --
/// stale JS presented as a successful hot update. A stick.toml or comptime
/// path is not a discovered `.rock` source, so `LiveProject::recompile` takes
/// the full-rebuild arm, whose session rebuild re-resolves declared inputs.
fn partition_web_event(
    paths: &std::collections::HashSet<PathBuf>,
    comptime_files: &std::collections::HashSet<PathBuf>,
) -> (Vec<PathBuf>, bool) {
    let mut sources = Vec::new();
    let mut has_asset_changes = false;
    for path in paths {
        let is_rock = path.extension().is_some_and(|extension| extension == "rock");
        let is_stick = path.file_name().and_then(|n| n.to_str()) == Some("stick.toml");
        if is_rock || is_stick || comptime_files.contains(path) {
            sources.push(path.clone());
        } else {
            has_asset_changes = true;
        }
    }
    (sources, has_asset_changes)
}

#[cfg(test)]
mod partition_tests {
    use super::partition_web_event;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn set(paths: &[&str]) -> HashSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn embeds_and_stick_toml_are_sources_not_assets() {
        let comptime = set(&["/proj/prebuilt/table.lua"]);
        let (sources, assets) = partition_web_event(
            &set(&["/proj/main.rock", "/proj/stick.toml", "/proj/prebuilt/table.lua"]),
            &comptime,
        );
        assert_eq!(sources.len(), 3, "all three drive a recompile: {sources:?}");
        assert!(!assets, "none of these is a bare asset");
    }

    #[test]
    fn a_plain_asset_still_reloads_without_recompiling() {
        let (sources, assets) =
            partition_web_event(&set(&["/proj/assets/sprite.png"]), &Default::default());
        assert!(sources.is_empty());
        assert!(assets);
    }
}

fn watch_web(project: &str, out: Option<&str>, port: u16) -> Result<()> {
    rock_host::project::ensure_workspace_root()?;
    let out_dir = web_out_dir(project, out);
    let (mut compiler, initial_assets) = match WebCompiler::init(project, &out_dir) {
        Ok((compiler, assets)) => (Some(compiler), assets),
        Err(e) => {
            log::error!(target: "WEB", "{e}");
            log::info!(target: "WEB", "watching for changes; the server will publish the first successful build");
            (None, Vec::new())
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

        let no_comptime = Default::default();
        let (source_paths, has_asset_changes) = partition_web_event(
            &event.paths,
            compiler.as_ref().map(|c| &c.comptime_files).unwrap_or(&no_comptime),
        );
        let outcome = if compiler.is_none() {
            WebCompiler::init(project, &state.out_dir).map(|(new_compiler, assets)| {
                compiler = Some(new_compiler);
                state.publish_assets(assets);
                WebCompile::Reload
            })
        } else if source_paths.is_empty() {
            Ok(WebCompile::NoChange)
        } else {
            compiler
                .as_mut()
                .expect("checked above")
                .recompile(project, &state.out_dir, &source_paths)
        };

        match outcome {
            Ok(outcome) => {
                let game_dir = web_game_dir(project);
                match collect_web_assets(Path::new(project), &game_dir) {
                    Ok(assets) => state.publish_assets(assets),
                    Err(e) => {
                        log::error!(target: "WEB", "{e}; keeping the last published asset map");
                    }
                }
                let kind = if has_asset_changes {
                    Some(BrowserEventKind::Reload)
                } else {
                    match outcome {
                        WebCompile::NoChange => None,
                        WebCompile::Overlay => Some(BrowserEventKind::Overlay),
                        WebCompile::Reload => Some(BrowserEventKind::Reload),
                    }
                };
                if let Some(kind) = kind {
                    state.reload.publish(kind);
                    match kind {
                        BrowserEventKind::Overlay => {
                            log::info!(target: "WEB", "Browser overlay updated")
                        }
                        BrowserEventKind::Reload => {
                            log::info!(target: "WEB", "Browser build updated")
                        }
                    }
                }
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
        for event in reloads {
            let kind = match event.kind {
                BrowserEventKind::Overlay => "overlay",
                BrowserEventKind::Reload => "reload",
            };
            write!(
                stream,
                "data: {{\"generation\":{},\"kind\":\"{}\"}}\n\n",
                event.generation, kind
            )?;
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

/// A `[string]` literal in the JS Vec repr. Unlike the Lua sibling
/// (`skull_codegen_lua::lua_string_vec`) `_data` is 0-based.
fn js_string_vec<S: AsRef<str>>(items: &[S]) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("{_data: [");
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&crate::js_test::js_str(s.as_ref()));
    }
    let _ = write!(out, "], len: {}, __ty: \"Vec\"}}", items.len());
    out
}

/// The `[int]` sibling of [`js_string_vec`] (a uniform block's member offsets).
fn js_int_vec(items: &[i64]) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("{_data: [");
    for (i, n) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{n}");
    }
    let _ = write!(out, "], len: {}, __ty: \"Vec\"}}", items.len());
    out
}

/// One `__rock_shader_register("<key>", "<glsl>", <slots>)` line per distinct
/// `rock_shader` pair -- the JS mirror of skull's Lua `shader_registration_lines`
/// (same collector, same Core330 profile: the profile is a property of the GL
/// context, and the web `stone_create_shader_from_combined` shim does the ES
/// rewrite at shader-compile time, like the native GLES path). Emitted after
/// the program (module load), before the boot line, so registrations queue in
/// PENDING_SHADER_SRCS pre-GL exactly as on a native cold start.
fn shader_register_lines_js(
    ws: &bone_semantic::inference::Workspace,
    cache: &skull_codegen_js::emit::JsEmitCache,
) -> Result<String> {
    use std::fmt::Write as _;
    let pairs =
        skull_mir::lower::collect_shader_pairs(ws).map_err(|e| anyhow::anyhow!(e.message))?;
    let programs: Vec<skull::shader_artifact::ShaderProgram> = pairs
        .iter()
        .map(|pair| skull::shader_artifact::ShaderProgram {
            vert: pair.vert,
            frag: pair.frag,
        })
        .collect();
    let shader_scx = skull_mir::lower::ScalarCtx::new_values_first_class(ws)
        .sharing(cache.interner_handle());
    let view = skull::shader_artifact::realize_shader_with_context(
        ws,
        cache.graph(),
        &skull::shader_artifact::ShaderArtifactRequest {
            spec: skull::target_projection::TargetSpec {
                shader: Some(skull::target_projection::ShaderTargetSpec::Glsl(
                    skull_codegen_glsl::GlslProfile::Core330,
                )),
                ..skull::target_projection::LUAJIT_DESKTOP
            },
            programs: &programs,
        },
        &shader_scx,
    )
    .ok_or_else(|| anyhow::anyhow!("GLSL shader view is unavailable for the JS session"))?;
    if view.program_views.len() != pairs.len() {
        anyhow::bail!("GLSL shader view/collection denominator differs for the JS session");
    }
    let mut out = String::new();
    let profile = skull_codegen_glsl::GlslProfile::Core330;
    for (pair, program_view) in pairs.iter().zip(&view.program_views) {
        let emission = match pair.vert {
            Some(_) => skull_codegen_glsl::emit_shader_pipeline_with_view(
                ws,
                &shader_scx,
                program_view,
                profile,
            ),
            None => skull_codegen_glsl::emit_shader_entry_with_view(
                ws,
                &shader_scx,
                program_view,
                profile,
            ),
        }
        .map_err(|e| anyhow::anyhow!(e.message))?;
        // The `@uniform` blocks' std140 layouts, in binding order (A1 at most
        // one, an A2 pipeline one per stage), as PARALLEL FLAT lists -- see the
        // Lua driver in skull/src/mir_emit.rs, which this mirrors. A SHORT call
        // is silent on JS (the missing argument reads `undefined`), which is
        // what rock-cli/tests/shader_register_arity_gate.rs exists to catch.
        let w = skull_codegen_glsl::BlockWire::of(
            emission.abi.uniform_blocks.iter(),
            emission.abi.uniform_array_blocks.iter(),
        );
        let fullscreen = pair.vert.is_none();
        let semantic_wire = emission.interface.semantic_wire(fullscreen);
        let _ = writeln!(
            out,
            "__rock_shader_register({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {});",
            crate::js_test::js_str(&pair.key),
            crate::js_test::js_str(&emission.source),
            js_string_vec(&emission.abi.texture_units),
            js_string_vec(&w.names),
            js_int_vec(&w.bindings),
            js_int_vec(&w.sizes),
            js_int_vec(&w.fingerprints),
            js_int_vec(&w.counts),
            js_string_vec(&w.member_names),
            js_int_vec(&w.member_offsets),
            js_int_vec(&w.member_kinds),
            js_int_vec(&w.member_capacities),
            js_int_vec(&w.member_strides),
            fullscreen,
            crate::js_test::js_str(&semantic_wire),
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
        Some(main) => p.push_str(&format!(
            "var __ROCK_WEB_READY = __rock_web_boot({main});\n"
        )),
        None => p.push_str("var __ROCK_WEB_READY = __rock_web_boot(null);\n"),
    }
    p
}

/// The loader page: a full-viewport canvas (`#stone-canvas`, the id the WebGL2
/// context boot in `JS_WEB_RUNTIME` will bind in MB) and the classic
/// `<script src="game.js">`.
fn index_html(title: &str, watch: bool) -> String {
    let reload = if watch {
        r#"<script>
(function () {
  var apply = Promise.resolve(__ROCK_WEB_READY);
  new EventSource('/__rock_reload').onmessage = function (message) {
    var update = JSON.parse(message.data);
    if (update.kind === 'reload') {
      location.reload();
      return;
    }
    apply = apply.then(function () {
      return new Promise(function (resolve, reject) {
        var script = document.createElement('script');
        script.src = '/incremental.js?v=' + update.generation;
        script.onload = resolve;
        script.onerror = reject;
        document.head.appendChild(script);
      });
    }).catch(function () { location.reload(); });
  };
})();
</script>
"#
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
<script src="game.js"></script>
{reload}\
</body>
</html>
"#
    )
}

#[cfg(test)]
mod shader_registration_removal_tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    const V0: &str = r#"
@shader(fragment)
fn frag_a(tc: Vec2) -> Vec4 {
    return Vec4 { x: tc.x, y: 0.0, z: 0.0, w: 1.0 }
}

@shader(fragment)
fn frag_b(tc: Vec2) -> Vec4 {
    return Vec4 { x: 0.0, y: tc.x, z: 0.0, w: 1.0 }
}

fn choose_a() -> string { return rock_shader(frag_a) }
fn choose_b() -> string { return rock_shader(frag_b) }
fn main() { print(choose_a()); print(choose_b()) }

@nomangle inline(js) fn __rock_shader_register(
    key: string,
    src: string,
    texture_slots: [string],
    names: [string],
    bindings: [int],
    sizes: [int],
    fingerprints: [int],
    counts: [int],
    member_names: [string],
    member_offsets: [int],
    member_kinds: [int],
    member_capacities: [int],
    member_strides: [int]
) = """
    (globalThis.__ROCK_SHADERS = globalThis.__ROCK_SHADERS || {})[key] = src;
"""
"#;

    fn v1() -> String {
        V0.replace("return rock_shader(frag_b)", "return \"removed\"")
    }

    fn key(
        ws: &bone_semantic::inference::Workspace,
        program: &skull::shader_artifact::ShaderProgram,
    ) -> String {
        let name = |fref: skull_mir::mir::FuncRef| {
            ws.module(fref.module).functions[fref.func].name.to_string()
        };
        match program.vert {
            Some(vert) => format!("{}+{}", name(vert), name(program.frag)),
            None => name(program.frag),
        }
    }

    fn logical_keys(
        ws: &bone_semantic::inference::Workspace,
        graph: &skull::instance_graph::InstanceGraph,
    ) -> BTreeSet<String> {
        let pairs = skull_mir::lower::collect_shader_pairs(ws).expect("shader collection");
        let programs: Vec<_> = pairs
            .iter()
            .map(|pair| skull::shader_artifact::ShaderProgram {
                vert: pair.vert,
                frag: pair.frag,
            })
            .collect();
        let scx = skull_mir::lower::ScalarCtx::new_values_first_class(ws)
            .sharing(Arc::clone(graph.interner()));
        let view = skull::shader_artifact::realize_shader_with_context(
            ws,
            graph,
            &skull::shader_artifact::ShaderArtifactRequest {
                spec: skull::target_projection::TargetSpec {
                    shader: Some(skull::target_projection::ShaderTargetSpec::Glsl(
                        skull_codegen_glsl::GlslProfile::Core330,
                    )),
                    ..skull::target_projection::LUAJIT_DESKTOP
                },
                programs: &programs,
            },
            &scx,
        )
        .expect("shader artifact view");
        assert!(
            view.program_views
                .iter()
                .all(|program| !program.stages.is_empty())
        );
        view.program_views
            .iter()
            .map(|program| key(ws, &program.program))
            .collect()
    }

    fn registration_keys(text: &str) -> BTreeSet<String> {
        let prefix = "__rock_shader_register(\"";
        text.lines()
            .filter_map(|line| {
                let start = line.find(prefix).map(|index| index + prefix.len())?;
                let end = line[start..].find('"')?;
                Some(line[start..start + end].to_string())
            })
            .collect()
    }

    #[test]
    fn web_reload_removing_one_shader_callsite_removes_one_runtime_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join("project");
        let output = dir.path().join("web");
        std::fs::create_dir_all(&project).expect("project");
        std::fs::write(project.join("stick.toml"), "[package]\nname = \"shader-removal\"\n")
            .expect("manifest");
        let source = project.join("main.rock");
        std::fs::write(&source, V0).expect("source");
        let project_name = project.to_str().expect("project path");
        let (mut compiler, _) =
            WebCompiler::init(project_name, &output).expect("initial web build");
        let baseline = std::fs::read_to_string(output.join("game.js")).expect("baseline game");
        let wanted = BTreeSet::from(["frag_a".to_string(), "frag_b".to_string()]);
        assert_eq!(registration_keys(&baseline), wanted);
        assert_eq!(logical_keys(compiler.project.workspace(), compiler.cache.graph()), wanted);

        std::fs::write(&source, v1()).expect("edited source");
        let reload = compiler
            .recompile(project_name, &output, std::slice::from_ref(&source))
            .expect("web reload");
        assert!(
            matches!(reload, WebCompile::Reload),
            "a shader key removal must require a page reload"
        );

        let current = BTreeSet::from(["frag_a".to_string()]);
        assert_eq!(logical_keys(compiler.project.workspace(), compiler.cache.graph()), current);
        let mut runtime = registration_keys(
            &std::fs::read_to_string(output.join("game.js")).unwrap(),
        );
        let incremental = std::fs::read_to_string(output.join("incremental.js"))
            .expect("incremental web artifact");
        runtime.extend(registration_keys(&incremental));
        assert_eq!(
            runtime, current,
            "web runtime registration population retained a removed key"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // No game in the tree emits `</script` today, so an integration test on a
    // real build cannot tell an escaping bug from an absent one. This gates the
    // pure function at its own granularity, with inputs that actually carry the
    // sequence.
    #[test]
    fn escape_script_close_neutralizes_every_case() {
        // The HTML tokenizer ends a script at `</script` regardless of case or
        // what follows, so all of these would truncate the program.
        assert_eq!(escape_script_close(r#"var s = "</script>";"#), r#"var s = "<\/script>";"#);
        assert_eq!(escape_script_close("a</SCRIPT>b"), "a<\\/SCRIPT>b");
        assert_eq!(escape_script_close("a</ScRiPt >b"), "a<\\/ScRiPt >b");
        // `</script` with no `>` still ends the element.
        assert_eq!(escape_script_close("x</scriptfoo"), "x<\\/scriptfoo");
        // Several in one program, and one at the very end.
        assert_eq!(escape_script_close("</script></script>"), "<\\/script><\\/script>");
    }

    #[test]
    fn escape_script_close_leaves_ordinary_program_text_alone() {
        // A rewrite that fired on any `<` or `</` would corrupt comparisons and
        // arrow functions, which is most of an emitted program.
        for src in ["a < b", "a </ b", "x=>x<2", "</style>", "</scrip", "<script>"] {
            assert_eq!(escape_script_close(src), src, "rewrote {src:?}");
        }
    }

    #[test]
    fn escape_script_close_is_byte_exact_on_multibyte_input() {
        // The scan walks BYTES; pushing a byte as a char would mangle any
        // non-ASCII in an emitted string literal into mojibake.
        let s = "let s = \"\u{00e9}\u{4e2d}\u{1f600}\";";
        assert_eq!(escape_script_close(s), s);
    }
}
