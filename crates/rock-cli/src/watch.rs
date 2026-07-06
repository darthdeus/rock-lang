use crate::*;

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
    // The stdlib is read from disk at runtime, so every compile-watch loop must
    // watch it too -- an `@pure` annotation or new stdlib fn must retrigger a
    // recompile without restarting `rock watch`. Honors ROCK_STDLIB_DIR.
    watch_paths.push(rock_host::stdlib_loader::stdlib_dir());
    let (tx, rx) = channel::<FileChangeEvent>();
    let _watcher = FileWatcher::default_for_rock_sources().watch(watch_paths, tx)?;
    log::info!(target: "WATCH", "{}", watching_msg);
    for event in rx {
        on_event(&event);
    }
    Ok(())
}
