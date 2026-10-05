//! Every frame of a sim entry is counted in its frame timing, including a frame
//! that raised, so one entry cannot abort the batch it shares a worker with.
//!
//! The runner reports `frame_count` from the frame counter and `frame_timing`
//! from per-frame samples; the parent rejects an entry whose two disagree.
//! A caught frame error used to be counted by the first and skipped by the
//! second, which aborted the whole worker lane. `raises_a_frame` reproduces
//! that regression; the other entries control early completion and continuation.
//! Caught errors remain logged for the sim gate to reject the run.
//!
//! Gated (compiles + runs a game): excluded from the default nextest profile.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn a_raised_frame_in_one_entry_does_not_abort_its_worker_batch() {
    let fixture = tempfile::tempdir().expect("timing fixture");
    let project = fixture.path().join("timing_fixture");
    std::fs::create_dir_all(project.join("tests/sims")).unwrap();
    std::fs::write(project.join("stick.toml"), format!(
        "[package]\nname = \"timing_fixture\"\nversion = \"0.1.0\"\ntype = \"bin\"\n[dependencies]\nstone = {{ path = {:?} }}\n[build.vars]\nGIT_SHA = \"fixture\"\n",
        workspace_root().join("stone")
    )).unwrap();
    std::fs::write(project.join("main.rock"), r#"
global FIXTURE_RAISE: bool = false

@update(stage = 200)
fn fixture_raise_system() {
    if FIXTURE_RAISE { error("fixture raised frame") }
}

fn main() { stone_run_game(GameConfig { title: "timing_fixture", width: 64, height: 64 }) }
"#).unwrap();
    std::fs::write(project.join("tests/sims/timing.rock"), r#"
@sim
co fn raises_a_frame() -> Coroutine<Wait> {
    start_game "timing_fixture"
    FIXTURE_RAISE = true
    await frame(3)
    FIXTURE_RAISE = false
    sim_expect(true, "ran past the raised frames")
    stop_sim()
}
@sim
co fn stops_on_first_resume() -> Coroutine<Wait> {
    start_game "timing_fixture"
    stop_sim()
}
@sim
co fn passes_after() -> Coroutine<Wait> {
    start_game "timing_fixture"
    await frame(2)
    sim_expect(true, "later entry ran")
    stop_sim()
}
"#).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_rock")).arg("sim").arg(&project)
        .args(["raises_a_frame", "stops_on_first_resume", "passes_after"])
        .args(["--workers", "-j", "1"])
        .current_dir(workspace_root())
        .env("ROCK_SIM_TIMEOUT_SECS", "60")
        .env("ROCK_SIM_LOG_DIR", fixture.path().join("history"))
        .output().expect("run timing fixture");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    eprintln!("{text}");

    assert!(text.contains("Error in frame"), "{text}");
    assert!(text.contains("fixture raised frame"), "{text}");
    assert!(!text.contains("entry frame timing"), "{text}");
    assert!(out.status.success(), "{text}");
    assert_eq!(text.matches("[STONE] Initialized successfully").count(), 1, "{text}");
    for name in ["raises_a_frame", "stops_on_first_resume", "passes_after"] {
        let prefix = format!("[sim] timing_fixture::{name}: ");
        let alt = format!("[sim] {name}: ");
        let verdicts: Vec<_> = text.lines()
            .filter(|l| l.starts_with(&prefix) || l.starts_with(&alt)).collect();
        assert_eq!(verdicts.len(), 1, "{name} needs exactly one verdict:\n{text}");
        assert!(verdicts[0].contains("PASS"), "{name}: {}\n{text}", verdicts[0]);
    }
}
