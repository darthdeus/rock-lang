//! A rejected reload edit is terminal for its child.
//!
//! Where the reload lane runs relative to the pool is pinned in
//! `sim_worker_pool::the_reload_lane_runs_beside_the_worker_pool_in_its_own_chunk`.
//!
//! Gated (compiles + runs a game): excluded from the default nextest profile.

use std::path::{Path, PathBuf};
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn rejected_reload_ends_the_child_without_waiting_for_the_watchdog() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let root = workspace_root();
    let out = crate::command::rock_bin()
        .args([
            "sim",
            "crates/rock-cli/test-data/reload-rejection",
            "reload_rejection_exits",
        ])
        .current_dir(&root)
        .env("ROCK_SIM_TIMEOUT_SECS", "120")
        .output_with_deadline()
        .expect("run rock sim");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(!out.status.success(), "a rejected edit must fail the sim:\n{combined}");
    assert!(
        combined.contains("`missing_reload_source.rock` is not a source"),
        "the rejection reason must survive:\n{combined}"
    );
    assert!(
        combined.contains("requests=1 applied=0 pending=0 rejected=1 failed=0"),
        "the request must be terminal in the ledger:\n{combined}"
    );
    assert!(
        !combined.contains("timed out after"),
        "a parent-side rejection is terminal, not a watchdog timeout:\n{combined}"
    );
}
