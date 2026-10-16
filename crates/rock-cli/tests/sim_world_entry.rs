//! SIM WORLD ENTRY: a `@sim` may take one entry argument, which the game's
//! single `@sim_entry` provider supplies when the runner starts it. No-argument
//! sims keep running unchanged, and a world-taking sim started with no active
//! world fails with the provider's named error instead of running its body.
//!
//! Gated (compiles + runs a game): excluded from the default nextest profile.

use crate::stone_harness::{HeavyCommandExt, SpawnBudget};
use std::path::{Path, PathBuf};
use std::process::{Output};

const FIXTURE: &str = "crates/rock-cli/test-data/sim-world-entry";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn rock_sim(names: &[&str]) -> (Output, String) {
    let out = crate::command::rock_bin()
        .arg("sim")
        .arg(FIXTURE)
        .args(names)
        .current_dir(workspace_root())
        .env("ROCK_SIM_TIMEOUT_SECS", "120")
        .output_with_deadline()
        .expect("run rock sim");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out, combined)
}

#[test]
fn a_world_taking_sim_receives_the_active_world() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (out, combined) = rock_sim(&["world_sim_receives_the_active_world"]);
    assert!(out.status.success(), "{combined}");
    assert!(
        combined.contains("world_sim_receives_the_active_world: PASS (2/2 expects passed)"),
        "the provider must pass the live world into the sim:\n{combined}"
    );
}

#[test]
fn a_no_argument_sim_still_runs() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (out, combined) = rock_sim(&["no_arg_sim_still_runs"]);
    assert!(out.status.success(), "{combined}");
    assert!(combined.contains("no_arg_sim_still_runs: PASS (1/1 expects passed)"), "{combined}");
}

#[test]
fn a_world_taking_sim_with_no_active_world_fails_with_the_named_error() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (out, combined) = rock_sim(&["world_sim_without_a_world"]);
    assert!(!out.status.success(), "a sim with no world must fail:\n{combined}");
    assert!(combined.contains("SimNoActiveWorld"), "the named error must surface:\n{combined}");
    assert!(
        !combined.contains("unreachable: the entry fails before the body"),
        "the sim body must not run without a world:\n{combined}"
    );
}
