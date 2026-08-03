//! Reload-lane sims run only after the worker pool has drained.
//!
//! A reload sim drives `SkullStone::apply_edit` against the one live session
//! and the one `incremental.lua`, so it must never overlap another child. The
//! runner puts those entries in an exclusive lane that runs after every
//! ordinary entry finishes.
//!
//! `ROCK_SIM_LIFECYCLE_LOG` records child start/end events in the parent. The
//! test asserts directly that no ordinary child is live when reload starts;
//! completion-order output is a secondary user-visible check.
//!
//! Gated (compiles + runs a game): excluded from the default nextest profile.

use std::collections::HashSet;
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
    let trace_dir = tempfile::tempdir().expect("trace tempdir");
    let trace_path = trace_dir.path().join("lifecycle.log");

    // Reload entry FIRST on the command line: if the lane were not exclusive,
    // input order would tend to put its record early rather than last.
    let mut args = vec!["sim", "testbed", RELOAD_SIM];
    args.extend_from_slice(&ORDINARY);
    args.extend_from_slice(&["-j", "4"]);

    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(&args)
        .current_dir(&root)
        .env("ROCK_SIM_TIMEOUT_SECS", "120")
        .env("ROCK_SIM_LIFECYCLE_LOG", &trace_path)
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

    let trace = std::fs::read_to_string(&trace_path)
        .unwrap_or_else(|e| panic!("read lifecycle trace {}: {e}", trace_path.display()));
    let mut active = HashSet::new();
    let mut saw_reload = false;
    let mut reload_active = false;
    for line in trace.lines() {
        let (event, name) = line
            .split_once('\t')
            .unwrap_or_else(|| panic!("malformed lifecycle event `{line}`"));
        match event {
            "start" if name == RELOAD_SIM => {
                assert!(
                    active.is_empty(),
                    "reload child started while ordinary children were live: {active:?}\n{trace}"
                );
                saw_reload = true;
                reload_active = true;
            }
            "start" => {
                assert!(
                    !reload_active,
                    "ordinary child `{name}` started while reload was live\n{trace}"
                );
                assert!(
                    active.insert(name.to_string()),
                    "duplicate start for `{name}`\n{trace}"
                );
            }
            "end" if name == RELOAD_SIM => {
                assert!(reload_active, "reload ended without a start\n{trace}");
                reload_active = false;
            }
            "end" => {
                assert!(
                    active.remove(name),
                    "end without start for `{name}`\n{trace}"
                );
            }
            _ => panic!("unknown lifecycle event `{line}`"),
        }
    }
    assert!(saw_reload, "reload child never started\n{trace}");
    assert!(!reload_active, "reload child left active\n{trace}");
    assert!(
        active.is_empty(),
        "ordinary children left active: {active:?}\n{trace}"
    );
    assert!(out.status.success(), "all sims should pass:\n{combined}");
}
