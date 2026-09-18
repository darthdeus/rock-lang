//! A MAG run whose settings do not come from the player's settings file must
//! not touch the player's telemetry root: its choice is not the player's.

use std::path::Path;
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

fn boot_mag(home: &Path, data: &Path, vault: &(&str, String), settings_override: Option<&Path>) {
    let scratch = tempfile::tempdir().expect("frame scratch");
    let mut command = Command::new(env!("CARGO_BIN_EXE_rock"));
    command
        .args(["frame", "mag", "-n", "3", "--output", &scratch.path().to_string_lossy()])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .env("HOME", home)
        .env("XDG_DATA_HOME", data)
        .env(vault.0, &vault.1)
        .current_dir(workspace_root());
    if let Some(path) = settings_override {
        command.env("MAG_SETTINGS_PATH_OVERRIDE", path);
    }
    let out = command.output().expect("spawn rock frame");
    assert!(out.status.success(), "rock frame mag failed: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn overridden_settings_never_withdraw_the_players_telemetry_consent() {
    // Resolved before HOME is substituted below.
    let vault = crate::stone_harness::art_vault_env();
    let home = tempfile::tempdir().expect("home");
    let data = home.path().join("data");
    let pref = data.join("darth.games").join("mag");
    std::fs::create_dir_all(&pref).expect("pref directory");
    std::fs::write(pref.join("settings.json"), r#"{"privacy": {"analytics": 2, "analytics_version": 3}}"#)
        .expect("consenting settings");

    boot_mag(home.path(), &data, &vault, None);
    let ledger_path = pref.join("reports").join(".telemetry-consent");
    let ledger = std::fs::read_to_string(&ledger_path).expect("a consenting boot records consent");
    assert!(ledger.lines().last().is_some_and(|line| line.starts_with("on ")),
        "a consenting boot leaves consent on: {ledger:?}");

    let other = home.path().join("other-settings.json");
    std::fs::write(&other, "{}").expect("undecided settings");
    boot_mag(home.path(), &data, &vault, Some(&other));

    assert!(!pref.join("reports").join(".telemetry-revoked").exists(),
        "a run with its own settings file recorded a withdrawal in the player's root");
    assert_eq!(std::fs::read_to_string(&ledger_path).expect("consent ledger"), ledger,
        "a run with its own settings file changed the player's consent ledger");
}
