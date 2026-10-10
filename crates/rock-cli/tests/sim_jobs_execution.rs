//! An explicit over-budget `--jobs` request runs when its entries narrow it
//! within budget: execution honors the same narrowing the plan does.
//!
//! Gated (compiles + runs a game): excluded from the default nextest profile.

use crate::stone_harness::{HeavyCommandExt, SpawnBudget};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FIXTURE: &str = "crates/rock-cli/test-data/sim-v1-names";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn named_sims_within_the_narrowed_budget_run_despite_an_over_budget_request() {
    // Two named entries narrow --jobs 1000 to two lanes, which fits this
    // host's RAM budget, so the run executes instead of refusing. The plan
    // half of the rule is pinned in `run_sim_jobs_budget`, and the
    // still-over-budget-after-narrowing refusal in the sibling 64-entry test
    // there: together the three pin narrow-then-check for plan and execution.
    let _spawn_budget = SpawnBudget::for_test(180);
    let out: Output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .arg("sim")
        .arg(FIXTURE)
        .args(["unique_short_name", "other_snapshot_baseline", "--jobs", "1000"])
        .current_dir(workspace_root())
        .env("ROCK_SIM_TIMEOUT_SECS", "120")
        .output_with_deadline()
        .expect("run narrowed over-budget sweep");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "a narrowed-within-budget request must run:\n{combined}");
    assert!(
        combined.contains("[sim] sim_v1_names::unique_short_name: PASS (1/1 expects passed)")
            && combined.contains(
                "[sim] sim_v1_names::other::other_snapshot_baseline: PASS (1/1 expects passed)"
            ),
        "both narrowed entries must run:\n{combined}"
    );
}
