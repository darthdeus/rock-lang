use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

pub(crate) use state::WatchHandle;

fn is_tmp_scratch_file(path: &Path, roots: &[PathBuf]) -> bool {
    match roots.iter().find_map(|r| path.strip_prefix(r).ok()) {
        Some(rel) => rel
            .components()
            .any(|c| c.as_os_str().to_str() == Some("tmp")),
        None => false,
    }
}

pub struct FileWatcher {
    debounce_ms: u64,
    restart_suffix: Option<PathBuf>,
    restart_files: Vec<PathBuf>,
    watch_extensions: Vec<String>,
    watch_all_files: bool,
    extra_files: Option<SharedWatchSet>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SharedWatchSet {
    paths: std::sync::Arc<parking_lot::Mutex<HashSet<PathBuf>>>,
    compiled: std::sync::Arc<parking_lot::Mutex<HashMap<PathBuf, u64>>>,
}

impl SharedWatchSet {
    fn paths(&self) -> parking_lot::MutexGuard<'_, HashSet<PathBuf>> {
        self.paths.lock()
    }
    pub(crate) fn set_compiled_hash(&self, path: PathBuf, hash: u64) {
        self.compiled.lock().insert(path, hash);
    }
    pub(crate) fn compiled_hash(&self, path: &Path) -> Option<u64> {
        self.compiled.lock().get(path).copied()
    }
    pub(crate) fn insert_with_hash(&self, path: PathBuf, hash: u64) {
        self.paths().insert(path.clone());
        self.set_compiled_hash(path, hash);
    }
    pub(crate) fn replace_with_hashes(&self, files: Vec<(PathBuf, u64)>) {
        let mut paths = self.paths();
        let mut compiled = self.compiled.lock();
        paths.clear();
        compiled.clear();
        for (path, hash) in files {
            paths.insert(path.clone());
            compiled.insert(path, hash);
        }
    }
}

impl FileWatcher {
    fn matches_restart_suffix(&self, path: &Path) -> bool {
        self.restart_suffix
            .as_ref()
            .is_some_and(|suffix| path.ends_with(suffix))
    }
}

#[derive(Debug, Clone)]
pub struct FileChangeEvent {
    pub paths: HashSet<PathBuf>,
    pub is_restart_trigger: bool,
}

impl FileWatcher {
    pub fn new() -> Self {
        Self {
            debounce_ms: 5,
            restart_suffix: None,
            restart_files: Vec::new(),
            watch_extensions: vec!["rock".to_string()],
            watch_all_files: false,
            extra_files: None,
        }
    }

    pub(crate) fn default_for_rock_sources() -> Self {
        Self {
            debounce_ms: 100,
            ..Self::new()
        }
    }

    pub fn with_restart_suffix(mut self, suffix: PathBuf) -> Self {
        self.restart_suffix = Some(suffix);
        self
    }

    pub(crate) fn with_restart_files(mut self, files: Vec<PathBuf>) -> Self {
        self.restart_files = files;
        self
    }

    pub(crate) fn with_extensions(mut self, extensions: Vec<String>) -> Self {
        self.watch_extensions = extensions;
        self
    }

    pub(crate) fn with_all_files(mut self) -> Self {
        self.watch_all_files = true;
        self
    }

    pub(crate) fn with_shared_extra_files(mut self, files: SharedWatchSet) -> Self {
        self.extra_files = Some(files);
        self
    }

    pub fn watch(
        self,
        paths: Vec<PathBuf>,
        change_tx: Sender<FileChangeEvent>,
    ) -> Result<WatchHandle> {
        state::start(self, paths, change_tx)
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
            writer.paths().insert(PathBuf::from("held"));
            panic!("poison probe");
        }));
        assert!(set.paths().contains(&PathBuf::from("held")));
    }
}

/// Changes are observed from file state (stamps, then content hashes) rather
/// than from OS change notifications.
mod state {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[derive(Clone, PartialEq, Eq)]
    struct Stamp {
        inode: u64,
        size: u64,
        modified: (i64, i64),
        changed: (i64, i64),
    }
    impl Stamp {
        #[cfg(unix)]
        fn read(path: &Path) -> std::io::Result<Self> {
            let meta = std::fs::metadata(path)?;
            Ok(Self {
                inode: meta.ino(),
                size: meta.len(),
                modified: (meta.mtime(), meta.mtime_nsec()),
                changed: (meta.ctime(), meta.ctime_nsec()),
            })
        }
        #[cfg(not(unix))]
        fn read(path: &Path) -> std::io::Result<Self> {
            let meta = std::fs::metadata(path)?;
            let modified = meta
                .modified()?
                .duration_since(UNIX_EPOCH)
                .map_or((0, 0), |d| (d.as_secs() as i64, d.subsec_nanos() as i64));
            Ok(Self {
                inode: 0,
                size: meta.len(),
                modified,
                changed: modified,
            })
        }
    }
    #[derive(Clone)]
    struct FileState {
        stamp: Stamp,
        hash: [u8; 32],
        verified_at: SystemTime,
    }
    fn path_error(error: std::io::Error, operation: &str, path: &Path) -> anyhow::Error {
        std::io::Error::new(
            error.kind(),
            format!("{operation} {}: {error}", path.display()),
        )
        .into()
    }
    fn stat_error(error: std::io::Error, path: &Path) -> anyhow::Error {
        path_error(error, "watch", path)
    }
    fn content_hash(path: &Path) -> std::io::Result<[u8; 32]> {
        let mut file = std::fs::File::open(path)?;
        let mut hash = Sha256::new();
        std::io::copy(&mut file, &mut hash)?;
        Ok(hash.finalize().into())
    }
    /// Coarsest filesystem timestamp granularity the watcher may meet: exFAT
    /// stamps modification at 2 s, coarser than HFS+ at 1 s and Linux HZ ticks.
    /// An equal stamp proves stasis only once the last verification is older.
    const FILESYSTEM_TICK: Duration = Duration::from_secs(2);
    fn modified_time(stamp: &Stamp) -> SystemTime {
        if stamp.modified.0 >= 0 {
            UNIX_EPOCH + Duration::new(stamp.modified.0 as u64, stamp.modified.1 as u32)
        } else {
            UNIX_EPOCH
        }
    }
    fn needs_reverify(verified_at: SystemTime, stamp: &Stamp) -> bool {
        match verified_at.duration_since(modified_time(stamp)) {
            Ok(elapsed) => elapsed < FILESYSTEM_TICK,
            Err(_) => true,
        }
    }
    #[cfg(unix)]
    fn directory_identity(metadata: &std::fs::Metadata, _: &Path) -> (u64, u64) {
        (metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    fn directory_identity(_: &std::fs::Metadata, path: &Path) -> (u64, u64) {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        (
            0,
            xxhash_rust::xxh3::xxh3_64(canonical.as_os_str().as_encoded_bytes()),
        )
    }
    struct Directory {
        stamp: Stamp,
        files: Vec<PathBuf>,
        children: Vec<PathBuf>,
        verified_at: SystemTime,
    }
    struct Observation {
        config: FileWatcher,
        roots: Vec<PathBuf>,
        directories: HashMap<PathBuf, Directory>,
        files: HashMap<PathBuf, FileState>,
        started: SystemTime,
        #[cfg(test)]
        directory_reads: u64,
    }
    impl Observation {
        fn admitted(&self, path: &Path) -> bool {
            if (!self.restart(path) && is_tmp_scratch_file(path, &self.roots))
                || path.as_os_str().to_string_lossy().ends_with('~')
            {
                return false;
            }
            self.restart(path)
                || self.config.watch_all_files
                || self
                    .config
                    .extra_files
                    .as_ref()
                    .is_some_and(|extra| extra.paths().contains(path))
                || path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        self.config
                            .watch_extensions
                            .iter()
                            .any(|wanted| wanted == ext)
                    })
        }
        fn restart(&self, path: &Path) -> bool {
            self.config.restart_files.iter().any(|file| file == path)
                || self.config.matches_restart_suffix(path)
        }
        fn descend(&self, path: &Path) -> bool {
            !is_tmp_scratch_file(path, &self.roots)
                && !path
                    .file_name()
                    .is_some_and(|name| name == ".git" || name == "target")
        }
        fn collect(
            &mut self,
            root: &Path,
            candidates: &mut HashSet<PathBuf>,
            visited: &mut HashSet<PathBuf>,
            ancestors: &mut HashSet<(u64, u64)>,
            now: SystemTime,
        ) -> Result<()> {
            let metadata = match std::fs::metadata(root) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(stat_error(error, root)),
            };
            let identity = directory_identity(&metadata, root);
            if !ancestors.insert(identity) {
                return Ok(());
            }
            visited.insert(root.to_path_buf());
            let stamp = match Stamp::read(root) {
                Ok(stamp) => stamp,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.directories.remove(root);
                    return Ok(());
                }
                Err(error) => return Err(stat_error(error, root)),
            };
            let reread = match self.directories.get(root) {
                None => true,
                Some(directory) if directory.stamp != stamp => true,
                Some(directory) => needs_reverify(directory.verified_at, &directory.stamp),
            };
            if reread {
                let mut files = Vec::new();
                let mut children = Vec::new();
                #[cfg(test)]
                {
                    self.directory_reads += 1;
                }
                for entry in std::fs::read_dir(root).map_err(|error| stat_error(error, root))? {
                    let entry = entry.map_err(|error| stat_error(error, root))?;
                    let path = entry.path();
                    let kind = entry
                        .file_type()
                        .map_err(|error| stat_error(error, &path))?;
                    let metadata = if kind.is_symlink() {
                        Some(std::fs::metadata(&path).map_err(|error| stat_error(error, &path))?)
                    } else {
                        None
                    };
                    if kind.is_dir() || metadata.as_ref().is_some_and(|meta| meta.is_dir()) {
                        if self.descend(&path) {
                            children.push(path);
                        } else if let Some(suffix) = &self.config.restart_suffix {
                            if suffix.parent().is_some_and(|parent| path.ends_with(parent)) {
                                if let Some(name) = suffix.file_name() {
                                    files.push(path.join(name));
                                }
                            }
                        }
                    } else if kind.is_file() || metadata.as_ref().is_some_and(|meta| meta.is_file())
                    {
                        files.push(path);
                    }
                }
                self.directories.insert(
                    root.to_path_buf(),
                    Directory {
                        stamp,
                        files,
                        children,
                        verified_at: now,
                    },
                );
            }
            let directory = &self.directories[root];
            candidates.extend(
                directory
                    .files
                    .iter()
                    .filter(|path| self.admitted(path))
                    .cloned(),
            );
            let children = directory.children.clone();
            for child in children {
                self.collect(&child, candidates, visited, ancestors, now)?;
            }
            ancestors.remove(&identity);
            Ok(())
        }
        fn scan(&mut self, initial: bool) -> Result<FileChangeEvent> {
            let now = SystemTime::now();
            let mut candidates = HashSet::new();
            let mut visited = HashSet::new();
            for root in self.roots.clone() {
                match std::fs::metadata(&root) {
                    Ok(meta) if meta.is_dir() => self.collect(
                        &root,
                        &mut candidates,
                        &mut visited,
                        &mut HashSet::new(),
                        now,
                    )?,
                    Ok(_) if self.admitted(&root) => {
                        candidates.insert(root);
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(stat_error(error, &root)),
                }
            }
            self.directories.retain(|path, _| visited.contains(path));
            if let Some(extra) = &self.config.extra_files {
                let inputs = extra.paths().clone();
                candidates.extend(inputs.into_iter().filter(|path| self.admitted(path)));
            }
            candidates.extend(
                self.config
                    .restart_files
                    .iter()
                    .filter(|path| self.admitted(path))
                    .cloned(),
            );
            candidates.extend(
                self.files
                    .keys()
                    .filter(|path| self.admitted(path))
                    .cloned(),
            );
            self.observe(candidates.into_iter().collect(), now, initial)
        }
        fn observe(
            &mut self,
            candidates: Vec<PathBuf>,
            now: SystemTime,
            initial: bool,
        ) -> Result<FileChangeEvent> {
            let mut event = FileChangeEvent {
                paths: HashSet::new(),
                is_restart_trigger: false,
            };
            let mut updates = Vec::new();
            for path in &candidates {
                let stamp = match Stamp::read(path) {
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
                    Err(error) => return Err(stat_error(error, path)),
                };
                let old = self.files.get(path);
                let stamps_equal = old.is_some_and(|old| old.stamp == stamp);
                if stamps_equal
                    && old.is_some_and(|old| !needs_reverify(old.verified_at, &old.stamp))
                {
                    continue;
                }
                let hash = match content_hash(path) {
                    Ok(hash) => hash,
                    // Atomic saves can replace the path between stat and open;
                    // leave the prior state intact for the next scan.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(stat_error(error, path)),
                };
                let relevant = if stamps_equal {
                    old.is_some_and(|old| old.hash != hash)
                } else {
                    let changed_after_start = stamp.changed.0 >= 0
                        && UNIX_EPOCH
                            + Duration::new(stamp.changed.0 as u64, stamp.changed.1 as u32)
                            > self.started;
                    let declared = self
                        .config
                        .extra_files
                        .as_ref()
                        .is_some_and(|extra| extra.paths().contains(path));
                    let declared_seed = old.is_none()
                        && declared
                        && match self
                            .config
                            .extra_files
                            .as_ref()
                            .and_then(|extra| extra.compiled_hash(path))
                        {
                            Some(compiled) => match std::fs::read_to_string(path)
                                .map(|text| skull_mir::mir::comptime_value_hash(&text))
                            {
                                Ok(current) => current == compiled,
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                    continue;
                                }
                                Err(_) => false,
                            },
                            None => !changed_after_start,
                        };
                    !declared_seed
                };
                updates.push((
                    path.clone(),
                    Some(FileState {
                        stamp,
                        hash,
                        verified_at: now,
                    }),
                ));
                if relevant && !initial {
                    event.is_restart_trigger |= self.restart(path);
                    event.paths.insert(path.clone());
                }
            }
            let retained: HashSet<&PathBuf> = candidates.iter().collect();
            self.files.retain(|path, _| retained.contains(&path));
            for (path, state) in updates {
                if let Some(state) = state {
                    self.files.insert(path, state);
                } else {
                    self.files.remove(&path);
                }
            }
            Ok(event)
        }
    }
    pub struct WatchHandle {
        stop: Sender<()>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for WatchHandle {
        fn drop(&mut self) {
            if self.stop.send(()).is_err() {
                log::debug!(target: "WATCH", "file watcher worker already stopped");
            }
            if self
                .thread
                .take()
                .is_some_and(|thread| thread.join().is_err())
            {
                log::error!(target: "WATCH", "file watcher worker panicked");
            }
        }
    }
    pub fn start(
        mut config: FileWatcher,
        paths: Vec<PathBuf>,
        change_tx: Sender<FileChangeEvent>,
    ) -> Result<WatchHandle> {
        let canonical = |path: PathBuf| -> Result<PathBuf> {
            let absolute = std::path::absolute(&path)
                .map_err(|error| path_error(error, "resolve watch path", &path))?;
            let existing = absolute
                .ancestors()
                .find(|ancestor| ancestor.exists())
                .ok_or_else(|| {
                    anyhow::anyhow!("no ancestor of watch path {} exists", path.display())
                })?;
            let rest = absolute
                .strip_prefix(existing)
                .expect("an ancestor prefixes its descendant");
            let mut resolved = existing
                .canonicalize()
                .map_err(|error| path_error(error, "canonicalize watch path", existing))?;
            // `join("")` would append a separator, turning a file into a directory path.
            resolved.extend(rest.components());
            Ok(resolved)
        };
        let roots = paths
            .into_iter()
            .map(canonical)
            .collect::<Result<Vec<_>>>()?;
        config.restart_files = config
            .restart_files
            .into_iter()
            .map(canonical)
            .collect::<Result<Vec<_>>>()?;
        let interval = Duration::from_millis(config.debounce_ms.max(1));
        let mut observation = Observation {
            config,
            roots,
            directories: HashMap::new(),
            files: HashMap::new(),
            started: SystemTime::now(),
            #[cfg(test)]
            directory_reads: 0,
        };
        observation.scan(true)?;
        let (stop, stopped) = channel();
        let thread = std::thread::Builder::new()
            .name("rock file state watcher".into())
            .spawn(move || {
                loop {
                    match stopped.recv_timeout(interval) {
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                    match observation.scan(false) {
                        Ok(event) if !event.paths.is_empty() => {
                            if change_tx.send(event).is_err() {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            log::error!(target: "WATCH", "file watcher scan failed: {error:#}")
                        }
                    }
                }
            })?;
        Ok(WatchHandle {
            stop,
            thread: Some(thread),
        })
    }
    #[cfg(all(test, unix))]
    mod tests {
        use super::*;
        fn observation(root: PathBuf, config: FileWatcher) -> Observation {
            let mut observation = Observation {
                config,
                roots: vec![root],
                directories: HashMap::new(),
                files: HashMap::new(),
                started: SystemTime::now(),
                directory_reads: 0,
            };
            observation.scan(true).unwrap();
            observation
        }
        fn stamp_ctime(stamp: &Stamp) -> SystemTime {
            UNIX_EPOCH + Duration::new(stamp.changed.0 as u64, stamp.changed.1 as u32)
        }
        fn set_mtime(path: &Path, time: SystemTime) {
            let duration = time
                .duration_since(UNIX_EPOCH)
                .expect("test time after epoch");
            let times = [
                libc::timespec {
                    tv_sec: duration.as_secs() as libc::time_t,
                    tv_nsec: duration.subsec_nanos() as _,
                },
                libc::timespec {
                    tv_sec: duration.as_secs() as libc::time_t,
                    tv_nsec: duration.subsec_nanos() as _,
                },
            ];
            use std::os::unix::ffi::OsStrExt;
            let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            let ret = unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0) };
            assert_eq!(
                ret,
                0,
                "utimensat {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
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
            let config = FileWatcher {
                watch_extensions: vec!["rock".into(), "toml".into()],
                extra_files: Some(extra.clone()),
                ..FileWatcher::new()
            };
            let mut observation = observation(root, config);
            assert!(observation.scan(false).unwrap().paths.is_empty());
            std::fs::write(&source, "after").unwrap();
            assert_eq!(
                observation.scan(false).unwrap().paths,
                HashSet::from([source])
            );
            extra.paths().insert(input.clone());
            assert!(
                observation.scan(false).unwrap().paths.is_empty(),
                "declaring an unchanged input must not manufacture a reload"
            );
            std::fs::write(&input, "input edited").unwrap();
            assert_eq!(
                observation.scan(false).unwrap().paths,
                HashSet::from([input])
            );
            let pending = dir.path().canonicalize().unwrap().join("pending.txt");
            std::fs::write(&pending, "changed before first admission scan").unwrap();
            let stamp = Stamp::read(&pending).unwrap();
            observation.started = UNIX_EPOCH
                + Duration::new(stamp.changed.0 as u64, stamp.changed.1 as u32)
                - Duration::from_nanos(1);
            extra.paths().insert(pending.clone());
            assert_eq!(
                observation.scan(false).unwrap().paths,
                HashSet::from([pending])
            );
        }
        #[test]
        fn state_changes_preserve_new_sources_atomic_saves_touch_and_removals() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let source = root.join("main.rock");
            std::fs::write(&source, "before").unwrap();
            let mut observation = observation(root.clone(), FileWatcher::new());
            std::fs::write(&source, "after").unwrap();
            assert_eq!(
                observation.scan(false).unwrap().paths,
                HashSet::from([source.clone()])
            );
            let temporary = source.with_extension("saving");
            std::fs::write(&temporary, "after").unwrap();
            std::fs::rename(&temporary, &source).unwrap();
            assert_eq!(
                observation.scan(false).unwrap().paths,
                HashSet::from([source.clone()]),
                "same-byte inode replacement is still an atomic save"
            );
            let modified = std::fs::metadata(&source).unwrap().modified().unwrap();
            std::fs::File::options()
                .write(true)
                .open(&source)
                .unwrap()
                .set_modified(modified + Duration::from_nanos(1))
                .unwrap();
            assert_eq!(
                observation.scan(false).unwrap().paths,
                HashSet::from([source.clone()])
            );
            let added = root.join("added.rock");
            std::fs::write(&added, "added").unwrap();
            assert_eq!(
                observation.scan(false).unwrap().paths,
                HashSet::from([added.clone()])
            );
            std::fs::remove_file(&added).unwrap();
            assert_eq!(
                observation.scan(false).unwrap().paths,
                HashSet::from([added])
            );
        }
        #[test]
        fn a_burst_of_saves_between_scans_is_one_change() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let source = root.join("main.rock");
            std::fs::write(&source, "seed").unwrap();
            let mut observation = observation(root, FileWatcher::new());
            for i in 0..10 {
                std::fs::write(&source, format!("fn value() -> int {{ {i} }}\n")).unwrap();
            }
            assert_eq!(
                observation.scan(false).unwrap().paths,
                HashSet::from([source])
            );
            assert!(observation.scan(false).unwrap().paths.is_empty());
        }
        #[test]
        fn unchanged_asset_directories_reuse_entries_and_build_outputs_are_pruned() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            for name in ["assets", "target", ".git", "tmp"] {
                std::fs::create_dir_all(root.join(name)).unwrap();
            }
            std::fs::write(root.join("main.rock"), "source").unwrap();
            for name in ["target", ".git", "tmp"] {
                std::fs::write(root.join(name).join("ignored.rock"), "build output").unwrap();
            }
            for i in 0..100 {
                std::fs::write(
                    root.join("assets").join(format!("image-{i}.png")),
                    [0; 1024],
                )
                .unwrap();
            }
            let mut observation = observation(root.clone(), FileWatcher::new());
            assert_eq!(observation.files.len(), 1);
            let old = SystemTime::now() - Duration::from_secs(5);
            for dir in observation.directories.keys().cloned().collect::<Vec<_>>() {
                set_mtime(&dir, old);
            }
            assert!(observation.scan(false).unwrap().paths.is_empty());
            let reads = observation.directory_reads;
            assert!(observation.scan(false).unwrap().paths.is_empty());
            assert_eq!(
                observation.directory_reads, reads,
                "unchanged asset directories must not be reread"
            );
            assert!(
                !observation
                    .directories
                    .keys()
                    .any(|path| path.ends_with("target")
                        || path.ends_with(".git")
                        || path.ends_with("tmp"))
            );
            let added = root.join("assets/source.rock");
            std::fs::write(&added, "new source").unwrap();
            let stamp = Stamp::read(&added).unwrap();
            observation.started =
                UNIX_EPOCH + Duration::new(stamp.changed.0 as u64, stamp.changed.1 as u32);
            assert_eq!(
                observation.scan(false).unwrap().paths,
                HashSet::from([added]),
                "legitimate new sources in assets remain visible"
            );
        }
        #[test]
        fn restart_removal_is_a_trigger() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            std::fs::create_dir(root.join("tmp")).unwrap();
            let restart = root.join("tmp/reload.signal");
            std::fs::write(&restart, "trigger").unwrap();
            for config in [
                FileWatcher {
                    restart_files: vec![restart.clone()],
                    ..FileWatcher::new()
                },
                FileWatcher {
                    restart_suffix: Some(PathBuf::from("tmp/reload.signal")),
                    ..FileWatcher::new()
                },
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
        fn restart_file_in_a_missing_directory_is_watched_until_created() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let restart = root.join("sdk/tmp/restart.txt");
            let manifest = root.join("Cargo.lock");
            std::fs::write(&manifest, "").unwrap();
            let (tx, rx) = channel();
            let _handle = start(
                FileWatcher {
                    restart_files: vec![restart.clone()],
                    ..FileWatcher::new()
                },
                vec![root.join("game"), manifest],
                tx,
            )
            .unwrap();
            std::fs::create_dir_all(restart.parent().unwrap()).unwrap();
            std::fs::write(&restart, "trigger").unwrap();
            let event = rx.recv_timeout(test_utils::HANG_BACKSTOP).unwrap();
            assert_eq!(event.paths, HashSet::from([restart]));
            assert!(event.is_restart_trigger);
        }
        #[test]
        fn unreadable_input_does_not_consume_another_files_edit() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let source = root.join("main.rock");
            let broken = root.join("broken.rock");
            std::fs::write(&source, "before").unwrap();
            let mut observation = observation(root.clone(), FileWatcher::new());
            std::fs::write(&source, "after").unwrap();
            std::os::unix::fs::symlink(&broken, &broken).unwrap();
            assert!(
                observation
                    .observe(
                        vec![source.clone(), broken.clone()],
                        SystemTime::now(),
                        false
                    )
                    .is_err()
            );
            std::fs::remove_file(&broken).unwrap();
            assert_eq!(
                observation.scan(false).unwrap().paths,
                HashSet::from([source])
            );
        }
        #[test]
        fn all_files_watch_follows_directory_links_without_cycles() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let external = tempfile::tempdir().unwrap();
            std::fs::write(external.path().join("data.txt"), "before").unwrap();
            std::os::unix::fs::symlink(external.path(), root.join("linked")).unwrap();
            std::os::unix::fs::symlink(&root, external.path().join("cycle")).unwrap();
            let mut observation = observation(
                root.clone(),
                FileWatcher {
                    watch_all_files: true,
                    ..FileWatcher::new()
                },
            );
            std::fs::write(external.path().join("data.txt"), "after").unwrap();
            assert_eq!(
                observation.scan(false).unwrap().paths,
                HashSet::from([root.join("linked/data.txt")])
            );
        }
        #[test]
        fn same_size_rewrite_within_filesystem_tick_is_not_missed() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let source = root.join("main.rock");
            std::fs::write(&source, "AAAA").unwrap();
            let mut observation = observation(root.clone(), FileWatcher::new());
            std::fs::write(&source, "BBBB").unwrap();
            let recorded = observation.files.get(&source).unwrap().stamp.clone();
            set_mtime(&source, modified_time(&recorded));
            let current = Stamp::read(&source).unwrap();
            assert_eq!(
                (current.size, current.modified),
                (recorded.size, recorded.modified)
            );
            observation.files.get_mut(&source).unwrap().stamp = current;
            assert_eq!(
                observation.scan(false).unwrap().paths,
                HashSet::from([source])
            );
        }
        #[test]
        fn directory_change_within_filesystem_tick_is_reread() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let pkg = root.join("pkg");
            std::fs::create_dir(&pkg).unwrap();
            std::fs::write(pkg.join("main.rock"), "source").unwrap();
            let mut observation = observation(root.clone(), FileWatcher::new());
            let added = pkg.join("added.rock");
            std::fs::write(&added, "added").unwrap();
            let recorded = observation.directories.get(&pkg).unwrap().stamp.clone();
            set_mtime(&pkg, modified_time(&recorded));
            let current = Stamp::read(&pkg).unwrap();
            observation.directories.get_mut(&pkg).unwrap().stamp = current;
            assert_eq!(
                observation.scan(false).unwrap().paths,
                HashSet::from([added])
            );
        }
        #[test]
        fn declared_input_edited_within_startup_tick_is_not_seeded_silently() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let input = root.join("input.txt");
            std::fs::write(&input, "seedseed").unwrap();
            let compiled =
                skull_mir::mir::comptime_value_hash(&std::fs::read_to_string(&input).unwrap());
            let extra: SharedWatchSet = Default::default();
            let config = FileWatcher {
                extra_files: Some(extra.clone()),
                ..FileWatcher::new()
            };
            let mut observation = observation(root.clone(), config);
            std::fs::write(&input, "edited!!").unwrap();
            let stamp = Stamp::read(&input).unwrap();
            observation.started = stamp_ctime(&stamp);
            extra.insert_with_hash(input.clone(), compiled);
            assert_eq!(
                observation.scan(false).unwrap().paths,
                HashSet::from([input])
            );
        }
    }
}
