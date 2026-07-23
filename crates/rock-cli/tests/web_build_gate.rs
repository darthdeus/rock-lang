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
