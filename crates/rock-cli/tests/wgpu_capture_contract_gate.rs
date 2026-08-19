//! Phase-A capture ownership and failure contracts.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn rock_command() -> Command {
    if cfg!(target_os = "macos") {
        let mut command = Command::new("caffeinate");
        command.args(["-du"]);
        command.arg(rock_bin());
        command
    } else {
        Command::new(rock_bin())
    }
}

fn run_sim(project: &str, sim: &str, renderer: &str, dir: &Path) -> (String, bool) {
    let mut cmd = rock_command();
    cmd.current_dir(repo_root())
        .args(["sim", project, sim, "--renderer", renderer])
        .env("ROCK_FRAME_TIMEOUT_SECS", "120")
        .env("WGPU_CAPTURE_CONTRACT_DIR", dir)
        .env("STONE_WGPU_READBACK", "1");
    let out = cmd.output().expect("spawn capture-contract sim");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

fn run_frame(renderer: &str, dir: &Path) -> (String, bool) {
    let mut cmd = rock_command();
    cmd.current_dir(repo_root())
        .args(["frame", "games/wgpu_renderer", "-n", "4", "--renderer", renderer])
        .env("ROCK_FRAME_TIMEOUT_SECS", "120")
        .env("WGPU_CAPTURE_CONTRACT", "1")
        .env("WGPU_CAPTURE_CONTRACT_AT", "3")
        .env("WGPU_CAPTURE_CONTRACT_DIR", dir)
        .env("STONE_WGPU_READBACK", "1");
    let out = cmd.output().expect("spawn capture-contract frame");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

fn decoded(path: &Path) -> image::RgbaImage {
    image::open(path)
        .unwrap_or_else(|e| panic!("{} did not decode: {e}", path.display()))
        .to_rgba8()
}

#[test]
fn animated_take_screenshot_is_the_same_presented_frame_on_both_renderers() {
    let gl_dir = tempfile::tempdir().expect("GL capture directory");
    let wgpu_dir = tempfile::tempdir().expect("WGPU capture directory");
    let (gl_text, gl_ok) = run_frame("gl", gl_dir.path());
    assert!(gl_ok, "GL capture-contract frame failed:\n{gl_text}");
    let (wgpu_text, wgpu_ok) = run_frame("wgpu", wgpu_dir.path());
    assert!(wgpu_ok, "WGPU capture-contract frame failed:\n{wgpu_text}");
    assert!(
        gl_text.contains(&format!("Screenshot saved to {}/first.png", gl_dir.path().display()))
            && gl_text.contains(&format!("Screenshot saved to {}/second.png", gl_dir.path().display())),
        "GL resolves both requests:\n{gl_text}"
    );
    assert!(
        wgpu_text.contains(&format!("Screenshot saved to {}/first.png", wgpu_dir.path().display()))
            && wgpu_text.contains(&format!("Screenshot saved to {}/second.png", wgpu_dir.path().display())),
        "WGPU resolves both requests:\n{wgpu_text}"
    );

    let gl_first = decoded(&gl_dir.path().join("first.png"));
    let gl_second = decoded(&gl_dir.path().join("second.png"));
    let wgpu_first = decoded(&wgpu_dir.path().join("first.png"));
    let wgpu_second = decoded(&wgpu_dir.path().join("second.png"));
    assert_eq!(gl_first.dimensions(), (64, 64));
    assert_eq!(wgpu_first.dimensions(), (64, 64));
    assert_eq!(gl_first, gl_second, "two GL requests must read one presented frame");
    assert_eq!(wgpu_first, wgpu_second, "two WGPU requests must read one presented frame");
    for (name, image) in [("GL", gl_first), ("WGPU", wgpu_first)] {
        assert_eq!(image.get_pixel(32, 16).0, [255, 0, 0, 255], "{name} captures red at the expected frame");
        assert_eq!(image.get_pixel(32, 48).0, [0, 255, 0, 255], "{name} captures green at the expected frame");
        assert_eq!(image.get_pixel(56, 56).0, [0, 0, 255, 255], "{name} captures the odd-frame marker");
    }
}

#[test]
fn a_capture_without_a_present_fails_loudly_on_both_renderers() {
    for (project, sim, renderer) in [
        ("testbed", "capture_without_present", "gl"),
        ("wgpu_renderer", "wgpu_capture_without_present", "wgpu"),
    ] {
        let dir = tempfile::tempdir().expect("capture directory");
        let (text, ok) = run_sim(project, sim, renderer, dir.path());
        assert!(!ok, "{renderer} accepted a request that never reached a present:\n{text}");
        assert!(
            text.contains("no presented frame before shutdown"),
            "{renderer} did not report the terminal capture failure:\n{text}"
        );
        assert!(
            !text.contains("Screenshot saved to"),
            "{renderer} reported a successful terminal capture:\n{text}"
        );
    }
}

#[test]
fn jit_blacklist_valid_name_keeps_the_real_macos_wgpu_run_jitted() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let dir = tempfile::tempdir().expect("JIT run directory");
    let (text, ok) = run_sim("wgpu_renderer", "wgpu_jit_blacklist_valid_name", "wgpu", dir.path());
    assert!(ok, "valid JIT blacklist target failed:\n{text}");
    assert!(
        text.contains("jit=LuaJIT") && text.contains("status=on"),
        "the real macOS WGPU run did not report global JIT enabled:\n{text}"
    );
}

#[test]
fn jit_blacklist_missing_name_is_a_bounded_run_failure() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let dir = tempfile::tempdir().expect("JIT run directory");
    let (text, ok) = run_sim("wgpu_renderer", "wgpu_jit_blacklist_missing_name", "wgpu", dir.path());
    assert!(!ok, "an unresolved JIT blacklist target was accepted:\n{text}");
    assert!(
        text.contains("stone_wgpu_missing_blacklist_target"),
        "the missing name did not cross the failure boundary:\n{text}"
    );
}
