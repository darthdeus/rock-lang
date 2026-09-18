//! A sim that outgrows `ROCK_SIM_MEMORY_MB` is killed and reported as such,
//! long before the wall-clock watchdog would have fired.
//!
//! Gated (compiles + runs a game): excluded from the default nextest profile.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

#[cfg(target_os = "linux")]
#[test]
fn a_sim_over_its_memory_cap_is_killed_and_named() {
    let root = workspace_root();
    let started = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["sim", "crates/rock-cli/test-data/sim-memory-cap", "memory_cap_stops_a_runaway"])
        .current_dir(&root)
        .env("ROCK_SIM_MEMORY_MB", "512")
        .env("ROCK_SIM_TIMEOUT_SECS", "120")
        .output()
        .expect("run rock sim");
    let elapsed = started.elapsed();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(!out.status.success(), "a runaway sim must fail:\n{combined}");
    assert!(
        combined.contains("exceeded its memory cap of 512 MB"),
        "the failure must name the cap:\n{combined}"
    );
    assert!(!combined.contains("timed out after"), "the cap, not the watchdog, must stop it:\n{combined}");
    assert!(elapsed < Duration::from_secs(90), "the cap must act before the watchdog: {elapsed:?}");
}
