//! Phase 2 acceptance tests for MAG simulation core:
//! 1. Repeatability: two runs of the same seeded headless sim yield identical gameplay digests.
//! 2. Presentation Independence: running headless vs rendered yields identical gameplay digests.
//!    (In Phase 2, this fails because presentation VFX/lighting consume from the shared RNG stream).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

static SIM_LOCK: Mutex<()> = Mutex::new(());

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn run_sim(name: &str) -> std::process::Output {
    let _guard = SIM_LOCK.lock().unwrap();
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["sim", "mag", name])
        .current_dir(workspace_root())
        .env("ROCK_SIM_TIMEOUT_SECS", "120")
        .output()
        .unwrap_or_else(|error| panic!("spawn {name}: {error}"))
}

fn assert_sim_pass(name: &str, output: &std::process::Output) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let verdicts = text
        .lines()
        .filter(|line| line.starts_with(&format!("[sim] {name}:")))
        .filter(|line| line.contains("PASS"))
        .collect::<Vec<_>>();
    assert_eq!(
        verdicts.len(),
        1,
        "expected one PASS verdict for {name}:\n{text}"
    );
    assert!(
        output.status.success(),
        "{name} exited unsuccessfully:\n{text}"
    );
}

fn extract_digest(name: &str, output: &std::process::Output) -> i64 {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for line in text.lines() {
        if let Some(pos) = line.find("MAG_SIM_DIGEST=") {
            let rest = &line[pos + "MAG_SIM_DIGEST=".len()..];
            let num_str = rest.split_whitespace().next().unwrap_or(rest);
            return num_str.parse::<i64>().expect("parse digest int");
        }
    }
    panic!("could not find MAG_SIM_DIGEST in output of {name}:\n{text}");
}

#[test]
fn mag_simulation_repeatability() {
    let out1 = run_sim("sim_repeatability_headless");
    assert_sim_pass("sim_repeatability_headless", &out1);
    let digest1 = extract_digest("sim_repeatability_headless", &out1);

    let out2 = run_sim("sim_repeatability_headless");
    assert_sim_pass("sim_repeatability_headless", &out2);
    let digest2 = extract_digest("sim_repeatability_headless", &out2);

    assert_ne!(digest1, 0, "digest must be non-zero");
    assert_eq!(digest1, digest2, "repeatability diverged across identical runs");
}

#[test]
fn mag_simulation_presentation_independence() {
    let out_headless = run_sim("sim_repeatability_headless");
    assert_sim_pass("sim_repeatability_headless", &out_headless);
    let headless_digest = extract_digest("sim_repeatability_headless", &out_headless);

    let out_rendered = run_sim("sim_presentation_rendered");
    assert_sim_pass("sim_presentation_rendered", &out_rendered);
    let rendered_digest = extract_digest("sim_presentation_rendered", &out_rendered);

    eprintln!("HEADLESS DIGEST: {headless_digest}, RENDERED DIGEST: {rendered_digest}");
    assert_eq!(
        headless_digest, rendered_digest,
        "presentation independence diverged: headless ({headless_digest}) != rendered ({rendered_digest})"
    );
}
