//! Every `rock sim` worker runs in a scope under `rock-sim.slice`, so the
//! slice's MemoryMax bounds all concurrent sims together.
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

#[cfg(target_os = "linux")]
#[test]
fn a_sim_worker_runs_in_the_shared_sim_slice() {
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["sim", "crates/rock-cli/test-data/sim-scope", "sim_worker_runs_in_the_sim_slice"])
        .current_dir(workspace_root())
        .env_remove("ROCK_SIM_SCOPE")
        .output()
        .expect("run rock sim");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "the worker must run in rock-sim.slice:\n{combined}");
}
