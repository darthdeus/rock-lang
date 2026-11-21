use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use atomic_refcell::AtomicRefCell;
use rock_core::{FileContents, SourceFile, SourceFiles};

pub trait SourceFilesExt {
    fn from_single_path<P: AsRef<Path>>(path: P) -> Result<SourceFiles>;
    fn from_canonical_paths(paths: &[PathBuf]) -> Result<SourceFiles>;
}

impl SourceFilesExt for SourceFiles {
    fn from_single_path<P: AsRef<Path>>(path: P) -> Result<SourceFiles> {
        Ok(SourceFiles::from_single_file(source_file_load(path)?))
    }

    fn from_canonical_paths(paths: &[PathBuf]) -> Result<SourceFiles> {
        load_paths(paths, |path| source_file_from_canonical_path(path))
    }
}

fn load_paths(
    paths: &[PathBuf],
    load: impl Fn(&PathBuf) -> Result<SourceFile> + Sync + Send,
) -> Result<SourceFiles> {
    let loaded: Vec<Result<SourceFile>> = {
        use rayon::prelude::*;
        paths.par_iter().map(load).collect()
    };
    let files = loaded.into_iter().collect::<Result<Vec<_>>>()?;

    let files = files
        .into_iter()
        .map(|x| Arc::new(AtomicRefCell::new(x)))
        .collect();

    Ok(SourceFiles {
        files,
        ..SourceFiles::empty()
    })
}

/// The `lstat`s run in parallel; the first failing file in order reports.
pub fn production_only(
    sources: &SourceFiles,
    project_root: &Path,
) -> std::result::Result<SourceFiles, rock_core::errors::CompilerError> {
    use rayon::prelude::*;
    let filter = sources.production_filter(project_root);
    let keep: Vec<_> = sources
        .files
        .par_iter()
        .map_init(std::collections::HashSet::new, |plain_dirs, file| {
            filter.keeps(&file.borrow(), plain_dirs)
        })
        .collect();
    let keep = keep
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(sources.retaining(&keep))
}

pub fn source_file_new_with_context(path: impl AsRef<Path>) -> Result<SourceFile> {
    let canonical = fs_err::canonicalize(path.as_ref())?;
    let mut source = source_file_from_canonical_path(&canonical)?;
    let start_dir = canonical.parent().unwrap_or(&canonical);
    if let Some((manifest_dir, graph)) =
        crate::stick::graph::PackageGraph::find_from_dir(start_dir)?
    {
        let owner = graph.package_of_file(&canonical).ok_or_else(|| {
            anyhow::anyhow!(
                "file {} is not owned by the package graph rooted at {}",
                canonical.display(),
                manifest_dir.display()
            )
        })?;
        let node = graph.package(owner);
        node.stamp(&mut source);
    }
    Ok(source)
}

pub(crate) fn source_file_load(path: impl AsRef<Path>) -> Result<SourceFile> {
    let path = fs_err::canonicalize(path.as_ref())
        .with_context(|| format!("Failed to resolve path '{}'", path.as_ref().display()))?;
    source_file_from_canonical_path(&path)
}

fn source_file_from_canonical_path(path: &Path) -> Result<SourceFile> {
    let contents = fs_err::read_to_string(path)
        .with_context(|| format!("Failed to read '{}'", path.display()))?;

    let utf8_path = path
        .to_str()
        .with_context(|| format!("Source path is not valid UTF-8: '{}'", path.display()))?;
    let filename = path
        .file_name()
        .with_context(|| format!("Source path has no file name: '{}'", path.display()))?;
    let file = FileContents::Filesystem {
        path: utf8_path.to_string(),
        filename: filename.to_string_lossy().to_string(),
    };

    Ok(SourceFile {
        file,
        contents,
        package: None,
        global_policy: rock_core::source_code::GlobalPolicy::Any,
    })
}

#[cfg(all(test, unix))]
mod tests {
    #[cfg(not(target_os = "macos"))]
    use std::os::unix::ffi::OsStrExt;

    // APFS/HFS+ cannot create filenames containing invalid UTF-8 bytes.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_utf8_source_path_is_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir
            .path()
            .join(std::ffi::OsStr::from_bytes(b"bad\xff.rock"));
        fs_err::write(&path, "fn main() {}\n").unwrap();
        let err = super::source_file_load(&path).expect_err("non-UTF-8 path must be rejected");
        assert!(format!("{err:#}").contains("not valid UTF-8"), "{err:#}");
    }
}
