//! Where `embed_string` reads package files from. A native compile reads the
//! filesystem; a host without one (a browser) supplies the files it has, and
//! a read of anything else rejects with a message naming that host.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::prelude::FxHashMap;

/// Why an embedded file could not be read. The lowering turns each into a
/// spanned rejection; the strings are the underlying cause.
#[derive(Debug)]
pub enum EmbedReadError {
    /// The file is missing or unreadable.
    Unreadable(String),
    /// The package root itself is missing or unreadable.
    RootUnreadable(String),
    /// The path leaves the package root.
    EscapesRoot,
}

/// The file reads `embed_string` and the artifact fingerprint make.
pub trait ComptimeFiles: Send + Sync + std::fmt::Debug {
    /// The package root a call in `calling_file` resolves against, if the
    /// file belongs to one.
    fn package_root(&self, calling_file: &str) -> Option<PathBuf>;
    /// Read `rel` under `root`. Returns the identity of the read (the path it
    /// landed on, which the fingerprint re-reads and a watcher watches) and
    /// the contents.
    fn read(&self, root: &Path, rel: &Path) -> Result<(String, String), EmbedReadError>;
    /// Re-read an input by the identity [`ComptimeFiles::read`] returned.
    fn reread(&self, identity: &str) -> Result<String, String>;
}

/// The host's [`ComptimeFiles`], carried on `ComptimeInputs`. Defaults to
/// the filesystem.
#[derive(Clone, Debug)]
pub struct EmbedFiles(pub Arc<dyn ComptimeFiles>);

impl Default for EmbedFiles {
    fn default() -> Self {
        Self(Arc::new(FsComptimeFiles))
    }
}

impl std::ops::Deref for EmbedFiles {
    type Target = dyn ComptimeFiles;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// Reads through the filesystem: the package root is the nearest directory
/// at or above an existing calling file that holds a `stick.toml`, and a read
/// must canonicalize inside the canonical root.
#[derive(Debug)]
pub struct FsComptimeFiles;

impl ComptimeFiles for FsComptimeFiles {
    fn package_root(&self, calling_file: &str) -> Option<PathBuf> {
        let file = Path::new(calling_file);
        if !file.exists() {
            return None;
        }
        let mut dir = file.parent();
        while let Some(d) = dir {
            if d.join("stick.toml").exists() {
                return Some(d.to_path_buf());
            }
            dir = d.parent();
        }
        None
    }

    fn read(&self, root: &Path, rel: &Path) -> Result<(String, String), EmbedReadError> {
        let file = root
            .join(rel)
            .canonicalize()
            .map_err(|e| EmbedReadError::Unreadable(e.to_string()))?;
        let canon_root =
            root.canonicalize().map_err(|e| EmbedReadError::RootUnreadable(e.to_string()))?;
        if !file.starts_with(&canon_root) {
            return Err(EmbedReadError::EscapesRoot);
        }
        let value =
            std::fs::read_to_string(&file).map_err(|e| EmbedReadError::Unreadable(e.to_string()))?;
        Ok((file.to_string_lossy().into_owned(), value))
    }

    fn reread(&self, identity: &str) -> Result<String, String> {
        std::fs::read_to_string(identity).map_err(|e| e.to_string())
    }
}

/// Files a host supplied in memory, keyed by package-relative path (`/`
/// separated, no `.` or `..`). Every calling file belongs to the one package
/// they form; its root is the empty path.
#[derive(Debug, Default)]
pub struct MemoryComptimeFiles {
    files: FxHashMap<String, String>,
}

impl MemoryComptimeFiles {
    pub fn new(files: impl IntoIterator<Item = (String, String)>) -> Self {
        Self { files: files.into_iter().collect() }
    }

    fn key(root: &Path, rel: &Path) -> Option<String> {
        let path = root.join(rel);
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => parts.push(part.to_str()?),
                Component::CurDir => {}
                Component::ParentDir => {
                    parts.pop()?;
                }
                Component::RootDir | Component::Prefix(_) => return None,
            }
        }
        Some(parts.join("/"))
    }
}

impl ComptimeFiles for MemoryComptimeFiles {
    fn package_root(&self, _calling_file: &str) -> Option<PathBuf> {
        Some(PathBuf::new())
    }

    fn read(&self, root: &Path, rel: &Path) -> Result<(String, String), EmbedReadError> {
        let key = Self::key(root, rel).ok_or(EmbedReadError::EscapesRoot)?;
        let value = self.files.get(&key).ok_or_else(|| {
            EmbedReadError::Unreadable(format!("this compiler host supplied no file `{key}`"))
        })?;
        Ok((key, value.clone()))
    }

    fn reread(&self, identity: &str) -> Result<String, String> {
        self.files
            .get(identity)
            .cloned()
            .ok_or_else(|| format!("this compiler host supplied no file `{identity}`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_files_resolve_inside_the_package_only() {
        let files = MemoryComptimeFiles::new([("data/a.txt".to_string(), "A".to_string())]);
        let root = files.package_root("main.rock").unwrap();
        let (id, text) = files.read(&root, Path::new("data/./a.txt")).unwrap();
        assert_eq!((id.as_str(), text.as_str()), ("data/a.txt", "A"));
        assert_eq!(files.reread(&id).unwrap(), "A");
        assert!(matches!(
            files.read(&root, Path::new("../a.txt")),
            Err(EmbedReadError::EscapesRoot)
        ));
        match files.read(&root, Path::new("b.txt")) {
            Err(EmbedReadError::Unreadable(why)) => assert!(why.contains("supplied no file `b.txt`")),
            other => panic!("unsupplied file must be unreadable: {other:?}"),
        }
    }
}
