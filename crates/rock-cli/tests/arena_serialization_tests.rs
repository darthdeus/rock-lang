//! An `Arena<T>` projects as its LIVE ROWS, on both the `--dump-state` walker
//! and the canonical replay-hash walker in `stone/engine/serialization.rock`.
//!
//! The Arena value a global holds is a weak `{_scene_id, _arena_id}` handle, so
//! a walker that renders it structurally emits two integers and the rows vanish
//! from every dump, save file and replay hash at once. The projection is the
//! contract; the handle fields are an implementation detail of where the rows
//! live. These assert the rows, and assert a non-zero row population so an
//! Arena that serialized as `[]` cannot pass as "no holes".
//!
//! `games/survivors` is the fixture: it keeps three Arena globals (`MOBS`,
//! `PROJECTILES`, `XP_GEMS`) and spawns into them within a few frames.
//!
//! Heavy: spawns two `rock frame` processes. -P full tier, because a GPU process
//! beside the other game-spawning tests aborts under contention rather than
//! reporting on its subject.

use std::path::Path;
use std::process::Command;
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("workspace root")
}

fn record_and_dump_survivors_state(log: &str, scratch: &Path) -> serde_json::Value {
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame",
            "games/survivors",
            "-n",
            "30",
            "--dump-state",
            "--record",
            log,
            "--checkpoint-interval",
            "5",
            "--output",
            &scratch.to_string_lossy(),
        ])
        .env("ROCK_TEST_SEED", "42")
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output_with_deadline()
        .expect("failed to spawn rock frame");
    assert!(
        out.status.success(),
        "rock frame survivors exited with {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("dump-state stdout was not valid JSON")
}

#[test]
fn arena_globals_project_as_live_rows_and_replay_deterministically() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("arena.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();
    let state = record_and_dump_survivors_state(&log_s, scratch.path());
    let pkg = &state["survivors"];

    for name in ["MOBS", "PROJECTILES", "XP_GEMS"] {
        let v = &pkg[name];
        assert!(
            v.is_array(),
            "{name} must project as an array of live rows, got: {v}\n\
             A `{{\"_scene_id\":..,\"_arena_id\":..}}` object here means the walker \
             rendered the weak handle instead of resolving its backing."
        );
    }

    // A non-zero denominator: an Arena that serialized as `[]` everywhere would
    // satisfy the shape assertion above while carrying no state at all.
    let mobs = pkg["MOBS"].as_array().expect("MOBS array");
    assert!(!mobs.is_empty(), "survivors has spawned mobs by frame 30; got an empty MOBS");

    // Rows are the ELEMENT type, not an arena entry wrapper. survivors is
    // modules-v1, so its type tag carries the module digest (`Mob_<8 hex>`).
    let row = &mobs[0];
    let ty = row["__ty"].as_str().unwrap_or("");
    assert!(
        ty.strip_prefix("Mob_")
            .is_some_and(|digest| digest.len() == 8 && digest.bytes().all(|b| b.is_ascii_hexdigit())),
        "an Arena row is the element itself; got: {row}"
    );
    assert!(row["health_cur"].is_i64(), "the row carries its own fields; got: {row}");

    // Holes are filtered: every emitted row is a real object, never a null
    // standing in for a removed slot.
    for (i, r) in mobs.iter().enumerate() {
        assert!(r.is_object(), "MOBS[{i}] is a removed-slot hole, not a row: {r}");
    }

    // This is the determinism arm, while the dump assertions above are the
    // content oracle. Record and replay sharing the walker could otherwise
    // agree after silently dropping every row.
    let text = std::fs::read_to_string(&log).unwrap();
    let hashed: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("c "))
        .filter_map(|rest| rest.split(' ').nth(1))
        .collect();
    assert!(
        hashed.iter().any(|name| name.ends_with("::MOBS")),
        "MOBS must be hashed by its logical StateSchema identity; hashed names: {hashed:?}"
    );
    assert!(
        !hashed.iter().any(|name| name.contains("<bone-stdlib:arena.rock>::__ARENA_REGISTRY")),
        "the physical Arena registry is not replay state; hashed names: {hashed:?}"
    );

    let scratch2 = tempfile::tempdir().unwrap();
    let rep = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame",
            "games/survivors",
            "-n",
            "30",
            "--replay",
            &log_s,
            "--output",
            &scratch2.path().to_string_lossy(),
        ])
        .env("ROCK_TEST_SEED", "42")
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output_with_deadline()
        .expect("spawn replay");
    assert!(
        rep.status.success(),
        "replay diverged from its own recording: {}",
        String::from_utf8_lossy(&rep.stderr)
    );
}
