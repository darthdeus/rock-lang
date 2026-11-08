use crate::*;

pub fn loaded_source_watch_paths(project: &str, roots: &[(u32, PathBuf)]) -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from(project)];
    match rock_stdlib::stdlib_origin() {
        rock_stdlib::StdlibSource::Dir(dir) => paths.push(dir),
        rock_stdlib::StdlibSource::Embedded => {
            log::info!(target: "WATCH", "the stdlib is built in; not watching a stdlib directory");
        }
    }
    for (id, root) in roots {
        // Discovery loads only the invoked directory for package zero.
        paths.push(if *id == 0 {
            root.join("stick.toml")
        } else {
            root.clone()
        });
    }
    paths.sort();
    paths.dedup();
    paths
}

pub fn project_engine_watch_paths(project: &str) -> Vec<PathBuf> {
    // Compilation may have failed before it retained a graph. Resolve the same
    // channel-filtered graph so edits to a broken dependency can repair it.
    let graph = (|| -> Result<Option<rock_host::stick::graph::PackageGraph>> {
        let Some((config_path, config)) =
            rock_host::stick::StickConfig::find_from_dir(Path::new(project))?
        else {
            return Ok(None);
        };
        Ok(Some(
            rock_host::stick::graph::PackageGraph::build_with_channel(
                config_path.parent().unwrap_or(&config_path),
                &config,
                &rock_host::stick::resolved_channel_for(&config),
            )?,
        ))
    })();
    let roots = match graph {
        Ok(Some(graph)) => graph
            .packages()
            .iter()
            .map(|package| (package.id.0, package.root.clone()))
            .collect(),
        Ok(None) => Vec::new(),
        Err(error) => {
            log::error!(target: "WATCH",
                "cannot resolve dependency watch roots: {error}; watching the project until its manifest is repaired");
            Vec::new()
        }
    };
    loaded_source_watch_paths(project, &roots)
}

pub fn reexec_current_binary() -> Result<()> {
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
        crate::invocation_log::exit(0);
    }
}

/// Shared compile-watch skeleton: build the default rock-source `FileWatcher`
/// on `watch_paths`, log `watching_msg` on the `WATCH` target, then call
/// `on_event` for every change until the channel closes. The initial build is
/// the caller's responsibility (per-backend state + signatures differ); this
/// owns only the watcher + event-pump boilerplate that the bone and skull
/// compile-watch loops both hand-rolled identically.
pub fn run_compile_watch_loop(
    mut watch_paths: Vec<PathBuf>,
    watching_msg: &str,
    mut on_event: impl FnMut(&FileChangeEvent),
) -> Result<()> {
    match rock_stdlib::stdlib_origin() {
        rock_stdlib::StdlibSource::Dir(dir) => watch_paths.push(dir),
        rock_stdlib::StdlibSource::Embedded => {
            log::info!(target: "WATCH", "the stdlib is built in; watching without a stdlib directory");
        }
    }
    let (tx, rx) = channel::<FileChangeEvent>();
    let _watcher = FileWatcher::default_for_rock_sources().watch(watch_paths, tx)?;
    log::info!(target: "WATCH", "{}", watching_msg);
    for event in rx {
        on_event(&event);
    }
    Ok(())
}
