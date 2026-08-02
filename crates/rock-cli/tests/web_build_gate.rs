//! stone-web Arc 2 MA gate: `rock web <project> --out <dir>` emits a static
//! browser build (`index.html` + `game.js`), and the emitted `game.js` -- with
//! its browser `print` (console.log) -- runs the program end to end under node
//! (a headless stand-in for the browser JS engine; the WebGL2 leaves land in MB).

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn node_available() -> bool {
    Command::new("node").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

#[test]
fn web_build_emits_files_and_runs() {
    let root = std::env::temp_dir().join("rock_web_build_gate");
    let proj = root.join("proj");
    let out = root.join("out");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("stick.toml"), "[package]\nname = \"proj\"\n").unwrap();
    std::fs::write(proj.join("math.rock"), "fn cube(x: int) -> int { return x * x * x }\n").unwrap();
    std::fs::write(
        proj.join("main.rock"),
        "fn main() { print(\"cube3={cube(3)}\") }\n",
    )
    .unwrap();

    let status = Command::new(rock_bin())
        .args(["web", proj.to_str().unwrap(), "--out", out.to_str().unwrap()])
        .status()
        .expect("spawn rock web");
    assert!(status.success(), "rock web failed");

    let index = out.join("index.html");
    let game = out.join("game.js");
    assert!(index.is_file(), "index.html not emitted");
    assert!(game.is_file(), "game.js not emitted");
    let html = std::fs::read_to_string(&index).unwrap();
    assert!(html.contains("game.js") && html.contains("stone-canvas"), "index.html shape wrong:\n{html}");

    if !node_available() {
        eprintln!("SKIP node run: node not found");
        return;
    }
    let out_run = Command::new("node").arg(&game).output().expect("spawn node");
    let stdout = String::from_utf8_lossy(&out_run.stdout);
    assert!(out_run.status.success(), "game.js threw under node:\n{}", String::from_utf8_lossy(&out_run.stderr));
    assert_eq!(stdout.trim(), "cube3=27", "web game.js output drifted:\n{stdout}");
}

/// `stone/engine/web/imgui_stubs.rock` hand-mirrors `stone/engine/ui/imgui.rock`'s
/// `inline(lua)` decl surface, and a hand-mirror drifts silently: an unstubbed
/// bridge only breaks the web build once some decorated debug system happens to
/// reference it. Seven had already drifted out of the mirror when this gate was
/// written. Generating the stubs is the real fix (docs/unfuck-backlog.md); until
/// then this makes the drift loud.
#[test]
fn imgui_web_stubs_cover_every_inline_lua_bridge() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let decls = std::fs::read_to_string(root.join("stone/engine/ui/imgui.rock"))
        .expect("read imgui.rock");
    let stubs = std::fs::read_to_string(root.join("stone/engine/web/imgui_stubs.rock"))
        .expect("read imgui_stubs.rock");

    // Only `inline(lua)` decls need a JS twin: a plain Rock fn emits portably.
    let mut missing = Vec::new();
    for (i, _) in decls.match_indices("inline(lua)") {
        let tail = &decls[i..];
        let Some(fpos) = tail.find("fn ") else { continue };
        let after = &tail[fpos + 3..];
        let name: String =
            after.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.').collect();
        if name.is_empty() {
            continue;
        }
        // Match the declaration, not an incidental mention in a comment/body.
        let decl = format!("fn {name}(");
        if !stubs.contains(&decl) {
            missing.push(name);
        }
    }
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "imgui.rock inline(lua) bridges with no inline(js) stub in \
         stone/engine/web/imgui_stubs.rock: {missing:?}"
    );
}
