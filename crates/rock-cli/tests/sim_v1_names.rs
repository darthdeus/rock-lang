//! A v1 package registers each `@sim` under its canonical DefKey
//! (`sim_v1_names::other::shared_short_name`). The runner still accepts the
//! declared short name when exactly one sim has it (spec sections 15 and 16),
//! and names the candidates when several do.
//!
//! Gated (compiles + runs a game): excluded from the default nextest profile.

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

fn rock_sim(names: &[&str]) -> (Output, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .arg("sim")
        .arg(FIXTURE)
        .args(names)
        .current_dir(workspace_root())
        .env("ROCK_SIM_TIMEOUT_SECS", "120")
        .output()
        .expect("run rock sim");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out, combined)
}

#[test]
fn a_v1_sim_runs_by_its_unambiguous_short_name() {
    let (out, combined) = rock_sim(&["unique_short_name"]);
    assert!(out.status.success(), "the short name must select the sim:\n{combined}");
    assert!(
        combined.contains("[sim] sim_v1_names::unique_short_name: PASS (1/1 expects passed)"),
        "the verdict must name the canonical sim:\n{combined}"
    );
    assert!(!combined.contains("unknown sim"), "{combined}");
}

#[test]
fn an_ambiguous_v1_sim_short_name_names_its_candidates() {
    let (out, combined) = rock_sim(&["shared_short_name"]);
    assert!(!out.status.success(), "an ambiguous short name must fail:\n{combined}");
    assert!(
        combined.contains("sim name `shared_short_name` is ambiguous")
            && combined.contains("sim_v1_names::shared_short_name")
            && combined.contains("sim_v1_names::other::shared_short_name"),
        "the error must list both candidates:\n{combined}"
    );
    assert!(!combined.contains("[sim] "), "no sim may start on an ambiguous name:\n{combined}");
}

#[test]
fn a_qualified_v1_sim_name_selects_one_of_two_same_short_names() {
    let (out, combined) = rock_sim(&["sim_v1_names::other::shared_short_name", "unique_short_name"]);
    assert!(out.status.success(), "{combined}");
    assert!(
        combined.contains("[sim] sim_v1_names::other::shared_short_name: PASS (1/1 expects passed)")
            && combined.contains("[sim] sim_v1_names::unique_short_name: PASS (1/1 expects passed)"),
        "{combined}"
    );
}
