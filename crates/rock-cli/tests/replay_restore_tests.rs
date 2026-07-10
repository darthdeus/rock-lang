//! P4 of docs/replay-scrub-plan.md: keyframe snapshot + restore. This is the
//! Phase-A RESTORE ORACLE, written RED before any restore machinery exists so
//! Phase B's restore path turns it green (TDD, per the arc's discipline).
//!
//! Contract: snapshotting the registry at tick T and restoring it later must
//! return the game EXACTLY to tick-T sim state -- same `current_tick`, and every
//! simulation-state global byte-identical to a plain run-to-T. The equality
//! oracle is the engine's own `__replay_is_sim_state` predicate output (the
//! `c <tick> <name> <hash>` checkpoint names via `sim_state_names`); we compare
//! those globals, not a duplicated Rust exclusion list.
//!
//! Intended Phase-B hooks (do NOT exist yet -- that is why this is RED and
//! #[ignore]d so it never gates `just t`): `STONE_SNAPSHOT_AT_TICK=T` captures a
//! keyframe; `STONE_RESTORE_AT_TICK=U` restores the most recent keyframe. With
//! neither implemented, the restore is a no-op: the run advances to U and
//! `current_tick` stays U (!= T), so the first assertion fails. Remove #[ignore]
//! when the restore path lands.
//!
//! Heavy: spawns `rock frame`. -P full tier (excluded from the default profile).

use std::path::Path;
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("workspace root")
}

/// Sim-state global names = the engine predicate's output (its `c`-line names),
/// consumed rather than re-listed in Rust.
fn sim_state_names(game: &str, frames: u32) -> std::collections::HashSet<String> {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("names.replay");
    let scratch = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", &frames.to_string(),
            "--record", &log.to_string_lossy(), "--checkpoint-interval", "1",
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(out.status.success(), "record failed: {}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(&log).unwrap();
    text.lines()
        .filter_map(|l| l.strip_prefix("c "))
        .filter_map(|rest| rest.split(' ').nth(1))
        .map(|s| s.to_string())
        .collect()
}

/// `rock frame <game> -n N --replay <log> --dump-state [env]` -> parsed JSON.
fn run_replay_dump(game: &str, frames: u32, log: &str, env: &[(&str, &str)]) -> serde_json::Value {
    let scratch = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", &frames.to_string(), "--replay", log,
            "--dump-state", "--output", &scratch.path().to_string_lossy(),
        ])
        .envs(env.iter().copied())
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn replay");
    assert!(out.status.success(), "replay failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).expect("dump-state JSON")
}

fn current_tick(s: &serde_json::Value) -> i64 {
    s["stone"]["current_tick"].as_i64().expect("current_tick")
}

#[test]
fn restore_returns_to_snapshot_tick_state() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("restore.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    // A log to replay deterministically.
    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "30", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    // Reference: replay run stepped to exactly tick 10 -> the true tick-10 state.
    let reference = run_replay_dump(
        game, 30, &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "10")],
    );
    assert_eq!(current_tick(&reference), 10, "reference must sit at tick 10");

    // Restore run: snapshot at tick 10, advance to 20, restore the keyframe.
    // A correct restore returns to tick-10 state.
    let restored = run_replay_dump(
        game, 30, &log_s,
        &[("STONE_SNAPSHOT_AT_TICK", "10"), ("STONE_RESTORE_AT_TICK", "20")],
    );

    assert_eq!(
        current_tick(&restored), 10,
        "after restoring the tick-10 keyframe, current_tick must be 10 (got {}); \
         restore path missing today",
        current_tick(&restored)
    );

    // Every simulation-state global must match the tick-10 reference exactly.
    let sim = sim_state_names(game, 30);
    let mut compared = 0;
    for (pkg, globals) in restored.as_object().expect("dump object") {
        let ref_pkg = &reference[pkg];
        for (name, val) in globals.as_object().expect("pkg object") {
            if !sim.contains(name) {
                continue;
            }
            assert_eq!(val, &ref_pkg[name], "restored sim-state {pkg}.{name} must match tick-10");
            compared += 1;
        }
    }
    assert!(compared > 0, "must compare at least one sim-state global");
}
