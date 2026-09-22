use anyhow::Result;
use notify::RecursiveMode;
use notify::event::ModifyKind;
use notify_debouncer_full::{DebouncedEvent, new_debouncer};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

type DebouncerCacheType = notify_debouncer_full::RecommendedCache;
pub(crate) type WatchHandle = notify_debouncer_full::Debouncer<notify::RecommendedWatcher, DebouncerCacheType>;

/// True for files inside a project's *own* `tmp/` output directory
/// (compiler scratch: `main.lua`, etc.), excluding the `restart.txt`
/// manual-restart trigger which must still get through.
///
/// The `tmp` segment is matched only in the path **relative to its
/// watched root** — so a project that itself lives under a `tmp`
/// component (every Linux `tempfile::tempdir()` is `/tmp/...`, and
/// scratch projects commonly sit under `/tmp`) is not misclassified.
/// Anchoring to the root is the whole point: an unrooted `tmp`
/// component match silently drops every source change under `/tmp`
/// and kills hot reload with no error or log. One predicate so the
/// two call sites can't drift.
fn is_tmp_scratch_file(path: &Path, roots: &[PathBuf]) -> bool {
    if path.file_name().and_then(|n| n.to_str()) == Some("restart.txt") {
        return false;
    }
    match roots.iter().find_map(|r| path.strip_prefix(r).ok()) {
        Some(rel) => rel
            .components()
            .any(|c| c.as_os_str().to_str() == Some("tmp")),
        // Outside every watched root: not our scratch. Be conservative
        // — never drop a path we can't attribute to a project tmp/.
        None => false,
    }
}

/// Configuration for the file watcher
pub struct FileWatcherConfig {
    /// Debounce delay in milliseconds
    pub debounce_ms: u64,
    /// Whether to filter out metadata-only changes (like touch)
    pub filter_metadata: bool,
    /// Whether to enable restart trigger file support (tmp/restart.txt)
    pub restart_trigger_enabled: bool,
    /// Exact restart files outside the source roots; their parents are watched
    /// non-recursively so scratch output never becomes a source root.
    pub restart_files: Vec<PathBuf>,
    /// File extensions to watch (e.g., ["rock"])
    pub watch_extensions: Vec<String>,
    pub watch_all_files: bool,
    pub ignored_roots: Vec<PathBuf>,
    /// Exact files watched IN ADDITION to the extension filter, shared with
    /// the owner so it can be updated after watcher creation. Used for
    /// embed_string inputs (docs/comptime-plan.md): the set is only known
    /// after a compile, and grows/shrinks as the game's embeds change.
    pub extra_files: Option<SharedWatchSet>,
}

/// Canonicalized paths consulted by the debounce filter on every event.
pub type SharedWatchSet = std::sync::Arc<parking_lot::Mutex<HashSet<PathBuf>>>;

impl Default for FileWatcherConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 5,
            filter_metadata: false,
            restart_trigger_enabled: false,
            restart_files: Vec::new(),
            watch_extensions: vec!["rock".to_string()],
            watch_all_files: false,
            ignored_roots: Vec::new(),
            extra_files: None,
        }
    }
}

/// File change event data
#[derive(Debug, Clone)]
pub struct FileChangeEvent {
    /// Paths that changed
    pub paths: HashSet<PathBuf>,
    /// Whether this is a restart trigger event
    pub is_restart_trigger: bool,
}

/// Unified file watcher for Rock compiler
pub struct FileWatcher {
    config: FileWatcherConfig,
}

impl FileWatcher {
    /// Create a new file watcher with default configuration
    pub fn new() -> Self {
        Self {
            config: FileWatcherConfig::default(),
        }
    }

    /// Canonical watcher preset for Rock source trees, shared by
    /// `rock stone`, `rock watch`, and `rock test --watch`. Single
    /// source of truth for the settings these used to each hand-roll
    /// (and drift on): 100ms debounce to batch rapid editor saves,
    /// metadata events allowed so `touch` retriggers, `.rock`
    /// extension, restart-trigger off. Stone opts into restart-trigger
    /// and the engine-rebuild extensions (rs/c/h/toml) on top. See
    /// TODO.md "Unify file-watcher configs".
    ///
    /// Only `debounce` differs from `FileWatcherConfig::default()`;
    /// the rest is spelled out so the canonical intent survives a
    /// future change to the struct default.
    pub fn default_for_rock_sources() -> Self {
        Self::new()
            .with_debounce(100)
            .with_metadata_filter(false)
            .with_restart_trigger(false)
            .with_extensions(vec!["rock".to_string()])
    }

    /// Set debounce delay in milliseconds
    pub fn with_debounce(mut self, ms: u64) -> Self {
        self.config.debounce_ms = ms;
        self
    }

    /// Enable/disable metadata change filtering
    pub fn with_metadata_filter(mut self, filter: bool) -> Self {
        self.config.filter_metadata = filter;
        self
    }

    /// Enable/disable restart trigger file support
    pub fn with_restart_trigger(mut self, enabled: bool) -> Self {
        self.config.restart_trigger_enabled = enabled;
        self
    }

    pub fn with_restart_files(mut self, files: Vec<PathBuf>) -> Self {
        self.config.restart_files = files;
        self
    }

    /// Set file extensions to watch
    pub fn with_extensions(mut self, extensions: Vec<String>) -> Self {
        self.config.watch_extensions = extensions;
        self
    }

    pub fn with_all_files(mut self) -> Self {
        self.config.watch_all_files = true;
        self
    }

    /// Watch these exact files as well as the extension set; the shared handle
    /// stays with the caller, which refreshes it after each compile.
    pub fn with_shared_extra_files(mut self, files: SharedWatchSet) -> Self {
        self.config.extra_files = Some(files);
        self
    }

    pub fn with_ignored_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.config.ignored_roots = roots;
        self
    }

    /// Start watching the specified paths
    pub fn watch(
        self,
        paths: Vec<PathBuf>,
        change_tx: Sender<FileChangeEvent>,
    ) -> Result<notify_debouncer_full::Debouncer<notify::RecommendedWatcher, DebouncerCacheType>>
    {
        let config = self.config;

        // Canonicalized watched roots, captured for `is_tmp_scratch_file`'s
        // root-relative `tmp/` check. notify reports event paths under the
        // canonical root, so the roots must be canonical to strip_prefix.
        let scratch_roots: Vec<PathBuf> = paths
            .iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
            .collect();
        let restart_files: HashSet<PathBuf> = config.restart_files.iter().map(|path| {
            // The trigger need not exist yet, but its parent must be watchable.
            path.parent().and_then(|parent| parent.canonicalize().ok())
                .map(|parent| parent.join(path.file_name().expect("restart file name")))
                .unwrap_or_else(|| path.clone())
        }).collect();
        let mut input_parents: HashSet<PathBuf> = restart_files.iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf)).collect();
        if let Some(extra) = &config.extra_files {
            input_parents.extend(extra.lock().iter()
                .filter_map(|path| path.parent().map(Path::to_path_buf)));
        }
        let directory_roots: Vec<PathBuf> = scratch_roots.iter()
            .filter(|path| path.is_dir()).cloned().collect();
        input_parents.extend(scratch_roots.iter().filter(|path| path.is_file())
            .filter_map(|path| path.parent().map(Path::to_path_buf)));
        input_parents.retain(|parent| !directory_roots.iter().any(|root| parent.starts_with(root)));
        let ignored_roots: Vec<PathBuf> = config
            .ignored_roots
            .iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
            .collect();

        // Track last data modification time for each file
        // to debounce metadata events against data events
        let mut data_write_timestamps: HashMap<PathBuf, Instant> = HashMap::new();
        const METADATA_DEBOUNCE_MS: u64 = 50;

        let mut debouncer = new_debouncer(
            Duration::from_millis(config.debounce_ms),
            None,
            move |result: Result<Vec<DebouncedEvent>, Vec<notify::Error>>| {
                match result {
                    Ok(events) => {
                        let mut affected_paths = HashSet::new();
                        let mut has_relevant_changes = false;
                        let mut is_restart_trigger = false;

                        for event in events {
                            let event_paths: Vec<&PathBuf> = event
                                .paths
                                .iter()
                                .filter(|path| {
                                    !ignored_roots.iter().any(|root| path.starts_with(root))
                                        && (scratch_roots.iter().any(|root| path.starts_with(root))
                                            || restart_files.contains(path.as_path())
                                            || config.extra_files.as_ref().is_some_and(|extra| extra.lock().contains(path.as_path())))
                                })
                                .collect();
                            if event_paths.is_empty() {
                                continue;
                            }
                            // Check if this is a restart trigger file.
                            // Match strictly `<...>/tmp/restart.txt` via the
                            // OS-native Path API: works on both `/` and `\`,
                            // and rejects stray `restart.txt` files outside
                            // a `tmp` directory (which the older
                            // `ends_with("/restart.txt")` substring check
                            // would have triggered on).
                            if config.restart_trigger_enabled {
                                for &path in &event_paths {
                                    let is_restart_file = path.file_name().and_then(|n| n.to_str())
                                        == Some("restart.txt");
                                    let parent_is_tmp = path
                                        .parent()
                                        .and_then(|p| p.file_name())
                                        .and_then(|n| n.to_str())
                                        == Some("tmp");
                                    if is_restart_file && parent_is_tmp {
                                        is_restart_trigger = true;
                                        has_relevant_changes = true;
                                        affected_paths.insert(path.clone());
                                    }
                                }
                            }

                            // Check if this event affects watched file types
                            let affects_watched_files = event_paths.iter().any(|path| {
                                if config.watch_all_files {
                                    return !path.to_string_lossy().ends_with('~');
                                }
                                if let Some(extra) = config.extra_files.as_ref() {
                                    let member = extra.lock().contains(path.as_path());
                                    if member {
                                        return true;
                                    }
                                }
                                if let Some(ext) = path.extension() {
                                    if let Some(ext_str) = ext.to_str() {
                                        // Ignore backup files ending with ~
                                        if path.to_string_lossy().ends_with('~') {
                                            return false;
                                        }
                                        return config
                                            .watch_extensions
                                            .contains(&ext_str.to_string());
                                    }
                                }
                                false
                            });

                            // Determine if this is a relevant event
                            let is_relevant = match event.kind {
                                notify::EventKind::Modify(modify_kind) => {
                                    match modify_kind {
                                        ModifyKind::Data(_) => {
                                            // Data modifications always go through
                                            // and update the timestamp for each affected file
                                            // BUT: don't track timestamps for tmp/ files (except restart.txt)
                                            let now = Instant::now();
                                            for &path in &event_paths {
                                                if !is_tmp_scratch_file(path, &scratch_roots) {
                                                    data_write_timestamps.insert(path.clone(), now);
                                                }
                                            }
                                            true
                                        }
                                        ModifyKind::Metadata(_) => {
                                            // Check if we should debounce this metadata event
                                            // against a recent data event on the same file
                                            let mut should_debounce = false;
                                            for &path in &event_paths {
                                                if let Some(last_data_write) =
                                                    data_write_timestamps.get(path)
                                                {
                                                    let elapsed = Instant::now()
                                                        .duration_since(*last_data_write);
                                                    if elapsed
                                                        < Duration::from_millis(
                                                            METADATA_DEBOUNCE_MS,
                                                        )
                                                    {
                                                        should_debounce = true;
                                                        break;
                                                    }
                                                }
                                            }

                                            if should_debounce {
                                                false
                                            } else {
                                                // Original metadata filtering logic

                                                is_restart_trigger
                                                    || (!config.filter_metadata
                                                        && affects_watched_files)
                                            }
                                        }
                                        ModifyKind::Name(_) => true,
                                        ModifyKind::Any => true,
                                        ModifyKind::Other => true,
                                    }
                                }
                                notify::EventKind::Remove(_) => true,
                                notify::EventKind::Create(_) => true,
                                notify::EventKind::Other => true,
                                notify::EventKind::Any => true,
                                // Access events never indicate a content change.
                                notify::EventKind::Access(_) => false,
                            };

                            if is_relevant && (affects_watched_files || is_restart_trigger) {
                                has_relevant_changes = true;
                                for &path in &event_paths {
                                    if is_tmp_scratch_file(path, &scratch_roots) {
                                        continue;
                                    }
                                    let is_backup = path.to_string_lossy().ends_with('~');
                                    if !path.is_dir() && !is_backup {
                                        affected_paths.insert(path.clone());
                                    }
                                }
                            }
                        }

                        if has_relevant_changes && !affected_paths.is_empty() {
                            let event = FileChangeEvent {
                                paths: affected_paths,
                                is_restart_trigger,
                            };
                            // Ignore send errors (receiver might have been dropped)
                            let _ = change_tx.send(event);
                        }
                    }
                    Err(errors) => {
                        for error in errors {
                            log::error!(target: "WATCH", "file watcher error: {}", error);
                        }
                    }
                }
            },
        )?;

        // Watch parents, not input inodes: editors replace the inode on save.
        // The callback admits only exact inputs outside the source roots.
        for parent in input_parents {
            if parent.exists() {
                debouncer.watch(&parent, RecursiveMode::NonRecursive)?;
            }
        }

        for root in directory_roots {
            debouncer.watch(&root, RecursiveMode::Recursive)?;
        }

        Ok(debouncer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_watch_set_survives_a_writer_panic() {
        let set: SharedWatchSet = Default::default();
        let writer = set.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            writer.lock().insert(PathBuf::from("held"));
            panic!("poison probe");
        }));
        assert!(set.lock().contains(&PathBuf::from("held")));
    }
}
