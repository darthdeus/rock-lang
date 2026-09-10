//! Reload-lane sims serialize on the compile session, not on the pool.
//!
//! A reload sim drives `SkullStone::apply_edit` against the one live session,
//! so two reload children must never be live at once. They used to run only
//! after every ordinary child had exited, because the overlay and a full
//! `main.lua` rewrite landed in the directory every child boots from. The
//! runner now hands the lane a private copy of that directory and runs it as
//! one serial lane inside the pool, so a reload child MAY overlap ordinary
//! children -- and with enough ordinary work queued, it does.
//!
//! `ROCK_SIM_LIFECYCLE_LOG` records child start/end events in the parent. The
//! test asserts both halves from that trace: no reload child starts while
//! another reload child is live, and at least one reload child is live while
//! an ordinary child is -- the overlap this lane exists to allow.
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

/// Both opt into the lane via `@sim_env("STONE_HOT_RELOAD", "1")`. The
/// ordinary entries deliberately outnumber the workers, so the pool is still
/// busy for the whole serial reload run.
const RELOAD_SIMS: [&str; 2] = ["reload_probe", "sys_reload_update_body"];
const ORDINARY: [&str; 5] = [
    "co_join2_tuple",
    "co_join3_tuple",
    "co_join_all_list",
    "co_field_poller",
    "co_wait_for_result",
];

#[test]
fn reload_lane_overlaps_the_pool_but_never_itself() {
    let root = workspace_root();
    let trace_dir = tempfile::tempdir().expect("trace tempdir");
    let trace_path = trace_dir.path().join("lifecycle.log");

    let mut args = vec!["sim", "testbed"];
    args.extend_from_slice(&RELOAD_SIMS);
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

    // Verdict lines only: the runner also prints why an entry is isolated
    // under the same `[sim] <name>: ` prefix.
    let records: Vec<&str> = combined
        .lines()
        .filter_map(|l| l.strip_prefix("[sim] "))
        .filter_map(|l| l.split_once(": "))
        .filter(|(_, rest)| rest.starts_with("PASS") || rest.starts_with("FAIL"))
        .map(|(name, _)| name)
        .filter(|n| RELOAD_SIMS.contains(n) || ORDINARY.contains(n))
        .collect();
    assert_eq!(
        records.len(),
        RELOAD_SIMS.len() + ORDINARY.len(),
        "expected one record per sim, got {records:?}\n{combined}"
    );

    let trace = std::fs::read_to_string(&trace_path)
        .unwrap_or_else(|e| panic!("read lifecycle trace {}: {e}", trace_path.display()));
    let mut ordinary_live: HashSet<String> = HashSet::new();
    let mut reload_live: Option<String> = None;
    let mut reload_starts = 0;
    let mut overlapped = false;
    for line in trace.lines() {
        let (event, name) = line
            .split_once('\t')
            .unwrap_or_else(|| panic!("malformed lifecycle event `{line}`"));
        let is_reload = RELOAD_SIMS.contains(&name);
        match event {
            // Process granularity; this property is about entries, and every
            // entry here is its own process anyway.
            "proc_start" | "proc_end" => {}
            "start" if is_reload => {
                assert!(
                    reload_live.is_none(),
                    "reload child `{name}` started while `{}` was live\n{trace}",
                    reload_live.as_deref().unwrap_or("")
                );
                reload_live = Some(name.to_string());
                reload_starts += 1;
                overlapped |= !ordinary_live.is_empty();
            }
            "start" => {
                assert!(
                    ordinary_live.insert(name.to_string()),
                    "duplicate start for `{name}`\n{trace}"
                );
                overlapped |= reload_live.is_some();
            }
            "end" if is_reload => {
                assert_eq!(
                    reload_live.as_deref(),
                    Some(name),
                    "reload child `{name}` ended out of order\n{trace}"
                );
                reload_live = None;
            }
            "end" => {
                assert!(
                    ordinary_live.remove(name),
                    "end without start for `{name}`\n{trace}"
                );
            }
            _ => panic!("unknown lifecycle event `{line}`"),
        }
    }
    assert_eq!(reload_starts, RELOAD_SIMS.len(), "every reload child must start\n{trace}");
    assert!(reload_live.is_none(), "reload child left active\n{trace}");
    assert!(
        ordinary_live.is_empty(),
        "ordinary children left active: {ordinary_live:?}\n{trace}"
    );
    assert!(
        overlapped,
        "the reload lane never overlapped the pool; it is serializing on more than the session\n{trace}"
    );
    assert!(out.status.success(), "all sims should pass:\n{combined}");
}

#[test]
fn rejected_reload_ends_the_child_without_waiting_for_the_watchdog() {
    let root = workspace_root();
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "sim",
            "crates/rock-cli/test-data/reload-rejection",
            "reload_rejection_exits",
        ])
        .current_dir(&root)
        .env("ROCK_SIM_TIMEOUT_SECS", "10")
        .output()
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
