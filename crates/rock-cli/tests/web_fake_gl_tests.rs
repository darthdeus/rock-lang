//! The fast `rock web` gates: build a game's web dir and drive 10 frames of the
//! emitted build under node against a recorded-call GL stub
//! (tools/web/fake_gl_smoke.js), asserting per frame >=1 clear, >=2 TRIANGLES
//! draws at the 40-byte (10-float) vertex stride, and `running` stays true.
//!
//! These run in the DEFAULT nextest profile, unlike their headless-Chrome
//! siblings in `web_smoke_tests` (~50s, still `-P full`). The whole web surface
//! used to sit behind `-P full`, so `rock web` could be completely dead while
//! `just t` and `just test-games` stayed green -- which is exactly how it stayed
//! dead across two merges (`9d3cac70c`). The four tests here cost ~1.2s total.
//! `.config/nextest.toml` filters by BINARY, so the split is what buys the
//! coverage; keep the slow Chrome tier out of this file.

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

// mag (v2 tier) in the node fake-GL env: boots through global init, the whole
// @init image-load phase (no fetch, so the harness stubs a 1x1 bitmap for each
// path in the preload manifest), and runs 10 full frames of the world render.
#[test]
fn web_fake_gl_smoke_mag() {
    run_fake_gl_smoke("mag");
}
