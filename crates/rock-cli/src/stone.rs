use crate::*;
use std::process::{Child, Command, Stdio};

#[path = "frame_compile_cache.rs"]
mod frame_compile_cache;

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

#[derive(Default)]
struct StoneChanges {
    needs_libstone_rebuild: bool,
    needs_compiler_rebuild: bool,
    sound_dirty: bool,
    changed_libstone: Vec<String>,
    changed_compiler: Vec<String>,
    changed_paths: std::collections::HashSet<PathBuf>,
}

fn categorize_stone_changes(
    event: &FileChangeEvent,
    sound_watch: &crate::file_watcher::SharedWatchSet,
    comptime_watch: &crate::file_watcher::SharedWatchSet,
) -> StoneChanges {
    let mut changes = StoneChanges::default();
    for path in &event.paths {
        // Normalize separators once so the substring checks below work
        // identically on Windows (`stone-rs\src\foo.rs`) and Unix
        // (`stone-rs/src/foo.rs`). All match strings use `/`.
        let path_str = path.to_string_lossy().replace('\\', "/");

        if sound_watch.lock().contains(path.as_path()) {
            changes.sound_dirty = true;
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
            changes.needs_libstone_rebuild = true;
            changes.changed_libstone.push(filename);
        } else if crate::watch::is_compiler_source(path) {
            changes.needs_compiler_rebuild = true;
            changes.changed_compiler.push(filename);
        } else if path_str.ends_with(".rock") {
            changes.changed_paths.insert(path.clone());
        } else if comptime_watch.lock().contains(path.as_path()) {
            // An embedded input is a source: routing it as a changed
            // non-source path makes LiveProject take the full-rebuild arm,
            // which re-reads the file (docs/comptime-plan.md).
            changes.changed_paths.insert(path.clone());
        } else if filename == "stick.toml" {
            // A [build.vars]/[engine]/[assets]/[release] edit mid-session: route as
            // a changed non-source path so LiveProject takes the
            // full-rebuild arm, whose session rebuild re-resolves declared
            // inputs. Dropping it here left stale vars until the next
            // structural edit.
            changes.changed_paths.insert(path.clone());
        }
    }

    changes
}

fn stone_source_watch_paths(
    project: &str,
    compiler: &dyn crate::game_runtime::GameCompiler,
) -> Vec<PathBuf> {
    match compiler.package_roots() {
        Some(roots) => crate::watch::loaded_source_watch_paths(project, roots),
        None => crate::watch::project_engine_watch_paths(project),
    }
}

fn retain_production_changes(
    changes: &mut std::collections::HashSet<PathBuf>,
    project: &str,
    package_roots: Option<&[(u32, PathBuf)]>,
    comptime_watch: &crate::file_watcher::SharedWatchSet,
) {
    let inputs = comptime_watch.lock();
    changes.retain(|path| {
        if path.extension().is_none_or(|extension| extension != "rock") || inputs.contains(path) {
            return true;
        }
        let canonical = path.canonicalize().or_else(|_| {
            let parent = path.parent().ok_or_else(|| std::io::Error::other("no parent"))?;
            let name = path.file_name().ok_or_else(|| std::io::Error::other("no file name"))?;
            parent.canonicalize().map(|parent| parent.join(name))
        });
        let Ok(canonical) = canonical else { return true };
        let roots = package_roots.unwrap_or(&[]);
        let owning_root = roots.iter()
            .filter_map(|(_, root)| {
                let root = root.canonicalize().ok()?;
                canonical.strip_prefix(&root).ok().map(|_| root)
            })
            .max_by_key(|root| root.components().count())
            .or_else(|| {
                roots.is_empty().then(|| {
                    PathBuf::from(project).canonicalize()
                        .unwrap_or_else(|_| PathBuf::from(project))
                })
            });
        let Some(root) = owning_root else { return true };
        let Ok(relative) = canonical.strip_prefix(&root) else { return true };
        !rock_core::source_files::SourceFiles::is_test_only_relative_path(relative)
    });
}

struct StoneWatcher {
    handle: Option<crate::file_watcher::WatchHandle>,
    paths: Vec<PathBuf>,
    inputs: std::collections::HashSet<PathBuf>,
    extra_files: crate::file_watcher::SharedWatchSet,
    restart_files: Vec<PathBuf>,
    tx: std::sync::mpsc::Sender<FileChangeEvent>,
}

impl StoneWatcher {
    fn new(
        extra_files: crate::file_watcher::SharedWatchSet,
        restart_files: Vec<PathBuf>,
        tx: std::sync::mpsc::Sender<FileChangeEvent>,
    ) -> Self {
        Self { handle: None, paths: Vec::new(), inputs: Default::default(), extra_files, restart_files, tx }
    }

    fn refresh(&mut self, paths: Vec<PathBuf>) -> Result<()> {
        let inputs = self.extra_files.lock().clone();
        if self.handle.is_some() && self.paths == paths && self.inputs == inputs {
            return Ok(());
        }
        // Arm new roots and input parents before dropping the previous watches.
        let handle = FileWatcher::default_for_rock_sources()
            .with_restart_trigger(true)
            .with_initial_create_filter(true)
            .with_restart_files(self.restart_files.clone())
            .with_extensions(vec!["rock".to_string(), "rs".to_string(), "c".to_string(), "h".to_string(), "toml".to_string()])
            .with_shared_extra_files(self.extra_files.clone())
            .watch(paths.clone(), self.tx.clone())?;
        self.handle = Some(handle);
        self.paths = paths;
        self.inputs = inputs;
        Ok(())
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
    crate::skull_backend::ensure_renderer_feature(path)?;
    let replay_source = replay_source_for(path)?;

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
    let sound_watch: crate::file_watcher::SharedWatchSet = Default::default();
    if let Some(sound_project) = crate::package::sound_project_path_for(path)? {
        sound_watch.lock().insert(sound_project.clone());
        comptime_watch.lock().insert(sound_project);
    }

    // Do initial compilation.
    // If it fails, we'll continue watching and start the game when compilation succeeds
    let initial_compilation_success = match crate::package::prepare_sound_bank(path)
        .and_then(|_| compiler.compile(&changed_paths, false))
    {
        Ok(_) => {
            clear_error_file();
            let mut watched: std::collections::HashSet<PathBuf> =
                compiler.comptime_watch_files().into_iter().collect();
            watched.extend(sound_watch.lock().iter().cloned());
            *comptime_watch.lock() = watched;
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
    // boot into the per-user replay archive so any bug is already a file. The
    // compiler-owned identity is refreshed before every process restart, so a
    // successor cannot inherit an earlier Artifact label.
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
        let build_id = compiler.artifact_fingerprint()?;
        Some(start_game_process(
            &output_path, app_args, &build_id, &replay_source, flight.as_ref(), None,
        )?)
    } else {
        None
    };

    // Set up file watcher for game files, stdlib, stone (engine Rock code),
    // stone-rs / stone-video (engine Rust crates), the rock compiler, and
    // tmp/ for the restart-trigger file. FileWatcher silently skips
    // nonexistent paths, so listing the optional crates is safe on machines
    // where they aren't checked out.
    let rock_root = PathBuf::from(rock_host::rock_dir());
    let mut engine_watch_paths = vec![
        rock_root.join("stone-rs/src"),
        rock_root.join("stone-rs/Cargo.toml"),
        rock_root.join("stone-rs/build.rs"),
    ];
    // Every optional lib, from the build table: a crate that can be BUILT on an
    // edit but is not WATCHED for one is a hot-reload hole that looks like the
    // edit did nothing.
    for lib in rock_host::engine_build::OPTIONAL_ENGINE_LIBS {
        engine_watch_paths.extend([
            rock_root.join(format!("{}/src", lib.crate_name)),
            rock_root.join(format!("{}/Cargo.toml", lib.crate_name)),
            rock_root.join(format!("{}/build.rs", lib.crate_name)),
        ]);
    }
    engine_watch_paths.extend(crate::watch::compiler_watch_paths());
    let mut watch_paths = stone_source_watch_paths(path, compiler.as_ref());
    watch_paths.extend(engine_watch_paths.iter().cloned());

    // Create channel for file change events
    let (change_tx, change_rx) = channel::<FileChangeEvent>();

    // Canonical rock-source watcher + stone-only opt-ins: manual
    // restart via `touch tmp/restart.txt`, and engine-rebuild
    // extensions (stone-rs / C / stick.toml changes rebuild libstone).
    let restart_files = vec![rock_root.join("tmp/restart.txt"), PathBuf::from("tmp/restart.txt")];
    let mut watcher = StoneWatcher::new(comptime_watch.clone(), restart_files, change_tx);
    watcher.refresh(watch_paths)?;

    info!("Watching for changes...");

    // Track what needs rebuilding
    let mut needs_libstone_rebuild = false;
    let mut needs_compiler_rebuild = false;
    let mut sound_dirty = false;
    let mut force_game_compile = !initial_compilation_success;

    for event in change_rx {
        // Check for manual restart trigger first
        let mut manual_restart_requested = false;
        if event.is_restart_trigger {
            info!("Manual restart triggered (tmp/restart.txt detected)");
            manual_restart_requested = true;
        }

        let mut changes = categorize_stone_changes(&event, &sound_watch, &comptime_watch);
        retain_production_changes(&mut changes.changed_paths, path, compiler.package_roots(), &comptime_watch);
        needs_libstone_rebuild |= changes.needs_libstone_rebuild;
        needs_compiler_rebuild |= changes.needs_compiler_rebuild;
        sound_dirty |= changes.sound_dirty;
        changed_paths.extend(changes.changed_paths);
        let changed_libstone = changes.changed_libstone;
        let changed_compiler = changes.changed_compiler;

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

        if sound_dirty {
            match crate::package::prepare_sound_bank(path) {
                Ok(_) => {
                    sound_dirty = false;
                    if game_process.is_none() {
                        force_game_compile = true;
                    }
                }
                Err(e) => {
                    let error_message = format!("Error compiling sound project: {}", e);
                    log::error!(target: "STONE", "{}", error_message);
                    write_error_file(&error_message);
                }
            }
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
        let has_game_changes = !changed_paths.is_empty() || force_game_compile;
        let mut compilation_succeeded = false;
        if has_game_changes || compiler_rebuilt {
            match compiler.compile(&changed_paths, compiler_rebuilt) {
                Ok(outcome) => {
                    // Consumed successfully; start the next edit clean. On error
                    // we keep them so the next tick retries the same changes.
                    changed_paths.clear();
                    force_game_compile = false;
                    compilation_succeeded = true;
                    if !sound_dirty { clear_error_file(); }
                    let mut watched: std::collections::HashSet<PathBuf> =
                        compiler.comptime_watch_files().into_iter().collect();
                    watched.extend(sound_watch.lock().iter().cloned());
                    *comptime_watch.lock() = watched.clone();
                    let mut next_paths = stone_source_watch_paths(path, compiler.as_ref());
                    next_paths.extend(engine_watch_paths.iter().cloned());
                    watcher.refresh(next_paths)?;
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
            let artifact_fingerprint = compiler.artifact_fingerprint()?;
            flight = flight_recorder_for(path, &artifact_fingerprint, no_record)?;
            if let Some(f) = &flight {
                info!("Flight recorder: {} (build {})", f.record_path, f.build_id);
            }
            info!("First successful compilation! Starting game...");
            let build_id = artifact_fingerprint;
            game_process = Some(start_game_process(
                &output_path, app_args, &build_id, &replay_source, flight.as_ref(), None,
            )?);
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
                let build_id = compiler.artifact_fingerprint()?;
                if replay_request.is_none() {
                    flight = flight_recorder_for(path, &build_id, no_record)?;
                    if let Some(f) = &flight {
                        info!("Flight recorder: {} (build {})", f.record_path, f.build_id);
                    }
                }
                game_process = Some(start_game_process(
                    &output_path, app_args, &build_id, &replay_source,
                    flight.as_ref(), replay_request.as_ref(),
                )?);
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
/// (docs/replay-scrub-plan.md P3.5). Each recording streams to a timestamped
/// file in the per-user replay archive, with `last.replay` as a relative symlink.
struct FlightRecorder {
    record_path: String,
    build_id: String,
    source: String,
    checkpoint_interval: usize,
}

const REPLAY_USAGE_WARN_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Live desync cadence for the always-on flight recorder.
///
/// A checkpoint canonically serializes and hashes every world-included global,
/// and its cost grows with MAG's live world. A populated session measured a
/// 2.0-second stall and 130 MiB heap spike at the former 3600-tick default.
/// Always-on recording must not perturb ordinary play, so checkpoints default
/// off. Set STONE_CHECKPOINT_INTERVAL to a positive tick cadence for a session
/// whose replay needs a desync oracle.
const FLIGHT_CHECKPOINT_INTERVAL: usize = 0;

fn parse_checkpoint_interval(raw: Option<&str>) -> usize {
    let Some(raw) = raw else { return FLIGHT_CHECKPOINT_INTERVAL };
    match raw.trim().parse::<usize>() {
        Ok(interval) => interval,
        Err(error) => {
            log::warn!(
                target: "STONE",
                "STONE_CHECKPOINT_INTERVAL is not a tick count ({error}); using {FLIGHT_CHECKPOINT_INTERVAL}"
            );
            FLIGHT_CHECKPOINT_INTERVAL
        }
    }
}

fn flight_checkpoint_interval() -> usize {
    let raw = std::env::var("STONE_CHECKPOINT_INTERVAL").ok();
    parse_checkpoint_interval(raw.as_deref())
}

fn replay_state_root() -> Option<std::path::PathBuf> {
    if let Some(root) = std::env::var_os("STONE_REPLAY_ROOT") {
        return Some(root.into());
    }
    if let Some(root) = std::env::var_os("XDG_STATE_HOME") {
        return Some(std::path::PathBuf::from(root).join("stone").join("replays"));
    }
    std::env::var_os("HOME").map(|home| {
        std::path::PathBuf::from(home)
            .join(".local").join("state").join("stone").join("replays")
    })
}

pub(crate) fn replay_source_for(game_path: &str) -> Result<String> {
    let project = fs_err::canonicalize(game_path)?;
    let source = rock_host::project::find_workspace_root(&project)
        .and_then(|root| project.strip_prefix(root).ok().map(std::path::Path::to_path_buf))
        .filter(|relative| !relative.as_os_str().is_empty())
        .unwrap_or(project);
    Ok(source.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

fn replay_archive_name(source: &str) -> String {
    use sha2::{Digest, Sha256};

    let readable = source.rsplit('/').next().unwrap_or("game").chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>();
    let digest = Sha256::digest(source.as_bytes());
    let id = digest[..4].iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    format!("{}-{id}", if readable.is_empty() { "game" } else { &readable })
}

fn replay_archive_dir(replay_root: &std::path::Path, source: &str) -> std::path::PathBuf {
    let relative = std::path::Path::new(source);
    let is_safe_relative = !relative.as_os_str().is_empty()
        && relative.components().all(|part| matches!(part, std::path::Component::Normal(_)));
    if is_safe_relative {
        replay_root.join(relative)
    } else {
        replay_root.join("_external").join(replay_archive_name(source))
    }
}

#[cfg(unix)]
fn replace_last_replay_alias(dir: &std::path::Path, target: &std::ffi::OsStr) -> Result<()> {
    use std::os::unix::fs::symlink;

    let pending = dir.join(format!(".last.replay-{}", std::process::id()));
    if pending.symlink_metadata().is_ok() {
        std::fs::remove_file(&pending)?;
    }
    symlink(target, &pending)?;
    std::fs::rename(pending, dir.join("last.replay"))?;
    Ok(())
}

#[cfg(windows)]
fn replace_last_replay_alias(dir: &std::path::Path, target: &std::ffi::OsStr) -> Result<()> {
    use std::os::windows::fs::symlink_file;

    let pending = dir.join(format!(".last.replay-{}", std::process::id()));
    if pending.symlink_metadata().is_ok() {
        std::fs::remove_file(&pending)?;
    }
    symlink_file(target, &pending)?;
    std::fs::rename(pending, dir.join("last.replay"))?;
    Ok(())
}

fn prepare_flight_recorder(
    replay_root: &std::path::Path,
    source: &str,
    artifact_fingerprint: &str,
    timestamp_ms: u128,
    pid: u32,
) -> Result<FlightRecorder> {
    let dir = replay_archive_dir(replay_root, source);
    std::fs::create_dir_all(&dir)?;

    let mut suffix = 0usize;
    let (record_path, filename) = loop {
        let filename = if suffix == 0 {
            format!("recording-{timestamp_ms}-{pid}.replay")
        } else {
            format!("recording-{timestamp_ms}-{pid}-{suffix}.replay")
        };
        let path = dir.join(&filename);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => break (path, filename),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => suffix += 1,
            Err(error) => return Err(error.into()),
        }
    };

    if let Err(error) = replace_last_replay_alias(&dir, std::ffi::OsStr::new(&filename)) {
        let _ = std::fs::remove_file(&record_path);
        return Err(error);
    }

    Ok(FlightRecorder {
        record_path: record_path.to_string_lossy().into_owned(),
        build_id: artifact_fingerprint.to_string(),
        source: source.to_string(),
        checkpoint_interval: flight_checkpoint_interval(),
    })
}

fn replay_usage(dir: &std::path::Path) -> (u64, usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return (0, 0) };
    entries.flatten().filter_map(|entry| {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("replay") {
            return None;
        }
        let metadata = std::fs::symlink_metadata(path).ok()?;
        metadata.file_type().is_file().then_some(metadata.len())
    }).fold((0, 0), |(bytes, count), len| (bytes + len, count + 1))
}

fn replay_usage_warning(dir: &std::path::Path, bytes: u64, recordings: usize) -> Option<String> {
    if bytes < REPLAY_USAGE_WARN_BYTES { return None }
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    Some(format!(
        "{} holds {gb:.1} GB across {recordings} recording(s); recordings are retained until you remove the ones you no longer need",
        dir.display()
    ))
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
    let Some(root) = replay_state_root() else {
        log::warn!(target: "STONE", "flight recorder disabled: HOME/XDG_STATE_HOME is unavailable");
        return Ok(None);
    };
    let source = match replay_source_for(game_path) {
        Ok(source) => source,
        Err(e) => {
            log::warn!(target: "STONE", "flight recorder disabled: could not identify replay source: {e}");
            return Ok(None);
        }
    };
    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    let flight = match prepare_flight_recorder(
        &root, &source, artifact_fingerprint, timestamp_ms, std::process::id(),
    ) {
        Ok(flight) => flight,
        Err(e) => {
            log::warn!(target: "STONE", "flight recorder disabled: could not prepare replay archive: {e}");
            return Ok(None);
        }
    };
    let dir = std::path::Path::new(&flight.record_path).parent().unwrap_or(&root);
    let (bytes, recordings) = replay_usage(dir);
    if let Some(warning) = replay_usage_warning(dir, bytes, recordings) {
        log::warn!(target: "STONE", "{warning}");
    }
    Ok(Some(flight))
}

/// Apply the flight-recorder env to a game-process command (both spawn branches).
fn apply_flight_env(cmd: &mut Command, flight: Option<&FlightRecorder>) {
    if let Some(f) = flight {
        cmd.env("STONE_RECORD", &f.record_path)
            .env("STONE_BUILD_ID", &f.build_id)
            .env("STONE_REPLAY_SOURCE", &f.source)
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
                cmd.env("STONE_BUILD_ID", &f.build_id)
                    .env("STONE_REPLAY_SOURCE", &f.source);
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
    build_id: &str,
    replay_source: &str,
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
        cmd.env("STONE_HOT_RELOAD", "1")
            .env("STONE_BUILD_ID", build_id)
            .env("STONE_REPLAY_SOURCE", replay_source);
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
            cmd.env("STONE_HOT_RELOAD", "1")
                .env("STONE_BUILD_ID", build_id)
                .env("STONE_REPLAY_SOURCE", replay_source);
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
    let perf_no_sections =
        std::env::var("PERF_NO_SECTIONS").unwrap_or_else(|_| "1".to_string());
    cmd.env("PERF_MODE", "1")
        .env("ROCK_FRAME_WATCHDOG", "1")
        .env("ROCK_PERF_STARTUP_WATCHDOG", "1")
        .env("STONE_HEADLESS", "1")
        .env("MAG_BOOT_PLAYING", "1")
        .env("PERF_NO_SECTIONS", perf_no_sections)
        .env("MAG_PERF_SAVE_FIXTURE", "1");
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
    gc_window_mode: &str,
    format: &str,
    app_args: &[String],
) -> Result<()> {
    use crate::common::ProcessManager;

    crate::compile_slots::report_spawn_phase("compile");
    let compiled = compile_game_for_run(backend, path, &output)?;
    let lua_file = &compiled.lua_file;
    crate::compile_slots::report_spawn_phase("runtime-prepare");

    // `frames == 1` is the clap default (user didn't pass --frames).
    // If --seconds is also missing, fall back to 300 frames (~5s at 60fps).
    let user_set_frames = frames != 1;
    let mut frame_cap: Option<usize> = match (user_set_frames, seconds) {
        (false, None) => Some(300),
        (true, _) => Some(frames),
        (false, Some(_)) => None,
    };

    // The GC allocation window (profiler: warm `gc_window_start` frames + N
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

    let mut cmd =
        crate::headless_scope::headless_command(rock_host::project::build_luajit_command(lua_file, app_args));
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
        cmd.env("PERF_GC_WINDOW_MODE", gc_window_mode);
    }
    cmd.env("PERF_FORMAT", format);

    match (frame_cap, seconds) {
        (Some(n), Some(s)) => info!("Running up to {} frames or {}s (whichever first)...", n, s),
        (Some(n), None) => info!("Running {} frames with profiling...", n),
        (None, Some(s)) => info!("Running for {}s with profiling...", s),
        (None, None) => unreachable!(),
    }

    // Asset loading is startup, not part of the budget for executing frames
    // and dumping the summary. The override bounds each phase independently.
    let timeout_override = frame_watchdog().override_secs;
    let startup_timeout = timeout_override.map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(120));
    let timeout_duration = if let Some(secs) = timeout_override {
        Duration::from_secs(secs)
    } else {
        let wall_budget = seconds.unwrap_or(0.0).max(15.0) + 15.0;
        Duration::from_secs_f64(wall_budget.max(30.0))
    };

    crate::compile_slots::report_spawn_phase("execution");
    let mut child = cmd.spawn()?;
    let process_result = ProcessManager::wait_with_startup_budget(
        &mut child, startup_timeout, timeout_duration,
    )?;

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
        // Filter out noise, show profile lines, errors and structured logger
        // records whose severity needs action.
        for line in stderr_str.lines() {
            if line.contains("[profile]")
                || line.contains("Error")
                || line.contains("error")
                || crate::sim_record::is_error_or_warning_log_record(line)
            {
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
    let timeout_secs = if process_result.timeout_phase == Some(crate::common::TimeoutPhase::Startup) {
        startup_timeout.as_secs()
    } else {
        timeout_duration.as_secs()
    };
    frame_exit_result_with_progress(
        process_result.timed_out,
        &process_result.output.status,
        timeout_secs,
        frame_cap,
        None,
        process_result.frame_progress.as_ref(),
        process_result.progress_stopped,
        process_result.timeout_phase,
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

pub(crate) fn apply_headless_audio_env(cmd: &mut Command) {
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

    crate::compile_slots::report_spawn_phase("compile");
    let compiled = frame_compile_cache::compile_game_for_frame(
        backend, path, &output, frames, requested_renderer.clone(),
    )?;
    let lua_file = &compiled.lua_file;
    crate::compile_slots::report_spawn_phase("runtime-prepare");
    let replay_source = replay_source_for(path)?;

    if frames == 1 {
        info!("Running stone for one frame...");
    } else {
        info!("Running stone for {} frames...", frames);
    }

    // Run the compiled Lua with luajit (stone handles window creation)
    let mut cmd =
        crate::headless_scope::headless_command(rock_host::project::build_luajit_command(lua_file, app_args));
    cmd.env("FRAME_COUNT", frames.to_string())
        .env("TAKE_SCREENSHOT", "1")  // Always take screenshot when running frames
        .env("STONE_HEADLESS", "1")  // No visible window for frame runs
        .env("ROCK_FRAME_WATCHDOG", "1")
        .env("STONE_BUILD_ID", &compiled.artifact_fingerprint)
        .env("STONE_REPLAY_SOURCE", replay_source);
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
    crate::compile_slots::report_spawn_phase("execution");
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
    let timeout_secs = if process_result.timeout_phase == Some(crate::common::TimeoutPhase::Startup) {
        watchdog.startup.as_secs()
    } else {
        watchdog.inactivity.as_secs()
    };
    let requested_renderer = requested_renderer.map(|renderer| match renderer {
        RendererBackend::Gl => "gl",
        RendererBackend::Wgpu => "wgpu",
    });
    frame_exit_result_with_progress(
        process_result.timed_out,
        &process_result.output.status,
        timeout_secs,
        Some(frames),
        requested_renderer,
        process_result.frame_progress.as_ref(),
        process_result.progress_stopped,
        process_result.timeout_phase,
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
#[cfg(test)]
fn frame_exit_result(
    timed_out: bool,
    status: &std::process::ExitStatus,
    timeout_secs: u64,
) -> Result<()> {
    frame_exit_result_with_progress(timed_out, status, timeout_secs, None, None, None, false,
        Some(crate::common::TimeoutPhase::TotalBudget))
}

fn frame_exit_result_with_progress(
    timed_out: bool,
    status: &std::process::ExitStatus,
    timeout_secs: u64,
    requested: Option<usize>,
    requested_renderer: Option<&str>,
    progress: Option<&crate::common::FrameProgress>,
    progress_stopped: bool,
    timeout_phase: Option<crate::common::TimeoutPhase>,
) -> Result<()> {
    if timed_out {
        let phase = timeout_phase.ok_or_else(|| anyhow::anyhow!("Stone timeout is missing its phase"))?;
        let (renderer, completed, requested) = match progress {
            Some(progress) => (
                progress.renderer.as_str(),
                progress.completed,
                progress.requested.or(requested),
            ),
            None => (requested_renderer.unwrap_or("unknown"), 0, requested),
        };
        let requested = requested.map(|frames| frames.to_string()).unwrap_or_else(|| "-".into());
        anyhow::bail!(
            "Stone process timed out after {timeout_secs}s -- phase={phase}, renderer={renderer}, \
             completed/requested={completed}/{requested} (last reported), progress_stopped={progress_stopped}; \
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
    let replay_source = replay_source_for(game)?;

    info!("Replaying {} in a window (close the window to exit)", replay_file);

    let mut cmd = rock_host::project::build_luajit_command(lua_file, app_args);
    if crate::headless_scope::inherits_headless() {
        cmd = crate::headless_scope::headless_command(cmd);
        apply_headless_audio_env(&mut cmd);
    }
    cmd.env("STONE_REPLAY", replay_file)
        .env("STONE_REPLAY_WINDOW", "1")
        .env("STONE_BUILD_ID", &compiled.artifact_fingerprint)
        .env("STONE_REPLAY_SOURCE", replay_source)
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
        apply_perf_env, apply_run_env, consume_request_file,
        flight_recorder_for, frame_exit_result, frame_watchdog_config,
        full_reload_requires_restart, parse_checkpoint_interval, prepare_flight_recorder,
        replay_usage, replay_usage_warning, session_usage, session_usage_warning,
        FlightRecorder, FLIGHT_CHECKPOINT_INTERVAL, REPLAY_USAGE_WARN_BYTES,
        SESSION_USAGE_WARN_BYTES,
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
        assert_eq!(env.get("PERF_NO_SECTIONS"), Some(&"1"));
        assert_eq!(env.get("MAG_PERF_SAVE_FIXTURE"), Some(&"1"));
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

    #[cfg(unix)]
    #[test]
    fn timeout_diagnostics_name_startup_and_frame_inactivity() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(0);
        let startup = super::frame_exit_result_with_progress(
            true, &status, 120, Some(240), None, None, false, Some(crate::common::TimeoutPhase::Startup),
        ).unwrap_err().to_string();
        assert!(startup.contains("phase=startup"), "{startup}");
        assert!(startup.contains("completed/requested=0/240"), "{startup}");

        let progress = crate::common::FrameProgress {
            renderer: "gl".into(), completed: 1, requested: Some(240),
        };
        let inactivity = super::frame_exit_result_with_progress(
            true, &status, 30, Some(240), None, Some(&progress), true,
            Some(crate::common::TimeoutPhase::FrameInactivity),
        ).unwrap_err().to_string();
        assert!(inactivity.contains("phase=frame inactivity"), "{inactivity}");
        assert!(inactivity.contains("renderer=gl, completed/requested=1/240"), "{inactivity}");
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
            source: "bitgun".into(),
            checkpoint_interval: FLIGHT_CHECKPOINT_INTERVAL,
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
        assert_eq!(recording.get("STONE_REPLAY_SOURCE").map(String::as_str), Some("bitgun"));
        assert_eq!(
            recording.get("STONE_CHECKPOINT_INTERVAL").map(String::as_str),
            Some(FLIGHT_CHECKPOINT_INTERVAL.to_string().as_str()),
            "a live recording must carry its desync cadence into the game process"
        );
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
    #[cfg(unix)]
    fn flight_recordings_are_timestamped_and_last_is_only_an_alias() {
        let state = tempfile::tempdir().unwrap();
        let first = prepare_flight_recorder(
            state.path(), "games/match3", "0123456789abcdef", 1_777_000_000_123, 41,
        ).unwrap();
        let first_path = std::path::Path::new(&first.record_path);
        assert!(first_path.starts_with(state.path()), "recording belongs under external state: {}", first_path.display());
        assert_eq!(
            first_path.parent().unwrap(),
            state.path().join("games").join("match3"),
            "the logical source is also the human-readable archive path"
        );
        assert_eq!(
            first_path.file_name().and_then(|name| name.to_str()),
            Some("recording-1777000000123-41.replay")
        );
        assert_eq!(first.source, "games/match3");

        let dir = first_path.parent().unwrap();
        let last = dir.join("last.replay");
        assert_eq!(
            std::fs::read_link(&last).unwrap(),
            first_path.file_name().unwrap(),
            "last.replay must be a relative symlink to the timestamped recording"
        );

        let second = prepare_flight_recorder(
            state.path(), "games/match3", "0123456789abcdef", 1_777_000_001_456, 41,
        ).unwrap();
        let second_path = std::path::Path::new(&second.record_path);
        assert_ne!(first.record_path, second.record_path);
        assert_eq!(std::fs::read_link(&last).unwrap(), second_path.file_name().unwrap());
        assert_eq!(first_path.parent(), second_path.parent(), "one logical source owns one archive");
    }

    /// The flight recorder must not synchronously hash the whole world during
    /// ordinary play. Desync checkpoints remain an explicit opt-in.
    #[test]
    fn flight_checkpoint_cadence_is_off_by_default_and_overridable() {
        assert_eq!(parse_checkpoint_interval(None), FLIGHT_CHECKPOINT_INTERVAL);
        assert_eq!(FLIGHT_CHECKPOINT_INTERVAL, 0, "ordinary play must not pay a periodic checkpoint stall");
        assert_eq!(parse_checkpoint_interval(Some("0")), 0);
        assert_eq!(parse_checkpoint_interval(Some(" 120 ")), 120);
        assert_eq!(
            parse_checkpoint_interval(Some("often")),
            FLIGHT_CHECKPOINT_INTERVAL,
            "an unparseable cadence falls back to the non-perturbing default"
        );
    }

    #[test]
    fn replay_usage_counts_recordings_without_double_counting_last() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.replay"), vec![b'a'; 300]).unwrap();
        std::fs::write(dir.path().join("b.replay"), vec![b'b'; 700]).unwrap();
        std::fs::write(dir.path().join("notes.txt"), vec![b'n'; 900]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("b.replay", dir.path().join("last.replay")).unwrap();

        let (bytes, recordings) = replay_usage(dir.path());
        assert_eq!(recordings, 2);
        assert_eq!(bytes, 1_000);
        assert!(replay_usage_warning(dir.path(), REPLAY_USAGE_WARN_BYTES - 1, 99).is_none());
        let warning = replay_usage_warning(dir.path(), REPLAY_USAGE_WARN_BYTES, recordings)
            .expect("the archive warns at its budget");
        assert!(warning.contains("2 recording(s)"), "{warning}");
        assert!(warning.contains(&dir.path().display().to_string()), "{warning}");
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

#[cfg(test)]
mod watch_reload_tests {
    use super::{categorize_stone_changes, retain_production_changes, stone_source_watch_paths, StoneWatcher};
    use crate::game_runtime::{GameCompiler, SkullGameCompiler};
    use crate::FileChangeEvent;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::mpsc::{channel, Receiver};
    use std::time::{Duration, Instant};

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        edited: PathBuf,
        main: PathBuf,
        compiler: SkullGameCompiler,
    }

    impl Fixture {
        fn new(sibling: bool) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("tmp/game");
            std::fs::create_dir_all(root.join("tmp")).unwrap();
            let root = root.canonicalize().unwrap();
            let manifest = if sibling {
                "[package]\nname = \"watch-game\"\n[dependencies]\nhelper = { path = \"../helper\" }\n"
            } else {
                "[package]\nname = \"watch-game\"\n"
            };
            std::fs::write(root.join("stick.toml"), manifest).unwrap();
            let caller = "global SCORE: int = 0\n@nomangle fn bump() { SCORE = leaf(SCORE) }\n";
            let leaf = "fn leaf(a: int) -> int { a + 1 }\n";
            let edited = if sibling {
                let helper = root.parent().unwrap().join("helper");
                std::fs::create_dir_all(&helper).unwrap();
                std::fs::write(helper.join("stick.toml"), "[package]\nname = \"helper\"\n").unwrap();
                std::fs::write(root.join("main.rock"), caller).unwrap();
                helper.join("lib.rock")
            } else {
                root.join("main.rock")
            };
            std::fs::write(&edited, if sibling { leaf.to_string() } else { format!("{caller}{leaf}") }).unwrap();
            let main = root.join("tmp/main.lua");
            let mut compiler = SkullGameCompiler::new(root.to_str().unwrap(), main.clone(), root.join("tmp/incremental.lua"));
            compiler.compile(&HashSet::new(), false).expect("initial compile");
            Self { _dir: dir, root, edited, main, compiler }
        }

        fn assert_reload(mut self, atomic: bool) {
            let before = std::fs::read_to_string(&self.main).unwrap();
            let roots = stone_source_watch_paths(self.root.to_str().unwrap(), &self.compiler);
            let (tx, rx) = channel();
            let mut watcher = StoneWatcher::new(Default::default(), Vec::new(), tx);
            watcher.refresh(roots).unwrap();
            let text = std::fs::read_to_string(&self.edited).unwrap().replace("a + 1", "a + 10");
            if atomic {
                let temporary = self.edited.with_extension("saving");
                std::fs::write(&temporary, text).unwrap();
                std::fs::rename(temporary, &self.edited).unwrap();
            } else {
                std::fs::write(&self.edited, text).unwrap();
            }
            let event = receive_path(&rx, &self.edited);
            let mut changes = categorize_stone_changes(&event, &Default::default(), &Default::default());
            retain_production_changes(&mut changes.changed_paths, self.root.to_str().unwrap(), self.compiler.package_roots(), &Default::default());
            assert!(changes.changed_paths.contains(&self.edited), "Stone dropped source edit: {:?}", event.paths);
            assert!(!changes.needs_compiler_rebuild && !changes.needs_libstone_rebuild);
            let outcome = self.compiler.compile(&changes.changed_paths, false).expect("reload compile");
            assert!(outcome.restart_reasons.is_empty(), "body edit must preserve live state");
            assert!(before == std::fs::read_to_string(&self.main).unwrap(), "body edit must preserve main.lua");
            let after = std::fs::read_to_string(self.main.with_file_name("incremental.lua")).expect("body-edit overlay");
            let script = format!("HOT_RELOAD = false\n{before}\nbump()\nHOT_RELOAD = true\n{after}\nHOT_RELOAD = false\nbump()\nassert(SCORE == 11, tostring(SCORE))\nprint('WATCH_RELOAD_PASS')\n");
            let output = rock_host::lua_runner::run_lua_code(&script).expect("run reloaded Lua");
            assert!(output.stdout.contains("WATCH_RELOAD_PASS"), "{}", output.combined());
        }
    }

    fn receive_path(rx: &Receiver<FileChangeEvent>, expected: &PathBuf) -> FileChangeEvent {
        let deadline = Instant::now() + Duration::from_secs(3);
        while let Ok(event) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            if event.paths.contains(expected) {
                return event;
            }
        }
        panic!("watcher never reported {}", expected.display());
    }

    // FSEvents can miss a write made shortly after `refresh` returns, and can
    // deliver the fixture's own setup writes, batched, after the stream starts;
    // both happen arbitrarily late under load. Rewrite `path` until it is
    // reported, so the wait covers a lost first write.
    fn write_until_reported(rx: &Receiver<FileChangeEvent>, path: &PathBuf, text: &str) -> FileChangeEvent {
        for _ in 0..8 {
            std::fs::write(path, text).unwrap();
            let attempt_end = Instant::now() + Duration::from_millis(1500);
            while let Ok(event) = rx.recv_timeout(attempt_end.saturating_duration_since(Instant::now())) {
                if event.paths.contains(path) {
                    return event;
                }
            }
        }
        panic!("watcher never reported {}", path.display());
    }

    // Events arrive in order, so once a fresh barrier file is reported every
    // earlier event, including the fixture's setup writes, has been consumed.
    // Returns the barrier, whose own trailing events may still arrive later.
    fn drain_setup_events(rx: &Receiver<FileChangeEvent>, dir: &std::path::Path) -> PathBuf {
        let barrier = dir.join("barrier.rock");
        write_until_reported(rx, &barrier, "fn barrier() {}\n");
        barrier
    }

    #[test]
    fn stone_direct_save_under_tmp_ancestor_reloads_live_state() {
        Fixture::new(false).assert_reload(false);
    }

    #[test]
    fn stone_atomic_save_under_tmp_ancestor_reloads_live_state() {
        Fixture::new(false).assert_reload(true);
    }

    #[test]
    fn stone_sibling_dependency_edit_reloads_live_state() {
        Fixture::new(true).assert_reload(false);
    }

    #[test]
    fn stone_sim_edit_does_not_rebuild_or_restart_the_game() {
        let fixture = Fixture::new(false);
        let sims = fixture.root.join("tests/sims");
        std::fs::create_dir_all(&sims).unwrap();
        let sim = sims.join("probe.rock");
        std::fs::write(&sim, "fn probe() {}\n").unwrap();
        let before = std::fs::read(&fixture.main).unwrap();
        let (tx, rx) = channel();
        let mut watcher = StoneWatcher::new(Default::default(), Vec::new(), tx);
        watcher.refresh(stone_source_watch_paths(fixture.root.to_str().unwrap(), &fixture.compiler)).unwrap();
        let barrier = drain_setup_events(&rx, &sims);

        let event = write_until_reported(&rx, &sim, "fn probe( {\n");
        let mut changes = categorize_stone_changes(&event, &Default::default(), &Default::default());
        // Setup writes of the fixture may be reported in the same batch; they are
        // not the edit under test.
        for setup in [&barrier, &fixture.root.join("main.rock"), &fixture.root.join("stick.toml")] {
            changes.changed_paths.remove(setup);
        }
        assert_eq!(changes.changed_paths, HashSet::from([sim.clone()]));
        retain_production_changes(&mut changes.changed_paths, fixture.root.to_str().unwrap(), fixture.compiler.package_roots(), &Default::default());
        assert!(changes.changed_paths.is_empty(), "sim edit reached the game compiler");
        assert_eq!(std::fs::read(&fixture.main).unwrap(), before, "sim edit rewrote the game artifact");

        std::fs::remove_file(&sim).unwrap();
        let mut removed = HashSet::from([sim.clone()]);
        retain_production_changes(&mut removed, fixture.root.to_str().unwrap(), fixture.compiler.package_roots(), &Default::default());
        assert!(removed.is_empty(), "deleted sim reached the game compiler");

        let inputs: crate::file_watcher::SharedWatchSet = Default::default();
        inputs.lock().insert(sim.clone());
        let mut embedded = HashSet::from([sim.clone()]);
        retain_production_changes(&mut embedded, fixture.root.to_str().unwrap(), fixture.compiler.package_roots(), &inputs);
        assert_eq!(embedded, HashSet::from([sim]), "declared embedded input must remain watched");
    }

    #[test]
    fn stone_scratch_and_unrelated_files_do_not_reload_but_restart_does() {
        let fixture = Fixture::new(false);
        let scratch = fixture.root.join("tmp/generated.rock");
        let restart_dir = fixture.root.parent().unwrap().join("tmp");
        std::fs::create_dir_all(&restart_dir).unwrap();
        let restart = restart_dir.join("restart.txt");
        let (tx, rx) = channel();
        let mut watcher = StoneWatcher::new(Default::default(), vec![restart.clone()], tx);
        watcher.refresh(stone_source_watch_paths(fixture.root.to_str().unwrap(), &fixture.compiler)).unwrap();
        std::fs::write(&scratch, "fn scratch() {}\n").unwrap();
        std::fs::write(restart_dir.join("unrelated.rock"), "fn unrelated() {}\n").unwrap();
        std::fs::write(&restart, "restart").unwrap();
        let event = receive_path(&rx, &restart);
        assert!(event.is_restart_trigger);
        assert_eq!(event.paths, HashSet::from([restart]));
        let changes = categorize_stone_changes(&event, &Default::default(), &Default::default());
        assert!(changes.changed_paths.is_empty());
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "scratch caused another event");
    }

    #[test]
    fn stone_embedded_inputs_follow_recompilation_and_atomic_saves() {
        // Invoking a package subdirectory loads its sources, but its legal
        // embedded inputs can still live elsewhere inside the package root.
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("tmp/package");
        let source_root = package.join("src");
        std::fs::create_dir_all(&source_root).unwrap();
        let package = package.canonicalize().unwrap();
        let source_root = source_root.canonicalize().unwrap();
        std::fs::create_dir_all(package.join("data")).unwrap();
        std::fs::create_dir_all(package.join("other")).unwrap();
        std::fs::create_dir_all(package.join("tmp")).unwrap();
        std::fs::write(package.join("stick.toml"), "[package]\nname = \"embed-watch\"\n").unwrap();
        let source = source_root.join("main.rock");
        std::fs::write(&source, "@nomangle fn banner() -> string { embed_string(\"data/value.txt\") }\n").unwrap();
        let input = package.join("data/value.txt");
        let next_input = package.join("other/value.txt");
        std::fs::write(&input, "watch-value-one").unwrap();
        std::fs::write(&next_input, "watch-value-two").unwrap();
        let main = package.join("tmp/main.lua");
        let mut compiler = SkullGameCompiler::new(source_root.to_str().unwrap(), main.clone(), package.join("tmp/incremental.lua"));
        compiler.compile(&HashSet::new(), false).unwrap();
        let extra: crate::file_watcher::SharedWatchSet = Default::default();
        *extra.lock() = compiler.comptime_watch_files().into_iter().collect();
        assert!(extra.lock().contains(&input));
        let (tx, rx) = channel();
        let mut watcher = StoneWatcher::new(extra.clone(), Vec::new(), tx);
        watcher.refresh(stone_source_watch_paths(source_root.to_str().unwrap(), &compiler)).unwrap();
        // An input-parent watch must not admit even a Rock source beside it.
        std::fs::write(package.join("data/unrelated.rock"), "fn unrelated() {}\n").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());

        for (text, atomic) in [("watch-direct", false), ("watch-atomic", true)] {
            if atomic {
                let temporary = input.with_extension("saving");
                std::fs::write(&temporary, text).unwrap();
                std::fs::rename(temporary, &input).unwrap();
            } else {
                std::fs::write(&input, text).unwrap();
            }
            let event = receive_path(&rx, &input);
            let changes = categorize_stone_changes(&event, &Default::default(), &extra);
            assert_eq!(changes.changed_paths, HashSet::from([input.clone()]));
            compiler.compile(&changes.changed_paths, false).unwrap();
            compiler.canonicalize_for_restart().unwrap();
            let lua = std::fs::read_to_string(&main).unwrap();
            let output = rock_host::lua_runner::run_lua_code(&format!("{lua}\nprint(banner())")).unwrap();
            assert_eq!(output.stdout.trim(), text, "{}", output.combined());
        }

        std::fs::write(&source, "@nomangle fn banner() -> string { embed_string(\"other/value.txt\") }\n").unwrap();
        let event = receive_path(&rx, &source);
        let changes = categorize_stone_changes(&event, &Default::default(), &extra);
        compiler.compile(&changes.changed_paths, false).unwrap();
        compiler.canonicalize_for_restart().unwrap();
        *extra.lock() = compiler.comptime_watch_files().into_iter().collect();
        assert!(!extra.lock().contains(&input));
        assert!(extra.lock().contains(&next_input));
        watcher.refresh(stone_source_watch_paths(source_root.to_str().unwrap(), &compiler)).unwrap();
        std::fs::write(&input, "unused input").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        std::fs::write(&next_input, "watch-refreshed").unwrap();
        let event = receive_path(&rx, &next_input);
        let changes = categorize_stone_changes(&event, &Default::default(), &extra);
        compiler.compile(&changes.changed_paths, false).unwrap();
        compiler.canonicalize_for_restart().unwrap();
        let lua = std::fs::read_to_string(main).unwrap();
        let output = rock_host::lua_runner::run_lua_code(&format!("{lua}\nprint(banner())")).unwrap();
        assert_eq!(output.stdout.trim(), "watch-refreshed", "{}", output.combined());
    }


    #[test]
    fn stone_added_dependency_is_watched_after_manifest_reload() {
        let mut fixture = Fixture::new(false);
        let helper = fixture.root.parent().unwrap().join("new-helper");
        std::fs::create_dir_all(&helper).unwrap();
        std::fs::write(helper.join("stick.toml"), "[package]\nname = \"new-helper\"\n").unwrap();
        let source = helper.join("lib.rock");
        std::fs::write(&source, "@nomangle fn library_banner() -> int { 7 }\n").unwrap();
        let (tx, rx) = channel();
        let mut watcher = StoneWatcher::new(Default::default(), Vec::new(), tx);
        watcher.refresh(stone_source_watch_paths(fixture.root.to_str().unwrap(), &fixture.compiler)).unwrap();
        let manifest = fixture.root.join("stick.toml");
        std::fs::write(&manifest, "[package]\nname = \"watch-game\"\n[dependencies]\nhelper = { path = \"../new-helper\" }\n").unwrap();
        let event = receive_path(&rx, &manifest);
        let changes = categorize_stone_changes(&event, &Default::default(), &Default::default());
        fixture.compiler.compile(&changes.changed_paths, false).unwrap();
        fixture.compiler.canonicalize_for_restart().unwrap();
        watcher.refresh(stone_source_watch_paths(fixture.root.to_str().unwrap(), &fixture.compiler)).unwrap();
        let before = std::fs::read_to_string(&fixture.main).unwrap();
        std::fs::write(&source, "@nomangle fn library_banner() -> int { 8 }\n").unwrap();
        let event = receive_path(&rx, &source);
        let changes = categorize_stone_changes(&event, &Default::default(), &Default::default());
        let outcome = fixture.compiler.compile(&changes.changed_paths, false).unwrap();
        assert!(outcome.restart_reasons.is_empty());
        assert!(before == std::fs::read_to_string(&fixture.main).unwrap(), "body edit must preserve main.lua");
        let after = std::fs::read_to_string(fixture.main.with_file_name("incremental.lua")).expect("dependency body-edit overlay");
        let script = format!("HOT_RELOAD = false\n{before}\nassert(library_banner() == 7)\nHOT_RELOAD = true\n{after}\nHOT_RELOAD = false\nprint(library_banner())");
        let output = rock_host::lua_runner::run_lua_code(&script).unwrap();
        assert_eq!(output.stdout.trim(), "8", "{}", output.combined());
    }

}
