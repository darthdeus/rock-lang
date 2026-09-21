//! Landing gaps that cross the Rock process boundary use the production sim.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn assert_sim_pass(name: &str, output: &std::process::Output) -> (usize, usize) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let verdicts = text
        .lines()
        .filter(|line| line.starts_with(&format!("[sim] {name}:")))
        .filter(|line| line.contains("PASS"))
        .collect::<Vec<_>>();
    assert_eq!(
        verdicts.len(),
        1,
        "expected one PASS verdict for {name}:\n{text}"
    );
    let verdict = verdicts[0];
    let counts = verdict
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(" expects passed)"))
        .and_then(|(count, _)| count.split_once('/'))
        .expect("PASS verdict must contain an expects denominator");
    let passed = counts.0.parse::<usize>().expect("passed count");
    let total = counts.1.parse::<usize>().expect("total count");
    assert!(
        total > 0,
        "{name} reported a zero expect population: {verdict}"
    );
    assert_eq!(
        passed, total,
        "{name} did not pass every expectation: {verdict}"
    );
    assert!(
        output.status.success(),
        "{name} exited unsuccessfully:\n{text}"
    );
    (passed, total)
}

fn run_sim(name: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["sim", "mag", name])
        .current_dir(workspace_root())
        .env("ROCK_SIM_TIMEOUT_SECS", "120")
        .output()
        .unwrap_or_else(|error| panic!("spawn {name}: {error}"))
}

#[test]
fn virtual_controller_reaches_the_production_rock_authority() {
    let output = run_sim("physical_pad_integration");
    assert_eq!(
        assert_sim_pass("physical_pad_integration", &output),
        (23, 23)
    );
}

#[test]
fn binding_changes_survive_a_fresh_process() {
    let root = workspace_root();
    let vault = crate::stone_harness::art_vault_env();
    let pref_root = tempfile::tempdir().expect("pref root");
    let home = pref_root.path().join("home");
    let data = pref_root.path().join("data");
    let config = pref_root.path().join("config");
    let state = pref_root.path().join("state");
    let cache = pref_root.path().join("cache");

    let run = |name: &str| {
        Command::new(env!("CARGO_BIN_EXE_rock"))
            .args(["sim", "mag", name])
            .current_dir(&root)
            .env("ROCK_SIM_TIMEOUT_SECS", "120")
            .env("HOME", &home)
            .env("XDG_DATA_HOME", &data)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_STATE_HOME", &state)
            .env("XDG_CACHE_HOME", &cache)
            .env(vault.0, &vault.1)
            .output()
            .unwrap_or_else(|error| panic!("spawn {name}: {error}"))
    };

    let writer = run("bindings_restart_writer");
    assert_sim_pass("bindings_restart_writer", &writer);
    let binding_path = root.join("tmp/spellbook-restart-bindings.json");
    assert!(
        binding_path.is_file(),
        "writer must leave the pref-root binding file"
    );
    let settings_path = root.join("tmp/spellbook-restart-settings.json");
    assert!(
        settings_path.is_file(),
        "writer must leave the pref-root settings file"
    );

    let reader = run("bindings_restart_reader");
    assert_sim_pass("bindings_restart_reader", &reader);
}

#[test]
fn controller_only_traversal_reaches_production_consumers() {
    let output = run_sim("controller_traversal");
    assert_sim_pass("controller_traversal", &output);
}
