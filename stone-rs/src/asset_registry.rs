use std::collections::{BTreeSet, HashMap};
use std::ffi::{c_char, CStr};
use std::io::Read;
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

/// Cheap presence check without copying the bytes.
pub fn contains_asset(path: &str) -> bool {
    with_asset(path, |_| ()).is_some()
}

/// Return the byte length of a registered asset without copying its contents.
pub fn asset_size(path: &str) -> Option<i64> {
    with_asset(path, |data| data.len() as i64)
}

fn with_asset<T>(path: &str, f: impl FnOnce(&[u8]) -> T) -> Option<T> {
    let normalized = normalize_key(path);
    let guard = REGISTRY.lock().unwrap();
    guard
        .as_ref()
        .and_then(|registry| registry.assets.get(&normalized))
        .map(|data| f(data))
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

/// `Ok(None)` when the file does not exist; other failures are logged as `Err`.
fn read_found(path: &Path, prefix: Option<usize>, what: &str) -> Result<Option<Vec<u8>>, ()> {
    let result = match prefix {
        Some(n) => std::fs::File::open(path).and_then(|file| {
            let mut bytes = Vec::with_capacity(n);
            file.take(n as u64).read_to_end(&mut bytes)?;
            Ok(bytes)
        }),
        None => std::fs::read(path),
    };
    match result {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            ::log::error!(target: "ASSETS", "failed to read {what} {}: {error}", path.display());
            Err(())
        }
    }
}

fn read_override(root: &Path, path: &str, prefix: Option<usize>) -> Result<Option<Vec<u8>>, ()> {
    let normalized = normalize_key(path);
    let rel = Path::new(&normalized);
    if rel.is_absolute()
        || rel.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Ok(None);
    }
    read_found(&root.join(rel), prefix, "asset override")
}

/// Priority: `<exe_dir>/mods/<path>`, then the embedded registry, then `<path>` on disk.
/// The mods override applies only to registered paths, so it cannot shadow saves or other
/// runtime-written files.
pub fn resolve_asset(path: &str) -> Option<Vec<u8>> {
    resolve(path, None)
}

pub fn resolve_asset_prefix(path: &str, n: usize) -> Option<Vec<u8>> {
    resolve(path, Some(n))
}

fn resolve(path: &str, prefix: Option<usize>) -> Option<Vec<u8>> {
    match mod_root() {
        ModRoot::Present(root) => resolve_with_root(Some(root), path, prefix),
        ModRoot::Failed if contains_asset(path) => None,
        ModRoot::Missing | ModRoot::Failed => resolve_with_root(None, path, prefix),
    }
}

fn resolve_with_root(root: Option<&Path>, path: &str, prefix: Option<usize>) -> Option<Vec<u8>> {
    if !contains_asset(path) {
        return read_found(Path::new(path), prefix, "asset").ok()?;
    }
    if let Some(root) = root {
        if let Some(bytes) = read_override(root, path, prefix).ok()? {
            return Some(bytes);
        }
    }
    // The registry is immutable after boot, so a positive contains_asset means the
    // asset resolves.
    with_asset(path, |bytes| {
        bytes[..prefix.unwrap_or(bytes.len()).min(bytes.len())].to_vec()
    })
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static UNIQ: AtomicU32 = AtomicU32::new(0);

    fn unique_key(pattern: &str) -> String {
        pattern.replace('#', &UNIQ.fetch_add(1, Ordering::SeqCst).to_string())
    }

    struct RegisteredAssetGuard(String);

    impl Drop for RegisteredAssetGuard {
        fn drop(&mut self) {
            if let Some(registry) = REGISTRY.lock().unwrap().as_mut() {
                registry.assets.remove(&self.0);
            }
        }
    }

    fn register_guarded_asset(key: &str, bytes: Vec<u8>) -> RegisteredAssetGuard {
        REGISTRY
            .lock()
            .unwrap()
            .get_or_insert_with(AssetRegistry::default)
            .insert(key.to_string(), bytes);
        RegisteredAssetGuard(key.to_string())
    }

    fn fresh_root() -> crate::test_support::TestRoot {
        crate::test_support::fresh_root("stone_modtest_")
    }

    #[test]
    fn override_wins_with_bounded_headers_and_falls_back_to_the_registry() {
        let root = fresh_root();
        let key = unique_key("world/header_#.bin");
        let _asset = register_guarded_asset(&key, b"embedded".to_vec());
        let modded = root.join(&key);
        std::fs::create_dir_all(modded.parent().unwrap()).unwrap();
        std::fs::write(&modded, b"mod header").unwrap();
        assert_eq!(
            resolve_with_root(Some(&root), &key, None),
            Some(b"mod header".to_vec())
        );
        std::fs::OpenOptions::new()
            .write(true)
            .open(&modded)
            .unwrap()
            .set_len(8 * 1024 * 1024)
            .unwrap();
        let header = resolve_with_root(Some(&root), &key, Some(3)).unwrap();
        assert_eq!(header, b"mod");
        assert!(header.capacity() <= 3);
        assert_eq!(
            resolve_asset_prefix(modded.to_str().unwrap(), 3),
            Some(b"mod".to_vec())
        );
        std::fs::remove_file(&modded).unwrap();
        assert_eq!(
            resolve_with_root(Some(&root), &key, Some(24)),
            Some(b"embedded".to_vec())
        );
        std::fs::create_dir(&modded).unwrap();
        assert_eq!(resolve_with_root(Some(&root), &key, Some(3)), None);
    }

    #[test]
    fn traversal_and_absolute_keys_never_escape_mods() {
        let root = fresh_root();
        for key in ["../secret", "a/../../secret", "/etc/passwd"] {
            assert!(matches!(read_override(&root, key, None), Ok(None)), "{key}");
        }
    }
}
