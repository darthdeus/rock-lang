//! Phase 2 acceptance tests for MAG simulation core:
//! 1. Repeatability: two runs of the same seeded headless sim yield identical gameplay digests.
//! 2. Presentation Independence: running headless vs rendered yields identical gameplay digests,
//!    after proving the two arms really do differ in the presentation randomness they consume.
//!
//! Both compare `mag_world_digest()` at its full 1/64 quantization. Coarsening
//! that scale to hide a divergence is how this pair first read green; see
//! `docs/mag-simulation-slice-results.md` section 3.3.

use std::path::{Path, PathBuf};
use std::process::Command;
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};
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
        .output_with_deadline()
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
        .filter(|line| line.starts_with(&format!("[sim] mag::{name}:")))
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

/// One `MAG_SIM_<MARKER>=<int>` line the sim logs. Missing is a panic rather
/// than a default: a sim that stopped short of the marker must not read as a
/// value the comparison can pass on.
fn extract_marker(marker: &str, name: &str, output: &std::process::Output) -> i64 {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let needle = format!("{marker}=");
    for line in text.lines() {
        if let Some(pos) = line.find(&needle) {
            let rest = &line[pos + needle.len()..];
            let num_str = rest.split_whitespace().next().unwrap_or(rest);
            return num_str
                .parse::<i64>()
                .unwrap_or_else(|error| panic!("parse {marker} of {name}: {error}"));
        }
    }
    panic!("could not find {marker} in output of {name}:\n{text}");
}

fn extract_digest(name: &str, output: &std::process::Output) -> i64 {
    extract_marker("MAG_SIM_DIGEST", name, output)
}

/// Two runs only catch a coin-flip divergence half the time. The defect this
/// pins (wall-clock-dependent native flow publication steering authoritative
/// mana through the wisp motes, `mag/flow.rock` `flow_job_advance`) flipped
/// about one run in two, so two samples agreed -- and read green -- as often
/// as not. Four runs put that at roughly one in eight.
const REPEATABILITY_RUNS: usize = 4;

#[test]
fn mag_simulation_repeatability() {
    let _spawn_budget = SpawnBudget::for_test(120);
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

/// The arms must agree on the gameplay digest AND be shown to differ in the
/// presentation randomness they consume. Until the scripted sequence reached
/// the magic-trial card's mote emitter the two arms consumed identical draws
/// from both streams, so this test agreed for a reason that had nothing to do
/// with the Phase 3 split -- a differential that cannot fail. The
/// discrimination assertions are what stop that from coming back silently:
/// lose the discriminator and this reddens rather than passing vacuously.
#[test]
fn mag_simulation_presentation_independence() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let _guard = SIM_LOCK.lock().unwrap();
    let out_headless = run_sim("sim_repeatability_headless");
    assert_sim_pass("sim_repeatability_headless", &out_headless);
    let headless_digest = extract_digest("sim_repeatability_headless", &out_headless);
    let headless_motes = extract_marker(
        "MAG_SIM_TRIAL_MOTES",
        "sim_repeatability_headless",
        &out_headless,
    );
    let headless_presentation_rng = extract_marker(
        "MAG_SIM_PRESENTATION_RNG",
        "sim_repeatability_headless",
        &out_headless,
    );

    let out_rendered = run_sim("sim_presentation_rendered");
    assert_sim_pass("sim_presentation_rendered", &out_rendered);
    let rendered_digest = extract_digest("sim_presentation_rendered", &out_rendered);
    let rendered_motes = extract_marker(
        "MAG_SIM_TRIAL_MOTES",
        "sim_presentation_rendered",
        &out_rendered,
    );
    let rendered_presentation_rng = extract_marker(
        "MAG_SIM_PRESENTATION_RNG",
        "sim_presentation_rendered",
        &out_rendered,
    );

    eprintln!(
        "HEADLESS: digest {headless_digest}, trial motes {headless_motes}, \
         presentation rng {headless_presentation_rng}"
    );
    eprintln!(
        "RENDERED: digest {rendered_digest}, trial motes {rendered_motes}, \
         presentation rng {rendered_presentation_rng}"
    );

    assert_eq!(
        headless_digest, rendered_digest,
        "presentation independence diverged: headless ({headless_digest}) != rendered ({rendered_digest})"
    );

    assert_eq!(
        headless_motes, 0,
        "the headless arm runs no draw phase and must emit no trial motes"
    );
    assert!(
        rendered_motes > 0,
        "the rendered arm emitted no trial motes ({rendered_motes}), so the arms do not \
         differ in presentation randomness and this differential cannot fail"
    );
    assert_ne!(
        headless_presentation_rng, rendered_presentation_rng,
        "both arms ended at the same presentation RNG state ({headless_presentation_rng}), \
         so the rendered arm consumed no presentation randomness the headless arm did not"
    );
}

/// The presentation stream must not reach simulation state at all.
///
/// `mag_simulation_presentation_independence` above cannot prove this: its two
/// arms draw the same presentation numbers until the trial card at the end of
/// the sequence, so every wisp mote gets the same seed in both arms and a
/// dependency on that seed stays invisible.
///
/// That coupling (reseeding the presentation stream moved the digest, through
/// mote curl noise -> wisp position -> `wisp_gather` -> `Player.mana_per_sec`
/// -> `player.mana_cur`) is now pinned by the package test
/// `code_presentation_reseed_keeps_wisp_state_and_digest` in
/// `mag/tests/simulation_digest_rules.rock`, which spawns wisp motes under
/// two presentation seeds and requires identical curl seeds, trajectories
/// and digest. The reseeded sim arm that used to drive this differential is
/// deleted, so there is no third sim leg to compare here.
