use crate::*;

fn is_compiler_crate_dir(name: &str) -> bool {
    matches!(name, "rock" | "skull" | "bone")
        || name.starts_with("rock-")
        || name.starts_with("skull-")
        || name.starts_with("bone-")
}

pub(crate) fn loaded_source_watch_paths(project: &str, roots: &[(u32, PathBuf)]) -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from(project), rock_host::stdlib_loader::stdlib_dir()];
    for (id, root) in roots {
        // Discovery loads only the invoked directory for package zero.
        paths.push(if *id == 0 { root.join("stick.toml") } else { root.clone() });
    }
    paths.sort();
    paths.dedup();
    paths
}

pub(crate) fn project_engine_watch_paths(project: &str) -> Vec<PathBuf> {
    // Compilation may have failed before it retained a graph. Resolve the same
    // channel-filtered graph so edits to a broken dependency can repair it.
    let graph = (|| -> Result<Option<rock_host::stick::graph::PackageGraph>> {
        let Some((config_path, config)) = rock_host::stick::StickConfig::find_from_dir(Path::new(project))? else {
            return Ok(None);
        };
        Ok(Some(rock_host::stick::graph::PackageGraph::build_with_channel(
            config_path.parent().unwrap_or(&config_path),
            &config,
            &rock_host::stick::resolved_channel_for(&config),
        )?))
    })();
    let roots = match graph {
        Ok(Some(graph)) => graph.packages().iter()
            .map(|package| (package.id.0, package.root.clone())).collect(),
        Ok(None) => Vec::new(),
        Err(error) => {
            log::error!(target: "WATCH",
                "cannot resolve dependency watch roots: {error}; watching the project until its manifest is repaired");
            Vec::new()
        }
    };
    loaded_source_watch_paths(project, &roots)
}

pub(crate) fn compiler_watch_paths() -> Vec<PathBuf> {
    let root = PathBuf::from(rock_host::rock_dir());
    let mut paths = vec![root.join("Cargo.toml"), root.join("Cargo.lock")];
    if let Ok(entries) = std::fs::read_dir(root.join("crates")) {
        for entry in entries.flatten() {
            if is_compiler_crate_dir(&entry.file_name().to_string_lossy()) {
                let crate_dir = entry.path();
                let src = crate_dir.join("src");
                if src.is_dir() {
                    paths.push(src);
                }
                let manifest = crate_dir.join("Cargo.toml");
                if manifest.is_file() {
                    paths.push(manifest);
                }
                let build_script = crate_dir.join("build.rs");
                if build_script.is_file() {
                    paths.push(build_script);
                }
            }
        }
    }
    paths
}

pub(crate) fn is_compiler_source(path: impl AsRef<Path>) -> bool {
    let path = path.as_ref().to_string_lossy().replace('\\', "/");
    if path.ends_with("Cargo.toml") || path.ends_with("Cargo.lock") {
        return true;
    }
    if path.ends_with("/build.rs") {
        let crate_name = path
            .trim_end_matches("/build.rs")
            .rsplit('/')
            .next()
            .unwrap_or("");
        return is_compiler_crate_dir(crate_name);
    }
    if let Some(idx) = path.find("/src/") {
        let crate_name = path[..idx].rsplit('/').next().unwrap_or("");
        return is_compiler_crate_dir(crate_name);
    }
    false
}

pub(crate) fn build_compiler() -> Result<()> {
    rock_host::engine_build::run_cargo(&["build", "--bin", "rock"], "Failed to build compiler")
}

pub(crate) fn reexec_current_binary() -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("cannot find current executable: {e}"))?;
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = std::process::Command::new(exe).args(args).exec();
        Err(anyhow::anyhow!("re-exec failed: {error}"))
    }
    #[cfg(not(unix))]
    {
        std::process::Command::new(exe).args(args).spawn()?;
        crate::ablation_report::exit_with_census(0);
    }
}

/// Shared compile-watch skeleton: build the default rock-source `FileWatcher`
/// on `watch_paths`, log `watching_msg` on the `WATCH` target, then call
/// `on_event` for every change until the channel closes. The initial build is
/// the caller's responsibility (per-backend state + signatures differ); this
/// owns only the watcher + event-pump boilerplate that the bone and skull
/// compile-watch loops both hand-rolled identically.
///
/// The game hot-reload loop (`do_stone_watch`) is NOT a caller — it needs a
/// customized watcher (restart trigger, libstone/compiler extensions) and runs
/// a child game process; it's unified separately as the `GameRuntime`.
pub fn run_compile_watch_loop(
    mut watch_paths: Vec<PathBuf>,
    watching_msg: &str,
    mut on_event: impl FnMut(&FileChangeEvent),
) -> Result<()> {
    watch_paths.push(rock_host::stdlib_loader::stdlib_dir());
    let (tx, rx) = channel::<FileChangeEvent>();
    let _watcher = FileWatcher::default_for_rock_sources().watch(watch_paths, tx)?;
    log::info!(target: "WATCH", "{}", watching_msg);
    for event in rx {
        on_event(&event);
    }
    Ok(())
}
