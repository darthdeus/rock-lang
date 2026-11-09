const SESSION_RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

use crate::compiler_watch::is_compiler_source;
use crate::*;
use std::process::{Child, Stdio};

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
                // `_exit`, not `exit`. These are `fork` children of a
                // multithreaded process (rayon builds a global pool), so they
                // must touch nothing the parent owns: a lock another thread held
                // at fork time stays held, and `exit` would flush the parent's
                // inherited stdio buffers.
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

/// Replace this process with the freshly rebuilt `rock` binary, preserving the
/// original `stone` arguments. The new process re-runs `do_stone_watch` from
/// scratch -- `IncrementalProject::init` re-emits main.lua with the NEW codegen -- so a
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
        error!(
            "stone: {e}; staying on the old compiler binary (Ctrl-C + rerun to pick up the change)"
        );
    }
}

fn full_reload_requires_restart(restart_reasons: &[String]) -> bool {
    !restart_reasons.is_empty()
}

#[derive(Default)]
struct StoneChanges {
    needs_libstone_rebuild: bool,
    needs_native_rebuild: bool,
    needs_compiler_rebuild: bool,
    sound_dirty: bool,
    changed_libstone: Vec<String>,
    changed_compiler: Vec<String>,
    changed_paths: std::collections::HashSet<PathBuf>,
}

fn categorize_stone_changes(
    event: &FileChangeEvent,
    sound_paths: &std::collections::HashSet<PathBuf>,
    sound_inputs: &crate::package::SoundWatchInputs,
    comptime: &std::collections::HashSet<PathBuf>,
    native: &rock_engine_host::native_libraries::NativeInputs,
) -> StoneChanges {
    let mut changes = StoneChanges::default();
    for path in &event.paths {
        if native.excludes(path) {
            continue;
        }
        // Normalize separators once so the substring checks below work
        // identically on Windows (`stone-rs\src\foo.rs`) and Unix
        // (`stone-rs/src/foo.rs`). All match strings use `/`.
        let path_str = path.to_string_lossy().replace('\\', "/");

        if sound_paths.contains(path.as_path()) || sound_inputs.contains(path) {
            changes.sound_dirty = true;
            continue;
        }

        let filename = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();

        if native.contains(path) && !path_str.ends_with(".rock") && filename != "stick.toml" {
            changes.needs_native_rebuild = true;
            continue;
        }
        let is_engine_crate = path_str.contains("stone-rs/");
        if is_engine_crate {
            changes.needs_libstone_rebuild = true;
            changes.changed_libstone.push(filename);
        } else if is_compiler_source(path) {
            changes.needs_compiler_rebuild = true;
            changes.changed_compiler.push(filename);
        } else if path_str.ends_with(".rock") {
            changes.changed_paths.insert(path.clone());
        } else if comptime.contains(path.as_path()) {
            // An embedded input is a source: routing it as a changed
            // non-source path makes LiveProject take the full-rebuild arm,
            // which re-reads the file .
            changes.changed_paths.insert(path.clone());
        } else if filename == "stick.toml" {
            changes.needs_native_rebuild = true;
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
    comptime: &std::collections::HashSet<PathBuf>,
) {
    changes.retain(|path| {
        if path.extension().is_none_or(|extension| extension != "rock") || comptime.contains(path) {
            return true;
        }
        let canonical = path.canonicalize().or_else(|_| {
            let parent = path
                .parent()
                .ok_or_else(|| std::io::Error::other("no parent"))?;
            let name = path
                .file_name()
                .ok_or_else(|| std::io::Error::other("no file name"))?;
            parent.canonicalize().map(|parent| parent.join(name))
        });
        let Ok(canonical) = canonical else {
            return true;
        };
        let roots = package_roots.unwrap_or(&[]);
        let owning_root = roots
            .iter()
            .filter_map(|(_, root)| {
                let root = root.canonicalize().ok()?;
                canonical.strip_prefix(&root).ok().map(|_| root)
            })
            .max_by_key(|root| root.components().count())
            .or_else(|| {
                roots.is_empty().then(|| {
                    PathBuf::from(project)
                        .canonicalize()
                        .unwrap_or_else(|_| PathBuf::from(project))
                })
            });
        let Some(root) = owning_root else { return true };
        let Ok(relative) = canonical.strip_prefix(&root) else {
            return true;
        };
        !crate::SourceFiles::is_test_only_relative_path(relative)
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
        Self {
            handle: None,
            paths: Vec::new(),
            inputs: Default::default(),
            extra_files,
            restart_files,
            tx,
        }
    }

    fn refresh(
        &mut self,
        paths: Vec<PathBuf>,
        inputs: &std::collections::HashSet<PathBuf>,
    ) -> Result<()> {
        if self.handle.is_some() && self.paths == paths && self.inputs == *inputs {
            return Ok(());
        }
        // Arm new roots and input parents before dropping the previous watches.
        let handle = FileWatcher::default_for_rock_sources()
            .with_restart_suffix(PathBuf::from("tmp/restart.txt"))
            .with_initial_create_filter(true)
            .with_restart_files(self.restart_files.clone())
            .with_extensions(vec![
                "rock".to_string(),
                "rs".to_string(),
                "c".to_string(),
                "h".to_string(),
                "toml".to_string(),
            ])
            .with_shared_extra_files(self.extra_files.clone())
            .watch(paths.clone(), self.tx.clone())?;
        self.handle = Some(handle);
        self.paths = paths;
        self.inputs = inputs.clone();
        Ok(())
    }
}

/// Content hash anchoring a declared input the compiler never reads (the
/// sound project file): the hash of what is on disk now. An unreadable file
/// anchors to 0, which only costs one extra watcher event before the seed
/// converges on the file's real bytes.
fn declaration_hash(path: &std::path::Path) -> u64 {
    std::fs::read_to_string(path)
        .map(|text| skull_mir::mir::comptime_value_hash(&text))
        .unwrap_or(0)
}

/// Reseed the shared comptime set and its routing mirror after a successful
/// build: the compiled program's declared inputs plus the sound project file.
fn reseed_comptime_watch(
    compiler: &dyn crate::game_runtime::GameCompiler,
    sound_paths: &std::collections::HashSet<PathBuf>,
    watched: &mut std::collections::HashSet<PathBuf>,
    comptime_watch: &crate::file_watcher::SharedWatchSet,
) {
    let mut hashed = compiler.comptime_watch_files_with_hashes();
    hashed.extend(
        sound_paths
            .iter()
            .map(|path| (path.clone(), declaration_hash(path))),
    );
    *watched = hashed.iter().map(|(path, _)| path.clone()).collect();
    comptime_watch.replace_with_hashes(hashed);
}

/// Produce a `rock stone` session's first program and return the identity the
/// game process reports for it. When the shared artifact cache has one for the
/// current content key, the session starts from that Lua and compiles nothing;
/// otherwise it compiles and publishes the result for the next session.
fn initial_session_build(
    cache_root: Option<&std::path::Path>,
    backend: crate::Backend,
    path: &str,
    output_path: &std::path::Path,
    compiler: &mut dyn crate::game_runtime::GameCompiler,
) -> Result<String> {
    use crate::frame_compile_cache::{SessionStart, session_start, store_session_artifact};
    let pending = match session_start(cache_root, backend, path, output_path)? {
        SessionStart::Hit(cached) => {
            let fingerprint = cached.artifact_fingerprint.clone();
            compiler.adopt_cached_artifact(cached)?;
            return Ok(fingerprint);
        }
        SessionStart::Miss(pending) => Some(pending),
        SessionStart::Off => None,
    };
    compiler.compile(&std::collections::HashSet::new(), false)?;
    let fingerprint = compiler.artifact_fingerprint()?;
    if let (Some(root), Some(pending)) = (cache_root, pending) {
        let embedded = compiler
            .comptime_watch_files_with_hashes()
            .into_iter()
            .map(|(path, _)| path)
            .collect::<Vec<_>>();
        if let Err(error) = store_session_artifact(
            root,
            &pending,
            output_path,
            &fingerprint,
            compiler.comptime_inputs(),
            &embedded,
        ) {
            log::warn!(target: "STONE", "cannot store this session's compile for the next launch: {error:#}");
        }
    }
    Ok(fingerprint)
}

pub(crate) fn do_stone_watch(
    path: &str,
    quiet: bool,
    app_args: &[String],
    backend: crate::Backend,
) -> Result<()> {
    if !quiet {
        info!("Starting Stone development environment");
        info!("Watching: {}, stdlib/, stone/, compiler", path);
        info!("Game will hot reload on code changes");
        if rock_engine_host::engine_build::prebuilt_game() {
            info!("Rock and Stone are prebuilt; project native libraries rebuild on edits");
        } else {
            info!("Process will restart on stone-rs/compiler changes");
        }
    }

    // Build engine cdylibs (stone-rs + optional extras per game's
    // [engine] needs) and the rock compiler in a single cargo invocation.
    // Two separate `cargo build` calls each pay cargo's graph-walk cost
    // and serialize on the target lock; one combined call halves the
    // perceived startup pause and streams progress to the user.
    build_all_for_game(path)?;

    let output_path =
        rock_engine_host::project::stone_session_main_lua_path(path, std::process::id());
    if let Some(parent) = output_path.parent() {
        fs_err::create_dir_all(parent)?;
    }
    if !quiet {
        info!("Session output: {}", output_path.display());
    }
    prune_stone_sessions(std::path::Path::new(
        rock_engine_host::project::STONE_SESSION_DIR,
    ));

    // The backend-specific incremental game compiler (skull: IncrementalProject two-file
    // split). It is the ONLY backend-specific piece of this loop; everything else
    // (builds, process, restart) is shared. The runtime owns the changed-path
    // accumulator the watcher feeds it.
    //
    // The compiler gets the canonical project root, which is also what the
    // artifact cache keys on: a compile stamps its `embed_root` from the dir it
    // was handed, so a relative dir would key the session's own artifact
    // differently from the `rock frame` compile of the same sources.
    let compiler_dir = PathBuf::from(path)
        .canonicalize()
        .map_err(|error| anyhow::anyhow!("canonicalize the project root {path}: {error}"))?;
    let compiler_dir = compiler_dir.to_string_lossy().into_owned();
    let mut compiler = crate::game_runtime::game_compiler_for(backend, &compiler_dir, &output_path);
    let mut changed_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    // embed_string inputs are sources the extension filter can't know
    // : the set comes from the compiled program, seeded
    // after every successful build, and shared with the watcher's filter.
    // `watched` mirrors the shared set for routing; every reseed writes both.
    let comptime_watch: crate::file_watcher::SharedWatchSet = Default::default();
    let mut sound_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    if let Some(sound_project) =
        (crate::registry::RuntimeServices::registered().sound_project_path)(path)?
    {
        sound_paths.insert(sound_project.clone());
        let hash = declaration_hash(&sound_project);
        comptime_watch.insert_with_hash(sound_project, hash);
    }
    let mut sound_inputs = crate::package::sound_watch_inputs(path).unwrap_or_else(|error| {
        log::error!(target: "SOUND", "cannot watch sound inputs: {error}; watching the project declaration for repair");
        Default::default()
    });
    let mut watched: std::collections::HashSet<PathBuf> = sound_paths.clone();

    // Do initial compilation.
    // If it fails, we'll continue watching and start the game when compilation succeeds
    let cache_root = match crate::frame_compile_cache::cache_root() {
        Ok(root) => root,
        Err(error) => {
            log::warn!(target: "STONE", "compile cache disabled: {error:#}");
            None
        }
    };
    let report_initial_failure = |error: &dyn std::fmt::Display| {
        let error_message = format!("Initial compilation failed: {}", error);
        log::error!(target: "STONE", "{}", error_message);
        log::info!(target: "STONE", "watching for changes; game will start when compilation succeeds");
    };
    let initial_build =
        match (crate::registry::RuntimeServices::registered().prepare_sound_bank)(path) {
            Err(e) => {
                report_initial_failure(&e);
                None
            }
            Ok(_) => match initial_session_build(
                cache_root.as_deref(),
                backend,
                &compiler_dir,
                &output_path,
                compiler.as_mut(),
            ) {
                Ok(build_id) => {
                    reseed_comptime_watch(
                        compiler.as_ref(),
                        &sound_paths,
                        &mut watched,
                        &comptime_watch,
                    );
                    Some(build_id)
                }
                Err(e) => {
                    report_initial_failure(&e);
                    None
                }
            },
        };
    let initial_compilation_success = initial_build.is_some();

    let mut game_process = if let Some(build_id) = initial_build {
        Some(start_game_process(&output_path, app_args, &build_id)?)
    } else {
        None
    };

    // Set up file watcher for game files, stdlib, stone (engine Rock code),
    // stone-rs / optional extras (engine Rust crates), the rock compiler, and
    // tmp/ for the restart-trigger file. FileWatcher silently skips
    // nonexistent paths, so listing the optional crates is safe on machines
    // where they aren't checked out.
    let rock_root = PathBuf::from(rock_host::rock_dir());
    let mut engine_watch_paths = vec![
        rock_root.join("stone-rs/src"),
        rock_root.join("stone-rs/Cargo.toml"),
        rock_root.join("stone-rs/build.rs"),
    ];
    if rock_engine_host::engine_build::prebuilt_game() || rock_host::project::is_release_layout() {
        engine_watch_paths.clear();
    } else {
        engine_watch_paths.extend(crate::compiler_watch::compiler_watch_paths());
    }
    let mut native_inputs = rock_engine_host::native_libraries::native_watch_inputs(
        &rock_engine_host::engine_build::game_engine_libs(path)?,
    )?;
    let mut watch_paths = stone_source_watch_paths(path, compiler.as_ref());
    watch_paths.extend(engine_watch_paths.iter().cloned());

    // Create channel for file change events
    let (change_tx, change_rx) = channel::<FileChangeEvent>();

    // Canonical rock-source watcher + stone-only opt-ins: manual
    // restart via `touch tmp/restart.txt`, and engine-rebuild
    // extensions (stone-rs / C / stick.toml changes rebuild libstone).
    let restart_files = vec![
        rock_root.join("tmp/restart.txt"),
        PathBuf::from("tmp/restart.txt"),
    ];
    let mut sound_watch = if sound_inputs.directories.is_empty() {
        None
    } else {
        Some(
            FileWatcher::default_for_rock_sources()
                .with_all_files()
                .watch(
                    sound_inputs.directories.iter().cloned().collect(),
                    change_tx.clone(),
                )?,
        )
    };
    let mut native_watcher = FileWatcher::default_for_rock_sources()
        .with_all_files()
        .watch(native_inputs.paths.clone(), change_tx.clone())?;
    let mut watcher = StoneWatcher::new(comptime_watch.clone(), restart_files, change_tx.clone());
    watcher.refresh(watch_paths, &watched)?;

    info!("Watching for changes...");

    // Track what needs rebuilding
    let mut needs_libstone_rebuild = false;
    let mut needs_compiler_rebuild = false;
    let mut sound_dirty = false;
    let mut force_game_compile = !initial_compilation_success;

    for event in change_rx {
        let _keep_native_watch = &native_watcher;
        // Check for manual restart trigger first
        let mut manual_restart_requested = false;
        if event.is_restart_trigger {
            info!("Manual restart triggered (tmp/restart.txt detected)");
            manual_restart_requested = true;
        }

        let mut changes = categorize_stone_changes(
            &event,
            &sound_paths,
            &sound_inputs,
            &watched,
            &native_inputs,
        );
        retain_production_changes(
            &mut changes.changed_paths,
            path,
            compiler.package_roots(),
            &watched,
        );
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

        if changes.needs_native_rebuild {
            let rebuilt = (|| -> Result<_> {
                let libs = rock_engine_host::engine_build::game_engine_libs(path)?;
                let inputs = rock_engine_host::native_libraries::native_watch_inputs(&libs)?;
                let watcher = FileWatcher::default_for_rock_sources()
                    .with_all_files()
                    .watch(inputs.paths.clone(), change_tx.clone())?;
                native_inputs = inputs;
                native_watcher = watcher;
                for lib in &libs {
                    rock_engine_host::engine_build::build_optional_engine_lib(lib)?;
                }
                Ok(())
            })();
            match rebuilt {
                Ok(()) => should_restart = true,
                Err(error) => {
                    error!("Project native rebuild failed: {error:#} (game keeps running)")
                }
            }
        }

        if needs_libstone_rebuild {
            info!("Rebuilding stone-rs...");
            match rock_engine_host::engine_build::build_engine_libs(path) {
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
            match crate::compiler_watch::build_compiler() {
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
            match crate::package::sound_watch_inputs(path) {
                Ok(inputs) => {
                    if inputs.directories != sound_inputs.directories || sound_watch.is_none() {
                        sound_watch = if inputs.directories.is_empty() {
                            None
                        } else {
                            Some(
                                FileWatcher::default_for_rock_sources()
                                    .with_all_files()
                                    .watch(
                                        inputs.directories.iter().cloned().collect(),
                                        change_tx.clone(),
                                    )?,
                            )
                        };
                    }
                    sound_inputs = inputs;
                }
                Err(error) => {
                    log::error!(target: "SOUND", "cannot update sound watches: {error}; retaining previous watches")
                }
            }
            match (crate::registry::RuntimeServices::registered().prepare_sound_bank)(path) {
                Ok(_) => {
                    sound_dirty = false;
                    if game_process.is_none() {
                        force_game_compile = true;
                    }
                }
                Err(e) => {
                    let error_message = format!("Error compiling sound project: {}", e);
                    log::error!(target: "STONE", "{}", error_message);
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

                    reseed_comptime_watch(
                        compiler.as_ref(),
                        &sound_paths,
                        &mut watched,
                        &comptime_watch,
                    );
                    let mut next_paths = stone_source_watch_paths(path, compiler.as_ref());
                    next_paths.extend(engine_watch_paths.iter().cloned());
                    watcher.refresh(next_paths, &watched)?;
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

                    continue;
                }
            }
        }

        // If game hasn't been started yet and compilation just succeeded, start it now
        if game_process.is_none() && compilation_succeeded {
            // A crashed/never-started process loads `main.lua` only -- fold any
            // overlay / deferred shape write in first, same as a restart.
            if let Err(e) = compiler.canonicalize_for_restart() {
                error!(
                    "fold before start failed: {} (start may load a stale main.lua)",
                    e
                );
            }
            let artifact_fingerprint = compiler.artifact_fingerprint()?;
            info!("First successful compilation! Starting game...");
            let build_id = artifact_fingerprint;
            game_process = Some(start_game_process(&output_path, app_args, &build_id)?);
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
                    error!(
                        "fold before restart failed: {} (restart may load a stale main.lua)",
                        e
                    );
                }
                game_process = Some(start_game_process(
                    &output_path,
                    app_args,
                    &compiler.artifact_fingerprint()?,
                )?);
                info!("Game process restarted");
            }
        } else if has_game_changes && game_process.is_some() {
            debug!("Game will hot reload automatically");
        }
    }

    Ok(())
}

/// Combined startup build: stone-rs + optional extras + rock compiler
/// in a single `cargo build` invocation. Streams cargo output directly so
/// the user sees "Compiling …" progress instead of a silent pause.
///
/// Used at `rock stone` startup. The watch loop's incremental rebuilds keep
/// using the per-target `build_engine_libs` / `build_compiler` since they
/// fire selectively based on which files changed.
fn build_all_for_game(game_path: &str) -> Result<()> {
    if rock_engine_host::engine_build::prebuilt_game() {
        // Validate every required staged library; the engine helper refuses
        // missing receipts instead of falling through to a source build.
        return rock_engine_host::engine_build::build_engine_libs(game_path);
    }
    if rock_host::project::is_release_layout() {
        return rock_engine_host::engine_build::build_engine_libs(game_path);
    }

    let optional_libs = rock_engine_host::engine_build::game_engine_libs(game_path)?;

    // `--lib --bin rock` selects both halves in ONE invocation: every default
    // member's lib/cdylib target plus the compiler binary. The two build in
    // parallel, and stone-rs's cdylib lands for the stage_cdylib below.
    //
    // No `-p`: resolver-2 unifies features over the SELECTED packages only, so
    // a narrower selection is a distinct feature resolution and rebuilds every
    // shared dep plus the whole compiler stack under a second -C metadata.
    // Staying on the default-member selection keeps one set of artifacts,
    // shared with rock.sh's build.
    info!("Building engine + compiler (cargo)...");

    rock_host::cargo::run_cargo(
        &["build", "--lib", "--bin", "rock"],
        "Failed to build engine + compiler",
    )?;
    rock_engine_host::engine_build::stage_cdylib("stone_rs", "stone")?;
    // One helper, not a second build+stage pair here: two of them drift, and
    // the drift is invisible until a lib builds on one path and not the other.
    for lib in &optional_libs {
        rock_engine_host::engine_build::build_optional_engine_lib(lib)?;
    }

    Ok(())
}

fn prune_stone_sessions(root: &std::path::Path) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            log::warn!(target: "STONE", "cannot inspect old sessions in {}: {error}", root.display());
            return;
        }
    };
    let mut removed = 0;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                log::warn!(target: "STONE", "cannot inspect session entry: {error}");
                continue;
            }
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let Some((game, pid)) = name.to_str().and_then(|name| name.rsplit_once('-')) else {
            continue;
        };
        let Some(pid) = pid
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid > 0 && *pid <= i32::MAX as u32)
        else {
            continue;
        };
        if game.is_empty() || crate::child_lifetime::pid_alive(pid) {
            continue;
        }
        let path = entry.path();
        let old = |path: &std::path::Path| {
            std::fs::symlink_metadata(path)
                .ok()
                .filter(|metadata| !metadata.file_type().is_symlink())
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > SESSION_RETENTION)
        };
        if !old(&path) || !old(&path.join("main.lua")) {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => removed += 1,
            Err(error) => log::warn!(target: "STONE", "cannot prune {}: {error}", path.display()),
        }
    }
    if removed > 0 {
        log::info!(target: "STONE", "Pruned {removed} inactive session(s) older than seven days from {}", root.display());
    }
}

fn start_game_process(
    lua_file: &std::path::Path,
    app_args: &[String],
    build_id: &str,
) -> Result<Child> {
    // Platform-specific process management for orphan prevention
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;

        let mut cmd = rock_engine_host::runtime::build_luajit_command(lua_file, app_args);
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        // Only the stone watch loop enables the in-game hot-reload watcher.
        cmd.env("STONE_HOT_RELOAD", "1")
            .env("STONE_BUILD_ID", build_id);

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
            let job = winapi::um::jobapi2::CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
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
                std::mem::size_of::<winapi::um::winnt::JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()
                    as u32,
            );
            if ok == 0 {
                anyhow::bail!(
                    "SetInformationJobObject failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            // Spawn the process
            let mut cmd = rock_engine_host::runtime::build_luajit_command(lua_file, app_args);
            cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
            // Only the stone watch loop enables the in-game hot-reload watcher.
            cmd.env("STONE_HOT_RELOAD", "1")
                .env("STONE_BUILD_ID", build_id);

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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(unix)]
    fn prebuilt_core_startup_never_invokes_cargo() {
        const CHILD: &str = "ROCK_PREBUILT_STARTUP_TEST";
        if let Ok(case) = std::env::var(CHILD) {
            let result = super::build_all_for_game(".");
            if case == "valid" {
                result.expect("a complete prebuilt receipt must start without Cargo");
            } else {
                result.expect_err("missing or corrupt prebuilt libraries must fail");
            }
            crate::compiler_watch::build_compiler().expect_err("prebuilt compiler is fixed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(dir.path().join("stone_ffi.rock"), "").unwrap();
        let cargo = dir.path().join("cargo");
        test_utils::write_executable(&cargo, "#!/bin/sh\ntouch \"$ROCK_CARGO_PROBE\"\nexit 97\n");
        let marker = dir.path().join("cargo-called");
        std::fs::create_dir(dir.path().join("tmp")).unwrap();
        std::fs::write(
            dir.path().join("stick.toml"),
            "[package]\nname = \"probe\"\n",
        )
        .unwrap();
        let ext = if cfg!(target_os = "macos") {
            "dylib"
        } else {
            "so"
        };
        // SHA-1 of empty bytes is an independent known digest. Source-drift
        // validation is covered by the engine receipt tests; here the receipt
        // proves that startup and compiler reload never invoke Cargo.
        let empty_sha1 = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        let receipt = dir.path().join("receipt.json");
        for case in ["missing", "valid", "corrupt"] {
            std::fs::write(dir.path().join(format!("tmp/libstone.{ext}")), b"").unwrap();
            let libs = [serde_json::json!({"src_stem": "stone", "dst_stem": "stone",
                "inputs": [], "inputs_sha1": empty_sha1, "staged_sha1": empty_sha1})];
            std::fs::write(
                &receipt,
                serde_json::to_vec(&serde_json::json!({
                    "version": 1, "workspace": dir.path().canonicalize().unwrap(), "libs": libs,
                }))
                .unwrap(),
            )
            .unwrap();
            if case == "corrupt" {
                std::fs::write(dir.path().join(format!("tmp/libstone.{ext}")), b"corrupt").unwrap();
            }
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "stone_watch::tests::prebuilt_core_startup_never_invokes_cargo",
                    "--nocapture",
                ])
                .current_dir(dir.path())
                .env(CHILD, case)
                .env("ROCK_PREBUILT_GAME", "1")
                .env("ROCK_DIR", dir.path())
                .env("ROCK_CARGO_PROBE", &marker)
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        dir.path().display(),
                        std::env::var("PATH").unwrap()
                    ),
                )
                .env_remove("ROCK_ENGINE_BUILD_RECEIPT");
            if case != "missing" {
                command.env("ROCK_ENGINE_BUILD_RECEIPT", &receipt);
            }
            let result = command.output().unwrap();
            assert!(
                result.status.success(),
                "{case}: {}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(!marker.exists(), "prebuilt startup invoked Cargo: {case}");
        }
    }
    #[test]
    fn structural_reload_requests_clean_restart() {
        assert!(full_reload_requires_restart(
            &["schema changed".to_string()]
        ));
        assert!(
            !full_reload_requires_restart(&[]),
            "a body-only edit must not request a restart"
        );
    }
    #[test]
    fn a_stone_session_never_writes_the_shared_main_lua() {
        let shared = rock_host::project::default_main_lua_path();
        let session = rock_engine_host::project::stone_session_main_lua_path("games/match3", 4242);
        assert_ne!(session, shared, "the session must have its own main.lua");
        assert!(
            session.starts_with(rock_engine_host::project::STONE_SESSION_DIR),
            "session output belongs under {}: {}",
            rock_engine_host::project::STONE_SESSION_DIR,
            session.display()
        );
        assert!(
            !shared.starts_with(rock_engine_host::project::STONE_SESSION_DIR),
            "the shared slot must stay outside the session root"
        );
    }

    /// Two concurrent sessions must not share a slot, and a session must keep
    /// the same one across its own recompiles (the path is recomputed nowhere,
    /// but a pid-stable name is what makes a restart land where the watcher
    /// is looking).
    #[test]
    fn session_dirs_separate_by_game_and_pid() {
        let p = |game, pid| rock_engine_host::project::stone_session_main_lua_path(game, pid);
        assert_ne!(
            p("sample", 1),
            p("sample", 2),
            "two sessions of one game differ"
        );
        assert_ne!(p("sample", 1), p("other", 1), "two games differ");
        assert_eq!(p("sample", 1), p("sample", 1), "the same session is stable");
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
    #[cfg(unix)]
    #[test]
    fn session_pruning_child() {}
    #[cfg(unix)]
    #[test]
    fn session_pruning_keeps_live_recent_and_unrecognized_entries() {
        use std::fs::{File, FileTimes};
        use std::time::{Duration, SystemTime};

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["stone_watch::tests::session_pruning_child", "--exact"])
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        assert!(child.wait().unwrap().success());
        let old = SystemTime::now() - SESSION_RETENTION - Duration::from_secs(60);
        let make_session = |name: &str, time| {
            let path = root.path().join(name);
            std::fs::create_dir(&path).unwrap();
            let file = File::create(path.join("main.lua")).unwrap();
            file.set_times(FileTimes::new().set_modified(time)).unwrap();
            File::open(&path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(time))
                .unwrap();
            path
        };
        let stale = make_session(&format!("my-game-{dead_pid}"), old);
        let recent = make_session(&format!("recent-{dead_pid}"), SystemTime::now());
        let active = make_session(&format!("sample-{}", std::process::id()), old);
        let unknown = make_session("notes", old);
        let invalid = make_session("sample-0", old);
        let link = root.path().join(format!("linked-{dead_pid}"));
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        std::fs::write(outside.path().join("keep"), "keep").unwrap();
        prune_stone_sessions(root.path());
        assert!(!stale.exists(), "old inactive session must be removed");
        for path in [recent, active, unknown, invalid, link] {
            assert!(path.exists(), "must preserve {}", path.display());
        }
        assert!(outside.path().join("keep").exists());
    }
}

#[cfg(test)]
mod watch_reload_tests {
    use super::{
        StoneChanges, StoneWatcher, categorize_stone_changes, retain_production_changes,
        stone_source_watch_paths,
    };
    use crate::FileChangeEvent;
    use crate::game_runtime::{GameCompiler, SkullGameCompiler};
    use bone_core::prelude::{Duration, Instant};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::mpsc::{Receiver, channel};
    use test_utils::HANG_BACKSTOP;

    fn run_lua(code: &str) -> anyhow::Result<rock_host::lua_runner::LuaRun> {
        rock_engine_host::runtime::run_lua_code_with_timeout(code, Some(HANG_BACKSTOP))
    }

    fn categorize(event: &FileChangeEvent) -> StoneChanges {
        categorize_stone_changes(
            event,
            &Default::default(),
            &Default::default(),
            &Default::default(),
            &Default::default(),
        )
    }

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
                std::fs::write(helper.join("stick.toml"), "[package]\nname = \"helper\"\n")
                    .unwrap();
                std::fs::write(root.join("main.rock"), caller).unwrap();
                helper.join("lib.rock")
            } else {
                root.join("main.rock")
            };
            std::fs::write(
                &edited,
                if sibling {
                    leaf.to_string()
                } else {
                    format!("{caller}{leaf}")
                },
            )
            .unwrap();
            let main = root.join("tmp/main.lua");
            let mut compiler = SkullGameCompiler::new(
                root.to_str().unwrap(),
                main.clone(),
                root.join("tmp/incremental.lua"),
            );
            compiler
                .compile(&HashSet::new(), false)
                .expect("initial compile");
            Self {
                _dir: dir,
                root,
                edited,
                main,
                compiler,
            }
        }

        fn watch(&self, restart: Vec<PathBuf>) -> (StoneWatcher, Receiver<FileChangeEvent>) {
            let (tx, rx) = channel();
            let mut watcher = StoneWatcher::new(Default::default(), restart, tx);
            watcher
                .refresh(
                    stone_source_watch_paths(self.root.to_str().unwrap(), &self.compiler),
                    &Default::default(),
                )
                .unwrap();
            (watcher, rx)
        }

        fn retain(&self, changes: &mut HashSet<PathBuf>, inputs: &HashSet<PathBuf>) {
            retain_production_changes(
                changes,
                self.root.to_str().unwrap(),
                self.compiler.package_roots(),
                inputs,
            );
        }

        fn assert_reload(mut self, atomic: bool) {
            let before = std::fs::read_to_string(&self.main).unwrap();
            let (_watcher, rx) = self.watch(Vec::new());
            let text = std::fs::read_to_string(&self.edited)
                .unwrap()
                .replace("a + 1", "a + 10");
            if atomic {
                let temporary = self.edited.with_extension("saving");
                std::fs::write(&temporary, text).unwrap();
                std::fs::rename(temporary, &self.edited).unwrap();
            } else {
                std::fs::write(&self.edited, text).unwrap();
            }
            let event = receive_path(&rx, &self.edited);
            let mut changes = categorize(&event);
            self.retain(&mut changes.changed_paths, &Default::default());
            assert!(
                changes.changed_paths.contains(&self.edited),
                "Stone dropped source edit: {:?}",
                event.paths
            );
            assert!(!changes.needs_compiler_rebuild && !changes.needs_libstone_rebuild);
            let outcome = self
                .compiler
                .compile(&changes.changed_paths, false)
                .expect("reload compile");
            assert!(
                outcome.restart_reasons.is_empty(),
                "body edit must preserve live state"
            );
            assert!(
                before == std::fs::read_to_string(&self.main).unwrap(),
                "body edit must preserve main.lua"
            );
            let after = std::fs::read_to_string(self.main.with_file_name("incremental.lua"))
                .expect("body-edit overlay");
            let script = format!(
                "HOT_RELOAD = false\n{before}\nbump()\nHOT_RELOAD = true\n{after}\nHOT_RELOAD = false\nbump()\nassert(SCORE == 11, tostring(SCORE))\nprint('WATCH_RELOAD_PASS')\n"
            );
            let output = run_lua(&script).expect("run reloaded Lua");
            assert!(
                output.stdout.contains("WATCH_RELOAD_PASS"),
                "{}",
                output.combined()
            );
        }
    }

    fn receive_path(rx: &Receiver<FileChangeEvent>, expected: &PathBuf) -> FileChangeEvent {
        let deadline = Instant::now() + HANG_BACKSTOP;
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
    fn write_until_reported(
        rx: &Receiver<FileChangeEvent>,
        path: &PathBuf,
        text: &str,
    ) -> FileChangeEvent {
        let deadline = Instant::now() + HANG_BACKSTOP;
        while Instant::now() < deadline {
            std::fs::write(path, text).unwrap();
            let attempt_end = Instant::now() + Duration::from_millis(1500);
            while let Ok(event) =
                rx.recv_timeout(attempt_end.saturating_duration_since(Instant::now()))
            {
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
        let (_watcher, rx) = fixture.watch(Vec::new());
        let barrier = drain_setup_events(&rx, &sims);

        let event = write_until_reported(&rx, &sim, "fn probe( {\n");
        let mut changes = categorize(&event);
        // Setup writes of the fixture may be reported in the same batch; they are
        // not the edit under test.
        for setup in [
            &barrier,
            &fixture.root.join("main.rock"),
            &fixture.root.join("stick.toml"),
        ] {
            changes.changed_paths.remove(setup);
        }
        assert_eq!(changes.changed_paths, HashSet::from([sim.clone()]));
        fixture.retain(&mut changes.changed_paths, &Default::default());
        assert!(
            changes.changed_paths.is_empty(),
            "sim edit reached the game compiler"
        );
        assert_eq!(
            std::fs::read(&fixture.main).unwrap(),
            before,
            "sim edit rewrote the game artifact"
        );

        std::fs::remove_file(&sim).unwrap();
        let mut removed = HashSet::from([sim.clone()]);
        fixture.retain(&mut removed, &Default::default());
        assert!(removed.is_empty(), "deleted sim reached the game compiler");

        let inputs = HashSet::from([sim.clone()]);
        let mut embedded = HashSet::from([sim.clone()]);
        fixture.retain(&mut embedded, &inputs);
        assert_eq!(
            embedded,
            HashSet::from([sim]),
            "declared embedded input must remain watched"
        );
    }

    #[test]
    fn stone_scratch_and_unrelated_files_do_not_reload_but_restart_does() {
        let fixture = Fixture::new(false);
        let scratch = fixture.root.join("tmp/generated.rock");
        let restart_dir = fixture.root.parent().unwrap().join("tmp");
        std::fs::create_dir_all(&restart_dir).unwrap();
        let restart = restart_dir.join("restart.txt");
        let (_watcher, rx) = fixture.watch(vec![restart.clone()]);
        std::fs::write(&scratch, "fn scratch() {}\n").unwrap();
        std::fs::write(restart_dir.join("unrelated.rock"), "fn unrelated() {}\n").unwrap();
        // Force create and write into separate batches: both may legitimately
        // report restart.txt, regardless of how quickly the OS coalesces them.
        std::fs::File::create(&restart).unwrap();
        let deadline = Instant::now() + HANG_BACKSTOP;
        let mut event = rx
            .recv_timeout(HANG_BACKSTOP)
            .expect("watcher never reported restart");
        std::fs::write(&restart, "restart").unwrap();
        loop {
            assert!(event.is_restart_trigger, "unexpected event: {event:?}");
            assert_eq!(event.paths, HashSet::from([restart.clone()]));
            let changes = categorize(&event);
            assert!(changes.changed_paths.is_empty());

            assert!(Instant::now() < deadline, "watcher never became quiet");
            match rx.recv_timeout(Duration::from_millis(300)) {
                Ok(next) => event = next,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(error) => panic!("watcher disconnected: {error}"),
            }
        }
    }

    #[test]
    fn sound_discovery_watches_added_and_removed_variants() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let variants = root.join("assets/variants");
        std::fs::create_dir_all(&variants).unwrap();
        std::fs::write(
            root.join("stick.toml"),
            "[package]\nname = \"sound-watch\"\n[sound]\nproject = \"sounds.json\"\n",
        )
        .unwrap();
        std::fs::write(root.join("sounds.json"), r#"{"version":2,"normalization_lufs":-18.0,"groups":{"fixture":{"gain_db":0,"bus":"sfx"}},"events":[{"id":"step","group":"fixture","steps":[{"layers":[{"sources":["assets/variants/take_*.wav"]}]}]}]}"#).unwrap();
        let removed = variants.join("take_old.wav");
        std::fs::write(&removed, b"existing").unwrap();
        let inputs = crate::package::sound_watch_inputs(root.to_str().unwrap()).unwrap();
        assert!(!inputs.contains(&variants.join("preview.wav")));
        assert!(!inputs.contains(&root.join("main.rock")));
        let (tx, rx) = channel();
        let _watch = crate::file_watcher::FileWatcher::default_for_rock_sources()
            .with_all_files()
            .watch(inputs.directories.iter().cloned().collect(), tx)
            .unwrap();
        let added = variants.join("take_new.wav");
        std::fs::write(&added, b"new").unwrap();
        for path in [&added, &removed] {
            if path == &removed {
                std::fs::remove_file(path).unwrap();
            }
            let deadline = Instant::now() + test_utils::HANG_BACKSTOP;
            loop {
                let event = rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap();
                if !event.paths.contains(path) {
                    continue;
                }
                let changes = categorize_stone_changes(
                    &event,
                    &Default::default(),
                    &inputs,
                    &Default::default(),
                    &Default::default(),
                );
                assert!(changes.sound_dirty);
                assert!(changes.changed_paths.is_empty());
                assert!(!changes.needs_compiler_rebuild && !changes.needs_libstone_rebuild);
                break;
            }
        }
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
        std::fs::write(
            package.join("stick.toml"),
            "[package]\nname = \"embed-watch\"\n",
        )
        .unwrap();
        let source = source_root.join("main.rock");
        std::fs::write(
            &source,
            "@nomangle fn banner() -> string { embed_string(\"data/value.txt\") }\n",
        )
        .unwrap();
        let input = package.join("data/value.txt");
        let next_input = package.join("other/value.txt");
        std::fs::write(&input, "watch-value-one").unwrap();
        std::fs::write(&next_input, "watch-value-two").unwrap();
        let main = package.join("tmp/main.lua");
        let mut compiler = SkullGameCompiler::new(
            source_root.to_str().unwrap(),
            main.clone(),
            package.join("tmp/incremental.lua"),
        );
        compiler.compile(&HashSet::new(), false).unwrap();
        let extra: crate::file_watcher::SharedWatchSet = Default::default();
        let declared = compiler.comptime_watch_files_with_hashes();
        extra.replace_with_hashes(declared.clone());
        let mut watched: HashSet<PathBuf> = declared.iter().map(|(path, _)| path.clone()).collect();
        assert!(watched.contains(&input));
        let (tx, rx) = channel();
        let mut watcher = StoneWatcher::new(extra.clone(), Vec::new(), tx);
        watcher
            .refresh(
                stone_source_watch_paths(source_root.to_str().unwrap(), &compiler),
                &watched,
            )
            .unwrap();
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
            let changes = categorize_stone_changes(
                &event,
                &Default::default(),
                &Default::default(),
                &watched,
                &Default::default(),
            );
            assert_eq!(changes.changed_paths, HashSet::from([input.clone()]));
            compiler.compile(&changes.changed_paths, false).unwrap();
            compiler.canonicalize_for_restart().unwrap();
            let lua = std::fs::read_to_string(&main).unwrap();
            let output = run_lua(&format!("{lua}\nprint(banner())")).unwrap();
            assert_eq!(output.stdout.trim(), text, "{}", output.combined());
        }

        std::fs::write(
            &source,
            "@nomangle fn banner() -> string { embed_string(\"other/value.txt\") }\n",
        )
        .unwrap();
        let event = receive_path(&rx, &source);
        let changes = categorize_stone_changes(
            &event,
            &Default::default(),
            &Default::default(),
            &watched,
            &Default::default(),
        );
        compiler.compile(&changes.changed_paths, false).unwrap();
        compiler.canonicalize_for_restart().unwrap();
        let declared = compiler.comptime_watch_files_with_hashes();
        extra.replace_with_hashes(declared.clone());
        watched = declared.iter().map(|(path, _)| path.clone()).collect();
        assert!(!watched.contains(&input));
        assert!(watched.contains(&next_input));
        watcher
            .refresh(
                stone_source_watch_paths(source_root.to_str().unwrap(), &compiler),
                &watched,
            )
            .unwrap();
        // A source write can produce trailing events after recompilation.
        rx.try_iter().for_each(drop);
        std::fs::write(&input, "unused input").unwrap();
        let deadline = Instant::now() + Duration::from_millis(300);
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(event) => assert!(
                    !event.paths.contains(&input),
                    "unwatched input reported: {event:?}"
                ),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(error) => panic!("watcher disconnected: {error}"),
            }
        }
        std::fs::write(&next_input, "watch-refreshed").unwrap();
        let event = receive_path(&rx, &next_input);
        let changes = categorize_stone_changes(
            &event,
            &Default::default(),
            &Default::default(),
            &watched,
            &Default::default(),
        );
        compiler.compile(&changes.changed_paths, false).unwrap();
        compiler.canonicalize_for_restart().unwrap();
        let lua = std::fs::read_to_string(main).unwrap();
        let output = run_lua(&format!("{lua}\nprint(banner())")).unwrap();
        assert_eq!(
            output.stdout.trim(),
            "watch-refreshed",
            "{}",
            output.combined()
        );
    }

    #[test]
    fn stone_added_dependency_is_watched_after_manifest_reload() {
        let mut fixture = Fixture::new(false);
        let helper = fixture.root.parent().unwrap().join("new-helper");
        std::fs::create_dir_all(&helper).unwrap();
        std::fs::write(
            helper.join("stick.toml"),
            "[package]\nname = \"new-helper\"\n",
        )
        .unwrap();
        let source = helper.join("lib.rock");
        std::fs::write(&source, "@nomangle fn library_banner() -> int { 7 }\n").unwrap();
        let (mut watcher, rx) = fixture.watch(Vec::new());
        let manifest = fixture.root.join("stick.toml");
        std::fs::write(&manifest, "[package]\nname = \"watch-game\"\n[dependencies]\nhelper = { path = \"../new-helper\" }\n").unwrap();
        let event = receive_path(&rx, &manifest);
        let changes = categorize(&event);
        fixture
            .compiler
            .compile(&changes.changed_paths, false)
            .unwrap();
        fixture.compiler.canonicalize_for_restart().unwrap();
        watcher
            .refresh(
                stone_source_watch_paths(fixture.root.to_str().unwrap(), &fixture.compiler),
                &Default::default(),
            )
            .unwrap();
        let before = std::fs::read_to_string(&fixture.main).unwrap();
        std::fs::write(&source, "@nomangle fn library_banner() -> int { 8 }\n").unwrap();
        let event = receive_path(&rx, &source);
        let changes = categorize(&event);
        let outcome = fixture
            .compiler
            .compile(&changes.changed_paths, false)
            .unwrap();
        assert!(outcome.restart_reasons.is_empty());
        assert!(
            before == std::fs::read_to_string(&fixture.main).unwrap(),
            "body edit must preserve main.lua"
        );
        let after = std::fs::read_to_string(fixture.main.with_file_name("incremental.lua"))
            .expect("dependency body-edit overlay");
        let script = format!(
            "HOT_RELOAD = false\n{before}\nassert(library_banner() == 7)\nHOT_RELOAD = true\n{after}\nHOT_RELOAD = false\nprint(library_banner())"
        );
        let output = run_lua(&script).unwrap();
        assert_eq!(output.stdout.trim(), "8", "{}", output.combined());
    }
}

#[cfg(test)]
mod session_cache_tests {
    use super::initial_session_build;
    use crate::Backend;
    use crate::game_runtime::{AdoptedArtifact, CompileOutcome, GameCompiler, SkullGameCompiler};
    use std::cell::Cell;
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    /// The compiler `initial_session_build` drives, counting the compiles it is
    /// asked for: a cache hit means this stays at zero.
    struct CountingCompiler {
        inner: SkullGameCompiler,
        compiles: Cell<usize>,
    }

    impl GameCompiler for CountingCompiler {
        fn compile(
            &mut self,
            changed: &HashSet<PathBuf>,
            force_full: bool,
        ) -> anyhow::Result<CompileOutcome> {
            self.compiles.set(self.compiles.get() + 1);
            self.inner.compile(changed, force_full)
        }

        fn canonicalize_for_restart(&mut self) -> anyhow::Result<()> {
            self.inner.canonicalize_for_restart()
        }

        fn comptime_watch_files_with_hashes(&self) -> Vec<(PathBuf, u64)> {
            self.inner.comptime_watch_files_with_hashes()
        }

        fn package_roots(&self) -> Option<&[(u32, PathBuf)]> {
            self.inner.package_roots()
        }

        fn artifact_fingerprint(&self) -> anyhow::Result<String> {
            self.inner.artifact_fingerprint()
        }

        fn comptime_inputs(&self) -> Option<&bone_semantic::inference::ComptimeInputs> {
            self.inner.comptime_inputs()
        }

        fn adopt_cached_artifact(&mut self, adopted: AdoptedArtifact) -> anyhow::Result<()> {
            self.inner.adopt_cached_artifact(adopted)
        }
    }

    struct Session {
        _dir: tempfile::TempDir,
        /// The private per-session output dir `do_stone_watch` compiles into.
        main: PathBuf,
        compiler: CountingCompiler,
    }

    fn session(project: &Path) -> Session {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main.lua");
        let compiler = CountingCompiler {
            inner: SkullGameCompiler::new(
                project.to_str().unwrap(),
                main.clone(),
                main.with_file_name("incremental.lua"),
            ),
            compiles: Cell::new(0),
        };
        Session {
            _dir: dir,
            main,
            compiler,
        }
    }

    fn project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("stick.toml"),
            "[package]\nname = \"session-cache-fixture\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("main.rock"), "fn main() { print(1) }\n").unwrap();
        let root = dir.path().canonicalize().unwrap();
        (dir, root)
    }

    fn build(project: &Path, cache: &Path, session: &mut Session) -> String {
        initial_session_build(
            Some(cache),
            Backend::Skull,
            &project.to_string_lossy(),
            &session.main,
            &mut session.compiler,
        )
        .expect("the session's first program")
    }

    /// The Lua a from-scratch compile emits now, independent of any cache.
    fn oracle_main_lua(project: &Path) -> (Vec<u8>, String) {
        let dir = tempfile::tempdir().unwrap();
        let artifact = crate::game_compile::compile_game(
            Backend::Skull,
            &project.to_string_lossy(),
            dir.path(),
        )
        .unwrap();
        (
            std::fs::read(dir.path().join(rock_host::project::MAIN_LUA_FILENAME)).unwrap(),
            artifact.artifact_fingerprint,
        )
    }

    #[test]
    fn a_second_session_on_unchanged_sources_starts_from_the_cached_compile() {
        let (_project_dir, project) = project();
        let cache = tempfile::tempdir().unwrap();
        let mut first = session(&project);
        let first_fingerprint = build(&project, cache.path(), &mut first);
        assert_eq!(
            first.compiler.compiles.get(),
            1,
            "the first session compiles"
        );

        let mut second = session(&project);
        let second_fingerprint = build(&project, cache.path(), &mut second);
        assert_eq!(
            second.compiler.compiles.get(),
            0,
            "a second session on unchanged sources must not run the initial compile",
        );
        assert_eq!(second_fingerprint, first_fingerprint);
        let (expected, oracle_fingerprint) = oracle_main_lua(&project);
        assert_eq!(
            second_fingerprint, oracle_fingerprint,
            "the cached identity is the compiler's current one"
        );
        assert_eq!(
            std::fs::read(&second.main).unwrap(),
            expected,
            "the session runs the compiler's current Lua"
        );
        assert!(
            !second.main.with_file_name("incremental.lua").exists(),
            "a session starting from a full artifact never boots beside a stale overlay",
        );
    }

    #[test]
    fn an_edited_source_misses_the_session_cache() {
        let (_project_dir, project) = project();
        let cache = tempfile::tempdir().unwrap();
        let mut first = session(&project);
        let first_fingerprint = build(&project, cache.path(), &mut first);

        std::fs::write(project.join("main.rock"), "fn main() { print(2) }\n").unwrap();
        let mut edited = session(&project);
        let edited_fingerprint = build(&project, cache.path(), &mut edited);
        assert_eq!(
            edited.compiler.compiles.get(),
            1,
            "an edited source must compile"
        );
        assert_ne!(edited_fingerprint, first_fingerprint);
        let (expected, oracle_fingerprint) = oracle_main_lua(&project);
        assert_eq!(edited_fingerprint, oracle_fingerprint);
        assert_eq!(
            std::fs::read(&edited.main).unwrap(),
            expected,
            "the session runs the edited program"
        );
    }

    #[test]
    fn a_disabled_cache_compiles_and_does_not_store() {
        let (_project_dir, project) = project();
        let cache = tempfile::tempdir().unwrap();
        let mut first = session(&project);
        initial_session_build(
            None,
            Backend::Skull,
            &project.to_string_lossy(),
            &first.main,
            &mut first.compiler,
        )
        .expect("a session with the cache off still compiles");
        assert_eq!(first.compiler.compiles.get(), 1);
        assert_eq!(
            std::fs::read_dir(cache.path()).unwrap().count(),
            0,
            "a disabled cache stores nothing"
        );

        let mut second = session(&project);
        initial_session_build(
            None,
            Backend::Skull,
            &project.to_string_lossy(),
            &second.main,
            &mut second.compiler,
        )
        .expect("a session with the cache off still compiles");
        assert_eq!(
            second.compiler.compiles.get(),
            1,
            "the cache is off, so nothing is served back"
        );
    }

    #[test]
    fn a_session_that_adopts_a_cached_artifact_replaces_the_running_program() {
        let (_project_dir, project) = project();
        let mut first = session(&project);
        first
            .compiler
            .compile(&HashSet::new(), false)
            .expect("initial compile");
        let adopted = AdoptedArtifact {
            artifact_fingerprint: first.compiler.artifact_fingerprint().unwrap(),
            embedded_inputs: Vec::new(),
        };

        let mut adopted_session = session(&project);
        adopted_session
            .compiler
            .adopt_cached_artifact(adopted.clone())
            .unwrap();
        assert_eq!(
            adopted_session.compiler.artifact_fingerprint().unwrap(),
            adopted.artifact_fingerprint,
            "the running cached program is the identity the session reports",
        );

        std::fs::write(project.join("main.rock"), "fn main() { print(3) }\n").unwrap();
        let outcome = adopted_session
            .compiler
            .compile(&HashSet::from([project.join("main.rock")]), false)
            .expect("the first edit builds the session");
        assert!(
            !outcome.restart_reasons.is_empty(),
            "the session's own program is not what the game booted, so the process restarts onto it",
        );
        let (expected, oracle_fingerprint) = oracle_main_lua(&project);
        assert_eq!(
            adopted_session.compiler.artifact_fingerprint().unwrap(),
            oracle_fingerprint
        );
        assert_eq!(
            std::fs::read(&adopted_session.main).unwrap(),
            expected,
            "the restart loads the edited program"
        );
    }
}
