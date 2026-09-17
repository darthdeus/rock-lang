//! Phase 2 acceptance tests for MAG simulation core:
//! 1. Repeatability: two runs of the same seeded headless sim yield identical gameplay digests.
//! 2. Presentation Independence: running headless vs rendered yields identical gameplay digests.
//!
//! Both compare `mag_world_digest()` at its full 1/64 quantization. Coarsening
//! that scale to hide a divergence is how this pair first read green; see
//! `docs/mag-simulation-slice-results.md` section 3.3.

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

/// Two runs only catch a coin-flip divergence half the time. The defect this
/// pins (wall-clock-dependent native flow publication steering authoritative
/// mana through the wisp motes, `mag/flow.rock` `flow_job_advance`) flipped
/// about one run in two, so two samples agreed -- and read green -- as often
/// as not. Four runs put that at roughly one in eight.
const REPEATABILITY_RUNS: usize = 4;

#[test]
fn mag_simulation_repeatability() {
    let _guard = SIM_LOCK.lock().unwrap();
    let digests: Vec<i64> = (0..REPEATABILITY_RUNS)
        .map(|_| {
            let out = run_sim("sim_repeatability_headless");
            assert_sim_pass("sim_repeatability_headless", &out);
            extract_digest("sim_repeatability_headless", &out)
        })
        .collect();

    assert_ne!(digests[0], 0, "digest must be non-zero");
    assert!(
        digests.iter().all(|d| *d == digests[0]),
        "repeatability diverged across {REPEATABILITY_RUNS} identical runs: {digests:?}"
    );
}

#[test]
fn mag_simulation_presentation_independence() {
    let _guard = SIM_LOCK.lock().unwrap();
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
