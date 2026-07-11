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
    assert_sim_state_matches(&restored, &reference, &sim, "tick-10");
}

/// Assert every sim-state global (`sim`) in `got` matches `want`, and at least
/// one was compared.
fn assert_sim_state_matches(
    got: &serde_json::Value,
    want: &serde_json::Value,
    sim: &std::collections::HashSet<String>,
    label: &str,
) {
    let mut compared = 0;
    for (pkg, globals) in got.as_object().expect("dump object") {
        let want_pkg = &want[pkg];
        for (name, val) in globals.as_object().expect("pkg object") {
            if !sim.contains(name) {
                continue;
            }
            assert_eq!(val, &want_pkg[name], "sim-state {pkg}.{name} must match {label}");
            compared += 1;
        }
    }
    assert!(compared > 0, "must compare at least one sim-state global");
}

/// P4 item 2: scrub to an ARBITRARY earlier tick. Cadence keyframes (every
/// `STONE_KEYFRAME_INTERVAL` ticks during replay) let a backward scrub restore
/// the nearest keyframe <= T and render-skipped-resim forward to T. Landing at
/// T must be byte-identical to a plain run-to-T -- the same determinism contract
/// the restore oracle proves, but exercising nearest-keyframe selection (T=25
/// with interval 10 restores the tick-20 keyframe, not tick 0).
#[test]
fn scrub_to_earlier_tick_matches_plain_run() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("scrub.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "60", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    // Reference: the true tick-25 state.
    let reference = run_replay_dump(
        game, 60, &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "25")],
    );
    assert_eq!(current_tick(&reference), 25, "reference must sit at tick 25");

    // Scrub run: play forward to tick 40, then scrub BACK to 25. With interval
    // 10 the nearest keyframe <= 25 is tick 20, so this restores tick 20 and
    // resims 20->25 (not a trivial restore-to-exact-tick).
    let scrubbed = run_replay_dump(
        game, 60, &log_s,
        &[
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_SCRUB_AT_TICK", "40"),
            ("STONE_SCRUB_TO_TICK", "25"),
        ],
    );

    assert_eq!(
        current_tick(&scrubbed), 25,
        "after scrubbing back to 25, current_tick must be 25 (got {})",
        current_tick(&scrubbed)
    );

    let sim = sim_state_names(game, 60);
    assert_sim_state_matches(&scrubbed, &reference, &sim, "tick-25");
}

fn keyframe_count(s: &serde_json::Value) -> i64 {
    s["__stone_diag"]["STONE_KEYFRAME_COUNT"].as_i64().expect("STONE_KEYFRAME_COUNT")
}

/// Loaded audio resources (MultiSound/Sound handles) are NOT simulation state:
/// the plan excludes audio from keyframes ("never rewinds; mute during scrub,
/// resume live"), the same carve-out render resources already get. bitgun's
/// `sfx_shotgun` is a `Sound` handle -- it must be absent from the sim-state
/// predicate's output (so it is neither snapshotted nor desync-hashed), while a
/// real gameplay global (`MOBS`) stays present.
#[test]
fn audio_resources_excluded_from_sim_state() {
    let sim = sim_state_names("bitgun", 3);
    assert!(
        sim.contains("MOBS"),
        "control: gameplay global MOBS must be sim state (got {} names)",
        sim.len()
    );
    assert!(
        !sim.contains("sfx_shotgun"),
        "audio Sound handle sfx_shotgun must be excluded from sim state"
    );
}

/// P5: a hot reload during replay invalidates keyframes (new code may have
/// changed state shapes, so old-code keyframes are unusable). We inject a reload
/// mid-replay (STONE_RELOAD_AT_FRAME, which during replay routes to the P5
/// handler) and assert the keyframe buffer is smaller than an identical run
/// without the reload, and that the reload was counted.
#[test]
fn reload_during_replay_invalidates_keyframes() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("reload.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "50", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    // Control: normal replay, cadence 10 -> keyframes accumulate to ~6.
    let control = run_replay_dump(game, 50, &log_s, &[("STONE_KEYFRAME_INTERVAL", "10")]);
    let control_kf = keyframe_count(&control);
    assert!(control_kf >= 5, "control should accumulate keyframes (got {control_kf})");
    assert_eq!(
        control["__stone_diag"]["STONE_REPLAY_RELOADS"].as_i64().unwrap_or(0), 0,
        "control run must not report a reload"
    );

    // Reloaded: same, but inject a reload at frame 48 -> keyframes invalidated,
    // so only the few captured after the reload remain.
    let reloaded = run_replay_dump(
        game, 50, &log_s,
        &[("STONE_KEYFRAME_INTERVAL", "10"), ("STONE_RELOAD_AT_FRAME", "48")],
    );
    assert!(
        reloaded["__stone_diag"]["STONE_REPLAY_RELOADS"].as_i64().unwrap_or(0) >= 1,
        "a reload during replay must be counted"
    );
    let reloaded_kf = keyframe_count(&reloaded);
    assert!(
        reloaded_kf < control_kf,
        "reload must invalidate keyframes: reloaded {reloaded_kf} should be < control {control_kf}"
    );
}

/// P6: the A-B loop. Enabling with A=10,B=20 keyframes A, plays A->B rendered,
/// and at B restores(A) and repeats -- each jump is a snapshot restore, not a
/// resim. Over 120 frames the loop must fire several times, current_tick must
/// stay inside [A,B], and -- the correctness point -- the landing state must be
/// byte-identical to a plain run-to that tick (the loop re-derives A->T
/// deterministically from the restored keyframe). The restore-ms timer must also
/// be populated (the A-jump seam measurement).
#[test]
fn ab_loop_cycles_and_restores_correct_state() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("loop.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "60", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    let looped = run_replay_dump(
        game, 120, &log_s,
        &[("STONE_LOOP_A", "10"), ("STONE_LOOP_B", "20"), ("STONE_KEYFRAME_INTERVAL", "10")],
    );
    let jumps = looped["__stone_diag"]["STONE_LOOP_JUMPS"].as_i64().unwrap_or(0);
    assert!(jumps >= 3, "the A-B loop must cycle several times over 120 frames (jumps={jumps})");
    let t = current_tick(&looped);
    assert!((10..=20).contains(&t), "loop must keep current_tick in [10,20] (got {t})");
    assert!(
        looped["__stone_diag"]["STONE_RESTORE_MS"].as_f64().unwrap_or(0.0) > 0.0,
        "the A-jump restore-ms timer must be populated"
    );

    // Correctness: the loop's landing state must match a plain run-to that tick.
    let reference = run_replay_dump(
        game, 60, &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", &t.to_string())],
    );
    assert_eq!(current_tick(&reference), t, "reference must sit at the loop's tick {t}");
    // `dt` is the instantaneous frame delta -- 0 on the paused reference, the
    // replayed value on the mid-play loop; a phase artifact, not divergence
    // (accumulated `time` still matches). Exclude it, like the PROFILER carve-out.
    let mut sim = sim_state_names(game, 60);
    sim.remove("dt");
    assert_sim_state_matches(&looped, &reference, &sim, &format!("loop tick-{t}"));
}

/// P6: the loop-boundary reload gate. A body-edit reload during an active loop
/// invalidates all keyframes EXCEPT the boot anchor (tick 0), so the plain
/// restore(A) at the next boundary would fail; the loop instead re-derives A
/// (restore boot -> resim boot->A -> re-snapshot) and keeps cycling. We inject a
/// reload at frame 45 (well into the loop) and assert the loop keeps jumping and
/// stays in [A,B] with correct state -- without the re-derive it would wedge the
/// moment A's keyframe is gone.
#[test]
fn ab_loop_survives_incremental_reload_via_rederive() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("loopreload.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "80", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    let looped = run_replay_dump(
        game, 150, &log_s,
        &[
            ("STONE_LOOP_A", "10"), ("STONE_LOOP_B", "20"),
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_RELOAD_AT_FRAME", "45"),   // incremental (STONE_RELOAD_FULL unset)
        ],
    );
    assert_eq!(
        looped["__stone_diag"]["STONE_REPLAY_RELOADS"].as_i64().unwrap_or(0), 1,
        "the injected reload must have fired once"
    );
    let jumps = looped["__stone_diag"]["STONE_LOOP_JUMPS"].as_i64().unwrap_or(0);
    // ~3-4 jumps happen before the frame-45 reload; requiring >= 6 proves the
    // loop kept cycling AFTER it (via the re-derive), not just before.
    assert!(jumps >= 6, "loop must keep cycling across the reload (jumps={jumps})");
    let t = current_tick(&looped);
    assert!((10..=20).contains(&t), "loop must stay in [10,20] after a reload (got {t})");

    let reference = run_replay_dump(
        game, 80, &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", &t.to_string())],
    );
    assert_eq!(current_tick(&reference), t, "reference must sit at tick {t}");
    // Exclude `dt` (phase artifact: paused reference 0 vs mid-play loop) as above.
    let mut sim = sim_state_names(game, 80);
    sim.remove("dt");
    assert_sim_state_matches(&looped, &reference, &sim, &format!("post-reload loop tick-{t}"));
}

/// Review fix 2: a loop wider than the keyframe window must NOT let eviction drop
/// its A keyframe (nor the tick-0 boot anchor). With A=10, B=90, window=30, at
/// tick ~90 the window floor is ~60, so a naive evict would drop A=10 and wedge
/// the next jump. A is exempt from eviction, so the loop keeps cycling in [A,B]
/// with correct state -- never silently diverging past B.
#[test]
fn wide_loop_keyframe_survives_eviction() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("wideloop.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "100", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    let looped = run_replay_dump(
        game, 250, &log_s,
        &[
            ("STONE_LOOP_A", "10"), ("STONE_LOOP_B", "90"),
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_KEYFRAME_WINDOW", "30"),   // narrower than the loop span (80)
        ],
    );
    let jumps = looped["__stone_diag"]["STONE_LOOP_JUMPS"].as_i64().unwrap_or(0);
    assert!(jumps >= 2, "wide loop must keep cycling despite eviction (jumps={jumps})");
    let t = current_tick(&looped);
    assert!(
        (10..=90).contains(&t),
        "wide loop must stay in [10,90]; escaping means A was evicted and the loop wedged (got {t})"
    );

    let reference = run_replay_dump(
        game, 100, &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", &t.to_string())],
    );
    assert_eq!(current_tick(&reference), t, "reference must sit at tick {t}");
    let mut sim = sim_state_names(game, 100);
    sim.remove("dt");
    assert_sim_state_matches(&looped, &reference, &sim, &format!("wide loop tick-{t}"));
}

/// P4 item 4: a bounded keyframe ring buffer. STONE_KEYFRAME_WINDOW (ticks, 0 =
/// unbounded) evicts keyframes older than the floor -- the newest keyframe <=
/// (now - window) -- so a long session's keyframe memory is bounded while a tick
/// at (or just below) the window start stays restorable, because the ONE
/// keyframe below the window start is retained. With interval 10 + window 30, the
/// scrub fires at tick 90 where the last eviction floor was the tick-50 keyframe
/// (nearest <= 89-30); scrubbing to 55 restores exactly that floor keyframe
/// (nearest <= 55 == 50). Landing at 55 proves the floor was RETAINED --
/// over-eviction that dropped it would leave no keyframe <= 55, so the scrub
/// would not fire and current_tick would stay at 90.
#[test]
fn bounded_keyframe_window_evicts_but_keeps_window_restorable() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("win.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "100", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    let reference = run_replay_dump(
        game, 100, &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "55")],
    );
    assert_eq!(current_tick(&reference), 55, "reference must sit at tick 55");

    // Unbounded control: normal forward play (NOT a seek -- seeking skips cadence
    // capture) over the 100-tick log -> keyframes at 0,10,..,100 (~11).
    let unbounded = run_replay_dump(
        game, 100, &log_s,
        &[("STONE_KEYFRAME_INTERVAL", "10")],
    );
    let unbounded_kf = keyframe_count(&unbounded);
    assert!(unbounded_kf >= 8, "unbounded control should retain ~10 keyframes (got {unbounded_kf})");

    // Bounded + scrub: window 30, play to 90 then scrub back to 75.
    let bounded = run_replay_dump(
        game, 100, &log_s,
        &[
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_KEYFRAME_WINDOW", "30"),
            ("STONE_SCRUB_AT_TICK", "90"),
            ("STONE_SCRUB_TO_TICK", "55"),
        ],
    );
    assert_eq!(
        current_tick(&bounded), 55,
        "bounded scrub must land at tick 55 (the floor keyframe at 50 must survive eviction)"
    );
    let bounded_kf = keyframe_count(&bounded);
    assert!(
        bounded_kf <= 5,
        "window 30 must bound the keyframe buffer well below the unbounded {unbounded_kf} (got {bounded_kf})"
    );
    let sim = sim_state_names(game, 100);
    assert_sim_state_matches(&bounded, &reference, &sim, "tick-55");
}
