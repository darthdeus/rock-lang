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

fn build_web(game: &str, out: &std::path::Path) {
    let status = Command::new(rock_bin())
        .current_dir(repo_root())
        .args(["web", game, "--out", out.to_str().unwrap()])
        .status()
        .expect("spawn rock web");
    assert!(status.success(), "rock web {game} failed");
    assert!(out.join("game.js").is_file(), "game.js not emitted");
    assert!(out.join("index.html").is_file(), "index.html not emitted");
}

fn run_fake_gl_smoke(game: &str) {
    if Command::new("node").arg("--version").output().map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP fake_gl_smoke ({game}): node not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    build_web(game, dir.path());
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
        "fake-GL smoke ({game}) failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn web_fake_gl_smoke() {
    run_fake_gl_smoke("games/match3");
}

// Chess: the fake-GL harness is game-agnostic (drives web_run_frame). Chess
// draws the board + panels every frame, well past the >=2 stride-40 TRIANGLES
// floor; the emit-unblock siblings (proc/file/timestamp) + WebAudio must not
// throw headlessly (no AudioContext -> silent stubs).
#[test]
fn web_fake_gl_smoke_chess() {
    run_fake_gl_smoke("games/chess");
}

#[test]
fn web_chrome_screenshot_nonblank_with_text() {
    let chrome = ["google-chrome", "google-chrome-stable", "chromium"]
        .iter()
        .find(|c| Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false));
    let Some(chrome) = chrome else {
        eprintln!("SKIP web_chrome_screenshot_nonblank_with_text: no chrome on PATH");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    build_web("games/match3", dir.path());

    // Serve over HTTP: `fetch` of the preloaded TTF is CORS-blocked on file://
    // ("serve with any static file server" is the deliverable's contract).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut server = Command::new("python3")
        .args(["-m", "http.server", &port.to_string(), "-d"])
        .arg(dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn http.server");
    std::thread::sleep(std::time::Duration::from_millis(300));

    let shot = dir.path().join("shot.png");
    let out = Command::new(chrome)
        .args([
            "--headless=new",
            "--no-sandbox",
            "--enable-unsafe-swiftshader",
            "--enable-logging=stderr",
            "--v=0",
            "--window-size=1280,720",
            // 10s virtual budget: software-WebGL (swiftshader) boot can outlast
            // a smaller budget and yield a blank pre-first-frame capture.
            "--virtual-time-budget=10000",
        ])
        .arg(format!("--screenshot={}", shot.display()))
        .arg(format!("http://127.0.0.1:{port}/index.html"))
        .output()
        .expect("spawn chrome");
    let _ = server.kill();
    let _ = server.wait();
    assert!(out.status.success(), "chrome exited non-zero");

    // MC observable: the canvas2d atlas bake ran (real TTF fetched + baked).
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("font atlas baked"),
        "canvas2d glyph atlas did not bake (no 'font atlas baked' console line).\nchrome stderr tail:\n{}",
        &stderr[stderr.len().saturating_sub(2000)..]
    );

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

// Chess in real headless Chrome: serves the build over HTTP (14 MB of book/
// opening/sound assets + font are fetched before main), bakes the glyph atlas,
// and the rendered board (light/dark tiles + pieces + side panels) must clear
// the >=8-color bar. The 10s virtual budget matches match3 -- a larger budget
// over-runs software-WebGL (swiftshader exhausts its staging pool and loses the
// context, exiting non-zero) after the frame is already captured.
#[test]
fn web_chrome_screenshot_nonblank_chess() {
    let chrome = ["google-chrome", "google-chrome-stable", "chromium"]
        .iter()
        .find(|c| Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false));
    let Some(chrome) = chrome else {
        eprintln!("SKIP web_chrome_screenshot_nonblank_chess: no chrome on PATH");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    build_web("games/chess", dir.path());

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut server = Command::new("python3")
        .args(["-m", "http.server", &port.to_string(), "-d"])
        .arg(dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn http.server");
    std::thread::sleep(std::time::Duration::from_millis(300));

    let shot = dir.path().join("shot.png");
    let out = Command::new(chrome)
        .args([
            "--headless=new",
            "--no-sandbox",
            "--enable-unsafe-swiftshader",
            "--enable-logging=stderr",
            "--v=0",
            "--window-size=1280,720",
            "--virtual-time-budget=10000",
        ])
        .arg(format!("--screenshot={}", shot.display()))
        .arg(format!("http://127.0.0.1:{port}/index.html"))
        .output()
        .expect("spawn chrome");
    let _ = server.kill();
    let _ = server.wait();
    assert!(out.status.success(), "chrome exited non-zero");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("font atlas baked"),
        "canvas2d glyph atlas did not bake (no 'font atlas baked' console line).\nchrome stderr tail:\n{}",
        &stderr[stderr.len().saturating_sub(2000)..]
    );

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
        "screenshot looks blank ({} distinct quantized colors); the chess board should produce many",
        colors.len()
    );
}
