const SESSION_RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

use crate::compiler_watch::is_compiler_source;
use crate::headless_scope::BoundChild;
use crate::*;
use std::process::Stdio;

/// Re-exec keeps our PID, so explicitly drop the game child before replacing the compiler.
fn reexec_with_new_binary(game_process: &mut Option<BoundChild>) {
    drop(game_process.take());
    info!("Compiler changed -- re-exec'ing rock with the new binary");
    if let Err(e) = crate::watch::reexec_current_binary() {
        error!(
            "stone: {e}; staying on the old compiler binary (Ctrl-C + rerun to pick up the change)"
        );
    }
}

#[derive(Default)]
struct StoneChanges {
    needs_native_rebuild: bool,
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
            changes.changed_libstone.push(filename);
        } else if is_compiler_source(path) {
            changes.changed_compiler.push(filename);
        } else if path_str.ends_with(".rock") || comptime.contains(path.as_path()) {
            changes.changed_paths.insert(path.clone());
        } else if filename == "stick.toml" {
            changes.needs_native_rebuild = true;
            changes.changed_paths.insert(path.clone());
        }
    }

    changes
}

fn stone_source_watch_paths(
    project: &str,
    compiler: &crate::game_runtime::GameCompiler,
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
        let handle = FileWatcher::default_for_rock_sources()
            .with_restart_suffix(PathBuf::from("tmp/restart.txt"))
            .filter_initial_creates()
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

fn watch_all_files<'a>(
    paths: impl IntoIterator<Item = &'a PathBuf>,
    tx: &std::sync::mpsc::Sender<FileChangeEvent>,
) -> Result<Option<crate::file_watcher::WatchHandle>> {
    let paths: Vec<PathBuf> = paths.into_iter().cloned().collect();
    if paths.is_empty() {
        return Ok(None);
    }
    let watch = FileWatcher::default_for_rock_sources()
        .with_all_files()
        .watch(paths, tx.clone())?;
    Ok(Some(watch))
}

fn declaration_hash(path: &std::path::Path) -> u64 {
    std::fs::read_to_string(path)
        .map(|text| skull_mir::mir::comptime_value_hash(&text))
        .unwrap_or(0)
}

fn reseed_comptime_watch(
    compiler: &crate::game_runtime::GameCompiler,
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

fn wait_for_reload_resume(control: &std::path::Path) -> Result<()> {
    let acknowledged = control.with_extension("held");
    let mut announced: Option<Vec<u8>> = None;
    while control.exists() {
        let requested = match fs_err::read(control) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        };
        if announced.as_ref() != Some(&requested) {
            fs_err::write(&acknowledged, &requested)?;
            info!("Reloads held by the running game; edits wait until resumed");
            announced = Some(requested);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if acknowledged.exists() {
        fs_err::remove_file(acknowledged)?;
        info!("Reloads resumed");
    }
    Ok(())
}

pub(crate) fn do_stone_watch(path: &str, quiet: bool, app_args: &[String]) -> Result<()> {
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

    let compiler_dir = PathBuf::from(path)
        .canonicalize()
        .map_err(|error| anyhow::anyhow!("canonicalize the project root {path}: {error}"))?;
    let compiler_dir = compiler_dir.to_string_lossy().into_owned();
    let mut compiler = crate::game_runtime::GameCompiler::new(
        &compiler_dir,
        output_path.clone(),
        output_path.with_file_name("incremental.lua"),
    );
    let mut changed_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    let comptime_watch: crate::file_watcher::SharedWatchSet = Default::default();
    let mut sound_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    if let Some(sound_project) = crate::package::sound_project_path_for(path)? {
        sound_paths.insert(sound_project.clone());
        let hash = declaration_hash(&sound_project);
        comptime_watch.insert_with_hash(sound_project, hash);
    }
    let mut sound_inputs = crate::package::sound_watch_inputs(path).unwrap_or_else(|error| {
        log::error!(target: "SOUND", "cannot watch sound inputs: {error}; watching the project declaration for repair");
        Default::default()
    });
    let mut watched: std::collections::HashSet<PathBuf> = sound_paths.clone();

    let initial_compilation_success = crate::frame_runtime::beside(
        || build_all_for_game(path),
        || {
            let result = crate::package::prepare_sound_bank(path)
                .and_then(|_| compiler.compile(&std::collections::HashSet::new(), false));
            Ok(match result {
                Ok(_) => {
                    reseed_comptime_watch(&compiler, &sound_paths, &mut watched, &comptime_watch);
                    true
                }
                Err(error) => {
                    log::error!(target: "STONE", "Initial compilation failed: {error}");
                    log::info!(target: "STONE", "watching for changes; game will start when compilation succeeds");
                    false
                }
            })
        },
    )?;

    let mut game_process = if initial_compilation_success {
        Some(start_game_process(&output_path, app_args)?)
    } else {
        None
    };

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
    let mut watch_paths = stone_source_watch_paths(path, &compiler);
    watch_paths.extend(engine_watch_paths.iter().cloned());

    let (change_tx, change_rx) = channel::<FileChangeEvent>();

    let restart_files = vec![
        rock_root.join("tmp/restart.txt"),
        PathBuf::from("tmp/restart.txt"),
    ];
    let mut sound_watch = watch_all_files(&sound_inputs.directories, &change_tx)?;
    let mut native_watcher = watch_all_files(&native_inputs.paths, &change_tx)?;
    let mut watcher = StoneWatcher::new(comptime_watch.clone(), restart_files, change_tx.clone());
    watcher.refresh(watch_paths, &watched)?;

    info!("Watching for changes...");

    let mut sound_dirty = false;
    let mut force_game_compile = !initial_compilation_success;

    let reload_control = output_path.with_file_name("reload.hold");
    loop {
        wait_for_reload_resume(&reload_control)?;
        let event = match change_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(event) => event,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        wait_for_reload_resume(&reload_control)?;
        let _keep_native_watch = &native_watcher;
        if event.is_restart_trigger {
            info!("Manual restart triggered (tmp/restart.txt detected)");
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
        sound_dirty |= changes.sound_dirty;
        changed_paths.extend(changes.changed_paths);
        let changed_libstone = changes.changed_libstone;
        let changed_compiler = changes.changed_compiler;

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

        let mut should_restart = event.is_restart_trigger;
        let mut compiler_rebuilt = false;

        if changes.needs_native_rebuild {
            let rebuilt = (|| -> Result<_> {
                let libs = rock_engine_host::engine_build::game_engine_libs(path)?;
                let inputs = rock_engine_host::native_libraries::native_watch_inputs(&libs)?;
                native_watcher = watch_all_files(&inputs.paths, &change_tx)?;
                native_inputs = inputs;
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

        if !changed_libstone.is_empty() {
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
        }

        if !changed_compiler.is_empty() {
            info!("Rebuilding compiler...");
            match crate::compiler_watch::build_compiler() {
                Ok(_) => {
                    info!("Compiler rebuilt successfully");
                    should_restart = true;
                    compiler_rebuilt = true;
                }
                Err(e) => {
                    error!(
                        "Error rebuilding compiler: {} (using previous compiler binary)",
                        e
                    );
                }
            }
        }

        if sound_dirty {
            match crate::package::sound_watch_inputs(path) {
                Ok(inputs) => {
                    if inputs.directories != sound_inputs.directories || sound_watch.is_none() {
                        sound_watch = watch_all_files(&inputs.directories, &change_tx)?;
                    }
                    sound_inputs = inputs;
                }
                Err(error) => {
                    log::error!(target: "SOUND", "cannot update sound watches: {error}; retaining previous watches")
                }
            }
            match crate::package::prepare_sound_bank(path) {
                Ok(_) => {
                    sound_dirty = false;
                    if game_process.is_none() {
                        force_game_compile = true;
                    }
                }
                Err(e) => log::error!(target: "STONE", "Error compiling sound project: {e}"),
            }
        }

        // This process still has the previous compiler mapped.
        if compiler_rebuilt {
            wait_for_reload_resume(&reload_control)?;
            reexec_with_new_binary(&mut game_process);
        }

        let has_game_changes = !changed_paths.is_empty() || force_game_compile;
        let mut compilation_succeeded = false;
        if has_game_changes || compiler_rebuilt {
            wait_for_reload_resume(&reload_control)?;
            match compiler.compile(&changed_paths, compiler_rebuilt) {
                Ok(outcome) => {
                    // Consumed successfully; start the next edit clean. On error
                    // we keep them so the next tick retries the same changes.
                    changed_paths.clear();
                    force_game_compile = false;
                    compilation_succeeded = true;

                    reseed_comptime_watch(&compiler, &sound_paths, &mut watched, &comptime_watch);
                    let mut next_paths = stone_source_watch_paths(path, &compiler);
                    next_paths.extend(engine_watch_paths.iter().cloned());
                    watcher.refresh(next_paths, &watched)?;
                    if !outcome.restart_reasons.is_empty() {
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

        // Compilation may have overlapped the hold request. Acknowledge only
        // after publication ends, before any child replacement can occur.
        wait_for_reload_resume(&reload_control)?;

        let restarting = should_restart && game_process.is_some();
        if restarting || (game_process.is_none() && compilation_succeeded) {
            if restarting {
                // The old process must be gone before the fold publishes a
                // deferred shape change it cannot hot-reload.
                info!("Restarting game process...");
                drop(game_process.take());
            }
            if let Err(e) = compiler.canonicalize_for_restart() {
                let action = if restarting { "restart" } else { "start" };
                error!("fold before {action} failed: {e} ({action} may load a stale main.lua)");
            }
            if !restarting {
                info!("First successful compilation! Starting game...");
            }
            game_process = Some(start_game_process(&output_path, app_args)?);
            if restarting {
                info!("Game process restarted");
            } else {
                info!("Game started successfully");
            }
        } else if has_game_changes && game_process.is_some() {
            debug!("Game will hot reload automatically");
        }
    }

    Ok(())
}

fn build_all_for_game(game_path: &str) -> Result<()> {
    if rock_engine_host::engine_build::prebuilt_game() {
        return rock_engine_host::engine_build::build_engine_libs(game_path);
    }
    if rock_host::project::is_release_layout() {
        return rock_engine_host::engine_build::build_engine_libs(game_path);
    }

    let optional_libs = rock_engine_host::engine_build::game_engine_libs(game_path)?;

    info!("Building engine + compiler (cargo)...");

    rock_host::cargo::run_cargo(
        &["build", "--lib", "--bin", "rock"],
        "Failed to build compiler",
    )?;
    rock_engine_host::engine_build::build_libstone()?;
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

fn start_game_process(lua_file: &std::path::Path, app_args: &[String]) -> Result<BoundChild> {
    let mut cmd = rock_engine_host::runtime::build_luajit_command(lua_file, app_args);
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    cmd.env("STONE_HOT_RELOAD", "1").env(
        "STONE_RELOAD_CONTROL",
        lua_file.with_file_name("reload.hold"),
    );
    Ok(crate::runtime_process::spawn_game(
        &mut cmd,
        lua_file.parent().unwrap(),
    )?)
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
    fn session_dirs_separate_by_game_and_pid() {
        let p = |game, pid| rock_engine_host::project::stone_session_main_lua_path(game, pid);
        let root = rock_engine_host::project::STONE_SESSION_DIR;
        assert!(p("games/nested", 7).starts_with(root));
        assert!(!rock_host::project::default_main_lua_path().starts_with(root));
        assert_ne!(p("sample", 1), p("sample", 2));
        assert_ne!(p("sample", 1), p("other", 1));
        assert_eq!(p("sample", 1), p("sample", 1));
        assert_eq!(
            p("games/nested", 7).components().count(),
            p("nested", 7).components().count()
        );
        assert!(p("games/nested", 7).to_string_lossy().contains("nested"));
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
    use crate::game_runtime::GameCompiler;
    use bone_core::prelude::{Duration, Instant};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::mpsc::{Receiver, channel};
    use test_utils::HANG_BACKSTOP;

    fn run_lua(code: &str) -> anyhow::Result<rock_host::lua_runner::LuaRun> {
        crate::compile_cmd::run_lua_code_with_timeout(code, Some(HANG_BACKSTOP))
            .map_err(|error| anyhow::anyhow!(error.message))
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
        compiler: GameCompiler,
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
            let leaf = "pub fn leaf(a: int) -> int { a + 1 }\n";
            let edited = if sibling {
                let helper = root.parent().unwrap().join("helper");
                std::fs::create_dir_all(&helper).unwrap();
                std::fs::write(helper.join("stick.toml"), "[package]\nname = \"helper\"\n")
                    .unwrap();
                std::fs::write(
                    root.join("main.rock"),
                    format!("use helper::leaf;\n{caller}"),
                )
                .unwrap();
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
            let mut compiler = GameCompiler::new(
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
            assert!(changes.changed_compiler.is_empty() && changes.changed_libstone.is_empty());
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
        let _watch = super::watch_all_files(&inputs.directories, &tx).unwrap();
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
                assert!(changes.changed_compiler.is_empty() && changes.changed_libstone.is_empty());
                break;
            }
        }
    }

    #[test]
    fn stone_embedded_inputs_follow_recompilation_and_atomic_saves() {
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
        let mut compiler = GameCompiler::new(
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
mod reload_hold_tests {
    use bone_core::prelude::{Duration, Instant};

    #[test]
    fn hold_acknowledges_and_waits_until_the_game_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let control = dir.path().join("reload.hold");
        std::fs::write(&control, "hold").unwrap();
        let path = control.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::wait_for_reload_resume(&path).unwrap();
            tx.send(()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !control.with_extension("held").exists() {
            assert!(Instant::now() < deadline, "hold was not acknowledged");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(150))
                .is_err()
        );
        std::fs::remove_file(&control).unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        assert!(!control.with_extension("held").exists());
    }
}
