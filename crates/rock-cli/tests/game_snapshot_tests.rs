use std::path::Path;
use std::process::Command;
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
}

/// Run `rock frame <game> -n <frames> --dump-state` and return the parsed JSON.
///
/// Each call gets a unique --output scratch dir so multiple tests can run
/// concurrently without racing on `<output>/main.lua`. libstone.dylib is
/// still staged to the shared workspace `tmp/` but that path is already
/// atomic-rename safe (see `stage_cdylib` in rock-cli/src/stone.rs).
fn run_frame_dump(
    game: &str,
    frames: u32,
    seed: u64,
    preset: Option<&str>,
) -> serde_json::Value {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let scratch = tempfile::tempdir().expect("create scratch dir for --output");
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let mut cmd = Command::new(rock_bin);
    cmd.args(["frame", game, "-n", &frames.to_string(), "--dump-state", "--output", &scratch_path])
        .env("ROCK_TEST_SEED", seed.to_string())
        .current_dir(workspace_root());

    if let Some(p) = preset {
        cmd.env("MATCH3_PRESET", p);
    }

    let output = cmd.output_with_deadline().expect("failed to spawn rock frame");
    assert!(
        output.status.success(),
        "rock frame {game} exited with {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.stdout.is_empty(),
        "rock frame {game} produced no stdout — did --dump-state work?"
    );

    serde_json::from_slice(&output.stdout)
        .expect("dump-state stdout was not valid JSON")
}

/// Extract only runtime fields (grid, score, moves, active_preset).
/// Omits constants and animation arrays so snapshots don't break on unrelated changes.
fn match3_runtime(state: &serde_json::Value) -> serde_json::Value {
    let m = &state["match3"];
    serde_json::json!({
        "active_preset": m["active_preset"],
        "grid":          m["grid"],
        "score":         m["score"],
        "moves":         m["moves"],
    })
}

/// Decode a tile index to its letter. Tile labels: R=0 B=1 G=2 Y=3 P=4 O=5.
fn tile_char(t: &serde_json::Value) -> char {
    match t.as_i64().unwrap_or(-1) {
        0 => 'R',
        1 => 'B',
        2 => 'G',
        3 => 'Y',
        4 => 'P',
        5 => 'O',
        _ => '?',
    }
}

/// Decode one row (0-based) of the 8x8 grid as a letter string like "YGBPYGBP".
fn grid_row(state: &serde_json::Value, row: usize) -> String {
    let data = &state["match3"]["grid"]["data"];
    (0..8).map(|col| tile_char(&data[row * 8 + col])).collect()
}

// ── determinism / structural assertions ──────────────────────────────────────
// These are plain assert_eq! tests — no snapshot involved.
// They encode invariants we always know to be true and give specific
// failure messages, unlike snapshot diffs.

#[test]
fn test_determinism_same_seed_produces_identical_boards() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let a = run_frame_dump("games/match3", 1, 42, None);
    let b = run_frame_dump("games/match3", 1, 42, None);
    assert_eq!(
        a["match3"]["grid"], b["match3"]["grid"],
        "same ROCK_TEST_SEED must produce identical boards on every run"
    );
}

#[test]
fn test_different_seeds_produce_different_boards() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let a = run_frame_dump("games/match3", 1, 42, None);
    let b = run_frame_dump("games/match3", 1, 43, None);
    assert_ne!(
        a["match3"]["grid"], b["match3"]["grid"],
        "different seeds should produce different boards — seeding has no effect"
    );
}

#[test]
fn test_initial_state_invariants() {
    let _spawn_budget = SpawnBudget::for_test(120);
    // Score, moves, and grid size must be the same regardless of preset.
    for preset in [None, Some("cascade"), Some("many_moves")] {
        let state = run_frame_dump("games/match3", 1, 42, preset);
        let m = &state["match3"];
        assert_eq!(m["score"], 0, "score must start at 0 (preset={preset:?})");
        assert_eq!(m["moves"], 0, "moves must start at 0 (preset={preset:?})");
        assert_eq!(
            m["grid"]["len"], 64,
            "grid must have exactly 64 tiles (preset={preset:?})"
        );
    }
}

#[test]
fn test_cascade_preset_grid_matches_definition() {
    let _spawn_budget = SpawnBudget::for_test(120);
    // The cascade preset string is "YGBPYGBP/GBPYGBPY/RBRPYGBP/BPBYGBPY/BOBYGBPY/ORRPYGBP/OGYBGYGB/YBGPYBGP".
    // Verify that parse_compact_grid decoded it correctly using the tile label encoding.
    let state = run_frame_dump("games/match3", 1, 42, Some("cascade"));

    assert_eq!(grid_row(&state, 0), "YGBPYGBP", "cascade row 0");
    assert_eq!(grid_row(&state, 1), "GBPYGBPY", "cascade row 1");
    assert_eq!(grid_row(&state, 2), "RBRPYGBP", "cascade row 2");
    assert_eq!(grid_row(&state, 7), "YBGPYBGP", "cascade last row");
    assert_eq!(state["match3"]["active_preset"], "cascade");
}

#[test]
fn test_many_moves_preset_grid_matches_definition() {
    let _spawn_budget = SpawnBudget::for_test(120);
    // "RGBYRPGB/BGRPYBGR/GRYPBGRP/RPBGYRPB/PYRGBPYR/YRPBGYRP/BPYRGBPY/GRPYBGRP"
    let state = run_frame_dump("games/match3", 1, 42, Some("many_moves"));

    assert_eq!(grid_row(&state, 0), "RGBYRPGB", "many_moves row 0");
    assert_eq!(grid_row(&state, 7), "GRPYBGRP", "many_moves last row");
    assert_eq!(state["match3"]["active_preset"], "many_moves");
}

#[test]
fn test_30_frames_no_input_board_unchanged() {
    let _spawn_budget = SpawnBudget::for_test(120);
    // With no player input the board must not change across frames.
    // Verifies the @update loop does not mutate state spuriously.
    let frame1 = run_frame_dump("games/match3", 1, 42, Some("cascade"));
    let frame30 = run_frame_dump("games/match3", 30, 42, Some("cascade"));
    assert_eq!(
        frame1["match3"]["grid"], frame30["match3"]["grid"],
        "board must not change without player input"
    );
    assert_eq!(frame30["match3"]["score"], 0);
    assert_eq!(frame30["match3"]["moves"], 0);
}

// ── snapshot tests ────────────────────────────────────────────────────────────
// These capture the full runtime state for regression detection.
// Run `just test-games-snapshot-update` after an intentional change.

#[test]
fn test_match3_cascade_preset_frame1() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let state = run_frame_dump("games/match3", 1, 42, Some("cascade"));
    insta::assert_json_snapshot!(match3_runtime(&state));
}

#[test]
fn test_match3_many_moves_preset_frame1() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let state = run_frame_dump("games/match3", 1, 42, Some("many_moves"));
    insta::assert_json_snapshot!(match3_runtime(&state));
}

// Seeded-random snapshots exercise the numeric 32-bit RNG state. A given
// Artifact/backend/seed combination must produce a deterministic board;
// streams may differ between backends.
#[test]
fn test_match3_seeded_random_board_frame1() {
    let _spawn_budget = SpawnBudget::for_test(120);
    // No preset — verifies seed alone produces a deterministic board.
    let state = run_frame_dump("games/match3", 1, 42, None);
    insta::assert_json_snapshot!(match3_runtime(&state));
}

#[test]
fn test_match3_seeded_random_board_alt_seed() {
    let _spawn_budget = SpawnBudget::for_test(120);
    // A second seed to widen coverage. Snapshot will differ from seed 42.
    let state = run_frame_dump("games/match3", 1, 99, None);
    insta::assert_json_snapshot!(match3_runtime(&state));
}

/// An errored coroutine must surface its error in the log. `last_error` has no
/// callers and the poller treats an errored coroutine identically to natural
/// completion, so without a poll-level report the raise is swallowed entirely
/// (docs/coroutine-allocation-plan.md "Errored coroutines must be LOGGED").
/// The sim (games/testbed/tests/sims/errored_co_logs.rock) drives a raising
/// coroutine through `poll`; this test asserts the error text reaches the
/// game's stderr, which `rock sim` forwards verbatim.
#[test]
fn test_errored_coroutine_is_logged() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let output = Command::new(rock_bin)
        .args(["sim", "testbed", "testbed::errored_co_logs"])
        .current_dir(workspace_root())
        .output_with_deadline()
        .expect("failed to spawn rock sim");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rock sim testbed errored_co_logs failed: {:?}\nstderr:\n{stderr}\nstdout:\n{stdout}",
        output.status
    );
    assert!(
        stderr.contains("boom-co-error-probe"),
        "an errored coroutine's message must reach the log (stderr); got:\n{stderr}"
    );
}
