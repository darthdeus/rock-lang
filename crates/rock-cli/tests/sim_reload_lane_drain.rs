//! Reload-lane sims run only after the worker pool has drained.
//!
//! A reload sim drives `SkullStone::apply_edit` against the one live session
//! and the one `incremental.lua`, so it must never overlap another child. The
//! runner puts those entries in an exclusive lane that runs after every
//! ordinary entry finishes.
//!
//! Records print in COMPLETION order, so "the reload entry's record is last"
//! is exactly the observable form of "it ran after the pool drained". If the
//! lane regressed to sharing the pool, the reload entry would routinely finish
//! before some ordinary sibling and its record would not be last.
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

/// `reload_probe` opts into the lane via `@sim_env("STONE_HOT_RELOAD", "1")`.
/// The others are ordinary and deliberately outnumber the workers, so the pool
/// is still busy when the lane would otherwise be scheduled.
const RELOAD_SIM: &str = "reload_probe";
const ORDINARY: [&str; 5] = [
    "co_join2_tuple",
    "co_join3_tuple",
    "co_join_all_list",
    "co_field_poller",
    "co_wait_for_result",
];

#[test]
fn reload_lane_records_land_after_the_pool_drains() {
    let root = workspace_root();

    // Reload entry FIRST on the command line: if the lane were not exclusive,
    // input order would tend to put its record early rather than last.
    let mut args = vec!["sim", "testbed", RELOAD_SIM];
    args.extend_from_slice(&ORDINARY);
    args.extend_from_slice(&["-j", "4"]);

    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(&args)
        .current_dir(&root)
        .env("ROCK_SIM_TIMEOUT_SECS", "120")
        .output()
        .expect("run rock sim");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Verdict lines in the order they were printed.
    let order: Vec<&str> = combined
        .lines()
        .filter_map(|l| l.strip_prefix("[sim] "))
        .filter_map(|l| l.split_once(':'))
        .map(|(name, _)| name)
        .filter(|n| *n == RELOAD_SIM || ORDINARY.contains(n))
        .collect();

    assert_eq!(
        order.len(),
        ORDINARY.len() + 1,
        "expected one record per sim, got {order:?}\n{combined}"
    );
    assert_eq!(
        order.last(),
        Some(&RELOAD_SIM),
        "the reload entry must be the last record -- it runs alone, after the \
         pool drains. Got {order:?}\n{combined}"
    );
    assert!(
        out.status.success(),
        "all sims should pass:\n{combined}"
    );
}
