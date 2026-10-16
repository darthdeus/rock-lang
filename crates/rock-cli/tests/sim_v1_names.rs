//! A v1 package registers each `@sim` under its canonical DefKey
//! (`sim_v1_names::other::shared_short_name`). The runner still accepts the
//! declared short name when exactly one sim has it (spec sections 15 and 16),
//! and names the candidates when several do.
//!
//! Gated (compiles + runs a game): excluded from the default nextest profile.

use crate::stone_harness::{HeavyCommandExt, SpawnBudget};
use std::path::{Path, PathBuf};
use std::process::{Output};

const FIXTURE: &str = "crates/rock-cli/test-data/sim-v1-names";

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
fn a_v1_sim_runs_by_its_unambiguous_short_name() {
    let _spawn_budget = SpawnBudget::for_test(120);
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
    let _spawn_budget = SpawnBudget::for_test(120);
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

/// A batch naming helpers that are not sims, plus one argv element holding
/// newline-joined names (`"$(cat names.txt)"`), used to reach the worker's
/// entry plan and die as `malformed entry plan line: <a valid sim>`.
#[test]
fn names_that_are_not_sims_are_all_refused_before_any_sim_boots() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (out, combined) = rock_sim(&[
        "unique_short_name",
        "helper_probe",
        "other_snapshot_baseline\nsim_v1_names::other::shared_short_name",
        "other_helper",
    ]);
    assert!(!out.status.success(), "a batch with non-sim names must fail:\n{combined}");
    for expected in [
        "unknown sim: helper_probe",
        "unknown sim: other_helper",
        "\"other_snapshot_baseline\\nsim_v1_names::other::shared_short_name\" contains whitespace",
    ] {
        assert!(combined.contains(expected), "missing `{expected}`:\n{combined}");
    }
    assert!(!combined.contains("malformed entry plan"), "{combined}");
    assert!(!combined.contains("[sim] "), "no sim may start on a refused batch:\n{combined}");
}

#[test]
fn a_qualified_v1_sim_name_selects_one_of_two_same_short_names() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (out, combined) = rock_sim(&["sim_v1_names::other::shared_short_name", "unique_short_name"]);
    assert!(out.status.success(), "{combined}");
    assert!(
        combined.contains("[sim] sim_v1_names::other::shared_short_name: PASS (1/1 expects passed)")
            && combined.contains("[sim] sim_v1_names::unique_short_name: PASS (1/1 expects passed)"),
        "{combined}"
    );
}

#[test]
fn a_v1_sim_snapshot_finds_its_baseline_beside_the_declaring_file() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (out, combined) = rock_sim(&["other_snapshot_baseline"]);
    assert!(out.status.success(), "the committed baseline must be found and match:\n{combined}");
    // The snapshot comparison is the sim's only expect.
    assert!(
        combined.contains("[sim] sim_v1_names::other::other_snapshot_baseline: PASS (1/1 expects passed)"),
        "the snapshot must be compared against tests/sims/other.snap:\n{combined}"
    );
}

/// The fixture files whose text declares `co fn <short>(`, sorted. An
/// anti-vacuity guard, not the oracle: it proves the fixture keeps the shape
/// a source-text scan cannot resolve (no match, or a match in two files), so
/// a passing run below must have used the compiled wire -> source mapping.
fn files_declaring(short: &str) -> Vec<String> {
    let sims = workspace_root().join("crates/rock-cli/test-data/sim-v1-names/tests/sims");
    let needle = format!("co fn {short}(");
    let mut entries: Vec<_> = std::fs::read_dir(&sims)
        .expect("read fixture sims dir")
        .map(|entry| entry.expect("sims dir entry").path())
        .collect();
    entries.sort();
    let mut found = Vec::new();
    for path in entries {
        if path.extension().and_then(|ext| ext.to_str()) != Some("rock") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read fixture sim source");
        if src.contains(&needle) {
            found.push(path.file_name().expect("file name").to_string_lossy().into_owned());
        }
    }
    found
}

/// The compiler's own wire -> source registry for the fixture, read from a
/// freshly prepared receipt: the oracle for where each sim was declared.
fn receipt_registry() -> std::collections::HashMap<String, String> {
    let dir = tempfile::tempdir().expect("receipt scratch");
    let out = crate::command::rock_bin()
        .arg("sim")
        .arg("--list")
        .arg(FIXTURE)
        .current_dir(workspace_root())
        .env("ROCK_SIM_COMPILE_RECEIPT_DIR", dir.path().join("receipts"))
        .env("ROCK_SIM_COMPILE_RECEIPT_PREPARE", "1")
        .env("ROCK_SIM_TIMEOUT_SECS", "120")
        .output_with_deadline()
        .expect("prepare fixture receipt");
    assert!(
        out.status.success(),
        "receipt preparation failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let project_receipt = std::fs::read_dir(dir.path().join("receipts"))
        .expect("read receipt root")
        .next()
        .expect("one project receipt")
        .expect("read project receipt")
        .path();
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(project_receipt.join("receipt.json")).expect("read receipt manifest"),
    )
    .expect("parse receipt manifest");
    manifest["registry"]
        .as_array()
        .expect("receipt registry")
        .iter()
        .map(|entry| {
            (
                entry["wire_name"].as_str().expect("wire name").to_string(),
                entry["source_file"].as_str().expect("source file").to_string(),
            )
        })
        .collect()
}

#[test]
fn same_named_v1_sims_in_different_files_resolve_their_own_snapshot_baselines() {
    let _spawn_budget = SpawnBudget::for_test(180);
    // Oracle: production's registry maps each qualified wire to its own file.
    let registry = receipt_registry();
    for (wire, file) in [
        ("sim_v1_names::shared_snapshot_probe", "tests/sims/root.rock"),
        ("sim_v1_names::other::shared_snapshot_probe", "tests/sims/other.rock"),
    ] {
        let source = registry.get(wire).unwrap_or_else(|| panic!("{wire} missing from {registry:?}"));
        assert!(
            source.ends_with(file),
            "{wire} must be recorded beside {file}, registry says {source}"
        );
    }
    // Guard: both files declare the same short name, so a source-text scan
    // reports ambiguity and no passing run below could have used one.
    assert_eq!(files_declaring("shared_snapshot_probe"), ["other.rock", "root.rock"]);
    // Each qualified name runs green against its own file's baseline (11 and
    // 13 differ, so a cross-wired lookup mismatches instead of passing).
    for (name, verdict) in [
        (
            "sim_v1_names::shared_snapshot_probe",
            "[sim] sim_v1_names::shared_snapshot_probe: PASS (1/1 expects passed)",
        ),
        (
            "sim_v1_names::other::shared_snapshot_probe",
            "[sim] sim_v1_names::other::shared_snapshot_probe: PASS (1/1 expects passed)",
        ),
    ] {
        let (out, combined) = rock_sim(&[name]);
        assert!(out.status.success(), "{name} must find its own baseline:\n{combined}");
        assert!(combined.contains(verdict), "{name} must compare its snapshot:\n{combined}");
    }
}
