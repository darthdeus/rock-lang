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

/// Is `name` a compiler crate dir? The bare `rock`/`skull`/`bone` crates plus
/// every `rock-*`/`skull-*`/`bone-*`. NOT `stone*` (those are the engine, rebuilt
/// separately as libstone).
fn is_compiler_crate_dir(name: &str) -> bool {
    matches!(name, "rock" | "skull" | "bone")
        || name.starts_with("rock-")
        || name.starts_with("skull-")
        || name.starts_with("bone-")
}

/// Every compiler crate's `src/` under the workspace root (CWD -- stone runs
/// after `ensure_workspace_root`). Enumerated rather than hardcoded so a newly
/// added crate is watched automatically.
fn compiler_crate_srcs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(".") {
        for e in entries.flatten() {
            if is_compiler_crate_dir(&e.file_name().to_string_lossy()) {
                let src = e.path().join("src");
                if src.is_dir() {
                    dirs.push(src);
                }
            }
        }
    }
    dirs
}

/// Classify a changed path as a compiler source (-> rebuild the rock binary):
/// a file under a compiler crate's `src/`, or any `Cargo.toml`/`Cargo.lock`.
/// Matches on the crate dir immediately before `/src/`, so it works for relative
/// and absolute paths and excludes the engine (`stone-rs/src`, handled earlier).
fn is_compiler_source(path_str: &str) -> bool {
    if path_str.contains("Cargo.") {
        return true;
    }
    if let Some(idx) = path_str.find("/src/") {
        let crate_name = path_str[..idx].rsplit('/').next().unwrap_or("");
        return is_compiler_crate_dir(crate_name);
    }
    false
}

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
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            error!("stone: cannot find own exe to re-exec ({e}); staying on the old compiler binary");
            return;
        }
    };
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    info!("Compiler changed -- re-exec'ing rock with the new binary");

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // exec replaces the image; returns only on failure.
        let err = Command::new(&exe).args(&args).exec();
        error!("stone: re-exec failed ({err}); staying on the old compiler binary (Ctrl-C + rerun to pick up the change)");
    }
    #[cfg(not(unix))]
    {
        match Command::new(&exe).args(&args).spawn() {
            Ok(_) => std::process::exit(0),
            Err(e) => error!("stone: re-exec spawn failed ({e}); staying on the old compiler binary"),
        }
    }
}

pub fn do_stone_watch(
    path: &str,
    quiet: bool,
    app_args: &[String],
    backend: crate::Backend,
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

    // Start the game process only if initial compilation succeeded
    let mut game_process = if initial_compilation_success {
        Some(start_game_process(app_args)?)
    } else {
        None
    };

    // Set up file watcher for game files, stdlib, stone (engine Rock code),
    // stone-rs / stone-video / stone-3d (engine Rust crates), the rock
    // compiler, and tmp/ for the restart-trigger file. FileWatcher silently
    // skips nonexistent paths, so listing the optional crates is safe on
    // machines where they aren't checked out.
    let mut watch_paths = vec![
        PathBuf::from(path),
        // The stdlib is read from disk at runtime, so watch the resolved
        // directory (honors ROCK_STDLIB_DIR; falls back to the repo `stdlib/`)
        // rather than a cwd-relative `stdlib`. A stdlib edit is not a compiler
        // source, so it re-cooks the game (which reads the fresh stdlib), no
        // binary rebuild.
        rock_host::stdlib_loader::stdlib_dir(),
        PathBuf::from("stone"),
        PathBuf::from("stone-rs/src"),
        PathBuf::from("stone-rs/Cargo.toml"),
        PathBuf::from("stone-rs/build.rs"),
        PathBuf::from("stone-video/src"),
        PathBuf::from("stone-video/Cargo.toml"),
        PathBuf::from("stone-video/build.rs"),
        PathBuf::from("stone-3d/src"),
        PathBuf::from("stone-3d/Cargo.toml"),
        PathBuf::from("Cargo.toml"),
        PathBuf::from("Cargo.lock"),
        PathBuf::from("tmp"),  // Watch tmp/ for restart.txt trigger
    ];
    // Every compiler crate's src/ -- the bare `rock`/`skull`/`bone` crates plus
    // every `rock-*`/`skull-*`/`bone-*`. Enumerated from the workspace root so a
    // new crate is watched without editing this list. A codegen edit there must
    // trigger a compiler rebuild+restart; without it a live stone session keeps
    // emitting against the old in-memory codegen (stale behavior, and during a
    // scaffold migration a stale main.lua the overlay can't load against).
    watch_paths.extend(compiler_crate_srcs());

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
            if path_str.contains("stone-rs/") || path_str.contains("stone-video/")
                || path_str.contains("stone-3d/") {
                needs_libstone_rebuild = true;
                changed_libstone.push(filename);
            } else if is_compiler_source(&path_str) {
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
            match build_compiler() {
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
                Ok(_) => {
                    // Consumed successfully; start the next edit clean. On error
                    // we keep them so the next tick retries the same changes.
                    changed_paths.clear();
                    compilation_succeeded = true;
                    clear_error_file();
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
            info!("First successful compilation! Starting game...");
            game_process = Some(start_game_process(app_args)?);
            info!("Game started successfully");
        }
        // Restart the game process if a rebuild actually produced new
        // artifacts (or the user manually triggered) and the game is up.
        else if should_restart {
            if let Some(ref mut process) = game_process {
                // A restart re-spawns luajit, which loads `main.lua` only -- it
                // does NOT dual-load any accumulated overlay. Let the backend
                // fold its overlay back into `main.lua` first (no-op for rock,
                // and for skull when no overlay is on disk) so the restarted
                // process gets the complete current program, not a stale baseline.
                if let Err(e) = compiler.canonicalize_for_restart() {
                    error!("fold before restart failed: {} (restart may load a stale main.lua)", e);
                }
                info!("Restarting game process...");
                kill_game_process(process)?;
                game_process = Some(start_game_process(app_args)?);
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
    let needs_3d = rock_host::engine_build::game_requests_3d(game_path)?
        && std::path::Path::new("stone-3d").exists();

    // Two invocations are required. A single `cargo build -p stone-rs
    // -p rock-cli --bin rock` builds only the `rock` bin (the `--bin`
    // filter narrows to that target across all listed packages), so
    // stone-rs's cdylib never gets compiled and the subsequent
    // stage_cdylib trips on a missing libstone_rs.dylib in fresh
    // checkouts / worktrees. Build the engine libs (no --bin filter)
    // first, then the compiler bin.
    info!("Building engine + compiler (cargo)...");

    let mut lib_args: Vec<&str> = vec!["build", "-p", "stone-rs"];
    if needs_video {
        lib_args.extend(["-p", "stone-video"]);
    }
    if needs_3d {
        lib_args.extend(["-p", "stone-3d"]);
    }
    rock_host::engine_build::run_cargo(&lib_args, "Failed to build engine libs")?;

    rock_host::engine_build::run_cargo(
        &["build", "-p", "rock-cli", "--bin", "rock"],
        "Failed to build rock compiler",
    )?;

    rock_host::engine_build::stage_cdylib("stone_rs", "stone")?;
    if needs_video {
        rock_host::engine_build::stage_cdylib("stone_video", "stone_video")?;
    }
    if needs_3d {
        rock_host::engine_build::stage_cdylib("stone_3d", "stone_3d")?;
    }

    Ok(())
}

/// Build the Rock compiler
fn build_compiler() -> Result<()> {
    rock_host::engine_build::run_cargo(&["build", "--bin", "rock"], "Failed to build compiler")
}

/// Start the game process with the correct library path and robust process management
fn start_game_process(app_args: &[String]) -> Result<Child> {
    let lua_file = rock_host::project::default_main_lua_path();

    // Platform-specific process management for orphan prevention
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;

        let mut cmd = rock_host::project::build_luajit_command(&lua_file, app_args);
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        // Only the stone watch loop enables the in-game hot-reload watcher.
        cmd.env("STONE_HOT_RELOAD", "1");

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

    // The GC-stopped window (profiler: warm 100 frames + N measured)
    // must run to completion to produce a result. Force a frame cap
    // that covers it (overriding a smaller --frames / --seconds) and
    // drop the seconds cap so frames deterministically govern.
    let seconds = if gc_window > 0 {
        let need = 100 + gc_window + 20;
        frame_cap = Some(frame_cap.map_or(need, |c| c.max(need)));
        None
    } else {
        seconds
    };

    let mut cmd = rock_host::project::build_luajit_command(&lua_file, app_args);
    cmd.env("PERF_MODE", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

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
    let wall_budget = seconds.unwrap_or(0.0).max(15.0) + 15.0;
    let timeout_duration = Duration::from_secs_f64(wall_budget.max(30.0));

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

    if process_result.timed_out {
        warn!("Process timed out after {}s", timeout_duration.as_secs());
    } else if !process_result.output.status.success() {
        log::error!(target: "STONE",
            "stone exited with code: {:?}", process_result.output.status.code());
    }

    Ok(())
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

    // Allow more time when app_args are present (the app may have its
    // own work to do, e.g. headless video extraction), or when
    // dump_state is set (JSON serialization of all globals can be slow).
    // ROCK_FRAME_TIMEOUT_SECS overrides the wall-clock watchdog for known
    // long-running `rock frame` jobs (e.g. `just pac bake`, which renders
    // thousands of frames at ~60fps and legitimately exceeds 30s).
    let timeout_duration = if let Some(secs) = std::env::var("ROCK_FRAME_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        Duration::from_secs(secs)
    } else if !app_args.is_empty() {
        Duration::from_secs(30)
    } else if dump_state {
        Duration::from_secs(15)
    } else {
        Duration::from_secs(3)
    };
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

    if process_result.timed_out {
        warn!("Stone process timed out after {} seconds", timeout_duration.as_secs());
        warn!("This usually indicates a runtime error or infinite loop in the game code");
    } else if process_result.output.status.success() {
        if frames == 1 {
            info!("One frame executed successfully");
        } else {
            info!("{} frames executed successfully", frames);
        }
    } else {
        error!("Stone exited with code: {:?}", process_result.output.status.code());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_compiler_source;

    #[test]
    fn compiler_source_classification() {
        // Compiler crate src files -> rebuild the rock binary.
        assert!(is_compiler_source("rock/src/main.rs"));
        assert!(is_compiler_source("rock-types/src/lib.rs"));
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
}
