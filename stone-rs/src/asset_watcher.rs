use crate::log_once;
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebouncedEvent, Debouncer, RecommendedCache};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

type Dbnc = Debouncer<notify::RecommendedWatcher, RecommendedCache>;

/// Coalesce the burst of events an editor emits per save (write + rename +
/// chmod) into one notification.
const DEBOUNCE_MS: u64 = 50;

struct WatcherInner {
    /// Kept alive so the watch stays active (dropping it stops watching).
    _debouncer: Dbnc,
    /// Canonical directories already handed to `notify` (watched non-recursively).
    watched_dirs: HashSet<PathBuf>,
}

#[derive(PartialEq, Eq)]
struct AssetStamp {
    modified: SystemTime,
    size: u64,
}

struct WatchedAsset {
    original: String,
    stamp: Option<AssetStamp>,
}

fn asset_stamp(path: &Path) -> std::io::Result<Option<AssetStamp>> {
    match path.metadata() {
        Ok(metadata) => Ok(Some(AssetStamp {
            modified: metadata.modified()?,
            size: metadata.len(),
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn registered() -> &'static Mutex<HashMap<PathBuf, WatchedAsset>> {
    static REGISTERED: OnceLock<Mutex<HashMap<PathBuf, WatchedAsset>>> = OnceLock::new();
    REGISTERED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Canonical keys with a pending (un-drained) change. Written by the
/// debouncer's background thread, drained by the game thread in `poll`.
fn changed() -> &'static Mutex<HashSet<PathBuf>> {
    static CHANGED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    CHANGED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn watcher() -> &'static Mutex<Option<WatcherInner>> {
    static WATCHER: OnceLock<Mutex<Option<WatcherInner>>> = OnceLock::new();
    WATCHER.get_or_init(|| Mutex::new(None))
}

thread_local! {
    /// Backing store for the pointers `stone_changed_asset_path` hands out;
    /// valid until the next `stone_poll_changed_assets`.
    static POLL_BUF: RefCell<Vec<CString>> = const { RefCell::new(Vec::new()) };
}

/// `canonical(parent)/file_name` -- the form `notify` reports for a file under
/// a watched directory. None if the parent dir can't be canonicalized (missing).
fn asset_key(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    let file = path.file_name()?;
    let canon_parent = parent.canonicalize().ok()?;
    Some(canon_parent.join(file))
}

fn immutable_eat_object(path: &Path) -> bool {
    let mut components = path.components();
    while let Some(component) = components.next() {
        if component.as_os_str().to_str() != Some(".eat") {
            continue;
        }
        let Some(digest) = components.next().and_then(|part| part.as_os_str().to_str()) else {
            return false;
        };
        return digest.len() == 64
            && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            && components.next().is_some();
    }
    false
}

/// The debounced-event handler -- runs on the debouncer's own thread.
fn on_events(result: Result<Vec<DebouncedEvent>, Vec<notify::Error>>) {
    let events = match result {
        Ok(events) => events,
        Err(errors) => {
            for error in errors {
                log::error!(target: "ASSETS", "asset watcher error: {error}");
            }
            return;
        }
    };

    let mut hints = Vec::new();
    let mut all = false;
    for event in &events {
        if matches!(event.kind, EventKind::Access(_)) {
            continue;
        }
        all |= event.need_rescan() && event.paths.is_empty();
        for path in &event.paths {
            hints.push(asset_key(path).unwrap_or_else(|| path.clone()));
        }
    }
    let mut hits = Vec::new();
    {
        let mut reg = registered().lock().unwrap();
        for (key, asset) in reg.iter_mut() {
            // Atomic saves may report the temporary sibling rather than the destination.
            if !all
                && !hints
                    .iter()
                    .any(|hint| key.starts_with(hint) || key.parent() == hint.parent())
            {
                continue;
            }
            match asset_stamp(key) {
                Ok(stamp) if stamp != asset.stamp => {
                    asset.stamp = stamp;
                    hits.push(key.clone());
                }
                Ok(_) => {}
                Err(error) => {
                    log::error!(target: "ASSETS", "cannot inspect watched asset {}: {error}", key.display())
                }
            }
        }
    }

    if !hits.is_empty() {
        let mut ch = changed().lock().unwrap();
        ch.extend(hits);
    }
}

#[no_mangle]
pub extern "C" fn stone_watch_asset(path: *const c_char) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if path.is_null()
        || !*ENABLED
            .get_or_init(|| std::env::var_os("STONE_HOT_RELOAD").is_some_and(|value| value != "0"))
    {
        return;
    }
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return,
    };
    let p = Path::new(path_str);

    // Eat changes the bank's selected path on rebake; generation files are immutable.
    if immutable_eat_object(p) {
        return;
    }
    let original = path_str.to_string();

    let key = match asset_key(p) {
        Some(k) => k,
        None => {
            log_once!(
                "ASSETS",
                "stone_watch_asset: cannot resolve dir for {path_str}; not watching"
            );
            return;
        }
    };
    let dir = match key.parent() {
        Some(d) => d.to_path_buf(),
        None => return,
    };

    let mut guard = watcher().lock().unwrap();
    if guard.is_none() {
        match new_debouncer(Duration::from_millis(DEBOUNCE_MS), None, on_events) {
            Ok(d) => {
                *guard = Some(WatcherInner {
                    _debouncer: d,
                    watched_dirs: HashSet::new(),
                })
            }
            Err(e) => {
                log_once!("ASSETS", "stone_watch_asset: failed to create watcher: {e}");
                return;
            }
        }
    }
    let inner = guard.as_mut().unwrap();

    if inner.watched_dirs.insert(dir.clone()) {
        if let Err(e) = inner._debouncer.watch(&dir, RecursiveMode::NonRecursive) {
            log_once!(
                "ASSETS",
                "stone_watch_asset: failed to watch {}: {e}",
                dir.display()
            );
            inner.watched_dirs.remove(&dir);
            return;
        }
    }
    drop(guard);

    match asset_stamp(&key) {
        Ok(stamp) => {
            registered()
                .lock()
                .unwrap()
                .insert(key, WatchedAsset { original, stamp });
        }
        Err(error) => {
            log::error!(target: "ASSETS", "cannot inspect watched asset {}: {error}", key.display())
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_poll_changed_assets() -> i32 {
    let keys: Vec<PathBuf> = {
        let mut ch = changed().lock().unwrap();
        ch.drain().collect()
    };
    if keys.is_empty() {
        POLL_BUF.with(|b| b.borrow_mut().clear());
        return 0;
    }

    let reg = registered().lock().unwrap();
    let mut buf: Vec<CString> = Vec::with_capacity(keys.len());
    for key in &keys {
        if let Some(asset) = reg.get(key) {
            if let Ok(cs) = CString::new(asset.original.as_str()) {
                buf.push(cs);
            }
        }
    }
    drop(reg);

    let len = buf.len() as i32;
    POLL_BUF.with(|b| *b.borrow_mut() = buf);
    len
}

/// How many paths the watcher currently has registered, and how many changes
/// are pending, as `registered,changed,dirs`.
#[no_mangle]
pub extern "C" fn stone_asset_watcher_state() -> *const c_char {
    let dirs = watcher()
        .lock()
        .unwrap()
        .as_ref()
        .map(|inner| inner.watched_dirs.len())
        .unwrap_or(0);
    crate::native_ledger::watcher_reply(format!(
        "{},{},{}",
        registered().lock().unwrap().len(),
        changed().lock().unwrap().len(),
        dirs
    ))
}

/// Queue a change for `path` exactly as the debouncer's thread would.
///
/// A PROBE. Driving the real filesystem from a sim would make the fixture
/// depend on inotify latency and on a writable asset directory; this reaches
/// the queue through the same store the background thread writes. Returns false
/// when the path is not a registered asset, so a fixture cannot queue something
/// `poll` would drop anyway.
#[no_mangle]
pub extern "C" fn stone_asset_watcher_inject_change(path: *const c_char) -> bool {
    if path.is_null() {
        return false;
    }
    let Ok(path_str) = (unsafe { CStr::from_ptr(path) }).to_str() else {
        return false;
    };
    let Some(key) = asset_key(Path::new(path_str)) else {
        return false;
    };
    if !registered().lock().unwrap().contains_key(&key) {
        return false;
    }
    changed().lock().unwrap().insert(key);
    true
}

#[no_mangle]
pub extern "C" fn stone_changed_asset_path(index: i32) -> *const c_char {
    if index < 0 {
        return std::ptr::null();
    }
    POLL_BUF.with(|b| {
        b.borrow()
            .get(index as usize)
            .map(|cs| cs.as_ptr())
            .unwrap_or(std::ptr::null())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, DataChange, ModifyKind};
    use std::ffi::CString;
    use std::time::Instant;

    fn run_watcher_test(name: &str, hot_reload: Option<&str>) {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([name, "--exact", "--nocapture"])
            .env("STONE_WATCH_TEST_CHILD", "1")
            .env_remove("STONE_HOT_RELOAD");
        if let Some(value) = hot_reload {
            command.env("STONE_HOT_RELOAD", value);
        }
        let output = command.output().expect("run isolated watcher test");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
            "the child must select exactly one watcher test"
        );
        assert!(
            output.status.success(),
            "STONE_HOT_RELOAD={hot_reload:?}:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn asset_watch_registration_requires_hot_reload() {
        if std::env::var_os("STONE_WATCH_TEST_CHILD").is_none() {
            for value in [None, Some("0"), Some("1"), Some(""), Some("yes")] {
                run_watcher_test(
                    "asset_watcher::tests::asset_watch_registration_requires_hot_reload",
                    value,
                );
            }
            return;
        }
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let path = CString::new(path.to_str().unwrap()).unwrap();
        stone_watch_asset(path.as_ptr());
        let state = unsafe { CStr::from_ptr(stone_asset_watcher_state()) }
            .to_str()
            .unwrap();
        let enabled = std::env::var_os("STONE_HOT_RELOAD").is_some_and(|value| value != "0");
        assert_eq!(
            state,
            if enabled { "1,0,1" } else { "0,0,0" },
            "disabled hot reload must create neither registrations nor OS watches"
        );
    }

    fn poll_paths() -> Vec<String> {
        let n = stone_poll_changed_assets();
        (0..n)
            .map(|i| {
                let p = stone_changed_asset_path(i);
                assert!(!p.is_null(), "changed asset path {i} was null");
                unsafe { CStr::from_ptr(p).to_string_lossy().into_owned() }
            })
            .collect()
    }

    #[test]
    fn watch_surfaces_registered_changes_and_ignores_the_rest() {
        if std::env::var_os("STONE_WATCH_TEST_CHILD").is_none() {
            run_watcher_test(
                "asset_watcher::tests::watch_surfaces_registered_changes_and_ignores_the_rest",
                Some("1"),
            );
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("watched.png");
        let sibling = root.path().join("unchanged.png");
        std::fs::write(&path, "before").unwrap();
        std::fs::write(&sibling, "unchanged").unwrap();
        let registered = path.to_str().unwrap().to_string();
        for file in [&path, &sibling] {
            stone_watch_asset(CString::new(file.to_str().unwrap()).unwrap().as_ptr());
        }
        // Only injected events may satisfy the assertions, regardless of OS delivery.
        *watcher().lock().unwrap() = None;
        assert_eq!(poll_paths(), Vec::<String>::new());
        let reported = root.path().canonicalize().unwrap();
        let event = |kind: EventKind, path: PathBuf| {
            DebouncedEvent::new(notify::Event::new(kind).add_path(path), Instant::now())
        };
        std::fs::write(&path, "changed content").unwrap();
        on_events(Ok(vec![event(EventKind::Other, reported.clone())]));
        assert_eq!(
            poll_paths(),
            vec![registered.clone()],
            "directory hints find only changed files"
        );
        let create = EventKind::Create(CreateKind::File);
        let modify = EventKind::Modify(ModifyKind::Data(DataChange::Content));
        on_events(Ok(vec![
            event(create, reported.join("watched.png")),
            event(modify, reported.join("watched.png")),
        ]));
        assert!(
            poll_paths().is_empty(),
            "later events for the same save are not another change"
        );
        let temporary = root.path().join("save.tmp");
        std::fs::write(&temporary, "atomic replacement content").unwrap();
        std::fs::rename(&temporary, &path).unwrap();
        on_events(Ok(vec![event(modify, reported.join("save.tmp"))]));
        assert_eq!(
            poll_paths(),
            vec![registered.clone()],
            "atomic-save sibling hints check the registered file"
        );
        std::fs::remove_file(&path).unwrap();
        on_events(Ok(vec![event(EventKind::Other, reported.clone())]));
        assert_eq!(
            poll_paths(),
            vec![registered.clone()],
            "deletion is a change"
        );
        std::fs::write(&path, "recreated").unwrap();
        on_events(Ok(vec![DebouncedEvent::new(
            notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan),
            Instant::now(),
        )]));
        assert_eq!(
            poll_paths(),
            vec![registered],
            "pathless rescan checks all registered files"
        );
        assert!(poll_paths().is_empty(), "poll drains the report");
    }
}
