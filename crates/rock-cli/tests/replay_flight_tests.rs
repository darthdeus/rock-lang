//! P3.5 of docs/replay-scrub-plan.md: the flight-recorder log-format additions
//! (v4) -- a `build <id>` header field and `reload <tick>` markers -- plus the
//! determinism warnings replay emits when a log was recorded against code that
//! has since changed. The always-on `rock stone` recorder wiring is
//! author-verified in a real window; the log format + warnings are testable
//! headless via the STONE_BUILD_ID / STONE_RELOAD_AT_FRAME hooks.
//!
//! Heavy: spawns `rock frame` against a compiled game. Excluded from the
//! default nextest profile; runs under `-P full`.

use std::path::Path;
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

/// Record a log with the given env, returning (temp dir, log path).
fn record_with_env(game: &str, frames: u32, env: &[(&str, &str)]) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("flight.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().expect("scratch");
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", &frames.to_string(), "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(out.status.success(), "record failed: {}", String::from_utf8_lossy(&out.stderr));
    (dir, log_s)
}

/// Replay a log with the given env; return the process stderr.
fn replay_stderr(game: &str, log: &str, env: &[(&str, &str)]) -> String {
    let out = replay_raw(game, log, env);
    assert!(out.status.success(), "replay must exit 0\nstderr:\n{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn replay_raw(game: &str, log: &str, env: &[(&str, &str)]) -> std::process::Output {
    let scratch = tempfile::tempdir().expect("scratch");
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "15", "--replay", log,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("spawn replay")
}

fn with_header(text: &str, version: &str, build: Option<&str>) -> String {
    let mut lines: Vec<String> = text.lines().map(ToOwned::to_owned).collect();
    assert!(!lines.is_empty(), "replay has a header");
    lines[0] = format!("STONE_REPLAY {version}");
    lines.retain(|line| !line.starts_with("build "));
    if let Some(build) = build {
        let tick = lines
            .iter()
            .position(|line| line.starts_with("t "))
            .expect("replay has a tick");
        lines.insert(tick, format!("build {build}"));
    }
    lines.join("\n") + "\n"
}

#[test]
fn new_recordings_stamp_a_real_exact_artifact_identity() {
    let (_dir, log) = record_with_env("bitgun", 15, &[]);
    let text = std::fs::read_to_string(&log).expect("read log");
    assert!(text.starts_with("STONE_REPLAY 13\n"), "new recordings must use v13\n{text}");
    assert!(
        text.lines().any(|line| line == "source 62697467756e"),
        "the logical project source `bitgun` must be encoded in the header\n{text}"
    );
    let revision = text
        .lines()
        .find_map(|line| line.strip_prefix("revision "))
        .expect("new recordings must carry their source revision");
    assert!(
        !revision.is_empty() && revision.len() % 2 == 0
            && revision.chars().all(|c| c.is_ascii_hexdigit()),
        "the source revision must be nonempty encoded bytes: {revision:?}"
    );
    let build = text
        .lines()
        .find_map(|line| line.strip_prefix("build "))
        .expect("new recordings must carry a build id");
    assert!(!build.is_empty() && build != "dev", "the build id must be real and nondegenerate: {build:?}");
    assert!(build.chars().all(|c| c.is_ascii_hexdigit()), "the exact artifact id is hex: {build:?}");
}

#[test]
fn new_format_rejects_a_different_project_source_before_the_first_game_tick() {
    let (_dir, log) = record_with_env("bitgun", 15, &[]);
    let text = std::fs::read_to_string(&log).expect("read log");
    let changed = text.lines().map(|line| {
        if line.starts_with("source ") { "source 6d6167".to_string() } else { line.to_string() }
    }).collect::<Vec<_>>().join("\n");
    std::fs::write(&log, changed).expect("rewrite source");

    let out = replay_raw("bitgun", &log, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a replay from another project source must be rejected\n{stderr}");
    assert!(
        stderr.contains("source mismatch") && stderr.contains("before first game tick"),
        "the refusal must name the source boundary\n{stderr}"
    );
}

#[test]
fn new_format_rejects_a_missing_source_revision_before_the_first_game_tick() {
    let (_dir, log) = record_with_env("bitgun", 15, &[]);
    let text = std::fs::read_to_string(&log).expect("read log");
    let changed = text
        .lines()
        .filter(|line| !line.starts_with("revision "))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&log, changed).expect("remove revision");

    let out = replay_raw("bitgun", &log, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a v13 replay without revision must reject\n{stderr}");
    assert!(
        stderr.contains("requires a recorded source revision"),
        "the rejection must name the missing identity: {stderr}"
    );
}

#[test]
fn legacy_v11_without_source_remains_playable() {
    let (_dir, log) = record_with_env("bitgun", 15, &[]);
    let legacy = std::fs::read_to_string(&log)
        .expect("read log")
        .lines()
        .filter(|line| !line.starts_with("source "))
        .map(|line| {
            if line == "STONE_REPLAY 13" { "STONE_REPLAY 11" } else { line }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&log, legacy).expect("write v11 replay");
    replay_stderr("bitgun", &log, &[]);
}

#[test]
fn new_format_rejects_a_build_mismatch_before_the_first_game_tick() {
    let (_dir, log) = record_with_env("bitgun", 15, &[("STONE_BUILD_ID", "buildAAA")]);
    let text = std::fs::read_to_string(&log).expect("read log");
    let new_text = with_header(&text, "8", Some("1111111111111111"));
    std::fs::write(&log, new_text).expect("rewrite build id");
    let out = replay_raw("bitgun", &log, &[("STONE_BUILD_ID", "2222222222222222")]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a mismatched exact artifact must be rejected\n{stderr}");
    assert!(
        stderr.contains("build mismatch") && stderr.contains("before first game tick"),
        "the rejection must happen before any game tick and name the mismatch\n{stderr}"
    );
}

#[test]
fn new_format_rejects_a_missing_build_id() {
    let (_dir, log) = record_with_env("bitgun", 15, &[]);
    let text = std::fs::read_to_string(&log).expect("read log");
    std::fs::write(&log, with_header(&text, "8", None)).expect("remove build id");
    let out = replay_raw("bitgun", &log, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a new-format replay without a build id must be rejected\n{stderr}");
    assert!(stderr.contains("non-degenerate build id"), "the rejection must name the missing identity\n{stderr}");
}

#[test]
fn new_format_rejects_a_degenerate_build_id() {
    let (_dir, log) = record_with_env("bitgun", 15, &[]);
    let text = std::fs::read_to_string(&log).expect("read log");
    std::fs::write(&log, with_header(&text, "8", Some("dev"))).expect("degrade build id");
    let out = replay_raw("bitgun", &log, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a new-format dev build id must be rejected\n{stderr}");
    assert!(stderr.contains("non-degenerate build id"), "the rejection must name the degenerate identity\n{stderr}");
}

#[test]
fn new_format_rejects_a_reload_marker_as_unstable() {
    let (_dir, log) = record_with_env("bitgun", 15, &[("STONE_RELOAD_AT_FRAME", "5")]);
    let text = std::fs::read_to_string(&log).expect("read log");
    assert!(text.lines().any(|line| line.starts_with("reload ")), "fixture must contain a reload marker");
    std::fs::write(&log, with_header(&text, "8", Some("0123456789abcdef"))).expect("rewrite format");
    let out = replay_raw("bitgun", &log, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a reload-marked new-format replay must be rejected\n{stderr}");
    assert!(stderr.contains("reload marker") && stderr.contains("stable"), "the rejection must explain instability\n{stderr}");
}

#[test]
fn emitted_lua_has_no_shadow_effect_plan() {
    let dir = tempfile::tempdir().expect("compile dir");
    let lua = dir.path().join("bitgun.lua");
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["compile", "../bitgun", "--output", &lua.to_string_lossy()])
        .current_dir(workspace_root())
        .output()
        .expect("spawn Lua compile");
    assert!(out.status.success(), "Lua compile failed\n{}", String::from_utf8_lossy(&out.stderr));
    let emitted = std::fs::read_to_string(&lua).expect("read emitted Lua");
    assert!(!emitted.contains("__ROCK_SHADOW_EFFECT_PLAN"), "emitted Lua still publishes the removed effect plan");
    assert!(!emitted.contains("ShadowEffectPlan"), "emitted Lua still names the removed effect plan type");
}

/// The v9 header stamps the exact build id, and a hot-reload during recording
/// writes a `reload <tick>` marker.
#[test]
fn record_stamps_build_id_and_reload_marker() {
    let (_dir, log) = record_with_env(
        "bitgun", 15,
        &[ ("STONE_RELOAD_AT_FRAME", "5")],
    );
    let text = std::fs::read_to_string(&log).expect("read log");
    assert!(text.starts_with("STONE_REPLAY 13"), "must be a v13 log\n{text}");
    let build = text.lines().find_map(|l| l.strip_prefix("build ")).expect("build header");
    assert!(build.len() == 16 && build.chars().all(|c| c.is_ascii_hexdigit()), "header must carry a real build id\n{text}");
    assert!(
        text.lines().any(|l| l.starts_with("reload ")),
        "a mid-recording reload must write a marker\n{text}"
    );
}

/// Legacy replay versions retain warning-only reload compatibility.
#[test]
fn replay_warns_on_reload_marked_log() {
    let (_dir, log) = record_with_env("bitgun", 15, &[("STONE_RELOAD_AT_FRAME", "5")]);
    let text = std::fs::read_to_string(&log).expect("read log");
    std::fs::write(&log, with_header(&text, "7", Some("buildAAA"))).expect("rewrite legacy log");
    let stderr = replay_stderr("bitgun", &log, &[]);
    assert!(
        stderr.contains("hot-reload marker") && stderr.to_lowercase().contains("determinism"),
        "replay must warn on a reload-marked log\nstderr:\n{stderr}"
    );
}

/// The F9 "save repro" action promotes the always-on recording to a permanent
/// tick-stamped `repro-<tick>.replay` next to the source log, and that file is a
/// valid from-boot log that replays cleanly. Exercised headless via the
/// STONE_SAVE_REPRO_AUTO_AT_FRAME hook (F9 itself is author-verified).
#[test]
fn f9_save_repro_writes_a_replayable_tick_stamped_file() {
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("last.replay");
    let scratch = tempfile::tempdir().expect("scratch");

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", "bitgun", "-n", "15", "--record", &log.to_string_lossy(),
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("STONE_SAVE_REPRO_AUTO_AT_FRAME", "8")
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    // The derived dest is repro-<tick>.replay in the log's dir.
    let repro = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("repro-")))
        .expect("F9 must write a repro-<tick>.replay file");
    let text = std::fs::read_to_string(&repro).expect("read repro");
    assert!(text.starts_with("STONE_REPLAY"), "repro must be a valid log\n{text}");

    let rep = Command::new(env!("CARGO_BIN_EXE_rock"))
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

/// Legacy replay versions warn on a DIFFERENT build id; the same build id does
/// not. Exact v8 rejection is covered above.
#[test]
fn replay_warns_on_build_id_mismatch_only() {
    let (_dir, log) = record_with_env("bitgun", 15, &[]);
    let text = std::fs::read_to_string(&log).expect("read log");
    std::fs::write(&log, with_header(&text, "7", Some("buildAAA"))).expect("rewrite legacy log");

    let mismatched = replay_stderr("bitgun", &log, &[]);
    assert!(
        mismatched.contains("built against buildAAA") && mismatched.contains("current build"),
        "a build mismatch must warn\nstderr:\n{mismatched}"
    );

    let build = text
        .lines()
        .find_map(|line| line.strip_prefix("build "))
        .expect("recorded build id");
    std::fs::write(&log, with_header(&text, "7", Some(build))).expect("restore matching legacy log");
    let matched = replay_stderr("bitgun", &log, &[]);
    assert!(
        !matched.contains("built against"),
        "a matching build id must NOT warn\nstderr:\n{matched}"
    );
}
