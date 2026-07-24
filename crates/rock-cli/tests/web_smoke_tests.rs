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
    run_fake_gl_smoke_dpr(game, "1");
}

// dpr != 1 additionally asserts default-FB orthos are LOGICAL-sized (the
// fractional-scaling bug: backing-sized ortho renders at 1/dpr while
// input/hit-testing stay logical, so clicks land on the wrong element).
fn run_fake_gl_smoke_dpr(game: &str, dpr: &str) {
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
        .arg(dpr)
        .output()
        .expect("spawn node smoke");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains("PASS"),
        "fake-GL smoke ({game}, dpr={dpr}) failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn web_fake_gl_smoke() {
    run_fake_gl_smoke("games/match3");
}

#[test]
fn web_fake_gl_smoke_hidpi() {
    run_fake_gl_smoke_dpr("games/match3", "2");
}

// Chess: the fake-GL harness is game-agnostic (drives web_run_frame). Chess
// draws the board + panels every frame, well past the >=2 stride-40 TRIANGLES
// floor; the emit-unblock siblings (proc/file/timestamp) + WebAudio must not
// throw headlessly (no AudioContext -> silent stubs).
#[test]
fn web_fake_gl_smoke_chess() {
    run_fake_gl_smoke("games/chess");
}

// One headless-Chrome screenshot smoke: build the game's web dir, serve it over
// HTTP (`fetch` of preloaded assets is CORS-blocked on file:// -- "serve with
// any static file server" is the deliverable's contract), screenshot after
// `budget_ms` of virtual time, and assert (a) the canvas2d glyph atlas baked
// (real TTF fetched + baked) and (b) the frame clears the >=8-quantized-color
// blank bar. Budget note: software-WebGL (swiftshader) boot can outlast a small
// budget (blank pre-first-frame capture), while a much larger one over-runs
// swiftshader's staging pool after the frame is already captured -- 10s works
// for match3/chess; mag's larger preload gets 30s.
fn run_chrome_screenshot(game: &str, budget_ms: u32, subject: &str) {
    let chrome = ["google-chrome", "google-chrome-stable", "chromium"]
        .iter()
        .find(|c| Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false));
    let Some(chrome) = chrome else {
        eprintln!("SKIP chrome screenshot ({game}): no chrome on PATH");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    build_web(game, dir.path());

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
        ])
        .arg(format!("--virtual-time-budget={budget_ms}"))
        .arg(format!("--screenshot={}", shot.display()))
        .arg(format!("http://127.0.0.1:{port}/index.html"))
        .output()
        .expect("spawn chrome");
    let _ = server.kill();
    let _ = server.wait();
    assert!(out.status.success(), "chrome exited non-zero ({game})");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("font atlas baked"),
        "canvas2d glyph atlas did not bake ({game}).\nchrome stderr tail:\n{}",
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
        "screenshot looks blank ({} distinct quantized colors); {subject} should produce many",
        colors.len()
    );
}

#[test]
fn web_chrome_screenshot_nonblank_with_text() {
    run_chrome_screenshot("games/match3", 10000, "match3 tiles");
}

// Chess in real headless Chrome: 14 MB of book/opening/sound assets + font are
// fetched before main; the rendered board (light/dark tiles + pieces + side
// panels) must clear the color bar.
#[test]
fn web_chrome_screenshot_nonblank_chess() {
    run_chrome_screenshot("games/chess", 10000, "the chess board");
}

// mag (v2 tier) in the node fake-GL env: boots through global init, the whole
// @init image-load phase (no fetch -> every stone_load_image takes the loud
// white-pixel fallback rung), and runs 10 full frames of the world render.
#[test]
fn web_fake_gl_smoke_mag() {
    run_fake_gl_smoke("mag");
}

// mag in real headless Chrome: ~250 MB of sprite sheets/audio/levels preload
// before main(), textures decode to ImageBitmaps and upload at @init, and the
// rendered scene (tile floor, walls, characters, glyph HUD) must clear the
// color bar. 30s virtual budget: the preload + first frame outlast the 10s the
// small games use.
#[test]
fn web_chrome_screenshot_nonblank_mag() {
    run_chrome_screenshot("mag", 30000, "the mag scene");
}
