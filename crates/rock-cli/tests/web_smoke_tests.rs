//! stone-web Arc 2 MB gates (game-integration tier, excluded from the default
//! nextest profile like game_snapshot_tests -- runs under `-P full`):
//!
//! - `web_fake_gl_smoke`: `rock web games/match3` -> tools/web/fake_gl_smoke.js
//!   drives 10 frames of the emitted build under node against a recorded-call
//!   GL stub, asserting per frame: >=1 clear, >=2 TRIANGLES draws at the
//!   40-byte (10-float) vertex stride, `running` stays true.
//! - `web_chrome_screenshot_nonblank`: headless Chrome renders the build and
//!   the screenshot must not be blank (>= 8 distinct colors: board tiles +
//!   frame + background). Skips with a log when chrome is absent.

use std::path::PathBuf;
use std::process::Command;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn build_match3_web(out: &std::path::Path) {
    let status = Command::new(rock_bin())
        .current_dir(repo_root())
        .args(["web", "games/match3", "--out", out.to_str().unwrap()])
        .status()
        .expect("spawn rock web");
    assert!(status.success(), "rock web games/match3 failed");
    assert!(out.join("game.js").is_file(), "game.js not emitted");
    assert!(out.join("index.html").is_file(), "index.html not emitted");
}

#[test]
fn web_fake_gl_smoke() {
    if Command::new("node").arg("--version").output().map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP web_fake_gl_smoke: node not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    build_match3_web(dir.path());
    let harness = repo_root().join("tools/web/fake_gl_smoke.js");
    let out = Command::new("node")
        .arg(&harness)
        .arg(dir.path().join("game.js"))
        .arg("10")
        .output()
        .expect("spawn node smoke");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains("PASS"),
        "fake-GL smoke failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn web_chrome_screenshot_nonblank() {
    let chrome = ["google-chrome", "google-chrome-stable", "chromium"]
        .iter()
        .find(|c| Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false));
    let Some(chrome) = chrome else {
        eprintln!("SKIP web_chrome_screenshot_nonblank: no chrome on PATH");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    build_match3_web(dir.path());
    let shot = dir.path().join("shot.png");
    let url = format!("file://{}/index.html", dir.path().display());
    let status = Command::new(chrome)
        .args([
            "--headless=new",
            "--no-sandbox",
            "--enable-unsafe-swiftshader",
            "--window-size=1280,720",
            // 10s virtual budget: software-WebGL (swiftshader) boot can outlast
            // a smaller budget and yield a blank pre-first-frame capture.
            "--virtual-time-budget=10000",
        ])
        .arg(format!("--screenshot={}", shot.display()))
        .arg(&url)
        .status()
        .expect("spawn chrome");
    assert!(status.success(), "chrome exited non-zero");
    let img = image::open(&shot).expect("read screenshot").to_rgba8();
    let mut colors = std::collections::HashSet::new();
    for p in img.pixels() {
        colors.insert((p[0] / 16, p[1] / 16, p[2] / 16));
        if colors.len() >= 8 {
            break;
        }
    }
    assert!(
        colors.len() >= 8,
        "screenshot looks blank ({} distinct quantized colors); match3 tiles should produce many",
        colors.len()
    );
}
