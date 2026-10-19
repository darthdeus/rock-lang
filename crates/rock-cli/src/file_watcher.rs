use anyhow::Result;
#[cfg(not(target_os = "macos"))]
use notify::RecursiveMode;
#[cfg(not(target_os = "macos"))]
use notify::event::ModifyKind;
#[cfg(not(target_os = "macos"))]
use notify_debouncer_full::{DebouncedEvent, new_debouncer};
// `HashMap` serves only the gated `InitialFiles` snapshot and `watch_notify` below.
#[cfg(any(not(target_os = "macos"), test))]
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
#[cfg(not(target_os = "macos"))]
use std::time::Duration;
use bone_core::prelude::Instant;

#[cfg(not(target_os = "macos"))]
type DebouncerCacheType = notify_debouncer_full::RecommendedCache;
#[cfg(not(target_os = "macos"))]
pub type WatchHandle = notify_debouncer_full::Debouncer<notify::RecommendedWatcher, DebouncerCacheType>;
#[cfg(target_os = "macos")]
pub type WatchHandle = macos::MacHandle;

/// True for files inside a project's *own* `tmp/` output directory
/// (compiler scratch: `main.lua`, etc.). Explicit restart inputs bypass this filter.
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
    /// Optional relative path suffix that the host treats as a restart input.
    pub restart_suffix: Option<PathBuf>,
    /// Exact restart files outside the source roots; their parents are watched
    /// non-recursively so scratch output never becomes a source root.
    pub restart_files: Vec<PathBuf>,
    /// File extensions to watch (e.g., ["rock"])
    pub watch_extensions: Vec<String>,
    pub watch_all_files: bool,
    pub ignored_roots: Vec<PathBuf>,
    /// Exact files watched IN ADDITION to the extension filter, shared with
    /// the owner so it can be updated after watcher creation. Used for
    /// embed_string inputs (the compiler contract): the set is only known
    /// after a compile, and grows/shrinks as the game's embeds change.
    pub extra_files: Option<SharedWatchSet>,
    /// Ignore macOS create history for unchanged files present when a watch starts.
    pub ignore_existing_creates: bool,
}

/// Canonicalized paths consulted by the debounce filter on every event.
pub type SharedWatchSet = std::sync::Arc<parking_lot::Mutex<HashSet<PathBuf>>>;

// Snapshot filter for the notify backend (`watch_notify`) and its unit test;
// the macOS watcher polls instead, so this is dead in non-test macOS builds.
#[cfg(any(not(target_os = "macos"), test))]
#[derive(PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    inode: u64,
}

#[cfg(any(not(target_os = "macos"), test))]
impl FileStamp {
    fn read(path: &Path) -> Option<Self> {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                log::warn!(target: "WATCH", "cannot inspect {}: {error}", path.display());
                return None;
            }
        };
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(error) => {
                log::warn!(target: "WATCH", "cannot read modification time for {}: {error}", path.display());
                return None;
            }
        };
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            len: metadata.len(),
            modified,
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }
}

#[cfg(any(not(target_os = "macos"), test))]
#[derive(Default)]
struct InitialFiles {
    stamps: HashMap<PathBuf, FileStamp>,
}

#[cfg(any(not(target_os = "macos"), test))]
impl InitialFiles {
    fn capture(roots: &[PathBuf], extensions: &[String], extra_files: &HashSet<PathBuf>, ignored_roots: &[PathBuf]) -> Self {
        let mut stamps = HashMap::new();
        for root in roots {
            if ignored_roots.iter().any(|ignored| root.starts_with(ignored)) {
                continue;
            }
            if !root.exists() {
                continue;
            }
            if root.is_file() {
                if let Some(stamp) = FileStamp::read(root) {
                    stamps.insert(root.clone(), stamp);
                }
                continue;
            }
            for entry in walkdir::WalkDir::new(root).into_iter().filter_entry(|entry| {
                !is_tmp_scratch_file(entry.path(), std::slice::from_ref(root))
                    && !ignored_roots.iter().any(|ignored| entry.path().starts_with(ignored))
            }) {
                match entry {
                    Ok(entry) if entry.file_type().is_file() => {
                        let path = entry.path();
                        let selected = path.extension().and_then(|ext| ext.to_str())
                            .is_some_and(|ext| extensions.iter().any(|candidate| candidate == ext));
                        if (selected || extra_files.contains(path)) && let Some(stamp) = FileStamp::read(path) {
                            stamps.insert(path.to_path_buf(), stamp);
                        }
                    }
                    Ok(_) => {}
                    Err(error) => log::warn!(target: "WATCH", "initial watch snapshot: {error}"),
                }
            }
        }
        for path in extra_files {
            if ignored_roots.iter().any(|ignored| path.starts_with(ignored)) {
                continue;
            }
            if let Some(stamp) = FileStamp::read(path) {
                stamps.insert(path.clone(), stamp);
            }
        }
        Self { stamps }
    }

    fn is_unchanged_create(&self, kind: &notify::EventKind, path: &Path) -> bool {
        matches!(kind, notify::EventKind::Create(_))
            && self.stamps.get(path).is_some_and(|initial| FileStamp::read(path).as_ref() == Some(initial))
    }
}

impl FileWatcherConfig {
    fn matches_restart_suffix(&self, path: &Path) -> bool {
        self.restart_suffix.as_ref().is_some_and(|suffix| path.ends_with(suffix))
    }
}

impl Default for FileWatcherConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 5,
            filter_metadata: false,
            restart_suffix: None,
            restart_files: Vec::new(),
            watch_extensions: vec!["rock".to_string()],
            watch_all_files: false,
            ignored_roots: Vec::new(),
            extra_files: None,
            ignore_existing_creates: false,
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
    /// `rock engine`, `rock watch`, and `rock test --watch`. Single
    /// source of truth for the settings these used to each hand-roll
    /// (and drift on): 100ms debounce to batch rapid editor saves,
    /// metadata events allowed so `touch` retriggers, `.rock`
    /// extension, no restart inputs. Hosts supply restart inputs and extra extensions. See
    /// TODO.md "Unify file-watcher configs".
    ///
    /// Only `debounce` differs from `FileWatcherConfig::default()`;
    /// the rest is spelled out so the canonical intent survives a
    /// future change to the struct default.
    pub fn default_for_rock_sources() -> Self {
        Self::new()
            .with_debounce(100)
            .with_metadata_filter(false)
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

    /// Supply a host's relative restart path, including inputs below skipped scratch directories.
    pub fn with_restart_suffix(mut self, suffix: PathBuf) -> Self {
        self.config.restart_suffix = Some(suffix);
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

    pub fn with_initial_create_filter(mut self, enabled: bool) -> Self {
        self.config.ignore_existing_creates = enabled;
        self
    }

    /// Start watching the specified paths
    pub fn watch(
        self,
        paths: Vec<PathBuf>,
        change_tx: Sender<FileChangeEvent>,
    ) -> Result<WatchHandle> {
        #[cfg(target_os = "macos")]
        { macos::start(self.config, paths, change_tx) }
        #[cfg(not(target_os = "macos"))]
        { self.watch_notify(paths, change_tx) }
    }

    #[cfg(not(target_os = "macos"))]
    fn watch_notify(self, paths: Vec<PathBuf>, change_tx: Sender<FileChangeEvent>) -> Result<WatchHandle> {
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
        let initial_files = config.ignore_existing_creates.then(|| {
            let mut extra_files = restart_files.clone();
            if let Some(extra) = &config.extra_files {
                extra_files.extend(extra.lock().iter().cloned());
            }
            InitialFiles::capture(&scratch_roots, &config.watch_extensions, &extra_files, &ignored_roots)
        });

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
                                        && !initial_files.as_ref().is_some_and(|initial| initial.is_unchanged_create(&event.kind, path))
                                        && (scratch_roots.iter().any(|root| path.starts_with(root))
                                            || restart_files.contains(path.as_path())
                                            || config.extra_files.as_ref().is_some_and(|extra| extra.lock().contains(path.as_path())))
                                })
                                .collect();
                            if event_paths.is_empty() {
                                continue;
                            }
                            for &path in &event_paths {
                                if restart_files.contains(path.as_path()) || config.matches_restart_suffix(path) {
                                    is_restart_trigger = true;
                                    has_relevant_changes = true;
                                    affected_paths.insert(path.clone());
                                }
                            }

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
                                            // Explicit restart inputs bypass the scratch filter.
                                            let now = Instant::now();
                                            for &path in &event_paths {
                                                if restart_files.contains(path.as_path()) || config.matches_restart_suffix(path) || !is_tmp_scratch_file(path, &scratch_roots) {
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
                                    if !restart_files.contains(path.as_path()) && !config.matches_restart_suffix(path) && is_tmp_scratch_file(path, &scratch_roots) {
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

    #[test]
    fn unchanged_preexisting_create_is_filtered_but_later_edits_are_not() {
        use notify::event::CreateKind;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rock");
        std::fs::write(&path, "a").unwrap();
        let ignored = dir.path().join("ignored");
        std::fs::create_dir(&ignored).unwrap();
        let ignored_source = ignored.join("omitted.rock");
        std::fs::write(&ignored_source, "ignored").unwrap();
        let initial = InitialFiles::capture(&[dir.path().to_path_buf()], &["rock".to_string()], &HashSet::new(), &[ignored]);
        assert!(!initial.stamps.contains_key(&ignored_source));
        let created = notify::EventKind::Create(CreateKind::File);
        assert!(initial.is_unchanged_create(&created, &path));
        let replacement = dir.path().join("saving");
        std::fs::write(&replacement, "a").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(!initial.is_unchanged_create(&created, &path));
        std::fs::write(&path, "longer body").unwrap();
        assert!(!initial.is_unchanged_create(&created, &path));
        std::fs::remove_file(&path).unwrap();
        assert!(!initial.is_unchanged_create(&created, &path));
        std::fs::write(&path, "recreated body").unwrap();
        assert!(!initial.is_unchanged_create(&created, &path));
        let new_path = dir.path().join("later.rock");
        std::fs::write(&new_path, "new source").unwrap();
        assert!(!initial.is_unchanged_create(&created, &new_path));
    }
}

#[cfg(any(target_os = "macos", all(test, unix)))]
mod macos {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::os::unix::fs::MetadataExt;
    #[cfg(target_os = "macos")]
    use std::sync::mpsc::{channel, RecvTimeoutError};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[derive(Clone, PartialEq, Eq)]
    struct Stamp {
        inode: u64,
        size: u64,
        modified: (i64, i64),
        changed: (i64, i64),
    }
    impl Stamp {
        fn read(path: &Path) -> std::io::Result<Self> {
            let meta = std::fs::metadata(path)?;
            Ok(Self { inode: meta.ino(), size: meta.len(), modified: (meta.mtime(), meta.mtime_nsec()), changed: (meta.ctime(), meta.ctime_nsec()) })
        }
    }
    #[derive(Clone)]
    struct FileState {
        stamp: Stamp,
        hash: [u8; 32],
    }
    fn content_hash(path: &Path) -> std::io::Result<[u8; 32]> {
        let mut file = std::fs::File::open(path)?;
        let mut hash = Sha256::new();
        std::io::copy(&mut file, &mut hash)?;
        Ok(hash.finalize().into())
    }
    struct Directory {
        stamp: Stamp,
        files: Vec<PathBuf>,
        children: Vec<PathBuf>,
    }
    struct Observation {
        config: FileWatcherConfig,
        roots: Vec<PathBuf>,
        directories: HashMap<PathBuf, Directory>,
        files: HashMap<PathBuf, FileState>,
        started: SystemTime,
        #[cfg(test)]
        directory_reads: u64,
    }
    impl Observation {
        fn admitted(&self, path: &Path) -> bool {
            if self.config.ignored_roots.iter().any(|root| path.starts_with(root))
                || (!self.restart(path) && is_tmp_scratch_file(path, &self.roots))
                || path.as_os_str().to_string_lossy().ends_with('~') {
                return false;
            }
            self.restart(path) || self.config.watch_all_files
                || self.config.extra_files.as_ref().is_some_and(|extra| extra.lock().contains(path))
                || path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| self.config.watch_extensions.iter().any(|wanted| wanted == ext))
        }
        fn restart(&self, path: &Path) -> bool {
            self.config.restart_files.iter().any(|file| file == path) || self.config.matches_restart_suffix(path)
        }
        fn descend(&self, path: &Path) -> bool {
            !self.config.ignored_roots.iter().any(|root| path.starts_with(root))
                && !is_tmp_scratch_file(path, &self.roots)
                && !path.file_name().is_some_and(|name| name == ".git" || name == "target")
        }
        fn collect(&mut self, root: &Path, candidates: &mut HashSet<PathBuf>, visited: &mut HashSet<PathBuf>, ancestors: &mut HashSet<(u64, u64)>) -> Result<()> {
            let metadata = match std::fs::metadata(root) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            let identity = (metadata.dev(), metadata.ino());
            // Follow directory links, but never recurse back into an ancestor.
            if !ancestors.insert(identity) { return Ok(()); }
            visited.insert(root.to_path_buf());
            let stamp = match Stamp::read(root) {
                Ok(stamp) => stamp,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.directories.remove(root);
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            if self.directories.get(root).is_none_or(|directory| directory.stamp != stamp) {
                let mut files = Vec::new();
                let mut children = Vec::new();
                #[cfg(test)]
                { self.directory_reads += 1; }
                for entry in std::fs::read_dir(root)? {
                    let entry = entry?;
                    let path = entry.path();
                    let kind = entry.file_type()?;
                    let metadata = if kind.is_symlink() { Some(std::fs::metadata(&path)?) } else { None };
                    if kind.is_dir() || metadata.as_ref().is_some_and(|meta| meta.is_dir()) {
                        if self.descend(&path) { children.push(path); }
                        else if let Some(suffix) = &self.config.restart_suffix {
                            if suffix.parent().is_some_and(|parent| path.ends_with(parent)) {
                                if let Some(name) = suffix.file_name() { files.push(path.join(name)); }
                            }
                        }
                    } else if kind.is_file() || metadata.as_ref().is_some_and(|meta| meta.is_file()) {
                        files.push(path);
                    }
                }
                self.directories.insert(root.to_path_buf(), Directory { stamp, files, children });
            }
            let directory = &self.directories[root];
            candidates.extend(directory.files.iter().filter(|path| self.admitted(path)).cloned());
            let children = directory.children.clone();
            for child in children { self.collect(&child, candidates, visited, ancestors)?; }
            ancestors.remove(&identity);
            Ok(())
        }
        fn scan(&mut self, initial: bool) -> Result<FileChangeEvent> {
            let mut candidates = HashSet::new();
            let mut visited = HashSet::new();
            for root in self.roots.clone() {
                match std::fs::metadata(&root) {
                    Ok(meta) if meta.is_dir() => self.collect(&root, &mut candidates, &mut visited, &mut HashSet::new())?,
                    Ok(_) if self.admitted(&root) => { candidates.insert(root); },
                    Ok(_) => {},
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                    Err(error) => return Err(error.into()),
                }
            }
            self.directories.retain(|path, _| visited.contains(path));
            if let Some(extra) = &self.config.extra_files {
                let inputs = extra.lock().clone();
                candidates.extend(inputs.into_iter().filter(|path| self.admitted(path)));
            }
            candidates.extend(self.config.restart_files.iter().filter(|path| self.admitted(path)).cloned());
            candidates.extend(self.files.keys().filter(|path| self.admitted(path)).cloned());
            self.observe(candidates.into_iter().collect(), initial)
        }
        fn observe(&mut self, candidates: Vec<PathBuf>, initial: bool) -> Result<FileChangeEvent> {
            let mut event = FileChangeEvent { paths: HashSet::new(), is_restart_trigger: false };
            let mut updates = Vec::new();
            for path in &candidates {
                let stamp = match Stamp::read(&path) {
                    Ok(stamp) => stamp,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        if self.files.contains_key(path) {
                            updates.push((path.clone(), None));
                            if !initial {
                                event.is_restart_trigger |= self.restart(path);
                                event.paths.insert(path.clone());
                            }
                        }
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
                let old = self.files.get(path);
                if old.is_some_and(|old| old.stamp == stamp) { continue; }
                let hash = match content_hash(&path) {
                    Ok(hash) => hash,
                    // Atomic saves can replace the path between stat and open;
                    // leave the prior state intact for the next scan.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                let changed_after_start = stamp.changed.0 >= 0
                    && UNIX_EPOCH + Duration::new(stamp.changed.0 as u64, stamp.changed.1 as u32) > self.started;
                // A newly declared embed may predate this watcher. Its seed
                // establishes a baseline; an edit since startup still gets through.
                let declared_seed = old.is_none() && !changed_after_start;
                let relevant = !declared_seed && (old.is_none_or(|old| old.hash != hash || old.stamp.inode != stamp.inode)
                    || !self.config.filter_metadata || self.restart(&path));
                updates.push((path.clone(), Some(FileState { stamp, hash })));
                if relevant && !initial {
                    event.is_restart_trigger |= self.restart(&path);
                    event.paths.insert(path.clone());
                }
            }
            // Commit stamps only with the complete event; later IO failures must
            // leave earlier edits eligible for the retry.
            let retained: HashSet<&PathBuf> = candidates.iter().collect();
            self.files.retain(|path, _| retained.contains(&path));
            for (path, state) in updates {
                if let Some(state) = state { self.files.insert(path, state); }
                else { self.files.remove(&path); }
            }
            Ok(event)
        }
    }
    #[cfg(target_os = "macos")]
    pub struct MacHandle {
        stop: Sender<()>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    #[cfg(target_os = "macos")]
    impl Drop for MacHandle {
        fn drop(&mut self) {
            // The worker may already have stopped when its consumer disconnected.
            if self.stop.send(()).is_err() { log::debug!(target: "WATCH", "file watcher worker already stopped"); }
            if self.thread.take().is_some_and(|thread| thread.join().is_err()) {
                log::error!(target: "WATCH", "file watcher worker panicked");
            }
        }
    }
    #[cfg(target_os = "macos")]
    pub fn start(mut config: FileWatcherConfig, paths: Vec<PathBuf>, change_tx: Sender<FileChangeEvent>) -> Result<MacHandle> {
        // Exact inputs may not exist yet; canonicalize their containing directory.
        let canonical = |path: PathBuf| -> Result<PathBuf> {
            if path.exists() { return Ok(path.canonicalize()?); }
            let parent = path.parent().ok_or_else(|| anyhow::anyhow!("watch path has no parent"))?;
            Ok(parent.canonicalize()?.join(path.file_name().ok_or_else(|| anyhow::anyhow!("watch path has no name"))?))
        };
        let roots = paths.into_iter().map(canonical).collect::<Result<Vec<_>>>()?;
        config.ignored_roots = config.ignored_roots.into_iter().map(canonical).collect::<Result<Vec<_>>>()?;
        config.restart_files = config.restart_files.into_iter().map(canonical).collect::<Result<Vec<_>>>()?;
        let interval = Duration::from_millis(config.debounce_ms.max(1));
        let mut observation = Observation { config, roots, directories: HashMap::new(), files: HashMap::new(), started: SystemTime::now(), #[cfg(test)] directory_reads: 0 };
        observation.scan(true)?;
        let (stop, stopped) = channel();
        let thread = std::thread::Builder::new().name("rock file state watcher".into()).spawn(move || {
            loop {
                match stopped.recv_timeout(interval) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {},
                }
                match observation.scan(false) {
                    Ok(event) if !event.paths.is_empty() => {
                        // A disconnected consumer ends this watcher's lifetime.
                        if change_tx.send(event).is_err() { break; }
                    }
                    Ok(_) => {},
                    Err(error) => log::error!(target: "WATCH", "file watcher scan failed: {error:#}"),
                }
            }
        })?;
        Ok(MacHandle { stop, thread: Some(thread) })
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        fn observation(root: PathBuf, config: FileWatcherConfig) -> Observation {
            let mut observation = Observation { config, roots: vec![root], directories: HashMap::new(), files: HashMap::new(), started: SystemTime::now(), directory_reads: 0 };
            observation.scan(true).unwrap();
            observation
        }
        #[test]
        fn initial_files_and_declared_inputs_do_not_create_false_changes() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let source = root.join("main.rock");
            let input = root.join("input.txt");
            std::fs::write(&source, "before").unwrap();
            std::fs::write(root.join("stick.toml"), "manifest seed").unwrap();
            std::fs::write(&input, "input seed").unwrap();
            let extra: SharedWatchSet = Default::default();
            let config = FileWatcherConfig { watch_extensions: vec!["rock".into(), "toml".into()], extra_files: Some(extra.clone()), ..Default::default() };
            let mut observation = observation(root, config);
            assert!(observation.scan(false).unwrap().paths.is_empty());
            std::fs::write(&source, "after").unwrap();
            assert_eq!(observation.scan(false).unwrap().paths, HashSet::from([source]));
            extra.lock().insert(input.clone());
            assert!(observation.scan(false).unwrap().paths.is_empty(), "declaring an unchanged input must not manufacture a reload");
            std::fs::write(&input, "input edit").unwrap();
            assert_eq!(observation.scan(false).unwrap().paths, HashSet::from([input]));
            let pending = dir.path().canonicalize().unwrap().join("pending.txt");
            std::fs::write(&pending, "changed before first admission scan").unwrap();
            extra.lock().insert(pending.clone());
            assert_eq!(observation.scan(false).unwrap().paths, HashSet::from([pending]));
        }
        #[test]
        fn state_changes_preserve_new_sources_atomic_saves_touch_and_removals() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let source = root.join("main.rock");
            std::fs::write(&source, "before").unwrap();
            let mut observation = observation(root.clone(), FileWatcherConfig { filter_metadata: true, ..Default::default() });
            let modified = std::fs::metadata(&source).unwrap().modified().unwrap();
            std::fs::File::options().write(true).open(&source).unwrap().set_modified(modified + Duration::from_nanos(1)).unwrap();
            assert!(observation.scan(false).unwrap().paths.is_empty(), "metadata filter must reject a nanosecond touch");
            std::fs::write(&source, "after").unwrap();
            assert_eq!(observation.scan(false).unwrap().paths, HashSet::from([source.clone()]));
            let temporary = source.with_extension("saving");
            std::fs::write(&temporary, "after").unwrap();
            std::fs::rename(&temporary, &source).unwrap();
            assert_eq!(observation.scan(false).unwrap().paths, HashSet::from([source.clone()]), "same-byte inode replacement is still an atomic save");
            observation.config.filter_metadata = false;
            let modified = std::fs::metadata(&source).unwrap().modified().unwrap();
            std::fs::File::options().write(true).open(&source).unwrap().set_modified(modified + Duration::from_nanos(1)).unwrap();
            assert_eq!(observation.scan(false).unwrap().paths, HashSet::from([source.clone()]));
            let added = root.join("added.rock");
            std::fs::write(&added, "added").unwrap();
            assert_eq!(observation.scan(false).unwrap().paths, HashSet::from([added.clone()]));
            std::fs::remove_file(&added).unwrap();
            assert_eq!(observation.scan(false).unwrap().paths, HashSet::from([added]));
        }
        #[test]
        fn unchanged_asset_directories_reuse_entries_and_build_outputs_are_pruned() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            for name in ["assets", "target", ".git", "tmp"] { std::fs::create_dir_all(root.join(name)).unwrap(); }
            std::fs::write(root.join("main.rock"), "source").unwrap();
            for name in ["target", ".git", "tmp"] { std::fs::write(root.join(name).join("ignored.rock"), "build output").unwrap(); }
            for i in 0..100 { std::fs::write(root.join("assets").join(format!("image-{i}.png")), [0; 1024]).unwrap(); }
            let mut observation = observation(root.clone(), Default::default());
            assert_eq!(observation.files.len(), 1);
            let reads = observation.directory_reads;
            assert!(observation.scan(false).unwrap().paths.is_empty());
            assert_eq!(observation.directory_reads, reads, "unchanged asset directories must not be reread");
            assert!(!observation.directories.keys().any(|path| path.ends_with("target") || path.ends_with(".git") || path.ends_with("tmp")));
            let added = root.join("assets/source.rock");
            std::fs::write(&added, "new source").unwrap();
            assert_eq!(observation.scan(false).unwrap().paths, HashSet::from([added]), "legitimate new sources in assets remain visible");
        }
        #[test]
        fn restart_removal_is_a_trigger() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            std::fs::create_dir(root.join("tmp")).unwrap();
            let restart = root.join("tmp/reload.signal");
            std::fs::write(&restart, "trigger").unwrap();
            for config in [
                FileWatcherConfig { restart_files: vec![restart.clone()], ..Default::default() },
                FileWatcherConfig { restart_suffix: Some(PathBuf::from("tmp/reload.signal")), ..Default::default() },
            ] {
                std::fs::write(&restart, "trigger").unwrap();
                let mut observation = observation(root.clone(), config);
                std::fs::remove_file(&restart).unwrap();
                let event = observation.scan(false).unwrap();
                assert_eq!(event.paths, HashSet::from([restart.clone()]));
                assert!(event.is_restart_trigger);
            }
        }
        #[test]
        fn unreadable_input_does_not_consume_another_files_edit() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let source = root.join("main.rock");
            let broken = root.join("broken.rock");
            std::fs::write(&source, "before").unwrap();
            let mut observation = observation(root.clone(), Default::default());
            std::fs::write(&source, "after").unwrap();
            // A symlink loop is a deterministic metadata IO error even as root.
            std::os::unix::fs::symlink(&broken, &broken).unwrap();
            assert!(observation.observe(vec![source.clone(), broken.clone()], false).is_err());
            std::fs::remove_file(&broken).unwrap();
            assert_eq!(observation.scan(false).unwrap().paths, HashSet::from([source]));
        }
        #[test]
        fn all_files_watch_follows_directory_links_without_cycles() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let external = tempfile::tempdir().unwrap();
            std::fs::write(external.path().join("data.txt"), "before").unwrap();
            std::os::unix::fs::symlink(external.path(), root.join("linked")).unwrap();
            std::os::unix::fs::symlink(&root, external.path().join("cycle")).unwrap();
            let mut observation = observation(root.clone(), FileWatcherConfig { watch_all_files: true, ..Default::default() });
            std::fs::write(external.path().join("data.txt"), "after").unwrap();
            assert_eq!(observation.scan(false).unwrap().paths, HashSet::from([root.join("linked/data.txt")]));
        }
        #[test]
        #[ignore = "manual watcher startup and steady-state measurement"]
        fn measure_source_watch_population() {
            use bone_core::prelude::Instant;
            let root = PathBuf::from(std::env::var_os("ROCK_WATCH_MEASURE_ROOT")
                .expect("set ROCK_WATCH_MEASURE_ROOT to the source tree to measure"));
            for (name, roots) in [("source", vec![root, rock_stdlib::stdlib_dir()])] {
                let config = FileWatcherConfig { debounce_ms: 100, watch_extensions: vec!["rock".into(), "rs".into(), "c".into(), "h".into(), "toml".into()], ..Default::default() };
                let mut observation = Observation { config, roots, directories: HashMap::new(), files: HashMap::new(), started: SystemTime::now(), directory_reads: 0 };
                let start = Instant::now();
                observation.scan(true).unwrap();
                let startup = start.elapsed();
                let reads = observation.directory_reads;
                let start = Instant::now();
                for _ in 0..100 { assert!(observation.scan(false).unwrap().paths.is_empty()); }
                println!("WATCH_MEASURE {name} startup_us={} warm_mean_us={} directories={} files={} warm_directory_reads={}", startup.as_micros(), start.elapsed().as_micros() / 100, observation.directories.len(), observation.files.len(), observation.directory_reads - reads);
            }
        }
    }

}
