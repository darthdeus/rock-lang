//! Two `rock sim` invocations must not share compiled output.
//!
//! Each invocation compiles its own project's `@sim` registry and then spawns
//! luajit children against it. While that output lives at a fixed workspace
//! path, a second invocation's compile replaces the chunk the first is about to
//! run, and the first reports `unknown sim: <name>` for names its own registry
//! definitely contains.
//!
//! `ROCK_SIM_SPAWN_BARRIER` makes the interleaving deterministic instead of
//! relying on losing a race: the victim parks after compiling, the other
//! invocation compiles a DIFFERENT project over the shared path, then the
//! victim is released and spawns.

use std::path::{Path, PathBuf};
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

fn workspace_root() -> PathBuf {
    // `crates/rock-cli/` -> workspace root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// A sim that exists in exactly one of the two projects, so a registry mixup
/// is reported as `unknown sim` rather than silently running the wrong body.
const VICTIM_PROJECT: &str = "testbed";
const VICTIM_SIM: &str = "testbed::co_join2_tuple";
const OTHER_PROJECT: &str = "match3";
const OTHER_SIM: &str = "auto_move";

#[test]
fn concurrent_sim_invocations_do_not_share_compiled_output() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let root = workspace_root();
    let rock = PathBuf::from(env!("CARGO_BIN_EXE_rock"));
    assert!(rock.exists(), "rock binary not built at {}", rock.display());

    let gate_dir = tempfile::tempdir().expect("tempdir");
    let gate = gate_dir.path().join("release.gate");

    // Victim: compiles testbed, parks at the barrier, then spawns.
    let victim = crate::command::rock(&rock)
        .current_dir(&root)
        .args(["sim", VICTIM_PROJECT, VICTIM_SIM])
        .env("ROCK_SIM_SPAWN_BARRIER", &gate)
        .env("ROCK_SIM_TIMEOUT_SECS", "60")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn_with_deadline()
        .expect("spawn victim");

    // While it parks, run the other project to completion. Its compile writes a
    // registry that does not contain VICTIM_SIM.
    let other = crate::command::rock(&rock)
        .current_dir(&root)
        .args(["sim", OTHER_PROJECT, OTHER_SIM])
        .env("ROCK_SIM_TIMEOUT_SECS", "60")
        .output_with_deadline()
        .expect("run other invocation");
    assert!(
        other.status.success(),
        "control invocation `{OTHER_PROJECT} {OTHER_SIM}` failed:\n{}",
        String::from_utf8_lossy(&other.stderr)
    );

    // Release the victim: it now spawns against whatever the shared path holds.
    std::fs::write(&gate, b"go").expect("write gate");

    let out = victim.wait_with_output().expect("victim output");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !stderr.contains("unknown sim"),
        "victim ran against another invocation's compiled output \
         (shared sim output path):\n{stderr}"
    );
    assert!(
        out.status.success(),
        "victim invocation failed:\n{stderr}"
    );
}
