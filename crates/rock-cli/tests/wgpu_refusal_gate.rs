//! A selected-wgpu refusal must fail BEFORE it has an effect.
//!
//! Both subjects used to refuse by CONTINUING. `set_canvas` logged and
//! returned, so `with_canvas` ran its body with the PREVIOUS target still
//! bound and the draws landed on the wrong surface. `acquire_mesh` now admits
//! the engine World+Scene pool, while a target-space acquire remains a
//! permanent rejection at the public boundary. Neither is observable from a
//! sim: both now raise, and Rock has no catch, so the sim would die with them.
//! A bounded run is the oracle instead -- `stone_run_game` exits non-zero when
//! a frame raised.
//!
//! The probe lives in `games/wgpu_renderer/refusal_probe.rock` behind two env
//! vars. The control run (same command, no probe) is what keeps this from
//! passing for the wrong reason.
//!
//! `assert_booted` is load-bearing. Without the Vulkan loader on this box the
//! game never initializes and no draw system runs. Bounded `rock frame` now
//! exits non-zero and does not print `3 frames executed successfully`, but a
//! gate must retain the boot marker as an independent non-vacuity assertion:
//! exit status alone does not prove that the measured process initialized.

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// `rock frame games/wgpu_renderer` for 3 frames, optionally with one probe on.
/// Returns (combined output, exit-status-success).
fn run_frame(probe: Option<&str>) -> (String, bool) {
    run_frame_with_driver(probe, None)
}

fn run_frame_with_driver(probe: Option<&str>, video_driver: Option<&str>) -> (String, bool) {
    let out_dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new(rock_bin());
    cmd.current_dir(repo_root())
        .args(["frame", "games/wgpu_renderer", "-n", "3"])
        .arg("--output")
        .arg(out_dir.path().join("main.lua"))
        .env("ROCK_FRAME_TIMEOUT_SECS", "120");
    if let Some(driver) = video_driver {
        cmd.env("SDL_VIDEODRIVER", driver);
    }
    if let Some(p) = probe {
        cmd.env(p, "1");
    }
    let out = cmd.output().expect("spawn rock frame");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

#[test]
fn failed_stone_initialization_fails_rock_frame() {
    let (text, ok) = run_frame_with_driver(None, Some("dummy"));
    assert!(
        text.contains("selected wgpu renderer requires a real SDL window driver")
            || text.contains("Failed to initialize"),
        "the fixture did not force a Stone initialization failure:\n{text}"
    );
    assert!(!ok, "rock frame must fail when Stone never initializes:\n{text}");
    assert!(
        !text.contains("frames executed successfully"),
        "rock frame claimed success after failed Stone initialization:\n{text}"
    );
}

/// The run reached a real frame. Stone initializing is the precondition for the
/// probe existing at all, and it is not implied by the exit status.
fn assert_booted(text: &str, what: &str) {
    assert!(
        !text.contains("Failed to initialize"),
        "{what}: Stone did not initialize, so no draw system ran and this gate \
         would measure nothing. On NixOS put the vulkan-loader lib dir on \
         LD_LIBRARY_PATH.\n{text}"
    );
    // The boot marker, not "frames executed successfully": a run whose frames
    // raised exits 3 and never prints the latter, so it cannot witness boot for
    // the two arms that are SUPPOSED to fail.
    assert!(
        text.contains("Initialized successfully"),
        "{what}: the run did not report a successful Stone init:\n{text}"
    );
}

#[test]
fn the_control_frame_run_is_clean() {
    let (text, ok) = run_frame(None);
    assert_booted(&text, "control");
    assert!(ok, "the unprobed selected-wgpu run must exit 0:\n{text}");
}

#[test]
fn set_canvas_refuses_before_running_the_with_canvas_body() {
    let (text, ok) = run_frame(Some("WGPU_PROBE_SET_CANVAS"));
    assert_booted(&text, "set_canvas probe");
    assert!(
        text.contains("cannot enter the GL target binder"),
        "the refusal did not reach set_canvas:\n{text}"
    );
    assert!(
        !text.contains("with_canvas body ran"),
        "set_canvas refused but with_canvas still ran its body against the \
         previously bound target:\n{text}"
    );
    assert!(!ok, "a raised frame must fail a bounded run:\n{text}");
}

#[test]
fn acquire_mesh_refuses_a_non_world_destination_at_the_boundary() {
    let (text, ok) = run_frame(Some("WGPU_PROBE_ACQUIRE_MESH"));
    assert_booted(&text, "acquire_mesh probe");
    assert!(
        text.contains("acquired meshes require a World+Scene DrawList"),
        "the permanent destination refusal did not reach acquire_mesh:\n{text}"
    );
    assert!(
        !text.contains("pixel_screen acquire_mesh returned a mesh"),
        "acquire_mesh refused but still handed back a mesh to fill:\n{text}"
    );
    assert!(!ok, "a raised frame must fail a bounded run:\n{text}");
}
