//! Preprocessor-dependent `SourceFile` / `SourceFiles` constructors.
//!
//! These can't live in `rock-core` (where `SourceFile`/`SourceFiles` are
//! defined) because they depend on the preprocessor: every load runs the
//! `#if`/`#script`/`#test`/`#use` directive pass to extract the
//! `is_script`/`is_test`/`is_test_util`/`used_packages` flags.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use atomic_refcell::AtomicRefCell;
use rock_core::{FileContents, SourceFile, SourceFiles};

use crate::preprocessor::{PreprocessorContext, preprocess};

/// Preprocessor-dependent SourceFiles constructors (can't be in rock-core since
/// they depend on the preprocessor).
pub trait SourceFilesExt {
    fn from_dir_preprocessed<P: AsRef<Path>>(source_dir: P) -> Result<SourceFiles>;
    fn from_single_path_preprocessed<P: AsRef<Path>>(path: P) -> Result<SourceFiles>;
    fn from_multiple_paths(paths: &[PathBuf], preprocess: bool) -> Result<SourceFiles>;
}

impl SourceFilesExt for SourceFiles {
    fn from_dir_preprocessed<P: AsRef<Path>>(source_dir: P) -> Result<SourceFiles> {
        source_files_from_dir_impl(source_dir, true)
    }

    fn from_single_path_preprocessed<P: AsRef<Path>>(path: P) -> Result<SourceFiles> {
        let file = source_file_load(path, true)?;
        Ok(SourceFiles::from_single_file(file))
    }

    fn from_multiple_paths(paths: &[PathBuf], preprocess: bool) -> Result<SourceFiles> {
        // Load in parallel, then collect in path order so the reported error
        // is still the first failing path's.
        let loaded: Vec<Result<SourceFile>> = {
            use rayon::prelude::*;
            paths
                .par_iter()
                .map(|path| source_file_load(path, preprocess))
                .collect()
        };
        let files = loaded.into_iter().collect::<Result<Vec<_>>>()?;

        let files = files
            .into_iter()
            .map(|x| Arc::new(AtomicRefCell::new(x)))
            .collect();

        Ok(SourceFiles {
            files,
            package_roots: Vec::new(),
        })
    }
}

fn source_files_from_dir_impl<P: AsRef<Path>>(
    source_dir: P,
    preprocess: bool,
) -> Result<SourceFiles> {
    let paths = crate::project::find_rock_files_at_path(source_dir)?;

    // Each file does read_to_string + preprocess. Paths are already canonical
    // (find_rock_files_at_path canonicalizes the base dir).
    let load = |path: &String| source_file_from_canonical_path(Path::new(path), preprocess);
    let loaded: Result<Vec<_>> = {
        use rayon::prelude::*;
        paths.par_iter().map(load).collect()
    };

    let files = loaded?
        .into_iter()
        .map(|x| Arc::new(AtomicRefCell::new(x)))
        .collect();

    Ok(SourceFiles {
        files,
        package_roots: Vec::new(),
    })
}

/// Scan a file's leading `//!` directives for `modules: v1`. Standalone
/// test/script sources declare their dialect explicitly; package files take
/// theirs from the manifest via the package graph. Only the first
/// ten lines count, mirroring `parse_lib_directive`.
pub fn parse_modules_directive(contents: &str) -> Result<rock_core::source_code::Dialect> {
    for line in contents.lines().take(10) {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("//!") else {
            continue;
        };
        let rest = rest.trim();
        if let Some(value) = rest.strip_prefix("modules:") {
            return match value.trim() {
                "v1" => Ok(rock_core::source_code::Dialect::V1),
                "legacy" | "" => Ok(rock_core::source_code::Dialect::Legacy),
                other => {
                    bail!("unknown `//! modules:` directive value `{other}` (known: legacy, v1)")
                }
            };
        }
    }
    Ok(rock_core::source_code::Dialect::Legacy)
}

// Helper function to process content and extract directives.
// The preprocessor always runs (to extract is_script/is_test/used_packages
// flags), but `apply` controls whether its output replaces the raw contents.
// Callers that format or display source pass `apply: false`; callers that
// compile pass `apply: true`.
fn preprocess_content(
    contents: &str,
    apply: bool,
) -> Result<(String, bool, bool, bool, Vec<String>)> {
    let context = PreprocessorContext::new("lua");
    let preprocessed = preprocess(contents, &context)?;

    let final_contents = if apply {
        preprocessed.content
    } else {
        contents.to_string()
    };

    Ok((
        final_contents,
        preprocessed.is_script,
        preprocessed.is_test,
        preprocessed.is_test_util,
        preprocessed.used_packages,
    ))
}

// Helper function to create a SourceFile with all required fields
fn create_source_file(
    file: FileContents,
    contents: String,
    is_script: bool,
    is_test: bool,
    is_test_util: bool,
    used_packages: Vec<String>,
) -> Result<SourceFile> {
    // A `//! modules: v1` directive opts a standalone source into v1. Package
    // loaders overwrite this with the manifest/graph fact afterwards.
    let dialect = parse_modules_directive(&contents)?;
    Ok(SourceFile {
        file,
        contents,
        is_script,
        is_test,
        is_test_util,
        used_packages,
        package: None,
        dialect,
        global_policy: rock_core::source_code::GlobalPolicy::Any,
    })
}

/// Load a source file without applying preprocessor output (used by the
/// formatter and LSP, which want the original text with #if blocks intact).
pub(crate) fn source_file_new(path: impl AsRef<Path>) -> Result<SourceFile> {
    source_file_load(path, false)
}

/// Load one unprocessed source with its package's parsing and ownership facts.
/// Standalone files retain their `//! modules:` directive and have no package.
pub fn source_file_new_with_context(path: impl AsRef<Path>) -> Result<SourceFile> {
    let canonical = fs_err::canonicalize(path.as_ref())?;
    let mut source = source_file_from_canonical_path(&canonical, false)?;
    let start_dir = canonical.parent().unwrap_or(&canonical);
    if let Some((manifest, config)) = crate::stick::StickConfig::find_from_dir(start_dir)? {
        let manifest_dir = manifest.parent().unwrap_or(&manifest);
        let channel = crate::stick::resolved_channel_for(&config);
        let graph =
            crate::stick::graph::PackageGraph::build_with_channel(manifest_dir, &config, &channel)?;
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

/// Load a source file and apply preprocessor output (used by the compilation
/// pipeline, which wants #if blocks resolved).
pub fn source_file_new_preprocessed(path: impl AsRef<Path>) -> Result<SourceFile> {
    source_file_load(path, true)
}

fn source_file_load(path: impl AsRef<Path>, preprocess: bool) -> Result<SourceFile> {
    let path = fs_err::canonicalize(path.as_ref())
        .with_context(|| format!("Failed to resolve path '{}'", path.as_ref().display()))?;
    source_file_from_canonical_path(&path, preprocess)
}

/// Load a source file from an already-canonical absolute path (skips canonicalize syscall).
fn source_file_from_canonical_path(path: &Path, preprocess: bool) -> Result<SourceFile> {
    let raw_contents = fs_err::read_to_string(path)
        .with_context(|| format!("Failed to read '{}'", path.display()))?;

    let (contents, is_script, is_test, is_test_util, used_packages) =
        preprocess_content(&raw_contents, preprocess)?;

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

    create_source_file(
        file,
        contents,
        is_script,
        is_test,
        is_test_util,
        used_packages,
    )
}

pub fn source_file_from_code(id_path: String, contents: String) -> SourceFile {
    // See source_file_from_path_and_contents above for the rationale.
    let (processed_contents, is_script, is_test, is_test_util, used_packages) =
        match preprocess_content(&contents, false) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("rock: preprocessor error for `{id_path}`: {e:#}");
                (contents.clone(), false, false, false, vec![])
            }
        };

    let file = FileContents::Code {
        id_path: id_path.clone(),
        contents,
    };

    create_source_file(
        file,
        processed_contents,
        is_script,
        is_test,
        is_test_util,
        used_packages,
    )
    .unwrap_or_else(|e| panic!("rock: {e:#}"))
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
        let err = super::source_file_new(&path).expect_err("non-UTF-8 path must be rejected");
        assert!(format!("{err:#}").contains("not valid UTF-8"), "{err:#}");
    }
}
