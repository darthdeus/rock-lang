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
    let emitted = crate::js_test::emit_js_program_project(project);
    let game_js = assemble_web_program(&emitted);
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

/// Assemble `game.js`: the shared runtime preludes (same set/order as the node
/// path) + `JS_WEB_RUNTIME` (browser print/config, in place of the node RNG/env
/// glue) + a scoped `var __G = {}` + the emitted program, then a boot line that
/// seeds the RNG and calls `main` when the entry defines one.
fn assemble_web_program(emitted: &skull_codegen_js::emit::JsProgram) -> String {
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
    p.push_str("\nvar __G = {};\n");
    // Deterministic default seed (a game re-seeds via random_seed() itself);
    // Date.now() matches the node path's non-deterministic default.
    p.push_str("__rock_rng_seed(Date.now());\n");
    p.push_str(&emitted.program);
    if let Some(main) = &emitted.main_js_name {
        // The engine web runner (MB) replaces this bare call with an rAF arm; at
        // MA a nullary main runs module-load-then-main, matching the node path.
        p.push_str(&format!("\n{main}();\n"));
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
