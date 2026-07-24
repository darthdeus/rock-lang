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

use crate::Result;
use std::path::{Path, PathBuf};

/// Build the static browser bundle for `project` into `out` (default
/// `<project>/web`). Reuses project-mode JS emission (`emit_js_program_project`)
/// -- so the same module walk, ordering, and `emit_program` the `rock run <dir>
/// --backend js` path exercises -- then assembles `game.js` + `index.html`.
pub fn build_web(project: &str, out: Option<&str>) -> Result<()> {
    let out_dir = match out {
        Some(o) => PathBuf::from(o),
        None => PathBuf::from(project).join("web"),
    };
    std::fs::create_dir_all(&out_dir)?;

    // Project-mode emission (diagnoses + exits on any rejection, like the run path).
    let (emitted, ws) = crate::js_test::emit_js_program_project(project);
    let register_lines = shader_register_lines_js(&ws)?;
    let game_dir = project.trim_end_matches('/');
    let preload = stage_assets(game_dir, &out_dir)?;
    let game_js = assemble_web_program(&emitted, &register_lines, &preload, game_dir);
    let title = Path::new(project)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("rock");
    let index_html = index_html(title);

    let game_js_path = out_dir.join("game.js");
    let index_path = out_dir.join("index.html");
    rock_host::project::write_atomic(&game_js_path, &game_js)?;
    rock_host::project::write_atomic(&index_path, &index_html)?;
    log::info!(target: "WEB", "{} -> {} ({} + game.js)", project, out_dir.display(), index_path.display());
    Ok(())
}

/// Stage the project's release asset set into the build dir and return the
/// boot-preload list -- registry keys fetched into `__ROCK_WEB_ASSETS` before
/// `main()` runs (each key doubles as the fetch URL relative to index.html).
///
/// Driven by stick.toml `[release].assets` (default `["assets"]`), mirroring
/// `tools/release/collect_assets.py`: each entry is staged under
/// `out_dir/{game_dir}/{rel}` and keyed `{game_dir}/{rel}` -- the exact string
/// the runtime builds (`{GAME_SOURCE_DIR}/{rel}` with GAME_SOURCE_DIR = the
/// repo-relative game dir on web, matching FIRE_GAME_DIR on native release).
/// `file_read` literals that already carry the `games/x/` prefix (chess's book
/// tables) resolve directly; asset_path-routed loads (sounds, fonts) match
/// because GAME_SOURCE_DIR is set from the same string.
///
/// The engine-bundled DroidSans is staged under `{game_dir}/assets/fonts/` as
/// the fallback the runner default config names (`assets/fonts/DroidSans.ttf`)
/// -- the web mirror of stone-rs's embedded `FALLBACK_FONT`.
fn stage_assets(game_dir: &str, out_dir: &Path) -> Result<Vec<String>> {
    let project = Path::new(game_dir);
    let mut preload: Vec<String> = Vec::new();
    let stage_one = |src: &Path, key: &str, preload: &mut Vec<String>| -> Result<()> {
        let dst = out_dir.join(key);
        std::fs::create_dir_all(dst.parent().expect("parent"))?;
        std::fs::copy(src, &dst)?;
        preload.push(key.to_string());
        Ok(())
    };

    for rel in release_assets(project) {
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
                let key = format!("{}/{}", game_dir, rel_to_game.to_string_lossy().replace('\\', "/"));
                stage_one(entry.path(), &key, &mut preload)?;
            }
        } else {
            let key = format!("{}/{}", game_dir, rel.replace('\\', "/"));
            stage_one(&src, &key, &mut preload)?;
        }
    }

    let fallback_key = format!("{}/assets/fonts/DroidSans.ttf", game_dir);
    if !preload.iter().any(|p| p == &fallback_key) {
        let src = repo_relative("stone-rs/fonts/DroidSans.ttf");
        if src.is_file() {
            stage_one(&src, &fallback_key, &mut preload)?;
        } else {
            log::warn!(target: "WEB", "engine fallback font missing at {}; text will not render", src.display());
        }
    }
    Ok(preload)
}

/// The `[release].assets` staging list from a project's stick.toml. Defaults to
/// `["assets"]` (plus `level.txt` when present) when the table is absent,
/// mirroring `tools/release/collect_assets.py`. A malformed stick.toml
/// propagates as an error rather than silently defaulting.
fn release_assets(project: &Path) -> Vec<String> {
    let path = project.join("stick.toml");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return default_release_assets(project),
    };
    let value: serde_json::Value = basic_toml::from_str(&content)
        .unwrap_or_else(|e| panic!("web: failed to parse {}: {}", path.display(), e));
    match value.get("release").and_then(|r| r.get("assets")).and_then(|a| a.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        None => default_release_assets(project),
    }
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

/// One `__rock_shader_register("<key>", "<glsl>")` line per distinct
/// `rock_shader` pair -- the JS mirror of skull's Lua `shader_registration_lines`
/// (same collector, same Core330 profile: the profile is a property of the GL
/// context, and the web `stone_create_shader_from_combined` shim does the ES
/// rewrite at shader-compile time, like the native GLES path). Emitted after
/// the program (module load), before the boot line, so registrations queue in
/// PENDING_SHADER_SRCS pre-GL exactly as on a native cold start.
fn shader_register_lines_js(ws: &bone_semantic::inference::Workspace) -> Result<String> {
    use std::fmt::Write as _;
    let pairs = skull_mir::lower::collect_shader_pairs(ws).map_err(|e| anyhow::anyhow!(e.message))?;
    let mut out = String::new();
    let profile = skull_codegen_glsl::GlslProfile::Core330;
    for pair in pairs {
        let fname = ws.module(pair.frag.module).functions[pair.frag.func].name;
        let glsl = match pair.vert {
            Some(vert) => {
                let vname = ws.module(vert.module).functions[vert.func].name;
                skull_codegen_glsl::emit_shader_pipeline(ws, vert.module, vname.as_str(), fname.as_str(), profile)
            }
            None => skull_codegen_glsl::emit_shader_entry(ws, pair.frag.module, fname.as_str(), profile),
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
    p.push_str(&format!("\nvar __ROCK_WEB_PRELOAD = [{}];\n", manifest.join(", ")));
    match &emitted.main_js_name {
        Some(main) => p.push_str(&format!("__rock_web_boot({main});\n")),
        None => p.push_str("__rock_web_boot(null);\n"),
    }
    p
}

/// The loader page: a full-viewport canvas (`#stone-canvas`, the id the WebGL2
/// context boot in `JS_WEB_RUNTIME` will bind in MB) and the classic
/// `<script src="game.js">`.
fn index_html(title: &str) -> String {
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
</body>
</html>
"#
    )
}
