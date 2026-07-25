use crate::*;
use std::process::{Child, Command, Stdio};

/// macOS-specific: Spawn a detached monitor process to ensure child cleanup
///
/// This function implements orphan process prevention for macOS, which doesn't have
/// Linux's PDEATHSIG mechanism. It uses the same battle-tested approach from the old
/// `rock love` CLI implementation.
///
/// **Why this approach:**
/// - macOS has no equivalent to Linux's PR_SET_PDEATHSIG
/// - kqueue with NOTE_EXIT is macOS-specific and wouldn't work cross-platform
/// - This shell script + double-fork pattern is simple, proven, and works
///
/// **How it works:**
/// 1. Double-fork creates a fully detached monitor process (reparented to init/PID 1)
/// 2. Monitor polls `kill -0 parent_pid` every 100ms to check if parent is alive
/// 3. When parent dies (even from SIGKILL), monitor kills the child process
/// 4. Uses SIGTERM first for graceful shutdown, then SIGKILL to guarantee cleanup
///
/// **Trade-offs:**
/// - Polling introduces minimal overhead (~0.1% CPU)
/// - Simple and reliable across all Unix-like systems
/// - Proven in production use (copied from original love.rs implementation)
///
/// **Alternatives considered:**
/// - kqueue with NOTE_EXIT: More efficient but macOS-only, would need separate Linux impl
/// - Accepting orphans: Not acceptable, leads to accumulation of zombie processes
///
/// This is the same implementation used successfully in `rock love` for years.
#[cfg(target_os = "macos")]
fn spawn_monitor_process(parent_pid: u32, child_pid: u32) {
    use std::process::Command;

    // Create a shell script that will monitor and kill the child when parent dies
    let script = format!(
        r#"#!/bin/bash
# Monitor parent PID {} and kill child PID {} when parent dies
while kill -0 {} 2>/dev/null; do
    sleep 0.1
done
# Parent died, kill the child
kill -TERM {} 2>/dev/null
sleep 0.1
kill -KILL {} 2>/dev/null
kill -KILL -{} 2>/dev/null  # Kill process group
"#,
        parent_pid, child_pid, parent_pid, child_pid, child_pid, child_pid
    );

    // Use nohup and disown to truly detach the monitoring process
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
       .arg(script)
       .stdin(std::process::Stdio::null())
       .stdout(std::process::Stdio::null())
       .stderr(std::process::Stdio::null());

    // Double fork to create an orphan process
    unsafe {
        let pid = libc::fork();
        if pid == 0 {
            // Child process - become session leader
            libc::setsid();

            // Second fork to ensure we can't acquire a terminal
            let pid2 = libc::fork();
            if pid2 == 0 {
                // Grandchild - run the monitoring script. Surface spawn
                // failure to stderr: silently dropping it (`let _ =`)
                // disables orphan prevention with no diagnostic, so a
                // shell that can't exec leaves game children alive after
                // `rock stone` exits (silent-errors audit, 2026-05-02).
                if let Err(e) = cmd.spawn() {
                    log::error!(target: "STONE",
                        "failed to spawn macOS orphan-prevention monitor (parent {parent_pid}, child {child_pid}): {e}"
                    );
                }
                std::process::exit(0);
            } else {
                // First child exits immediately
                std::process::exit(0);
            }
        } else if pid > 0 {
            // Parent waits for first child to exit
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
        }
    }
}

/// Run the Stone game development environment with hot reload support
///
/// This command combines:
/// - Watch mode for game/stdlib/stone/compiler changes
/// - Automatic libstone rebuilds
/// - Game process management with hot reload
/// - Automatic restart when libstone or compiler changes

/// Replace this process with the freshly rebuilt `rock` binary, preserving the
/// original `stone` arguments. The new process re-runs `do_stone_watch` from
/// scratch -- `SkullStone::init` re-emits main.lua with the NEW codegen -- so a
/// compiler edit takes effect live without a manual restart. The game child is
/// killed first: `exec` keeps our PID, so its PDEATHSIG never fires and it would
/// otherwise linger as a second luajit beside the one the new process spawns. On
/// Unix `exec` only returns on failure (logged; caller falls back to the old
/// binary). Non-Unix has no `exec`, so spawn a replacement and exit.
fn reexec_with_new_binary(game_process: &mut Option<Child>) {
    if let Some(mut p) = game_process.take() {
        let _ = kill_game_process(&mut p);
    }
    info!("Compiler changed -- re-exec'ing rock with the new binary");
    if let Err(e) = crate::watch::reexec_current_binary() {
        error!("stone: {e}; staying on the old compiler binary (Ctrl-C + rerun to pick up the change)");
    }
}

pub fn do_stone_watch(
    path: &str,
    quiet: bool,
    app_args: &[String],
    backend: crate::Backend,
    no_record: bool,
) -> Result<()> {
    rock_host::project::ensure_workspace_root()?;

    if !quiet {
        info!("Starting Stone development environment");
        info!("Watching: {}, stdlib/, stone/, compiler", path);
        info!("Game will hot reload on code changes");
        info!("Process will restart on stone-rs/compiler changes");
    }

    // Build engine cdylibs (stone-rs + optional stone-video per game's
    // [engine] needs) and the rock compiler in a single cargo invocation.
    // Two separate `cargo build` calls each pay cargo's graph-walk cost
    // and serialize on the target lock; one combined call halves the
    // perceived startup pause and streams progress to the user.
    build_all_for_game(path)?;

    let output_path = rock_host::project::default_main_lua_path();

    // The backend-specific incremental game compiler (skull: SkullStone two-file
    // split). It is the ONLY backend-specific piece of this loop; everything else
    // (builds, process, restart) is shared. The runtime owns the changed-path
    // accumulator the watcher feeds it.
    let mut compiler =
        crate::game_runtime::game_compiler_for(backend, path, &output_path);
    let mut changed_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    // Do initial compilation.
    // If it fails, we'll continue watching and start the game when compilation succeeds
    let initial_compilation_success = match compiler.compile(&changed_paths, false) {
        Ok(_) => {
            clear_error_file();
            true
        }
        Err(e) => {
            let error_message = format!("Initial compilation failed: {}", e);
            log::error!(target: "STONE", "{}", error_message);
            log::info!(target: "STONE", "watching for changes; game will start when compilation succeeds");
            write_error_file(&error_message);
            false
        }
    };

    // Always-on flight recorder (P3.5): record every `rock stone` session from
    // boot to a gitignored <game>/replays/last.replay so any bug is already a
    // file. Computed once (build id = the initial compile); a restart re-spawns
    // with the same config.
    let flight = flight_recorder_for(path, &output_path, no_record);
    if let Some(f) = &flight {
        info!("Flight recorder: {} (build {})", f.record_path, f.build_id);
    }

    // Start the game process only if initial compilation succeeded
    let mut game_process = if initial_compilation_success {
        Some(start_game_process(app_args, flight.as_ref(), None)?)
    } else {
        None
    };

    // Set up file watcher for game files, stdlib, stone (engine Rock code),
    // stone-rs / stone-video (engine Rust crates), the rock compiler, and
    // tmp/ for the restart-trigger file. FileWatcher silently skips
    // nonexistent paths, so listing the optional crates is safe on machines
    // where they aren't checked out.
    let mut watch_paths = crate::watch::project_engine_watch_paths(path);
    watch_paths.extend([
        PathBuf::from("stone-rs/src"),
        PathBuf::from("stone-rs/Cargo.toml"),
        PathBuf::from("stone-rs/build.rs"),
        PathBuf::from("stone-video/src"),
        PathBuf::from("stone-video/Cargo.toml"),
        PathBuf::from("stone-video/build.rs"),
        PathBuf::from("tmp"),  // Watch tmp/ for restart.txt trigger
    ]);
    watch_paths.extend(crate::watch::compiler_watch_paths());

    // Create channel for file change events
    let (change_tx, change_rx) = channel::<FileChangeEvent>();

    // Canonical rock-source watcher + stone-only opt-ins: manual
    // restart via `touch tmp/restart.txt`, and engine-rebuild
    // extensions (stone-rs / C / stick.toml changes rebuild libstone).
    let _watcher = FileWatcher::default_for_rock_sources()
        .with_restart_trigger(true)
        .with_extensions(vec!["rock".to_string(), "rs".to_string(), "c".to_string(), "h".to_string(), "toml".to_string()])
        .watch(watch_paths, change_tx)?;

    info!("Watching for changes...");

    // Track what needs rebuilding
    let mut needs_libstone_rebuild = false;
    let mut needs_compiler_rebuild = false;

    for event in change_rx {
        // Check for manual restart trigger first
        let mut manual_restart_requested = false;
        if event.is_restart_trigger {
            info!("Manual restart triggered (tmp/restart.txt detected)");
            manual_restart_requested = true;
        }

        // Categorize the changed files (skip tmp/ files that aren't restart triggers)
        let mut changed_libstone = Vec::new();
        let mut changed_compiler = Vec::new();
        let mut changed_game = Vec::new();

        for path in &event.paths {
            // Normalize separators once so the substring checks below work
            // identically on Windows (`stone-rs\src\foo.rs`) and Unix
            // (`stone-rs/src/foo.rs`). All match strings use `/`.
            let path_str = path.to_string_lossy().replace('\\', "/");

            // Skip tmp/ directory files (they're only for restart trigger which we already handled)
            if path_str.contains("tmp/") {
                continue;
            }

            let filename = path.file_name().map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();

            // Match stone-rs/ and stone-video/ BEFORE the generic "Cargo." check
            // below: `stone-rs/Cargo.toml` would otherwise be routed to the
            // compiler-rebuild branch and never trigger a libstone rebuild.
            if path_str.contains("stone-rs/") || path_str.contains("stone-video/") {
                needs_libstone_rebuild = true;
                changed_libstone.push(filename);
            } else if crate::watch::is_compiler_source(path) {
                needs_compiler_rebuild = true;
                changed_compiler.push(filename);
            } else if path_str.ends_with(".rock") {
                changed_paths.insert(path.clone());
                changed_game.push(filename);
            }
        }

        // Print change summary only for libstone/compiler changes (game changes
        // are already visible in the "Recompiling N module (foo.rock)..." line)
        if !changed_libstone.is_empty() || !changed_compiler.is_empty() {
            let mut parts = Vec::new();
            if !changed_libstone.is_empty() {
                parts.push(format!("libstone: {}", changed_libstone.join(", ")));
            }
            if !changed_compiler.is_empty() {
                parts.push(format!("compiler: {}", changed_compiler.join(", ")));
            }
            info!("Changed: {}", parts.join(" | "));
        }

        // Rebuild engine + compiler in dependency order. On failure we log
        // and proceed instead of aborting the iteration: the previous
        // successful build of each is still on disk, so the running game
        // can keep iterating against the old dylib / old compiler binary
        // until the user fixes the error. Without this, a typo in
        // stone-rs/foo.rs would also block hot-reload of unrelated game
        // edits saved in the same tick.
        //
        // `should_restart` only accumulates from rebuilds that actually
        // succeeded (plus manual restart) — we don't restart the game
        // process to load a dylib that didn't get produced.
        let mut should_restart = manual_restart_requested;
        let mut compiler_rebuilt = false;

        if needs_libstone_rebuild {
            info!("Rebuilding stone-rs...");
            match rock_host::engine_build::build_engine_libs(path) {
                Ok(_) => {
                    info!("stone-rs rebuilt successfully");
                    should_restart = true;
                }
                Err(e) => {
                    error!(
                        "Error rebuilding stone-rs: {} (game keeps running with previous libstone)",
                        e
                    );
                }
            }
            needs_libstone_rebuild = false;
        }

        if needs_compiler_rebuild {
            info!("Rebuilding compiler...");
            match crate::watch::build_compiler() {
                Ok(_) => {
                    info!("Compiler rebuilt successfully");
                    should_restart = true;
                    compiler_rebuilt = true;
                    // The next compile passes force_full=true so the backend
                    // drops its caches and re-analyzes with the new compiler.
                }
                Err(e) => {
                    error!(
                        "Error rebuilding compiler: {} (using previous compiler binary)",
                        e
                    );
                }
            }
            needs_compiler_rebuild = false;
        }

        // A successful compiler rebuild put a NEW `rock` binary on disk, but THIS
        // process is still the old one -- it would keep emitting with the old
        // in-memory codegen. Re-exec the freshly built binary so the new compiler
        // actually takes effect: the re-spawned process's `init` regenerates
        // main.lua from the new codegen (no manual restart, no stale-overlay
        // desync). Only returns if the exec itself failed, in which case we log
        // and fall through to a best-effort rebuild+restart on the old binary.
        if compiler_rebuilt {
            reexec_with_new_binary(&mut game_process);
        }

        // Recompile game if user changes are pending or the compiler was
        // just rebuilt (force_full re-analyzes with the new compiler).
        let has_game_changes = !changed_paths.is_empty();
        let mut compilation_succeeded = false;
        if has_game_changes || compiler_rebuilt {
            match compiler.compile(&changed_paths, compiler_rebuilt) {
                Ok(outcome) => {
                    // Consumed successfully; start the next edit clean. On error
                    // we keep them so the next tick retries the same changes.
                    changed_paths.clear();
                    compilation_succeeded = true;
                    clear_error_file();
                    if !outcome.restart_types.is_empty() {
                        info!(
                            "type shape changed ({}) -- live state cannot be migrated, restarting game process",
                            outcome.restart_types.join(", ")
                        );
                        should_restart = true;
                    }
                }
                Err(e) => {
                    let error_message = format!("Error compiling game: {}", e);
                    log::error!(target: "STONE", "{}", error_message);
                    write_error_file(&error_message);
                    continue;
                }
            }
        }

        // If game hasn't been started yet and compilation just succeeded, start it now
        if game_process.is_none() && compilation_succeeded {
            // A crashed/never-started process loads `main.lua` only -- fold any
            // overlay / deferred shape write in first, same as a restart.
            if let Err(e) = compiler.canonicalize_for_restart() {
                error!("fold before start failed: {} (start may load a stale main.lua)", e);
            }
            info!("First successful compilation! Starting game...");
            game_process = Some(start_game_process(app_args, flight.as_ref(), None)?);
            info!("Game started successfully");
        }
        // Restart the game process if a rebuild actually produced new
        // artifacts (or the user manually triggered) and the game is up.
        else if should_restart {
            if let Some(ref mut process) = game_process {
                // Kill FIRST: a shape-changed compile deferred its main.lua
                // write precisely so the old process never hot-reloads the
                // new-shape program -- the canonicalize below is what writes
                // it. (For plain restarts the fold is idempotent; a dead
                // process cannot race the write either way.) A restart
                // re-spawns luajit, which loads `main.lua` only -- it does NOT
                // dual-load any accumulated overlay, so the fold must run
                // before the new process starts.
                info!("Restarting game process...");
                kill_game_process(process)?;
                if let Err(e) = compiler.canonicalize_for_restart() {
                    error!("fold before restart failed: {} (restart may load a stale main.lua)", e);
                }
                // In-session load: if the F7 picker requested a replay, relaunch
                // into windowed playback of it (read-once) instead of the live game.
                let replay_request = consume_replay_request();
                if let Some(ref p) = replay_request {
                    match p.loop_ab {
                        Some((a, b)) => info!(
                            "Loading replay {} with A-B loop [{a},{b}] (shape-edit restart); flight recorder off",
                            p.path
                        ),
                        None => info!(
                            "Loading replay {} (F7 picker); flight recorder off for this run",
                            p.path
                        ),
                    }
                }
                game_process =
                    Some(start_game_process(app_args, flight.as_ref(), replay_request.as_ref())?);
                info!("Game process restarted");
            }
        } else if has_game_changes && game_process.is_some() {
            debug!("Game will hot reload automatically");
        }
    }

    Ok(())
}


/// Combined startup build: stone-rs + (optional) stone-video + rock compiler
/// in a single `cargo build` invocation. Streams cargo output directly so
/// the user sees "Compiling …" progress instead of a silent pause.
///
/// Used at `rock stone` startup. The watch loop's incremental rebuilds keep
/// using the per-target `build_engine_libs` / `build_compiler` since they
/// fire selectively based on which files changed.
fn build_all_for_game(game_path: &str) -> Result<()> {
    if rock_host::project::is_release_layout() {
        return Ok(());
    }

    let needs_video = rock_host::engine_build::game_requests_video(game_path)?
        && std::path::Path::new("stone-video").exists();

    // Two invocations are required. A single `cargo build --lib --bin rock`
    // builds only the `rock` bin (the `--bin` filter narrows targets across
    // the whole selection), so stone-rs's cdylib never gets compiled and the
    // subsequent stage_cdylib trips on a missing libstone_rs.dylib in fresh
    // checkouts / worktrees. Build the engine libs (no --bin filter) first,
    // then the compiler bin.
    //
    // Neither call passes `-p`: resolver-2 unifies features over the SELECTED
    // packages only, so a narrower selection is a distinct feature resolution
    // and rebuilds every shared dep plus the whole compiler stack under a
    // second -C metadata. Keeping both on the default-member selection matches
    // the justfile's `cargo run --bin rock` and keeps one set of artifacts.
    info!("Building engine + compiler (cargo)...");

    rock_host::engine_build::run_cargo(&["build", "--lib"], "Failed to build engine libs")?;
    if needs_video {
        rock_host::engine_build::run_cargo(
            &["build", "-p", "stone-video"],
            "Failed to build stone-video",
        )?;
    }

    rock_host::engine_build::run_cargo(
        &["build", "--bin", "rock"],
        "Failed to build rock compiler",
    )?;

    rock_host::engine_build::stage_cdylib("stone_rs", "stone")?;
    if needs_video {
        rock_host::engine_build::stage_cdylib("stone_video", "stone_video")?;
    }

    Ok(())
}

/// Flight recorder config: always-on record-from-boot for `rock stone`
/// (docs/replay-scrub-plan.md P3.5). The recording streams to a gitignored
/// `<game>/replays/last.replay`; a crash / the F9 "save repro" key promote it
/// to a permanent file. Checkpoints default OFF for the flight recorder --
/// playback doesn't need them and each one serializes the whole state; the
/// K=1..60 desync cadence stays a `rock frame --checkpoint-interval` setting.
struct FlightRecorder {
    record_path: String,
    build_id: String,
    checkpoint_interval: usize,
}

/// Content hash of the compiled game (FNV-1a-64, hex) -- the replay-header build
/// id. Changes whenever codegen output changes, so a log replayed against a
/// different build warns. Falls back to "dev" if the file can't be read.
fn build_id_for_lua(lua_file: &std::path::Path) -> String {
    match std::fs::read(lua_file) {
        Ok(bytes) => {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in &bytes {
                h ^= *b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            format!("{h:016x}")
        }
        Err(_) => "dev".to_string(),
    }
}

/// Ensure `<game>/replays/` exists and is gitignored (a `.gitignore` of `*`
/// keeps flight logs -- and itself -- out of version control). Returns the dir.
fn ensure_flight_dir(game_path: &str) -> Result<std::path::PathBuf> {
    let dir = std::path::Path::new(game_path).join("replays");
    std::fs::create_dir_all(&dir)?;
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        // `*` ignores every entry in this dir including .gitignore itself, so
        // flight recordings never show up as untracked/committed.
        std::fs::write(&gitignore, "*\n")?;
    }
    Ok(dir)
}

/// Build the flight-recorder config for `rock stone`, or None when disabled.
/// Opt-out via `--no-record` or STONE_NO_FLIGHT_RECORDER=1; a user-set
/// STONE_RECORD in the environment wins (explicit recording elsewhere is
/// respected).
fn flight_recorder_for(
    game_path: &str,
    lua_file: &std::path::Path,
    no_record: bool,
) -> Option<FlightRecorder> {
    if no_record {
        return None;
    }
    if std::env::var("STONE_NO_FLIGHT_RECORDER").as_deref() == Ok("1") {
        return None;
    }
    if std::env::var("STONE_RECORD").is_ok() {
        return None;
    }
    let dir = match ensure_flight_dir(game_path) {
        Ok(d) => d,
        Err(e) => {
            log::warn!(target: "STONE", "flight recorder disabled: could not create replays dir: {e}");
            return None;
        }
    };
    Some(FlightRecorder {
        record_path: dir.join("last.replay").to_string_lossy().into_owned(),
        build_id: build_id_for_lua(lua_file),
        checkpoint_interval: 0,
    })
}

/// Apply the flight-recorder env to a game-process command (both spawn branches).
fn apply_flight_env(cmd: &mut Command, flight: Option<&FlightRecorder>) {
    if let Some(f) = flight {
        cmd.env("STONE_RECORD", &f.record_path)
            .env("STONE_BUILD_ID", &f.build_id)
            .env("STONE_CHECKPOINT_INTERVAL", f.checkpoint_interval.to_string());
    }
}

/// Env for a game-process spawn: a windowed replay of `replay` (in-session load
/// from the F7 picker) OR the always-on flight recorder. Replaying and recording
/// are mutually exclusive, so a load request suppresses the recorder for that
/// run. Build-id (reused from the flight config -- same compiled lua) lets
/// replay_init warn on a mismatch.
/// A consumed in-session load request: a replay path, plus optional A-B loop
/// markers (P6 shape-edit survival -- a full reload during a loop restarts the
/// process and resumes the loop at A).
#[derive(Debug, PartialEq)]
struct ReplayRequest {
    path: String,
    loop_ab: Option<(i64, i64)>,
}

/// Parse a request line: `path` or `path\tloop\t<A>\t<B>`. A malformed loop
/// suffix degrades to a plain replay (path only) rather than being dropped.
fn parse_replay_request(raw: &str) -> ReplayRequest {
    let mut parts = raw.split('\t');
    let path = parts.next().unwrap_or("").to_string();
    let loop_ab = match (parts.next(), parts.next(), parts.next()) {
        (Some("loop"), Some(a), Some(b)) => match (a.parse::<i64>(), b.parse::<i64>()) {
            (Ok(a), Ok(b)) => Some((a, b)),
            _ => None,
        },
        _ => None,
    };
    ReplayRequest { path, loop_ab }
}

fn apply_run_env(cmd: &mut Command, flight: Option<&FlightRecorder>, replay: Option<&ReplayRequest>) {
    match replay {
        Some(req) => {
            cmd.env("STONE_REPLAY", &req.path).env("STONE_REPLAY_WINDOW", "1");
            if let Some((a, b)) = req.loop_ab {
                cmd.env("STONE_LOOP_A", a.to_string()).env("STONE_LOOP_B", b.to_string());
            }
            if let Some(f) = flight {
                cmd.env("STONE_BUILD_ID", &f.build_id);
            }
        }
        None => apply_flight_env(cmd, flight),
    }
}

/// Consume a pending in-session load request (the F7 picker's Play button, or a
/// P6 loop shape-edit restart, writes `tmp/replay_request.txt` and touches
/// `tmp/restart.txt`). Read-once: the file is removed so a later plain restart
/// (code edit) does not re-enter replay mode.
fn consume_replay_request() -> Option<ReplayRequest> {
    consume_request_file(std::path::Path::new("tmp/replay_request.txt"))
        .map(|raw| parse_replay_request(&raw))
}

/// Read-once request consumer: returns the trimmed contents and removes the
/// file, or None if absent/empty. Split out so it is testable without the
/// fixed `tmp/` path.
fn consume_request_file(path: &std::path::Path) -> Option<String> {
    let requested = std::fs::read_to_string(path).ok()?;
    // Removal failure is load-bearing: if the file survives, every later restart
    // (a plain code edit) silently re-enters replay mode. Surface it.
    if let Err(e) = std::fs::remove_file(path) {
        log::warn!(
            target: "STONE",
            "could not remove replay request {} ({e}); later restarts may re-enter replay mode",
            path.display()
        );
    }
    let trimmed = requested.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// Start the game process with the correct library path and robust process
/// management. `replay` Some => relaunch as a windowed replay (in-session load)
/// instead of the flight-recorded live game.
fn start_game_process(
    app_args: &[String],
    flight: Option<&FlightRecorder>,
    replay: Option<&ReplayRequest>,
) -> Result<Child> {
    let lua_file = rock_host::project::default_main_lua_path();

    // Platform-specific process management for orphan prevention
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;

        let mut cmd = rock_host::project::build_luajit_command(&lua_file, app_args);
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        // Only the stone watch loop enables the in-game hot-reload watcher.
        cmd.env("STONE_HOT_RELOAD", "1");
        // Flight recorder (or in-session replay load; P3.5).
        apply_run_env(&mut cmd, flight, replay);

        // 1) Linux: Ensure the child gets PDEATHSIG (SIGTERM) if parent dies
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                // If parent dies, deliver SIGTERM to this child
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        // 2) Set process group to enable proper cleanup
        // This creates a new process group with the child as leader
        cmd.process_group(0);

        let child = cmd.spawn()?;
        let pid = child.id();
        rock_host::process_management::register_child_process(pid);

        // 3) macOS: PDEATHSIG doesn't exist, so spawn a detached
        // monitor that kills the child if the parent dies (even on
        // parent SIGKILL).
        #[cfg(target_os = "macos")]
        spawn_monitor_process(std::process::id(), pid);

        Ok(child)
    }

    #[cfg(windows)]
    {
        // Windows: Use Job Objects for automatic cleanup when parent dies
        //
        // Job Object Lifetime Semantics:
        // 1. We create a Job Object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE flag
        // 2. We assign the child process to this job
        // 3. We intentionally leak the job handle with std::mem::forget()
        // 4. The handle stays alive for the lifetime of the parent process
        // 5. When parent exits (even SIGKILL), Windows closes all handles
        // 6. Job Object cleanup kills all processes in the job automatically
        //
        // This is the same pattern used in the original love.rs implementation.
        // Alternative approaches would require storing the handle somewhere, which
        // complicates the architecture without improving reliability.
        unsafe {
            // Create a Job object
            let job = winapi::um::jobapi2::CreateJobObjectW(
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if job.is_null() {
                anyhow::bail!(
                    "CreateJobObjectW failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            // Configure job: kill all processes when job handle is closed
            let mut info: winapi::um::winnt::JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
                std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags =
                winapi::um::winnt::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = winapi::um::jobapi2::SetInformationJobObject(
                job,
                winapi::um::winnt::JobObjectExtendedLimitInformation,
                &mut info as *mut _ as *mut _,
                std::mem::size_of::<winapi::um::winnt::JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(
                ) as u32,
            );
            if ok == 0 {
                anyhow::bail!(
                    "SetInformationJobObject failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            // Spawn the process
            let mut cmd = rock_host::project::build_luajit_command(&lua_file, app_args);
            cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
            // Only the stone watch loop enables the in-game hot-reload watcher.
            cmd.env("STONE_HOT_RELOAD", "1");
            // Flight recorder (or in-session replay load; P3.5).
            apply_run_env(&mut cmd, flight, replay);

            let child = cmd.spawn()?;

            // Assign process to job object
            use std::os::windows::io::AsRawHandle;
            let child_handle = child.as_raw_handle() as winapi::shared::ntdef::HANDLE;
            let assigned = winapi::um::jobapi2::AssignProcessToJobObject(job, child_handle);
            if assigned == 0 {
                anyhow::bail!(
                    "AssignProcessToJobObject failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            // Intentionally leak the job handle so it stays alive for parent's lifetime
            // When parent process exits, Windows automatically closes the handle and kills
            // all processes in the job. This is the intended behavior.
            std::mem::forget(job);

            let pid = child.id();
            rock_host::process_management::register_child_process(pid);
            Ok(child)
        }
    }
}

/// Kill the game process and reap the zombie.
///
/// `Child::kill` sends SIGKILL on Unix / `TerminateProcess` on Windows; both
/// are guaranteed to terminate the target. The follow-up `wait()` then reaps
/// the zombie — without it the child PID lingers in the process table until
/// the parent exits. No timeout / polling loop is needed: SIGKILL can't be
/// caught or ignored, so `wait()` returns essentially immediately.
fn kill_game_process(child: &mut Child) -> Result<()> {
    rock_host::process_management::unregister_child_process(child.id());

    if let Err(e) = child.kill() {
        // ESRCH (already exited) is not a real error here — fall through to wait().
        warn!("Error killing game process: {}", e);
    }

    if let Err(e) = child.wait() {
        warn!("Error waiting for game process to exit: {}", e);
    }

    Ok(())
}

/// Run the game for N frames with profiling enabled and display performance summary
pub fn do_perf(
    backend: crate::Backend,
    path: &str,
    output: String,
    frames: usize,
    seconds: Option<f64>,
    warmup_seconds: f64,
    gc_window: usize,
    gc_window_start: usize,
    format: &str,
    app_args: &[String],
) -> Result<()> {
    use crate::common::ProcessManager;

    let lua_file = compile_game_for_run(backend, path, &output)?;

    // `frames == 1` is the clap default (user didn't pass --frames).
    // If --seconds is also missing, fall back to 300 frames (~5s at 60fps).
    let user_set_frames = frames != 1;
    let mut frame_cap: Option<usize> = match (user_set_frames, seconds) {
        (false, None) => Some(300),
        (true, _) => Some(frames),
        (false, Some(_)) => None,
    };

    // The GC-stopped window (profiler: warm `gc_window_start` frames + N
    // measured) must run to completion to produce a result. Force a frame cap
    // that covers start + window + tail (overriding a smaller --frames /
    // --seconds) and drop the seconds cap so frames deterministically govern.
    let seconds = if gc_window > 0 {
        let need = gc_window_start + gc_window + 20;
        frame_cap = Some(frame_cap.map_or(need, |c| c.max(need)));
        None
    } else {
        seconds
    };

    let mut cmd = rock_host::project::build_luajit_command(&lua_file, app_args);
    cmd.env("PERF_MODE", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Pin the RNG so the measured scene is byte-identical run-to-run: the
    // prelude otherwise seeds `random()` from os.time() (whole-second
    // granularity), which drifts any RNG-driven content (mag's torch embers)
    // across runs and made graphics.draw_calls a non-gateable metric. Matches
    // the sim runner's default (run_sim.rs); an explicit env still overrides.
    if std::env::var_os("ROCK_TEST_SEED").is_none() {
        cmd.env("ROCK_TEST_SEED", "42");
    }

    // Pass both caps when set. Runner exits on whichever hits first (min).
    if let Some(n) = frame_cap {
        cmd.env("FRAME_COUNT", n.to_string());
    }
    if let Some(secs) = seconds {
        cmd.env("PERF_SECONDS", secs.to_string());
    }

    // Warmup: profiler skips per-section stat aggregation until
    // this many wall-clock seconds have elapsed. Default 2.0s.
    cmd.env("PERF_WARMUP_SECONDS", warmup_seconds.to_string());
    if gc_window > 0 {
        cmd.env("PERF_GC_WINDOW", gc_window.to_string());
        cmd.env("PERF_GC_WINDOW_START", gc_window_start.to_string());
    }
    cmd.env("PERF_FORMAT", format);

    match (frame_cap, seconds) {
        (Some(n), Some(s)) => info!("Running up to {} frames or {}s (whichever first)...", n, s),
        (Some(n), None) => info!("Running {} frames with profiling...", n),
        (None, Some(s)) => info!("Running for {}s with profiling...", s),
        (None, None) => unreachable!(),
    }

    // Give the game enough headroom to finish + dump summary. Base of
    // 30s covers the default frame-count run; extend by seconds limit.
    // ROCK_FRAME_TIMEOUT_SECS overrides it (same escape hatch as `rock frame`)
    // for genuinely long benches -- and keeps frame_exit_result's message honest.
    let timeout_duration = if let Some(secs) = std::env::var("ROCK_FRAME_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        Duration::from_secs(secs)
    } else {
        let wall_budget = seconds.unwrap_or(0.0).max(15.0) + 15.0;
        Duration::from_secs_f64(wall_budget.max(30.0))
    };

    let mut child = cmd.spawn()?;
    let process_result = ProcessManager::wait_with_timeout(&mut child, timeout_duration)?;

    // Extract and display the perf summary from stdout
    if !process_result.output.stdout.is_empty() {
        let stdout_str = String::from_utf8_lossy(&process_result.output.stdout);
        const DELIMITER: &str = "--- PERF_SUMMARY ---";
        if let Some(idx) = stdout_str.find(DELIMITER) {
            let summary = stdout_str[idx + DELIMITER.len()..].trim();
            // Result output, NOT logging: the perf summary is the command's
            // answer, written to stdout for piping/consumption.
            println!("{}", summary);
        } else {
            // No summary found, print raw output
            warn!("No perf summary found in output");
            if !stdout_str.trim().is_empty() {
                // Result output, NOT logging: raw game stdout is the
                // command's output when no structured summary is present.
                println!("{}", stdout_str.trim());
            }
        }
    }

    // Show stderr (game logs) only if verbose or there was an error
    if !process_result.output.stderr.is_empty() {
        let stderr_str = String::from_utf8_lossy(&process_result.output.stderr);
        // Filter out noise, show only profile lines and errors
        for line in stderr_str.lines() {
            if line.contains("[profile]") || line.contains("Error") || line.contains("error") {
                // Result output, NOT logging: forwarding the spawned game
                // process's own stderr lines verbatim, not rock-cli diagnostics.
                eprintln!("{}", line);
            }
        }
    }

    // A perf run that timed out or the child failed did NOT produce a valid
    // summary -- error out (nonzero exit) instead of the old warn-and-Ok(0),
    // which let a corrupt/partial benchmark JSON sail past the gate recipes
    // (justfile render-bench). Same classifier as `rock frame`.
    frame_exit_result(
        process_result.timed_out,
        &process_result.output.status,
        timeout_duration.as_secs(),
    )?;

    Ok(())
}

/// Wall-clock watchdog for a `rock frame` run: a startup allowance plus a
/// per-frame budget, so the ceiling tracks the work actually requested. A flat
/// budget starved slow-booting games -- mag spends ~1.5s registering props and
/// loading the level before frame 1, leaving `-n 100` at ~24ms/frame no room.
/// ROCK_FRAME_TIMEOUT_SECS overrides it outright.
fn frame_timeout(frames: usize, dump_state: bool, app_args: &[String]) -> Duration {
    if let Some(secs) = std::env::var("ROCK_FRAME_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        return Duration::from_secs(secs);
    }

    const STARTUP: Duration = Duration::from_secs(5);
    const PER_FRAME: Duration = Duration::from_millis(50);

    // app_args (headless video extraction) and dump_state (JSON-serializing
    // every global) add work the frame count doesn't predict, so they raise the
    // floor -- a big -n still outgrows it.
    let floor = if !app_args.is_empty() {
        Duration::from_secs(30)
    } else if dump_state {
        Duration::from_secs(15)
    } else {
        Duration::ZERO
    };

    (STARTUP + PER_FRAME * frames as u32).max(floor)
}

/// Run the game for a specified number of frames and take a screenshot
///
/// This is useful for debugging and testing - compiles the game and runs it
/// for N frames, taking a screenshot at the end.
pub fn do_one_frame(
    backend: crate::Backend,
    path: &str,
    output: String,
    frames: usize,
    _headless: bool,
    dump_state: bool,
    record: Option<&str>,
    replay: Option<&str>,
    checkpoint_interval: Option<usize>,
    app_args: &[String],
) -> Result<()> {
    use crate::common::ProcessManager;

    let lua_file = compile_game_for_run(backend, path, &output)?;

    if frames == 1 {
        info!("Running stone for one frame...");
    } else {
        info!("Running stone for {} frames...", frames);
    }

    // Run the compiled Lua with luajit (stone handles window creation)
    let mut cmd = rock_host::project::build_luajit_command(&lua_file, app_args);
    cmd.env("FRAME_COUNT", frames.to_string())
        .env("TAKE_SCREENSHOT", "1")  // Always take screenshot when running frames
        .env("STONE_HEADLESS", "1");  // No visible window for frame runs
    // Replay record/playback (docs/replay-scrub-plan.md). The game process
    // reads these env vars in stone/engine/replay.rock, same pattern as
    // DUMP_STATE / SIM_DT.
    if let Some(f) = record {
        cmd.env("STONE_RECORD", f);
    }
    if let Some(f) = replay {
        cmd.env("STONE_REPLAY", f);
    }
    if let Some(n) = checkpoint_interval {
        cmd.env("STONE_CHECKPOINT_INTERVAL", n.to_string());
    }
    if dump_state {
        // Capture stdout to parse the DUMP_STATE JSON out of it.
        cmd.env("DUMP_STATE", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    } else {
        // Stream the game's output live so long runs (e.g. `just pac bake`,
        // ~80s) show progress instead of buffering until the process exits.
        cmd.stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    }

    // Use spawn instead of output to implement timeout
    let mut child = cmd.spawn()?;

    let timeout_duration = frame_timeout(frames, dump_state, app_args);
    let process_result = ProcessManager::wait_with_timeout(&mut child, timeout_duration)?;

    // Print stdout and stderr.
    // In dump_state mode: game logs go to stderr, only the extracted JSON goes to stdout.
    // In normal mode: everything goes to stdout as before.
    if !process_result.output.stdout.is_empty() {
        let stdout_str = String::from_utf8_lossy(&process_result.output.stdout);
        const DELIMITER: &str = "--- DUMP_STATE ---";
        if dump_state {
            if let Some(idx) = stdout_str.find(DELIMITER) {
                let before = stdout_str[..idx].trim();
                if !before.is_empty() {
                    // Result output, NOT logging: forwarding the spawned
                    // game's own stdout (pre-JSON) verbatim.
                    eprint!("{}", before);
                }
                let json = stdout_str[idx + DELIMITER.len()..].trim();
                // Result output, NOT logging: the dumped state JSON is the
                // command's answer, written to stdout for consumption.
                println!("{}", json);
            } else {
                // Result output, NOT logging: raw game stdout passthrough.
                print!("{}", stdout_str);
            }
        } else {
            // Result output, NOT logging: raw game stdout passthrough.
            print!("{}", stdout_str);
        }
    }
    if !process_result.output.stderr.is_empty() {
        // Result output, NOT logging: forwarding the spawned game
        // process's own stderr verbatim, not rock-cli diagnostics.
        eprint!("{}", String::from_utf8_lossy(&process_result.output.stderr));
    }

    // A watchdog timeout means the run did NOT complete -- treat it as a failure
    // (nonzero exit), not a warning-and-exit-0. Exiting 0 on timeout is precisely
    // how a load-starved run once masqueraded as a clean-but-empty log
    // (docs/replay-scrub-plan.md note 6).
    frame_exit_result(
        process_result.timed_out,
        &process_result.output.status,
        timeout_duration.as_secs(),
    )?;
    if frames == 1 {
        info!("One frame executed successfully");
    } else {
        info!("{} frames executed successfully", frames);
    }

    Ok(())
}

/// Classify a finished `rock frame` run: Ok only on a clean zero-exit. A
/// watchdog timeout or a nonzero child exit is an error, so the CLI exits
/// nonzero (test-games / callers actually fail). Split out to be unit-testable
/// without spawning a real hang.
fn frame_exit_result(
    timed_out: bool,
    status: &std::process::ExitStatus,
    timeout_secs: u64,
) -> Result<()> {
    if timed_out {
        anyhow::bail!(
            "Stone process timed out after {timeout_secs}s -- no clean exit (a hang, \
             an infinite loop, or a run starved past the watchdog). For a legitimately \
             long run, raise ROCK_FRAME_TIMEOUT_SECS."
        );
    }
    if !status.success() {
        // Propagate the child's failure (e.g. the runner's GL-error exit 3).
        anyhow::bail!("Stone exited with code: {:?}", status.code());
    }
    Ok(())
}

/// `rock replay <game> <file>` — windowed replay playback
/// (docs/replay-scrub-plan.md P3.5). Compiles the game and runs it in a real
/// window (NOT headless, NOT frame-capped), driven from the replay log; the
/// runner freezes at the last recorded tick and keeps the window open with the
/// timeline bar live (STONE_REPLAY_WINDOW=1). Interactive: waits until the
/// developer closes the window. Unlike `rock frame --replay`, there is no
/// screenshot, no FRAME_COUNT, and no wall-clock watchdog.
pub fn do_replay_window(
    backend: crate::Backend,
    game: &str,
    output: String,
    replay_file: &str,
    app_args: &[String],
) -> Result<()> {
    let lua_file = compile_game_for_run(backend, game, &output)?;

    info!("Replaying {} in a window (close the window to exit)", replay_file);

    let mut cmd = rock_host::project::build_luajit_command(&lua_file, app_args);
    cmd.env("STONE_REPLAY", replay_file)
        .env("STONE_REPLAY_WINDOW", "1")
        // Stamp the CURRENT build so replay_init can warn when the log was
        // recorded against different code (build-id mismatch).
        .env("STONE_BUILD_ID", build_id_for_lua(std::path::Path::new(&lua_file)))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    // Interactive: no timeout watchdog. Block until the game process exits
    // (window closed / esc / quit). A test can bound the run by setting
    // FRAME_COUNT (and STONE_HEADLESS) in the child env -- both are inherited.
    let status = cmd.spawn()?.wait()?;
    if !status.success() {
        anyhow::bail!("replay exited with code: {:?}", status.code());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        apply_run_env, build_id_for_lua, consume_request_file, ensure_flight_dir,
        flight_recorder_for, frame_exit_result, frame_timeout,
        FlightRecorder,
    };
    use crate::watch::is_compiler_source;

    /// `--no-record` disables the flight recorder outright: no config, and no
    /// side effects either -- the replays dir must not be created (the check
    /// runs before ensure_flight_dir).
    #[test]
    fn no_record_flag_disables_flight_recorder() {
        let game = std::env::temp_dir().join("rock_no_record_flag_test_game");
        let _ = std::fs::remove_dir_all(&game);
        let flight =
            flight_recorder_for(game.to_str().unwrap(), std::path::Path::new("tmp/main.lua"), true);
        assert!(flight.is_none(), "--no-record must disable the flight recorder");
        assert!(
            !game.join("replays").exists(),
            "--no-record must not create the replays dir"
        );
    }

    /// A `rock frame` run exits nonzero on a watchdog timeout (the run did not
    /// complete) and on a nonzero child exit; only a clean zero-exit is Ok. The
    /// Err propagates through do_one_frame -> main -> a nonzero process exit.
    /// (A timed-out run's killed status can even read "success" -- the timeout
    /// flag must win.)
    #[cfg(unix)]
    #[test]
    fn timed_out_frame_run_is_an_error() {
        use std::os::unix::process::ExitStatusExt;
        let ok = std::process::ExitStatus::from_raw(0);
        let failed = std::process::ExitStatus::from_raw(3 << 8); // exit code 3
        assert!(frame_exit_result(false, &ok, 3).is_ok(), "clean zero-exit is Ok");
        assert!(
            frame_exit_result(true, &ok, 3).is_err(),
            "a timeout must be an error even if the killed status looks like success"
        );
        assert!(
            frame_exit_result(false, &failed, 3).is_err(),
            "a nonzero child exit must propagate"
        );
    }

    /// In-session load: the picker's request file is read once, returns the
    /// path, and is removed so a later plain restart doesn't re-enter replay.
    #[test]
    fn consume_request_file_is_read_once() {
        let dir = tempfile::tempdir().unwrap();
        let req = dir.path().join("replay_request.txt");
        std::fs::write(&req, "  bitgun/replays/repro-42.replay\n").unwrap();
        assert_eq!(
            consume_request_file(&req).as_deref(),
            Some("bitgun/replays/repro-42.replay"),
            "trimmed path returned"
        );
        assert!(!req.exists(), "request file must be removed after consuming");
        assert_eq!(consume_request_file(&req), None, "second read is empty");
        // Empty/whitespace request is ignored (not a spurious replay mode).
        std::fs::write(&req, "   \n").unwrap();
        assert_eq!(consume_request_file(&req), None, "blank request -> None");
    }

    /// A load request spawns in windowed-replay mode (and suppresses the
    /// recorder); no request keeps the flight recorder on.
    #[test]
    fn apply_run_env_picks_replay_over_flight() {
        let flight = FlightRecorder {
            record_path: "bitgun/replays/last.replay".into(),
            build_id: "abc123".into(),
            checkpoint_interval: 0,
        };
        let env_of = |replay: Option<&super::ReplayRequest>| -> std::collections::HashMap<String, String> {
            let mut cmd = std::process::Command::new("true");
            apply_run_env(&mut cmd, Some(&flight), replay);
            cmd.get_envs()
                .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
                .collect()
        };
        let plain = super::ReplayRequest { path: "bitgun/replays/repro-7.replay".into(), loop_ab: None };
        let replaying = env_of(Some(&plain));
        assert_eq!(replaying.get("STONE_REPLAY").map(String::as_str), Some("bitgun/replays/repro-7.replay"));
        assert_eq!(replaying.get("STONE_REPLAY_WINDOW").map(String::as_str), Some("1"));
        assert!(!replaying.contains_key("STONE_RECORD"), "replay run must not also record");
        assert!(!replaying.contains_key("STONE_LOOP_A"), "a plain load carries no loop markers");
        let recording = env_of(None);
        assert_eq!(recording.get("STONE_RECORD").map(String::as_str), Some("bitgun/replays/last.replay"));
        assert!(!recording.contains_key("STONE_REPLAY"), "live run must not be in replay mode");

        // P6 shape-edit restart: a loop-carrying request sets STONE_LOOP_A/B so
        // the relaunched process resumes the loop.
        let looped = super::ReplayRequest {
            path: "mag/replays/last.replay".into(),
            loop_ab: Some((30, 90)),
        };
        let env = env_of(Some(&looped));
        assert_eq!(env.get("STONE_LOOP_A").map(String::as_str), Some("30"));
        assert_eq!(env.get("STONE_LOOP_B").map(String::as_str), Some("90"));
    }

    /// The request line parses `path` and the optional `\tloop\tA\tB` suffix; a
    /// malformed loop suffix degrades to a plain replay rather than being dropped.
    #[test]
    fn parse_replay_request_reads_loop_markers() {
        use super::{parse_replay_request, ReplayRequest};
        assert_eq!(
            parse_replay_request("mag/replays/last.replay"),
            ReplayRequest { path: "mag/replays/last.replay".into(), loop_ab: None }
        );
        assert_eq!(
            parse_replay_request("mag/replays/last.replay\tloop\t30\t90"),
            ReplayRequest { path: "mag/replays/last.replay".into(), loop_ab: Some((30, 90)) }
        );
        assert_eq!(
            parse_replay_request("p\tloop\tNaN\t90").loop_ab, None,
            "malformed loop suffix -> plain replay"
        );
    }

    #[test]
    fn build_id_is_deterministic_and_content_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.lua");
        let b = dir.path().join("b.lua");
        std::fs::write(&a, b"print('hello')").unwrap();
        std::fs::write(&b, b"print('world')").unwrap();
        // Deterministic: same bytes -> same id across calls (no per-process seed).
        assert_eq!(build_id_for_lua(&a), build_id_for_lua(&a));
        // Content-sensitive: a codegen change changes the id (mismatch warning).
        assert_ne!(build_id_for_lua(&a), build_id_for_lua(&b));
        // Missing file -> "dev" fallback, never a panic.
        assert_eq!(build_id_for_lua(&dir.path().join("missing.lua")), "dev");
    }

    #[test]
    fn ensure_flight_dir_creates_gitignored_dir() {
        let game = tempfile::tempdir().unwrap();
        let game_s = game.path().to_string_lossy().into_owned();
        let dir = ensure_flight_dir(&game_s).unwrap();
        assert!(dir.is_dir(), "replays/ must be created");
        assert_eq!(dir.file_name().unwrap(), "replays");
        let gitignore = dir.join(".gitignore");
        assert_eq!(
            std::fs::read_to_string(&gitignore).unwrap(),
            "*\n",
            "replays/ must be self-gitignoring"
        );
        // Idempotent: a second call must not error or clobber.
        assert!(ensure_flight_dir(&game_s).is_ok());
    }

    #[test]
    fn compiler_source_classification() {
        // Compiler crate src files -> rebuild the rock binary.
        assert!(is_compiler_source("rock-host/src/lib.rs"));
        assert!(is_compiler_source("rock-cli/src/stone.rs"));
        assert!(is_compiler_source("skull/src/lib.rs"));
        assert!(is_compiler_source("skull-codegen-lua/src/lib.rs"));
        assert!(is_compiler_source("bone-lower/src/lib.rs"));
        assert!(is_compiler_source("bone/src/x.rs"));
        // Absolute paths resolve by the crate dir before /src/.
        assert!(is_compiler_source("/home/me/rock-skull/skull-transform/src/inline.rs"));
        // Cargo manifests/lockfile -> rebuild.
        assert!(is_compiler_source("Cargo.toml"));
        assert!(is_compiler_source("skull/Cargo.toml"));
        assert!(is_compiler_source("rock-cli/build.rs"));

        // The engine is NOT a compiler source (rebuilt separately as libstone).
        assert!(!is_compiler_source("stone-rs/src/audio.rs"));
        assert!(!is_compiler_source("stone-video/src/lib.rs"));
        // Game / stdlib / engine Rock code are not compiler sources.
        assert!(!is_compiler_source("mag/mobs.rock"));
        assert!(!is_compiler_source("stdlib/unified.rock"));
        assert!(!is_compiler_source("stone/engine/lib.rock"));
        // A crate whose name merely contains a prefix mid-path isn't matched.
        assert!(!is_compiler_source("some/rocky/src/x.rs"));
    }

    /// The `rock frame` watchdog scales with the frames requested: a flat
    /// budget starved any slow-booting game (mag spends ~1.5s on prop
    /// registration + level load before frame 1, so `-n 100` at ~24ms/frame
    /// needs ~3.9s and died under the old flat 3s).
    #[test]
    fn frame_timeout_scales_with_frame_count() {
        let secs = |n| frame_timeout(n, false, &[]).as_secs_f64();

        // Startup allowance dominates a short run; still bounded, so a genuine
        // hang on `-n 5` fails fast rather than waiting out a long budget.
        assert_eq!(secs(1), 5.05, "1 frame = 5s startup + 50ms");
        assert!(secs(5) < 6.0, "short runs stay tight");

        // The case that regressed: mag's real numbers now fit.
        assert_eq!(secs(100), 10.0, "5s startup + 100 * 50ms");
        assert!(secs(100) > 3.9, "must exceed mag's measured -n 100 cost");

        // Strictly increasing in frames -- no cap that reintroduces a flat wall.
        assert!(secs(1000) > secs(100), "budget keeps growing with -n");

        // dump_state / app_args floors still apply, and never shrink the budget.
        assert!(frame_timeout(1, true, &[]) >= frame_timeout(1, false, &[]));
        assert!(
            frame_timeout(10_000, false, &[]) >= frame_timeout(10_000, true, &[]),
            "a large -n outgrows the dump_state floor rather than being clamped to it"
        );
    }
}
