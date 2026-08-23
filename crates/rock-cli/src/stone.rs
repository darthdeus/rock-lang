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
                // `_exit`, not `exit` and NEVER the census-printing exit. These
                // are `fork` children of a multithreaded process (rayon builds a
                // global pool), so they must touch nothing the parent owns: the
                // ablation registry is behind a `Mutex` that another thread may
                // have held at fork time, and the census is the PARENT's to
                // report. Routing these through it printed inherited results --
                // twice, once per child -- and `exit` would additionally flush
                // the parent's inherited stdio buffers.
                libc::_exit(0);
            } else {
                // First child exits immediately. Same reasoning as above.
                libc::_exit(0);
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

fn full_reload_requires_restart(restart_reasons: &[String]) -> bool {
    !restart_reasons.is_empty()
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

    let output_path =
        rock_host::project::stone_session_main_lua_path(path, std::process::id());
    if let Some(parent) = output_path.parent() {
        fs_err::create_dir_all(parent)?;
    }
    if !quiet {
        info!("Session output: {}", output_path.display());
    }
    report_session_usage();

    // The backend-specific incremental game compiler (skull: SkullStone two-file
    // split). It is the ONLY backend-specific piece of this loop; everything else
    // (builds, process, restart) is shared. The runtime owns the changed-path
    // accumulator the watcher feeds it.
    let mut compiler =
        crate::game_runtime::game_compiler_for(backend, path, &output_path);
    let mut changed_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    // embed_string inputs are sources the extension filter can't know
    // (docs/comptime-plan.md): the set comes from the compiled program, seeded
    // after every successful build, and shared with the watcher's filter.
    let comptime_watch: crate::file_watcher::SharedWatchSet = Default::default();

    // Do initial compilation.
    // If it fails, we'll continue watching and start the game when compilation succeeds
    let initial_compilation_success = match compiler.compile(&changed_paths, false) {
        Ok(_) => {
            clear_error_file();
            *comptime_watch.lock() = compiler.comptime_watch_files().into_iter().collect();
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
    // file. The compiler-owned identity is refreshed before every process
    // restart, so a successor cannot inherit an earlier Artifact label.
    let mut flight = if initial_compilation_success {
        let artifact_fingerprint = compiler.artifact_fingerprint()?;
        flight_recorder_for(path, &artifact_fingerprint, no_record)?
    } else {
        None
    };
    if let Some(f) = &flight {
        info!("Flight recorder: {} (build {})", f.record_path, f.build_id);
    }

    // Start the game process only if initial compilation succeeded
    let mut game_process = if initial_compilation_success {
        Some(start_game_process(&output_path, app_args, flight.as_ref(), None)?)
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
        PathBuf::from("tmp"),  // Watch tmp/ for restart.txt trigger
    ]);
    // Every optional lib, from the build table: a crate that can be BUILT on an
    // edit but is not WATCHED for one is a hot-reload hole that looks like the
    // edit did nothing.
    for lib in rock_host::engine_build::OPTIONAL_ENGINE_LIBS {
        watch_paths.extend([
            PathBuf::from(format!("{}/src", lib.crate_name)),
            PathBuf::from(format!("{}/Cargo.toml", lib.crate_name)),
            PathBuf::from(format!("{}/build.rs", lib.crate_name)),
        ]);
    }
    watch_paths.extend(crate::watch::compiler_watch_paths());

    // Create channel for file change events
    let (change_tx, change_rx) = channel::<FileChangeEvent>();

    // Canonical rock-source watcher + stone-only opt-ins: manual
    // restart via `touch tmp/restart.txt`, and engine-rebuild
    // extensions (stone-rs / C / stick.toml changes rebuild libstone).
    let _watcher = FileWatcher::default_for_rock_sources()
        .with_restart_trigger(true)
        .with_extensions(vec!["rock".to_string(), "rs".to_string(), "c".to_string(), "h".to_string(), "toml".to_string()])
        .with_shared_extra_files(comptime_watch.clone())
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

            // Match the engine crates BEFORE the generic "Cargo." check below:
            // `stone-rs/Cargo.toml` would otherwise be routed to the
            // compiler-rebuild branch and never trigger a libstone rebuild.
            // Optional crates come from the build table, so a new one is
            // routed here without a second edit.
            let is_engine_crate = path_str.contains("stone-rs/")
                || rock_host::engine_build::OPTIONAL_ENGINE_LIBS
                    .iter()
                    .any(|l| path_str.contains(&format!("{}/", l.crate_name)));
            if is_engine_crate {
                needs_libstone_rebuild = true;
                changed_libstone.push(filename);
            } else if crate::watch::is_compiler_source(path) {
                needs_compiler_rebuild = true;
                changed_compiler.push(filename);
            } else if path_str.ends_with(".rock") {
                changed_paths.insert(path.clone());
                changed_game.push(filename);
            } else if comptime_watch.lock().contains(path.as_path()) {
                // An embedded input is a source: routing it as a changed
                // non-source path makes LiveProject take the full-rebuild arm,
                // which re-reads the file (docs/comptime-plan.md).
                changed_paths.insert(path.clone());
                changed_game.push(filename);
            } else if filename == "stick.toml" {
                // A [build.vars]/[engine]/[assets]/[release] edit mid-session: route as
                // a changed non-source path so LiveProject takes the
                // full-rebuild arm, whose session rebuild re-resolves declared
                // inputs. Dropping it here left stale vars until the next
                // structural edit.
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
                    *comptime_watch.lock() = compiler.comptime_watch_files().into_iter().collect();
                    if full_reload_requires_restart(&outcome.restart_reasons) {
                        info!(
                            "live reload requires a process restart ({})",
                            outcome.restart_reasons.join(", ")
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
            if flight.is_none() {
                let artifact_fingerprint = compiler.artifact_fingerprint()?;
                flight = flight_recorder_for(path, &artifact_fingerprint, no_record)?;
            }
            info!("First successful compilation! Starting game...");
            game_process = Some(start_game_process(&output_path, app_args, flight.as_ref(), None)?);
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
                if let Some(f) = flight.as_mut() {
                    f.build_id = compiler.artifact_fingerprint()?;
                }
                game_process =
                    Some(start_game_process(&output_path, app_args, flight.as_ref(), replay_request.as_ref())?);
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

    // Same table the incremental path builds from, so startup and rebuild
    // cannot disagree about which optional libs this game gets.
    // Same table the incremental path builds from, so startup and rebuild
    // cannot disagree about which optional libs this game gets.
    let optional_libs = rock_host::engine_build::game_engine_libs(game_path)?;

    // `--lib --bin rock` selects both halves in ONE invocation: every default
    // member's lib/cdylib target plus rock-cli's bin. The two build in
    // parallel, and stone-rs's cdylib lands for the stage_cdylib below.
    //
    // No `-p`: resolver-2 unifies features over the SELECTED packages only, so
    // a narrower selection is a distinct feature resolution and rebuilds every
    // shared dep plus the whole compiler stack under a second -C metadata.
    // Staying on the default-member selection keeps one set of artifacts,
    // shared with rock.sh's build.
    info!("Building engine + compiler (cargo)...");

    rock_host::engine_build::run_cargo(
        &["build", "--lib", "--bin", "rock"],
        "Failed to build engine + compiler",
    )?;
    rock_host::engine_build::stage_cdylib("stone_rs", "stone")?;
    // One helper, not a second build+stage pair here: two of them drift, and
    // the drift is invisible until a lib builds on one path and not the other.
    for lib in &optional_libs {
        rock_host::engine_build::build_optional_engine_lib(lib)?;
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
    artifact_fingerprint: &str,
    no_record: bool,
) -> Result<Option<FlightRecorder>> {
    if no_record {
        return Ok(None);
    }
    if std::env::var("STONE_NO_FLIGHT_RECORDER").as_deref() == Ok("1") {
        return Ok(None);
    }
    if std::env::var("STONE_RECORD").is_ok() {
        return Ok(None);
    }
    if artifact_fingerprint.is_empty() || artifact_fingerprint == "dev" {
        anyhow::bail!("cannot start flight recorder without a real Artifact fingerprint");
    }
    let dir = match ensure_flight_dir(game_path) {
        Ok(d) => d,
        Err(e) => {
            log::warn!(target: "STONE", "flight recorder disabled: could not create replays dir: {e}");
            return Ok(None);
        }
    };
    Ok(Some(FlightRecorder {
        record_path: dir.join("last.replay").to_string_lossy().into_owned(),
        build_id: artifact_fingerprint.to_string(),
        checkpoint_interval: 0,
    }))
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

/// Session output is never swept, so the only thing between the author and a
/// full disk is this line. 2 GB is ~400 mag builds (5.3 MB each) -- a lot of
/// sessions, and still small next to `target/`.
const SESSION_USAGE_WARN_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Bytes under `root` and how many session directories they are spread over.
/// An unreadable entry is skipped: this is a courtesy warning, and failing a
/// stone launch over a stat error would be worse than under-reporting.
fn session_usage(root: &std::path::Path) -> (u64, usize) {
    fn bytes_under(dir: &std::path::Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => bytes_under(&e.path()),
                Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => 0,
            })
            .sum()
    }
    let sessions = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .count()
        })
        .unwrap_or(0);
    (bytes_under(root), sessions)
}

fn session_usage_warning(bytes: u64, sessions: usize) -> Option<String> {
    if bytes < SESSION_USAGE_WARN_BYTES {
        return None;
    }
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    Some(format!(
        "{} holds {gb:.1} GB across {sessions} session(s). Nothing here is ever \
         deleted -- old sessions are kept on purpose. Delete the ones you don't \
         need: rm -rf {}/*",
        rock_host::project::STONE_SESSION_DIR,
        rock_host::project::STONE_SESSION_DIR
    ))
}

fn report_session_usage() {
    let (bytes, sessions) = session_usage(std::path::Path::new(
        rock_host::project::STONE_SESSION_DIR,
    ));
    if let Some(warning) = session_usage_warning(bytes, sessions) {
        log::warn!(target: "STONE", "{warning}");
    }
}

/// Start the game process with the correct library path and robust process
/// management. `replay` Some => relaunch as a windowed replay (in-session load)
/// instead of the flight-recorded live game.
fn start_game_process(
    lua_file: &std::path::Path,
    app_args: &[String],
    flight: Option<&FlightRecorder>,
    replay: Option<&ReplayRequest>,
) -> Result<Child> {

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
fn apply_perf_env(cmd: &mut Command) {
    cmd.env("PERF_MODE", "1")
        .env("STONE_HEADLESS", "1")
        .env("MAG_BOOT_PLAYING", "1");
}

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

    let compiled = compile_game_for_run(backend, path, &output)?;
    let lua_file = &compiled.lua_file;

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

    let mut cmd = rock_host::project::build_luajit_command(lua_file, app_args);
    apply_perf_env(&mut cmd);
    cmd.env("STONE_BUILD_ID", &compiled.artifact_fingerprint);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

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

/// A bounded frame run gets a startup allowance, then must report completed
/// frames often enough to show it is alive. The old total budget grew with the
/// requested frame count and killed legitimate slow runs. The explicit env
/// override remains the escape hatch for unusually slow startup or frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameWatchdogConfig {
    startup: Duration,
    inactivity: Duration,
    override_secs: Option<u64>,
}

fn frame_watchdog_config(override_secs: Option<u64>) -> FrameWatchdogConfig {
    let duration = override_secs.map(Duration::from_secs);
    FrameWatchdogConfig {
        startup: duration.unwrap_or(Duration::from_secs(5)),
        inactivity: duration.unwrap_or(Duration::from_secs(5)),
        override_secs,
    }
}

fn frame_watchdog() -> FrameWatchdogConfig {
    frame_watchdog_config(
        std::env::var("ROCK_FRAME_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok()),
    )
}

fn apply_headless_audio_env(cmd: &mut Command) {
    if std::env::var_os("STONE_AUDIO_STUB").is_none() {
        cmd.env("STONE_AUDIO_STUB", "1");
    }
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
    requested_renderer: Option<RendererBackend>,
) -> Result<()> {
    use crate::common::ProcessManager;

    let compiled = compile_game_for_run(backend, path, &output)?;
    let lua_file = &compiled.lua_file;

    if frames == 1 {
        info!("Running stone for one frame...");
    } else {
        info!("Running stone for {} frames...", frames);
    }

    // Run the compiled Lua with luajit (stone handles window creation)
    let mut cmd = rock_host::project::build_luajit_command(lua_file, app_args);
    cmd.env("FRAME_COUNT", frames.to_string())
        .env("TAKE_SCREENSHOT", "1")  // Always take screenshot when running frames
        .env("STONE_HEADLESS", "1")  // No visible window for frame runs
        .env("ROCK_FRAME_WATCHDOG", "1")
        .env("STONE_BUILD_ID", &compiled.artifact_fingerprint);
    apply_headless_audio_env(&mut cmd);
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
        cmd.env("DUMP_STATE", "1");
    }
    // ProcessManager drains both pipes in background threads, so the
    // watchdog can observe frame markers without risking a full pipe.
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    // Use spawn instead of output to implement the progress watchdog.
    let mut child = cmd.spawn()?;

    let watchdog = frame_watchdog();
    let process_result = ProcessManager::wait_with_frame_watchdog(
        &mut child,
        watchdog.startup,
        watchdog.inactivity,
        !dump_state,
        true,
    )?;

    // In dump_state mode, game stdout carries the JSON result while stderr was
    // already forwarded by ProcessManager. Normal output was forwarded live.
    if dump_state && !process_result.output.stdout.is_empty() {
        let stdout_str = String::from_utf8_lossy(&process_result.output.stdout);
        const DELIMITER: &str = "--- DUMP_STATE ---";
        const PERF_DELIMITER: &str = "--- PERF_SUMMARY ---";
        if let Some(idx) = stdout_str.find(DELIMITER) {
            let before = stdout_str[..idx].trim();
            if !before.is_empty() {
                // Result output, NOT logging: forwarding the spawned
                // game's own stdout (pre-JSON) verbatim.
                eprint!("{}", before);
            }
            let after = &stdout_str[idx + DELIMITER.len()..];
            let (json, trailing) = after.find(PERF_DELIMITER).map_or((after, ""), |perf_idx| {
                (&after[..perf_idx], &after[perf_idx..])
            });
            // Result output, NOT logging: the dumped state JSON is the
            // command's answer, written to stdout for consumption.
            println!("{}", json.trim());
            let trailing = trailing.trim();
            if !trailing.is_empty() {
                eprint!("{}", trailing);
            }
        } else {
            // Result output, NOT logging: raw game stdout passthrough.
            print!("{}", stdout_str);
        }
    }

    // A watchdog timeout means the run did NOT complete -- treat it as a failure
    // (nonzero exit), not a warning-and-exit-0. Exiting 0 on timeout is precisely
    // how a load-starved run once masqueraded as a clean-but-empty log
    // (docs/replay-scrub-plan.md note 6).
    let timeout_secs = if process_result.frame_progress.is_some() {
        watchdog.inactivity.as_secs()
    } else {
        watchdog.startup.as_secs()
    };
    let requested_renderer = requested_renderer.map(|renderer| match renderer {
        RendererBackend::Gl => "gl",
        RendererBackend::Wgpu => "wgpu",
    });
    frame_exit_result_with_progress(
        process_result.timed_out,
        &process_result.output.status,
        timeout_secs,
        frames,
        requested_renderer,
        process_result.frame_progress.as_ref(),
        process_result.progress_stopped,
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
    frame_exit_result_with_progress(timed_out, status, timeout_secs, 0, None, None, false)
}

fn frame_exit_result_with_progress(
    timed_out: bool,
    status: &std::process::ExitStatus,
    timeout_secs: u64,
    requested: usize,
    requested_renderer: Option<&str>,
    progress: Option<&crate::common::FrameProgress>,
    progress_stopped: bool,
) -> Result<()> {
    if timed_out {
        let (renderer, completed, requested) = match progress {
            Some(progress) => (
                progress.renderer.as_str(),
                progress.completed,
                progress.requested.unwrap_or(requested),
            ),
            None => (requested_renderer.unwrap_or("unknown"), 0, requested),
        };
        anyhow::bail!(
            "Stone process timed out after {timeout_secs}s -- renderer={renderer}, \
             completed/requested={completed}/{requested}, progress_stopped={progress_stopped}; \
             no clean exit (a hang, an infinite loop, or a run starved past the watchdog). \
             For a legitimately slow run, raise ROCK_FRAME_TIMEOUT_SECS."
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
    let compiled = compile_game_for_run(backend, game, &output)?;
    let lua_file = &compiled.lua_file;

    info!("Replaying {} in a window (close the window to exit)", replay_file);

    let mut cmd = rock_host::project::build_luajit_command(lua_file, app_args);
    if std::env::var("STONE_HEADLESS").as_deref() == Ok("1") {
        apply_headless_audio_env(&mut cmd);
    }
    cmd.env("STONE_REPLAY", replay_file)
        .env("STONE_REPLAY_WINDOW", "1")
        .env("STONE_BUILD_ID", &compiled.artifact_fingerprint)
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
        apply_perf_env, apply_run_env, consume_request_file, ensure_flight_dir,
        flight_recorder_for, frame_exit_result, frame_watchdog_config, full_reload_requires_restart,
        session_usage, session_usage_warning, FlightRecorder, SESSION_USAGE_WARN_BYTES,
    };
    use crate::watch::is_compiler_source;
    use std::time::Duration;

    #[test]
    fn structural_reload_requests_clean_restart() {
        assert!(full_reload_requires_restart(&["schema changed".to_string()]));
        assert!(
            !full_reload_requires_restart(&[]),
            "a body-only edit must not request a restart"
        );
    }

    #[test]
    fn perf_process_is_headless_and_boots_mag_without_screenshot_mode() {
        let mut cmd = std::process::Command::new("true");
        apply_perf_env(&mut cmd);
        let env: std::collections::HashMap<_, _> = cmd
            .get_envs()
            .filter_map(|(key, value)| Some((key.to_str()?, value?.to_str()?)))
            .collect();

        assert_eq!(env.get("PERF_MODE"), Some(&"1"));
        assert_eq!(env.get("STONE_HEADLESS"), Some(&"1"));
        assert_eq!(env.get("MAG_BOOT_PLAYING"), Some(&"1"));
        assert!(!env.contains_key("MAG_SCREENSHOT"));
    }
    /// The whole point of the private session dir: `rock stone` must not write
    /// the slot every other command writes. A shared `tmp/main.lua` is loaded
    /// into the live game by the watcher (docs/hot-reload-chunk-identity.md).
    #[test]
    fn a_stone_session_never_writes_the_shared_main_lua() {
        let shared = rock_host::project::default_main_lua_path();
        let session = rock_host::project::stone_session_main_lua_path("games/match3", 4242);
        assert_ne!(session, shared, "the session must have its own main.lua");
        assert!(
            session.starts_with(rock_host::project::STONE_SESSION_DIR),
            "session output belongs under {}: {}",
            rock_host::project::STONE_SESSION_DIR,
            session.display()
        );
        assert!(
            !shared.starts_with(rock_host::project::STONE_SESSION_DIR),
            "the shared slot must stay outside the session root"
        );
    }

    /// Two concurrent sessions must not share a slot, and a session must keep
    /// the same one across its own recompiles (the path is recomputed nowhere,
    /// but a pid-stable name is what makes a restart land where the watcher
    /// is looking).
    #[test]
    fn session_dirs_separate_by_game_and_pid() {
        let p = |game, pid| rock_host::project::stone_session_main_lua_path(game, pid);
        assert_ne!(p("mag", 1), p("mag", 2), "two sessions of one game differ");
        assert_ne!(p("mag", 1), p("bitgun", 1), "two games differ");
        assert_eq!(p("mag", 1), p("mag", 1), "the same session is stable");
        // A nested project path contributes its name, not its separators.
        let nested = p("games/match3", 7);
        assert!(
            nested.to_string_lossy().contains("match3"),
            "the dir should name the game: {}",
            nested.display()
        );
        assert_eq!(
            nested.components().count(),
            p("match3", 7).components().count(),
            "a nested game path must not deepen the session dir"
        );
    }

    /// Nothing is ever deleted, so the warning is the only thing standing
    /// between the author and a full disk. Both sides, and the threshold is
    /// asserted from the constant rather than a literal repeated here.
    #[test]
    fn session_usage_warns_only_past_the_budget() {
        assert!(
            session_usage_warning(SESSION_USAGE_WARN_BYTES - 1, 100).is_none(),
            "under budget must stay quiet"
        );
        let warning = session_usage_warning(SESSION_USAGE_WARN_BYTES, 154)
            .expect("at the budget the warning fires");
        assert!(warning.contains("154"), "name the session count: {warning}");
        assert!(
            warning.contains(rock_host::project::STONE_SESSION_DIR),
            "name the directory to clean: {warning}"
        );
        assert!(warning.contains("2.0 GB"), "name the size: {warning}");
    }

    /// The scan must count bytes in nested session dirs, not just the top
    /// level, and report the session count it measured over.
    #[test]
    fn session_usage_counts_every_session() {
        let root = tempfile::tempdir().expect("tempdir");
        for (session, bytes) in [("mag-1", 300usize), ("mag-2", 700), ("bitgun-9", 24)] {
            let dir = root.path().join(session);
            std::fs::create_dir_all(&dir).expect("mkdir session");
            std::fs::write(dir.join("main.lua"), vec![b'x'; bytes]).expect("write main");
        }
        // A stray file at the root is not a session and contributes no count.
        std::fs::write(root.path().join("notes.txt"), b"hi").expect("write stray");

        let (bytes, sessions) = session_usage(root.path());
        assert_eq!(sessions, 3, "three session dirs");
        assert_eq!(bytes, 300 + 700 + 24 + 2, "every byte under the root");
    }

    /// `--no-record` disables the flight recorder outright: no config, and no
    /// side effects either -- the replays dir must not be created (the check
    /// runs before ensure_flight_dir).
    #[test]
    fn no_record_flag_disables_flight_recorder() {
        let game = std::env::temp_dir().join("rock_no_record_flag_test_game");
        let _ = std::fs::remove_dir_all(&game);
        let flight = flight_recorder_for(game.to_str().unwrap(), "fp", true).unwrap();
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

    /// The frame watchdog bounds startup and inactivity independently of the
    /// requested frame count. A legitimate slow run can therefore outlive its
    /// old total budget, while a child that stops reporting still fails.
    #[test]
    fn frame_watchdog_preserves_progress_and_override_contracts() {
        let default = frame_watchdog_config(None);
        assert_eq!(default.startup, Duration::from_secs(5));
        assert_eq!(default.inactivity, Duration::from_secs(5));
        assert_eq!(default.override_secs, None);

        let override_config = frame_watchdog_config(Some(60));
        assert_eq!(override_config.startup, Duration::from_secs(60));
        assert_eq!(override_config.inactivity, Duration::from_secs(60));
        assert_eq!(override_config.override_secs, Some(60));
    }
}
