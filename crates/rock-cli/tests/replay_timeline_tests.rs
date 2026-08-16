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
        // Default watchdog headroom for the -P full parallel-load flake (note 6);
        // applied before .envs so a per-test override still wins.
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
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
/// per-tick checkpoint so every `c <tick> <identity> <hash>` line names exactly
/// the StateSchema entries the engine hashes as simulation state, and collect
/// their logical dump-state names.
/// Consuming this (rather than duplicating the exclusion list in Rust) is the
/// P3.5 unification -- the StateImagePlan is the single
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
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(out.status.success(), "record failed: {}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(&log).expect("read log");
    let names: std::collections::HashSet<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("c "))
        .filter_map(|rest| rest.split(' ').nth(1))
        .map(|identity| identity.rsplit_once("::").map_or(identity, |(_, name)| name).to_string())
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
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
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

/// `rock frame` owns `STONE_HEADLESS`; ordinary headless playback must pass
/// through the same audio gate even when it is not currently seeking.
#[test]
fn headless_frame_runs_are_quiet() {
    let state = run_frame_env(
        "bitgun",
        5,
        &[],
        &[("STONE_AUDIO_PROBE", "1"), ("STONE_AUDIO_AUTHORITY_PROBE", "1")],
    );
    let mutes = state["__stone_diag"]["AUDIO_QUIET_MUTES"].as_i64().unwrap_or(0);
    assert!(mutes > 0, "a headless frame run must suppress its audio probe");
    assert_eq!(
        state["__stone_diag"]["AUDIO_STUB_ACTIVE"].as_i64(),
        Some(1),
        "a headless frame run must not open the native audio path"
    );
}

#[test]
fn headless_audio_quiet_does_not_suppress_mag_vfx() {
    let state = run_frame_env(
        "mag",
        2,
        &[],
        &[("MAG_SCREENSHOT", "1"), ("MAG_HEADLESS_VFX_PROBE", "1")],
    );
    assert!(
        state["__stone_diag"]["MAG_HEADLESS_VFX_SPLASHES"].as_i64().unwrap_or(0) > 0,
        "headless audio muting must not suppress Mag's decorative spell VFX"
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
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
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

#[test]
fn shift_right_uses_the_modifier_mapping_for_fast_forward() {
    let (_dir, log) = record_short_log("bitgun", 100);
    let jumped = run_frame_env(
        "bitgun",
        50,
        &["--replay", &log],
        &[
            ("STONE_REPLAY_WINDOW", "1"),
            ("STONE_HEADLESS", "1"),
            ("STONE_START_PAUSED", "1"),
            ("STONE_TIMELINE_KEY_AT", "3"),
            ("STONE_TIMELINE_KEY", "right"),
            ("STONE_TIMELINE_KEY_2", "lshift"),
        ],
    );
    assert_eq!(
        current_tick(&jumped),
        30,
        "Shift+Right must use the live modifier channel for the 30-tick fast-forward"
    );
}

#[test]
fn punctuation_tool_bindings_reach_the_time_scale_ladder() {
    let slower = run_frame_env(
        "bitgun",
        10,
        &[],
        &[
            ("STONE_TIME_SCALE", "1"),
            ("STONE_TIMELINE_KEY_AT", "3"),
            ("STONE_TIMELINE_KEY", "["),
        ],
    );
    assert_eq!(slower["stone"]["TIME_SCALE"], 0.5, "[ must select the slower rung");

    let faster = run_frame_env(
        "bitgun",
        10,
        &[],
        &[
            ("STONE_TIME_SCALE", "1"),
            ("STONE_TIMELINE_KEY_AT", "3"),
            ("STONE_TIMELINE_KEY", "]"),
        ],
    );
    assert_eq!(faster["stone"]["TIME_SCALE"], 2.0, "] must select the faster rung");

    let reset = run_frame_env(
        "bitgun",
        10,
        &[],
        &[
            ("STONE_TIME_SCALE", "0.25"),
            ("STONE_TIMELINE_KEY_AT", "3"),
            ("STONE_TIMELINE_KEY", "\\"),
        ],
    );
    assert_eq!(reset["stone"]["TIME_SCALE"], 1.0, "\\ must reset the scale");
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
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
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
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn replay");
    assert!(
        rep.status.success(),
        "the saved repro must replay cleanly\nstderr:\n{}",
        String::from_utf8_lossy(&rep.stderr)
    );
}

// --- tool input vs game input ----------------------------------------------

/// Record `game` and rewrite every tick's held-key field, so the log carries a
/// keypress the recording session never made. The `t` line is
/// `t <dt> <mx> <my> <wheel> <imgui-capture> <keys> <buttons>`; only field 6
/// moves, so the run is otherwise the one that was recorded.
fn record_log_with_keys(game: &str, frames: u32, keys_at: &[(usize, &str)]) -> tempfile::TempDir {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("keys.replay");
    let scratch = tempfile::tempdir().expect("scratch");
    let out = Command::new(rock_bin)
        .args([
            "frame", game, "-n", &frames.to_string(),
            "--record", &log.to_string_lossy(),
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(out.status.success(), "record failed: {}", String::from_utf8_lossy(&out.stderr));

    let text = std::fs::read_to_string(&log).expect("read log");
    let mut tick = 0usize;
    let mut planted = 0usize;
    let rewritten = text
        .lines()
        .map(|line| {
            if !line.starts_with("t ") {
                return line.to_string();
            }
            let mut fields: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(fields.len(), 8, "recorded tick shape changed: {line}");
            if let Some((_, key)) = keys_at.iter().find(|(at, _)| *at == tick) {
                fields[6] = key;
                planted += 1;
            }
            tick += 1;
            fields.join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(planted, keys_at.len(), "every planted key must land on a recorded tick");
    std::fs::write(&log, rewritten + "\n").expect("write log");
    dir
}

/// A recorded keypress drives the GAME, never the developer's tools. Engine
/// debug windows are session state: they must not be toggled by a log, because
/// a hidden seek skips the phase that toggles them, so a recorded toggle is a
/// divergence between ordinary playback and seek resimulation by construction.
///
/// Both sides of the gate: the recorded arm must not fire, and the live ImGui
/// arm must -- otherwise "the shortcut is dead" would pass as "the log is
/// ignored".
#[test]
fn recorded_keys_cannot_toggle_engine_debug_windows() {
    let dir = record_log_with_keys(
        "bitgun",
        40,
        &[
            (20, "o"),
            (21, "f8"),
            (22, "f6"),
            (23, "f10"),
            (24, "lctrl,f8"),
            (25, "lctrl,f6"),
            (26, "lctrl,f10"),
        ],
    );
    let log = dir.path().join("keys.replay");
    let log_s = log.to_string_lossy().into_owned();

    let replayed = run_frame_env(
        "bitgun", 40, &["--replay", &log_s], &[("STONE_TOOL_KEY_PROBE", "1")],
    );
    let engine = &replayed["stone"]["ENGINE"];
    // Every field these keys reach, not only the windows: F6 and F8 are each
    // bound twice (SFX browser + visibility, pipeline inspector + profiler),
    // and F10 has its graphics-stats window consumer.
    let recorded_consumers = [
        ("show_graphics_stats_window", "o/F10"),
        ("show_pipeline_inspector", "f8"),
        ("show_sfx_browser", "f6"),
        ("debug_draw_visibility", "f6"),
        ("profiler_enabled", "f8"),
    ];
    assert_eq!(recorded_consumers.len(), 5, "recorded consumer census must be nonzero");
    for (field, key) in recorded_consumers {
        assert_eq!(
            engine[field].as_bool(), Some(false),
            "a recorded '{key}' must not toggle {field}: {engine}"
        );
    }

    // The public typed authority must retain punctuation names. There is no
    // manually repeated enum census: the exhaustive ToolKey match is the
    // static source of truth.
    let diag = &replayed["__stone_diag"];
    assert_eq!(diag["TOOL_KEY_BACKSLASH_NAME"], "\\");
    assert!(diag.get("TOOL_KEYS_DECLARED").is_none(), "the false census must be gone: {diag}");
    assert!(diag.get("TOOL_KEYS_PROBED").is_none(), "the false census must be gone: {diag}");
    assert!(diag.get("TOOL_KEYS_UNMAPPED").is_none(), "the false census must be gone: {diag}");

    // The live arm, through the same ImGui IO the timeline shortcuts use.
    let injected = run_frame_env(
        "bitgun", 20, &[],
        &[("STONE_TIMELINE_KEY_AT", "5"), ("STONE_TIMELINE_KEY", "o")],
    );
    assert_eq!(
        injected["stone"]["ENGINE"]["show_graphics_stats_window"].as_bool(), Some(true),
        "a live 'o' must still open the graphics stats window: {}",
        injected["stone"]["ENGINE"]
    );

    let bare_function_consumers = [
        ("f6", vec!["show_sfx_browser", "debug_draw_visibility"]),
        ("f8", vec!["show_pipeline_inspector", "profiler_enabled"]),
        ("f10", vec!["show_graphics_stats_window"]),
    ];
    let bare_consumer_count: usize = bare_function_consumers.iter().map(|(_, fields)| fields.len()).sum();
    assert_eq!(bare_consumer_count, 5, "bare F-key consumer census must be nonzero");
    for (key, fields) in bare_function_consumers {
        let bare = run_frame_env(
            "bitgun", 20, &[],
            &[("STONE_TIMELINE_KEY_AT", "5"), ("STONE_TIMELINE_KEY", key)],
        );
        for field in fields {
            assert_eq!(
                bare["stone"]["ENGINE"][field].as_bool(), Some(false),
                "a live bare '{key}' must remain inert for {field}: {}",
                bare["stone"]["ENGINE"]
            );
        }
    }

    let live_chords = [
        ("f6", vec!["show_sfx_browser", "debug_draw_visibility"]),
        ("f8", vec!["show_pipeline_inspector", "profiler_enabled"]),
        ("f10", vec!["show_graphics_stats_window"]),
    ];
    let live_consumer_count: usize = live_chords.iter().map(|(_, fields)| fields.len()).sum();
    assert_eq!(live_consumer_count, 5, "Ctrl+F consumer census must be nonzero");
    for (key, fields) in live_chords {
        let chord = run_frame_env(
            "bitgun",
            20,
            &[],
            &[
                ("STONE_TIMELINE_KEY_AT", "5"),
                ("STONE_TIMELINE_KEY", key),
                ("STONE_TIMELINE_KEY_2", "lctrl"),
            ],
        );
        for field in fields {
            assert_eq!(
                chord["stone"]["ENGINE"][field].as_bool(), Some(true),
                "a live Ctrl+'{key}' must reach {field}: {}",
                chord["stone"]["ENGINE"]
            );
        }
    }

    let dynamic = run_frame_stderr(
        "bitgun", 20, &[],
        &[("STONE_TIMELINE_KEY_AT", "5"), ("STONE_TIMELINE_KEY", "not-a-key")],
    );
    assert!(
        dynamic.contains("tool key 'not-a-key' has no ImGui mapping"),
        "dynamic tool input must retain its per-key unmapped diagnostic: {dynamic}"
    );
}

// --- recorded environment ---------------------------------------------------

/// `rock frame` with extra env, returning stderr (where the logger writes).
fn run_frame_stderr(game: &str, frames: u32, extra: &[&str], env: &[(&str, &str)]) -> String {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let scratch = tempfile::tempdir().expect("scratch dir");
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let frames_s = frames.to_string();
    let mut args: Vec<&str> = vec!["frame", game, "-n", &frames_s, "--output", &scratch_path];
    args.extend_from_slice(extra);
    let out = Command::new(rock_bin)
        .args(&args)
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("failed to spawn rock frame");
    assert!(
        out.status.success(),
        "rock frame {game} {extra:?} exited {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A replay is only deterministic with respect to the environment it was
/// recorded in: env vars gate real simulation (`STONE_QUIET` reached a game's
/// `@update` system), and nothing in the log recorded them. The header carries
/// the declared set and a mismatch is named, the same way a build change is.
///
/// Both sides: a differing value must warn AND name the variable, and a
/// matching one must stay silent -- a warning that always fires is not a gate.
#[test]
fn a_replay_names_the_environment_it_disagrees_with() {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("env.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().expect("scratch");
    let rec = Command::new(rock_bin)
        .args([
            "frame", "bitgun", "-n", "10", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .env("STONE_QUIET", "1")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    let text = std::fs::read_to_string(&log).expect("read log");
    let env_lines: Vec<&str> = text.lines().filter(|l| l.starts_with("env ")).collect();
    assert!(
        !env_lines.is_empty(),
        "the header must record the declared environment"
    );
    let quiet = env_lines
        .iter()
        .find(|l| l.starts_with("env STONE_QUIET "))
        .expect("STONE_QUIET must be recorded");
    assert_eq!(
        *quiet, "env STONE_QUIET 1 31",
        "a present value is recorded as a presence bit plus hex"
    );
    // The effective quiet is DERIVED from these too, and it is not readable at
    // header time (replay_init precedes the @early_init that computes it), so
    // the sensor has to name the derivation's inputs or a windowed record and a
    // headless replay would agree while running the gate in opposite states.
    // Structural, not behavioral: `rock frame` always sets STONE_HEADLESS, so a
    // headless test cannot produce the windowed arm.
    for derived in ["STONE_HEADLESS", "ROCK_RUN_SIM"] {
        assert!(
            env_lines.iter().any(|l| l.starts_with(&format!("env {derived} "))),
            "the recorded environment must name {derived}, which the quiet gate derives from: {env_lines:?}"
        );
    }

    let mismatch = run_frame_stderr(
        "bitgun", 10, &["--replay", &log_s], &[("STONE_QUIET", "0")],
    );
    assert!(
        mismatch.contains("STONE_QUIET"),
        "a differing environment must be named: stderr:\n{mismatch}"
    );

    let matching = run_frame_stderr(
        "bitgun", 10, &["--replay", &log_s], &[("STONE_QUIET", "1")],
    );
    assert!(
        !matching.contains("recorded environment"),
        "a matching environment must not warn: stderr:\n{matching}"
    );
}

#[test]
fn mag_screenshot_environment_reaches_header_and_mismatch_warning() {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("mag-screenshot.replay");
    let scratch = tempfile::tempdir().expect("scratch");
    let out = Command::new(rock_bin)
        .args([
            "frame", "mag", "-n", "3", "--record", &log.to_string_lossy(),
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .env("MAG_SCREENSHOT", "1")
        .current_dir(workspace_root())
        .output()
        .expect("spawn Mag record");
    assert!(out.status.success(), "record failed: {}", String::from_utf8_lossy(&out.stderr));

    let text = std::fs::read_to_string(&log).expect("read log");
    assert!(
        text.lines().any(|line| line == "env MAG_SCREENSHOT 1 31"),
        "MAG_SCREENSHOT must be recorded as a game environment key:\n{text}"
    );

    let mismatch = run_frame_stderr(
        "mag", 3, &["--replay", &log.to_string_lossy()], &[("MAG_SCREENSHOT", "0")],
    );
    assert!(
        mismatch.contains("recorded environment differs: MAG_SCREENSHOT"),
        "a changed screenshot mode must be named by replay env checking:\n{mismatch}"
    );
}

/// The v5 environment encoding has to survive arbitrary values, because an env
/// var is arbitrary bytes. Each of these breaks a `K=V` pairs-joined-by-spaces
/// encoding: a value with a space or an `=` is silently truncated or
/// mis-split, and unset / empty / a literal `-` are three different runs that a
/// `-` sentinel collapses into one.
///
/// `MAG_SCENE` is the vehicle because it is a GAME-declared key
/// (`GameConfig.replay_env_keys`), so the same run proves the game's
/// contribution reaches the header at all.
#[test]
fn the_recorded_environment_survives_awkward_values() {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let cases: [(&str, Option<&str>, &str); 5] = [
        ("unset", None, "env MAG_SCENE 0 "),
        ("empty", Some(""), "env MAG_SCENE 1 "),
        ("a literal dash", Some("-"), "env MAG_SCENE 1 2d"),
        ("spaces", Some("two words"), "env MAG_SCENE 1 74776f20776f726473"),
        ("an equals sign", Some("k=v"), "env MAG_SCENE 1 6b3d76"),
    ];
    for (label, value, expected) in cases {
        let dir = tempfile::tempdir().expect("dir");
        let log = dir.path().join("awkward.replay");
        let scratch = tempfile::tempdir().expect("scratch");
        let mut cmd = Command::new(rock_bin);
        cmd.args([
            "frame", "mag", "-n", "3", "--record", &log.to_string_lossy(),
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root());
        match value {
            Some(v) => { cmd.env("MAG_SCENE", v); }
            None => { cmd.env_remove("MAG_SCENE"); }
        }
        let out = cmd.output().expect("spawn record");
        assert!(out.status.success(), "{label}: record failed: {}", String::from_utf8_lossy(&out.stderr));

        let text = std::fs::read_to_string(&log).expect("read log");
        let line = text
            .lines()
            .find(|l| l.starts_with("env MAG_SCENE "))
            .unwrap_or_else(|| panic!("{label}: the game-declared key must reach the header:\n{text}"));
        assert_eq!(line, expected, "{label}: value must round-trip exactly");

        // And the decode side agrees: replaying under the SAME value is silent.
        let same = run_frame_stderr_env(
            "mag", 3, &["--replay", &log.to_string_lossy()], value,
        );
        assert!(
            !same.contains("MAG_SCENE"),
            "{label}: an identical environment must not warn: stderr:\n{same}"
        );
        // Changing it to a value the encoding could confuse with this one must warn.
        let other = run_frame_stderr_env("mag", 3, &["--replay", &log.to_string_lossy()], Some("-"));
        if value != Some("-") {
            assert!(
                other.contains("MAG_SCENE"),
                "{label}: a differing environment must be named: stderr:\n{other}"
            );
        }
    }
}

/// `run_frame_stderr`, but MAG_SCENE is set to an exact value or removed --
/// `envs()` cannot express "unset", which is one of the cases under test.
fn run_frame_stderr_env(
    game: &str,
    frames: u32,
    extra: &[&str],
    mag_scene: Option<&str>,
) -> String {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let scratch = tempfile::tempdir().expect("scratch dir");
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let frames_s = frames.to_string();
    let mut args: Vec<&str> = vec!["frame", game, "-n", &frames_s, "--output", &scratch_path];
    args.extend_from_slice(extra);
    let mut cmd = Command::new(rock_bin);
    cmd.args(&args)
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root());
    match mag_scene {
        Some(v) => { cmd.env("MAG_SCENE", v); }
        None => { cmd.env_remove("MAG_SCENE"); }
    }
    let out = cmd.output().expect("failed to spawn rock frame");
    assert!(
        out.status.success(),
        "rock frame {game} {extra:?} exited {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}
