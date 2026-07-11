//! P3 of docs/replay-scrub-plan.md: timeline controls (pause / step /
//! run-to-tick) and the auto-record ring buffer. The ImGui timeline bar
//! itself is visual and left to the author; here we drive the testable core
//! headless via env (STONE_START_PAUSED / STONE_STEP_FRAMES / STONE_RUN_TO_TICK)
//! and observe the effect through --dump-state (current_tick / time frozen or
//! advanced by the exact amount).
//!
//! Heavy: spawns `rock frame`. Excluded from the default nextest profile
//! (see `.config/nextest.toml`); runs under `-P full`.

use std::path::Path;
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

/// `rock frame <game> -n N --dump-state [extra…]` with extra env; parsed JSON.
fn run_frame_env(
    game: &str,
    frames: u32,
    extra: &[&str],
    env: &[(&str, &str)],
) -> serde_json::Value {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let scratch = tempfile::tempdir().expect("scratch dir");
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let frames_s = frames.to_string();
    let mut args: Vec<&str> = vec![
        "frame", game, "-n", &frames_s, "--dump-state", "--output", &scratch_path,
    ];
    args.extend_from_slice(extra);
    let out = Command::new(rock_bin)
        .args(&args)
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("failed to spawn rock frame");
    assert!(
        out.status.success(),
        "rock frame {game} {extra:?} env={env:?} exited {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "dump-state not valid JSON ({e})\nstdout:\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn current_tick(state: &serde_json::Value) -> i64 {
    state["stone"]["current_tick"].as_i64().expect("current_tick")
}

/// The engine-owned simulation-state predicate's OUTPUT: record `game` with a
/// per-tick checkpoint so every `c <tick> <name> <hash>` line names exactly the
/// globals the engine hashes as simulation state, and collect that name set.
/// Consuming this (rather than duplicating the exclusion list in Rust) is the
/// P3.5 unification -- the Lua `__replay_is_sim_state` predicate is the single
/// source of truth.
fn sim_state_names(game: &str, frames: u32) -> std::collections::HashSet<String> {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("simnames.replay");
    let scratch = tempfile::tempdir().expect("scratch");
    let out = Command::new(rock_bin)
        .args([
            "frame", game, "-n", &frames.to_string(),
            "--record", &log.to_string_lossy(),
            "--checkpoint-interval", "1",
            "--output", &scratch.path().to_string_lossy(),
        ])
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(out.status.success(), "record failed: {}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(&log).expect("read log");
    let names: std::collections::HashSet<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("c "))
        .filter_map(|rest| rest.split(' ').nth(1))
        .map(|s| s.to_string())
        .collect();
    assert!(!names.is_empty(), "record must emit checkpoint lines naming sim-state globals");
    names
}

/// A run started paused advances no game tick and no game time, however many
/// frames render.
#[test]
fn start_paused_freezes_tick_and_time() {
    let s = run_frame_env("bitgun", 30, &[], &[("STONE_START_PAUSED", "1")]);
    assert_eq!(current_tick(&s), 0, "paused run must not advance the tick counter");
    assert_eq!(
        s["stone"]["time"].as_f64(),
        Some(0.0),
        "paused run must not accumulate game time\ntime={}",
        s["stone"]["time"]
    );
}

/// Queued step-frames advance exactly that many ticks, then re-freeze.
#[test]
fn step_frames_advance_exactly_n_ticks() {
    let s = run_frame_env(
        "bitgun",
        30,
        &[],
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "3")],
    );
    assert_eq!(
        current_tick(&s),
        3,
        "3 queued steps must advance exactly 3 ticks, then freeze"
    );
}

/// Run-to-tick fast-forwards to the target tick, then pauses there.
#[test]
fn run_to_tick_stops_at_target() {
    let s = run_frame_env("bitgun", 40, &[], &[("STONE_RUN_TO_TICK", "10")]);
    assert_eq!(
        current_tick(&s),
        10,
        "run-to-tick must advance to the target then pause"
    );
}

/// Piece 4 of P3: fast-forward (run-to-tick) skips the render pass on the
/// intermediate ticks, but must reach the SAME simulation state as stepping
/// there with rendering. Record a log, then replay it two ways to tick 25 --
/// seek (render-skipped) vs step (rendered).
///
/// Scope is the engine's simulation-state predicate (P3.5): compare every
/// global the engine hashes as sim state (from `sim_state_names`), across all
/// packages. Render OUTPUT legitimately differs -- skipping render leaves the
/// batch pools / shadow scratch / graphics-stats counters and game Materials'
/// `cached_batch` unbuilt (`RENDER_BATCHES`, `SHADOW_*`, `stats_history_*`,
/// `tileset_mat`, ...); those are excluded by the predicate, not compared here.
/// Tick 25 is far enough that the render-only material batches have grown, so
/// this would catch a predicate that failed to exclude them.
#[test]
fn seek_reaches_same_state_as_stepping() {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("seek.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().expect("rec scratch");

    // Record a log to replay.
    let rec = Command::new(rock_bin)
        .args([
            "frame", "bitgun", "-n", "30", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    // Seek (render-skipped) to tick 25 vs step (rendered) to tick 25.
    let seek = run_frame_env("bitgun", 30, &["--replay", &log_s], &[("STONE_RUN_TO_TICK", "25")]);
    let step = run_frame_env(
        "bitgun",
        30,
        &["--replay", &log_s],
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "25")],
    );
    assert_eq!(current_tick(&seek), 25, "seek must reach tick 25");
    assert_eq!(current_tick(&step), 25, "step must reach tick 25");

    // Compare every simulation-state global (the predicate's output), across
    // packages, restricted to the names the engine hashes.
    let sim = sim_state_names("bitgun", 30);
    let mut compared = 0;
    for (pkg, globals) in seek.as_object().expect("dump is an object") {
        let step_pkg = &step[pkg];
        for (name, sv) in globals.as_object().expect("package is an object") {
            if !sim.contains(name) {
                continue;
            }
            assert_eq!(
                sv, &step_pkg[name],
                "sim-state global {pkg}.{name} must match between render-skipped seek and rendered step"
            );
            compared += 1;
        }
    }
    assert!(compared > 0, "must have compared at least one sim-state global");
}

/// P3.5 item 5: audio is suppressed while fast-forward-seeking. A render-skipped
/// seek still runs every update system per intermediate tick, so their per-tick
/// SFX triggers would machine-gun. `STONE_AUDIO_PROBE=1` fires one probe through
/// the real `play_random_sound` gate each ticking frame; `AUDIO_SEEK_MUTES`
/// counts the ones the gate silenced *because of the seek*. During a run-to-tick
/// seek every probe is muted; a normal run mutes none.
#[test]
fn audio_muted_only_while_seeking() {
    let seek = run_frame_env(
        "bitgun",
        40,
        &[],
        &[("STONE_AUDIO_PROBE", "1"), ("STONE_RUN_TO_TICK", "25")],
    );
    let normal = run_frame_env("bitgun", 25, &[], &[("STONE_AUDIO_PROBE", "1")]);
    let seek_mutes = seek["__stone_diag"]["AUDIO_SEEK_MUTES"]
        .as_i64()
        .expect("AUDIO_SEEK_MUTES present in dump");
    // A normal run never fires the seek mute, so the diag counter is absent -> 0.
    let normal_mutes = normal["__stone_diag"]["AUDIO_SEEK_MUTES"].as_i64().unwrap_or(0);
    assert!(
        seek_mutes > 0,
        "seek must suppress the per-tick SFX probe (muted {seek_mutes})"
    );
    assert_eq!(
        normal_mutes, 0,
        "normal (non-seek) play must not suppress audio (muted {normal_mutes})"
    );
}

/// P4 item 5: a BACKWARD scrub mutes audio through the SAME single gate as a
/// forward seek (no second gate). The scrub's resim (from the restored keyframe
/// forward to the target) runs render-skipped with `timeline_seeking()` true, so
/// `audio_gate_muted()` suppresses the per-tick SFX probe during it. Replay to
/// tick 40, scrub back to 25 (interval 10 -> restore tick-20 keyframe, resim
/// 20->25) and assert the probe was muted; break-verify by flipping the gate.
#[test]
fn backward_scrub_mutes_audio_through_the_same_gate() {
    let (_dir, log) = record_short_log("bitgun", 60);
    let scrubbed = run_frame_env(
        "bitgun",
        60,
        &["--replay", &log],
        &[
            ("STONE_AUDIO_PROBE", "1"),
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_SCRUB_AT_TICK", "40"),
            ("STONE_SCRUB_TO_TICK", "25"),
            // 60-tick replay under -P full parallelism can be starved past the
            // 3s watchdog (the note-6 flake class); raise it like the other
            // heavy-spawn tests.
            ("ROCK_FRAME_TIMEOUT_SECS", "60"),
        ],
    );
    assert_eq!(current_tick(&scrubbed), 25, "scrub must land at tick 25");
    let mutes = scrubbed["__stone_diag"]["AUDIO_SEEK_MUTES"]
        .as_i64()
        .expect("AUDIO_SEEK_MUTES present in dump");
    assert!(
        mutes > 0,
        "the backward-scrub resim must suppress the per-tick SFX probe (muted {mutes})"
    );
}

/// P3.5 item 5: the recorded-cursor ghost draws during WINDOWED replay (the OS
/// cursor is wherever the developer's real mouse is, so replay marks the
/// recorded mouse position). It is a viewer overlay -- only windowed playback
/// draws it; headless batch reproduction (`rock frame --replay`) does not, so
/// the extra draw call never perturbs the byte-identity guard.
#[test]
fn cursor_ghost_draws_only_during_windowed_replay() {
    let (_dir, log) = record_short_log("bitgun", 20);
    let windowed = run_frame_env(
        "bitgun",
        20,
        &["--replay", &log],
        &[("STONE_REPLAY_WINDOW", "1"), ("STONE_HEADLESS", "1")],
    );
    // Plain headless replay (no window): the ghost must stay off.
    let headless = run_frame_env("bitgun", 20, &["--replay", &log], &[]);
    let ghost_windowed = windowed["__stone_diag"]["REPLAY_GHOST_DRAWS"]
        .as_i64()
        .expect("REPLAY_GHOST_DRAWS present in dump");
    // Headless replay never draws the ghost, so the diag counter is absent -> 0.
    let ghost_headless = headless["__stone_diag"]["REPLAY_GHOST_DRAWS"].as_i64().unwrap_or(0);
    assert!(
        ghost_windowed > 0,
        "recorded-cursor ghost must draw during windowed replay (drew {ghost_windowed})"
    );
    assert_eq!(
        ghost_headless, 0,
        "cursor ghost must not draw in headless batch replay (drew {ghost_headless})"
    );
}

/// P3.5 item 5: the replay picker's listing + header parse. `list_replays(dir)`
/// enumerates `*.replay`, counts `t ` lines as the tick count, and reads the v4
/// `build <id>` header. Drive it headless via STONE_REPLAY_PICKER_DIR and read
/// the observable globals; non-`.replay` files are ignored.
#[test]
fn replay_picker_lists_and_parses_headers() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(
        dir.path().join("a.replay"),
        "STONE_REPLAY 4\nseed 1\nbuild aaa111\nt 0 0 0 0 0  \nt 0 0 0 0 0  \nt 0 0 0 0 0  \n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("b.replay"),
        "STONE_REPLAY 4\nseed 2\nt 0 0 0 0 0  \nt 0 0 0 0 0  \n",
    )
    .unwrap();
    // A non-replay file must be ignored by the listing.
    std::fs::write(dir.path().join("notes.txt"), "ignore me").unwrap();

    let s = run_frame_env(
        "bitgun",
        2,
        &[],
        &[("STONE_REPLAY_PICKER_DIR", &dir.path().to_string_lossy())],
    );
    let count = s["__stone_diag"]["REPLAY_PICKER_COUNT"]
        .as_i64()
        .expect("REPLAY_PICKER_COUNT present");
    let summary = s["__stone_diag"]["REPLAY_PICKER_SUMMARY"]
        .as_str()
        .expect("REPLAY_PICKER_SUMMARY present");
    assert_eq!(count, 2, "must list exactly the two .replay files (got {count}, summary {summary:?})");
    assert!(
        summary.contains("a.replay:3:aaa111"),
        "a.replay: 3 ticks + build aaa111; summary was {summary:?}"
    );
    assert!(
        summary.contains("b.replay:2:"),
        "b.replay: 2 ticks + no build; summary was {summary:?}"
    );
}

/// P3.5 item 5 (regression): the cursor ghost must mark the RECORDED pointer,
/// not the developer's live OS mouse, on a frozen frame. `sim_mouse_override`
/// is cleared every frame end and set only on ticking frames, so a paused /
/// frozen replay would leave `mouse_screen()` falling back to the live mouse --
/// exactly when you'd be inspecting the frame. We rewrite a recorded log's
/// mouse to a distinctive (123,456) (headless capture leaves it at 0,0, which
/// is indistinguishable from the live-mouse fallback), freeze a windowed replay
/// past log-end, and assert the ghost drew at the recorded position.
#[test]
fn cursor_ghost_marks_recorded_pointer_when_frozen() {
    let (_dir, log) = record_short_log("bitgun", 10);
    let text = std::fs::read_to_string(&log).unwrap();
    let patched = text
        .lines()
        .map(|l| match l.strip_prefix("t ") {
            Some(rest) => {
                let mut f: Vec<String> = rest.split(' ').map(str::to_string).collect();
                if f.len() >= 3 {
                    f[1] = "123".into();
                    f[2] = "456".into();
                }
                format!("t {}", f.join(" "))
            }
            None => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&log, patched).unwrap();

    // 40 frames over a 10-tick log: replay applies 10 ticks then freezes for the
    // rest. The dump is captured on a frozen frame.
    let s = run_frame_env(
        "bitgun",
        40,
        &["--replay", &log],
        &[("STONE_REPLAY_WINDOW", "1"), ("STONE_HEADLESS", "1")],
    );
    let gx = s["__stone_diag"]["REPLAY_GHOST_POS_X"]
        .as_f64()
        .expect("REPLAY_GHOST_POS_X present in dump");
    assert_eq!(
        gx, 123.0,
        "frozen ghost must mark the recorded pointer (123), not the live mouse (drew at x={gx})"
    );
}

/// P3.5 item 5 (regression): the ghost must not draw before any tick has
/// applied a recorded pointer -- a start-paused windowed replay is frozen at
/// tick 0 with an unset stash, and the old `or 0.0` fallback pinned a crosshair
/// at top-left. With the fix the ghost skips entirely (0 draws).
#[test]
fn cursor_ghost_skips_before_first_tick() {
    let (_dir, log) = record_short_log("bitgun", 20);
    let s = run_frame_env(
        "bitgun",
        20,
        &["--replay", &log],
        &[
            ("STONE_REPLAY_WINDOW", "1"),
            ("STONE_HEADLESS", "1"),
            ("STONE_START_PAUSED", "1"),
        ],
    );
    // Ghost never draws before the first tick, so the diag counter is absent -> 0.
    let draws = s["__stone_diag"]["REPLAY_GHOST_DRAWS"].as_i64().unwrap_or(0);
    assert_eq!(
        draws, 0,
        "ghost must not draw before the first tick applies a recorded pointer (drew {draws})"
    );
}

/// Record a short log for the keyboard-shortcut tests. Returns the temp dir
/// (keeps the file alive) + the log path.
fn record_short_log(game: &str, frames: u32) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("kb.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().expect("scratch");
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", &frames.to_string(), "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(out.status.success(), "record failed: {}", String::from_utf8_lossy(&out.stderr));
    (dir, log_s)
}

/// P3.5: the replay timeline SPACE shortcut toggles play/pause by reading ImGui
/// key state, NOT the replayed `keys_down` (the log owns that during replay).
/// We inject a Space key event into ImGui at frame 3 of a windowed replay (the
/// recorded log has no keys down, so keys_down never carries Space) and assert
/// the run freezes: current_tick stops advancing well before the ~20-tick log
/// end. Only the ImGui-IO path could cause that.
#[test]
fn space_toggles_pause_via_imgui_not_replayed_input() {
    let (_dir, log) = record_short_log("bitgun", 20);
    let frozen = run_frame_env(
        "bitgun",
        40,
        &["--replay", &log],
        &[
            ("STONE_REPLAY_WINDOW", "1"),
            ("STONE_HEADLESS", "1"),
            ("STONE_TIMELINE_KEY_AT", "3"),
            ("STONE_TIMELINE_KEY", "space"),
        ],
    );
    let ct = current_tick(&frozen);
    assert!(
        ct < 10,
        "Space must pause the replay via ImGui IO (froze at tick {ct}, expected < 10, log is ~20)"
    );
}

/// P3.5: the RIGHT shortcut steps exactly one tick, again via ImGui IO. Start
/// paused (tick frozen at 0), inject Right at frame 3, and assert the timeline
/// advanced exactly one tick.
#[test]
fn right_steps_one_tick_via_imgui_not_replayed_input() {
    let (_dir, log) = record_short_log("bitgun", 20);
    let stepped = run_frame_env(
        "bitgun",
        40,
        &["--replay", &log],
        &[
            ("STONE_REPLAY_WINDOW", "1"),
            ("STONE_HEADLESS", "1"),
            ("STONE_START_PAUSED", "1"),
            ("STONE_TIMELINE_KEY_AT", "3"),
            ("STONE_TIMELINE_KEY", "right"),
        ],
    );
    assert_eq!(
        current_tick(&stepped), 1,
        "Right must step exactly one tick from a paused start"
    );
}

/// P6: the loop-marker keys go through ImGui IO like the other timeline keys, NOT
/// the replayed keys_down. Start paused, step to a non-zero tick, inject `b`, and
/// assert loop marker B was set to that tick (via the non-dumped diag). The
/// recorded log never carries `b` in keys_down, so only the ImGui-IO path sets it.
#[test]
fn loop_marker_b_set_via_imgui_not_replayed_input() {
    let (_dir, log) = record_short_log("bitgun", 20);
    let marked = run_frame_env(
        "bitgun",
        40,
        &["--replay", &log],
        &[
            ("STONE_REPLAY_WINDOW", "1"),
            ("STONE_HEADLESS", "1"),
            ("STONE_START_PAUSED", "1"),
            ("STONE_STEP_FRAMES", "5"),
            ("STONE_TIMELINE_KEY_AT", "9"),
            ("STONE_TIMELINE_KEY", "b"),
        ],
    );
    assert_eq!(
        marked["__stone_diag"]["STONE_LOOP_MARK_B"].as_i64().unwrap_or(-1),
        5,
        "`b` while paused at tick 5 must set loop marker B = 5 via ImGui IO"
    );
}

/// Piece 5 of P3: the auto-record "save repro" action snapshots the live
/// from-boot log to a permanent file that replays cleanly. (Record-from-boot
/// is the always-on ring buffer that ships now; a bounded "last N seconds"
/// window needs P4 keyframes -- see stone/engine/replay.rock.) Records while
/// saving a repro at frame 10, then replays the saved repro.
#[test]
fn save_repro_writes_a_replayable_from_boot_log() {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("live.replay");
    let repro = dir.path().join("saved.replay");
    let scratch = tempfile::tempdir().expect("scratch");

    let rec = Command::new(rock_bin)
        .args([
            "frame", "bitgun", "-n", "30", "--record", &log.to_string_lossy(),
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("STONE_SAVE_REPRO_AT_FRAME", "10")
        .env("STONE_SAVE_REPRO_PATH", repro.to_string_lossy().as_ref())
        .current_dir(workspace_root())
        .output()
        .expect("spawn record+save");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    let text = std::fs::read_to_string(&repro).expect("saved repro must exist");
    assert!(text.starts_with("STONE_REPLAY"), "repro must have the header\n{text}");
    let ticks = text.lines().filter(|l| l.starts_with("t ")).count();
    assert!(ticks >= 5, "repro must contain the from-boot ticks; got {ticks}");

    let rep = Command::new(rock_bin)
        .args([
            "frame", "bitgun", "-n", "1000", "--replay", &repro.to_string_lossy(),
            "--output", &scratch.path().to_string_lossy(),
        ])
        .current_dir(workspace_root())
        .output()
        .expect("spawn replay");
    assert!(
        rep.status.success(),
        "the saved repro must replay cleanly\nstderr:\n{}",
        String::from_utf8_lossy(&rep.stderr)
    );
}
