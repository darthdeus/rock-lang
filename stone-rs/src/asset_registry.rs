//! In-memory asset registry for fire (embedded game runner).
//!
//! Assets are registered via `stone_register_asset` before the game starts.
//! Loading functions (image, sound, font, file_read) check the registry
//! before hitting disk, making the game work with embedded assets.

use std::collections::{BTreeSet, HashMap};
use std::ffi::{c_char, CStr};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Default)]
struct AssetRegistry {
    assets: HashMap<String, Vec<u8>>,
    directories: HashMap<String, BTreeSet<String>>,
}

impl AssetRegistry {
    fn insert(&mut self, path: String, bytes: Vec<u8>) {
        for (slash, _) in path.match_indices('/') {
            let Some(child) = path[slash + 1..].split('/').next() else {
                continue;
            };
            if !child.is_empty() {
                self.directories
                    .entry(path[..slash].to_string())
                    .or_default()
                    .insert(child.to_string());
            }
        }
        self.assets.insert(path, bytes);
    }
}

static REGISTRY: Mutex<Option<AssetRegistry>> = Mutex::new(None);

/// Register an asset in the in-memory registry.
/// Called by fire to populate assets before the game starts.
#[no_mangle]
pub extern "C" fn stone_register_asset(path: *const c_char, data: *const u8, len: usize) {
    if path.is_null() || data.is_null() {
        return;
    }
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => normalize_key(s),
        Err(_) => return,
    };
    let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();

    let mut guard = REGISTRY.lock().unwrap();
    guard
        .get_or_insert_with(AssetRegistry::default)
        .insert(path_str, bytes);
}

/// Look up an asset by path. Returns None if not registered.
/// Normalizes path separators (backslash → forward slash) for cross-platform compatibility.
pub fn get_asset(path: &str) -> Option<Vec<u8>> {
    let normalized = normalize_key(path);
    let guard = REGISTRY.lock().unwrap();
    guard
        .as_ref()
        .and_then(|registry| registry.assets.get(&normalized).cloned())
}

/// Cheap presence check without copying the bytes.
pub fn contains_asset(path: &str) -> bool {
    let normalized = normalize_key(path);
    let guard = REGISTRY.lock().unwrap();
    guard
        .as_ref()
        .is_some_and(|registry| registry.assets.contains_key(&normalized))
}

/// Return the byte length of a registered asset without copying its contents.
pub fn asset_size(path: &str) -> Option<i64> {
    let normalized = normalize_key(path);
    let guard = REGISTRY.lock().unwrap();
    guard
        .as_ref()
        .and_then(|registry| registry.assets.get(&normalized))
        .map(|data| data.len() as i64)
}

pub fn directory_entries(path: &str) -> Vec<String> {
    let key = directory_key(path);
    let guard = REGISTRY.lock().unwrap();
    guard
        .as_ref()
        .and_then(|registry| registry.directories.get(&key))
        .map(|entries| entries.iter().cloned().collect())
        .unwrap_or_default()
}

pub fn contains_directory(path: &str) -> bool {
    let key = directory_key(path);
    let guard = REGISTRY.lock().unwrap();
    guard
        .as_ref()
        .is_some_and(|registry| registry.directories.contains_key(&key))
}

fn directory_key(path: &str) -> String {
    normalize_key(path).trim_end_matches('/').to_string()
}

fn normalize_key(path: &str) -> String {
    path.replace('\\', "/")
}

/// Player-override directory, resolved once from the executable location as
/// `<exe_dir>/mods/`. A missing directory skips probes; an inspection error
/// blocks registered-asset fallback after it is logged.
enum ModRoot {
    Missing,
    Present(PathBuf),
    Failed,
}

fn mod_root() -> &'static ModRoot {
    static MOD_ROOT: OnceLock<ModRoot> = OnceLock::new();
    MOD_ROOT.get_or_init(|| {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(error) => {
                ::log::error!(target: "ASSETS", "failed to resolve executable path for mods: {error}");
                return ModRoot::Failed;
            }
        };
        let Some(parent) = exe.parent() else {
            ::log::error!(target: "ASSETS", "executable path has no parent for mods directory");
            return ModRoot::Failed;
        };
        let dir = parent.join("mods");
        match std::fs::metadata(&dir) {
            Ok(metadata) if metadata.is_dir() => ModRoot::Present(dir),
            Ok(_) => ModRoot::Missing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ModRoot::Missing,
            Err(error) => {
                ::log::error!(
                    target: "ASSETS",
                    "failed to inspect mods directory {}: {error}",
                    dir.display()
                );
                ModRoot::Failed
            }
        }
    })
}

enum OverrideRead {
    Missing,
    Found(Vec<u8>),
    Failed,
}

/// Read `<root>/<path>` if present. Invalid keys and missing files are misses;
/// I/O failures are logged and stop resolution rather than falling through to
/// the embedded copy.
fn read_override(root: &Path, path: &str, prefix: Option<usize>) -> OverrideRead {
    let normalized = path.replace('\\', "/");
    let rel = Path::new(&normalized);
    if rel.is_absolute()
        || rel.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return OverrideRead::Missing;
    }
    let override_path = root.join(rel);
    let result = match prefix {
        Some(n) => std::fs::File::open(&override_path).and_then(|mut file| {
            let mut bytes = vec![0; n];
            let read = read_up_to(&mut file, &mut bytes)?;
            bytes.truncate(read);
            Ok(bytes)
        }),
        None => std::fs::read(&override_path),
    };
    match result {
        Ok(bytes) => OverrideRead::Found(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => OverrideRead::Missing,
        Err(error) => {
            ::log::error!(
                target: "ASSETS",
                "failed to read asset override {}: {error}",
                override_path.display()
            );
            OverrideRead::Failed
        }
    }
}

/// Resolve asset bytes for `path`, in priority order:
///   1. `<exe_dir>/mods/<path>` on disk — player override of a SHIPPED asset.
///   2. embedded registry — fire release assets.
///   3. `<path>` on disk relative to cwd — dev mode / loose files.
/// `None` for misses or logged I/O errors. Single asset-byte source for every
/// loader (image/audio/font/file_read) so priority can't drift.
///
/// The mods override fires ONLY for paths that are in the embedded registry.
/// `stone_file_read` also serves runtime-written files (saves, settings,
/// editor state) that are never registered, so a `mods/` file cannot shadow
/// them and corrupt a read-after-write — this is precisely "override an
/// existing shipped asset", not "intercept every relative read".
pub fn resolve_asset(path: &str) -> Option<Vec<u8>> {
    match mod_root() {
        ModRoot::Present(root) => resolve_with_root(Some(root), path),
        ModRoot::Missing => resolve_with_root(None, path),
        ModRoot::Failed if contains_asset(path) => None,
        ModRoot::Failed => resolve_with_root(None, path),
    }
}

/// The first `n` bytes, for a caller that only needs a container header.
/// Copies only the requested registry bytes and bounds disk reads, including
/// player overrides, so header probes do not load entire sheets or sounds.
pub fn resolve_asset_prefix(path: &str, n: usize) -> Option<Vec<u8>> {
    match mod_root() {
        ModRoot::Present(root) => resolve_prefix_with_root(Some(root), path, n),
        ModRoot::Missing => resolve_prefix_with_root(None, path, n),
        ModRoot::Failed if contains_asset(path) => None,
        ModRoot::Failed => resolve_prefix_with_root(None, path, n),
    }
}

fn resolve_prefix_with_root(root: Option<&Path>, path: &str, n: usize) -> Option<Vec<u8>> {
    if contains_asset(path) {
        if let Some(root) = root {
            match read_override(root, path, Some(n)) {
                OverrideRead::Found(bytes) => return Some(bytes),
                OverrideRead::Missing => {}
                OverrideRead::Failed => return None,
            }
        }
        let normalized = normalize_key(path);
        let guard = REGISTRY.lock().unwrap();
        return guard
            .as_ref()
            .and_then(|registry| registry.assets.get(&normalized))
            .map(|bytes| bytes[..n.min(bytes.len())].to_vec());
    }
    let mut f = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            ::log::error!(target: "ASSETS", "failed to open asset prefix {path}: {error}");
            return None;
        }
    };
    let mut buf = vec![0u8; n];
    let read = match read_up_to(&mut f, &mut buf) {
        Ok(read) => read,
        Err(error) => {
            ::log::error!(target: "ASSETS", "failed to read asset prefix {path}: {error}");
            return None;
        }
    };
    buf.truncate(read);
    Some(buf)
}

/// Fills `buf` as far as the file allows. A short read is the file being
/// smaller than the prefix, not a failure.
fn read_up_to(f: &mut std::fs::File, buf: &mut [u8]) -> Result<usize, std::io::Error> {
    use std::io::Read;
    let mut total = 0;
    while total < buf.len() {
        match f.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(total)
}

fn resolve_with_root(root: Option<&Path>, path: &str) -> Option<Vec<u8>> {
    if contains_asset(path) {
        if let Some(root) = root {
            match read_override(root, path, None) {
                OverrideRead::Found(bytes) => return Some(bytes),
                OverrideRead::Missing => {}
                OverrideRead::Failed => return None,
            }
        }
        // Registry is populated once at boot and immutable after, so a
        // positive contains_asset means get_asset resolves.
        return get_asset(path);
    }
    match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            ::log::error!(target: "ASSETS", "failed to read asset {path}: {error}");
            None
        }
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static UNIQ: AtomicU32 = AtomicU32::new(0);

    struct RegisteredAssetGuard(String);

    impl Drop for RegisteredAssetGuard {
        fn drop(&mut self) {
            let mut guard = REGISTRY.lock().unwrap();
            let Some(registry) = guard.as_mut() else {
                return;
            };
            registry.assets.remove(&self.0);
            let mut path = Path::new(&self.0);
            while let Some(parent) = path.parent() {
                let Some(child) = path.file_name().and_then(|name| name.to_str()) else {
                    break;
                };
                let parent_key = parent.to_string_lossy().into_owned();
                let empty = if let Some(children) = registry.directories.get_mut(&parent_key) {
                    children.remove(child);
                    children.is_empty()
                } else {
                    false
                };
                if empty {
                    registry.directories.remove(&parent_key);
                }
                path = parent;
            }
        }
    }

    fn register_guarded_asset(key: String, bytes: Vec<u8>) -> RegisteredAssetGuard {
        REGISTRY
            .lock()
            .unwrap()
            .get_or_insert_with(AssetRegistry::default)
            .insert(key.clone(), bytes);
        RegisteredAssetGuard(key)
    }

    #[test]
    fn registered_asset_fixture_restores_registry_after_failure() {
        let key = format!("cleanup_{}/asset.bin", UNIQ.fetch_add(1, Ordering::SeqCst));
        let directory = Path::new(&key)
            .parent()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let result = std::panic::catch_unwind(|| {
            let _asset = register_guarded_asset(key.clone(), vec![]);
            panic!("intentional fixture cleanup failure");
        });

        assert!(result.is_err(), "the fixture must exercise unwind cleanup");
        assert!(
            !contains_asset(&key),
            "failed fixture must remove its registered asset"
        );
        assert!(
            directory_entries(&directory).is_empty(),
            "failed fixture must remove registry directories"
        );
    }

    fn fresh_root() -> PathBuf {
        let n = UNIQ.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("stone_modtest_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn registered_header_does_not_retain_a_full_asset_allocation() {
        let key = format!("header_{}.bin", UNIQ.fetch_add(1, Ordering::SeqCst));
        let _asset = register_guarded_asset(key.clone(), vec![42; 8 * 1024 * 1024]);
        let header = resolve_asset_prefix(&key, 24).unwrap();
        assert_eq!(header, vec![42; 24]);
        assert!(
            header.capacity() <= 24,
            "header retained {} bytes",
            header.capacity()
        );
        assert_eq!(resolve_asset_prefix(&key, 0), Some(vec![]));
    }

    #[test]
    fn override_and_loose_headers_are_bounded_and_preserve_resolution() {
        let root = fresh_root();
        let key = format!("header_{}.bin", UNIQ.fetch_add(1, Ordering::SeqCst));
        let _asset = register_guarded_asset(key.clone(), b"embedded".to_vec());
        let modded = root.join(&key);
        std::fs::write(&modded, b"mod header").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&modded)
            .unwrap()
            .set_len(8 * 1024 * 1024)
            .unwrap();
        let header = resolve_prefix_with_root(Some(&root), &key, 3).unwrap();
        assert_eq!(header, b"mod");
        assert!(header.capacity() <= 3);
        assert_eq!(
            resolve_asset_prefix(modded.to_str().unwrap(), 3),
            Some(b"mod".to_vec())
        );
        std::fs::remove_file(&modded).unwrap();
        assert_eq!(
            resolve_prefix_with_root(Some(&root), &key, 24),
            Some(b"embedded".to_vec())
        );
        std::fs::create_dir(&modded).unwrap();
        assert_eq!(resolve_prefix_with_root(Some(&root), &key, 3), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn override_dir_wins_over_registry() {
        let root = fresh_root();
        // Unique key so the shared global registry can't collide across tests.
        let key = format!("world/floor_{}.png", UNIQ.fetch_add(1, Ordering::SeqCst));
        let embedded = b"EMBEDDED".to_vec();
        {
            let mut guard = REGISTRY.lock().unwrap();
            guard
                .get_or_insert_with(AssetRegistry::default)
                .insert(key.clone(), embedded.clone());
        }
        // No override on disk yet -> registry copy.
        assert_eq!(resolve_with_root(Some(&root), &key), Some(embedded));
        // Drop a modded file mirroring the key -> it wins.
        let modded = root.join(&key);
        std::fs::create_dir_all(modded.parent().unwrap()).unwrap();
        std::fs::write(&modded, b"MODDED").unwrap();
        assert_eq!(
            resolve_with_root(Some(&root), &key),
            Some(b"MODDED".to_vec())
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn non_shipped_paths_are_never_overridden() {
        // A runtime-written file (save/config) not in the registry must not be
        // shadowed by a mods/ file at the same relative path -- otherwise a
        // read-after-write of a save would return stale mod content.
        let root = fresh_root();
        let key = format!("saves/state_{}.json", UNIQ.fetch_add(1, Ordering::SeqCst));
        let modded = root.join(&key);
        std::fs::create_dir_all(modded.parent().unwrap()).unwrap();
        std::fs::write(&modded, b"MOD_SAVE").unwrap();
        // Unregistered key -> override must NOT fire; falls to cwd disk, which
        // has no such unique file -> None.
        assert_eq!(resolve_with_root(Some(&root), &key), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn traversal_and_absolute_keys_never_escape_mods() {
        let root = fresh_root();
        assert!(matches!(
            read_override(&root, "../secret", None),
            OverrideRead::Missing
        ));
        assert!(matches!(
            read_override(&root, "a/../../secret", None),
            OverrideRead::Missing
        ));
        assert!(matches!(
            read_override(&root, "/etc/passwd", None),
            OverrideRead::Missing
        ));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn miss_falls_through_to_none_without_mods() {
        let key = format!("nope_{}.png", UNIQ.fetch_add(1, Ordering::SeqCst));
        assert_eq!(resolve_with_root(None, &key), None);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_override_does_not_fall_through_to_embedded_asset() {
        let root = fresh_root();
        let key = format!("loop_{}/shipped.bin", UNIQ.fetch_add(1, Ordering::SeqCst));
        let override_path = root.join(&key);
        std::fs::create_dir_all(override_path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&override_path, &override_path).unwrap();
        let observed = std::fs::read(&override_path).unwrap_err();
        let is_loop = observed.raw_os_error() == Some(libc::ELOOP);
        let _asset = register_guarded_asset(key.clone(), b"embedded bytes".to_vec());

        let resolution = resolve_with_root(Some(&root), &key);
        std::fs::remove_dir_all(root).unwrap();

        assert!(
            is_loop,
            "independent override read must report ELOOP: {observed}"
        );
        assert!(
            resolution.is_none(),
            "a failed override read must not become an embedded-asset hit"
        );
    }

    #[test]
    fn embedded_directory_entries_are_direct_sorted_and_deduplicated() {
        let stem = format!("list_{}", UNIQ.fetch_add(1, Ordering::SeqCst));
        let dir = format!("game/assets/{stem}");
        {
            let mut guard = REGISTRY.lock().unwrap();
            let registry = guard.get_or_insert_with(AssetRegistry::default);
            registry.insert(format!("{dir}/z.ogg"), vec![]);
            registry.insert(format!("{dir}/a.ogg"), vec![]);
            registry.insert(format!("{dir}/nested/one.ogg"), vec![]);
            registry.insert(format!("{dir}/nested/two.ogg"), vec![]);
            registry.insert(format!("{dir}ish/not-a-child.ogg"), vec![]);
        }

        assert_eq!(
            directory_entries(&dir),
            vec![
                "a.ogg".to_string(),
                "nested".to_string(),
                "z.ogg".to_string()
            ]
        );
        assert_eq!(
            directory_entries(&dir.replace('/', "\\")),
            vec![
                "a.ogg".to_string(),
                "nested".to_string(),
                "z.ogg".to_string()
            ],
            "all registry queries must share separator normalization"
        );
        assert!(contains_directory(&dir) && contains_directory(&format!("{dir}/nested/")));
        assert!(
            !contains_directory(&format!("{dir}/a.ogg")),
            "an asset is not a directory"
        );
        assert!(!contains_directory(&format!("{dir}/missing")));
    }
}
