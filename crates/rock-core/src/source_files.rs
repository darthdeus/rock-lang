use crate::prelude::*;
use crate::errors::CompilerError;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use atomic_refcell::AtomicRefCell;

/// The set of files one compilation sees, with the package layout they came
/// from.
#[derive(Clone, Debug)]
pub struct SourceFiles {
    /// Shared so a diagnostic can hand a single file to another component
    /// without copying its text.
    pub files: Vec<Arc<AtomicRefCell<SourceFile>>>,
    /// Canonical roots of the packages the graph loaded, keyed by `PackageTag`
    /// id. Left empty by standalone and synthetic source sets, which own no
    /// package.
    pub package_roots: Vec<(u32, PathBuf)>,
}

impl SourceFiles {
    pub fn empty() -> Self {
        Self {
            files: Vec::new(),
            package_roots: Vec::new(),
        }
    }

    pub fn from_single_file(file: SourceFile) -> Self {
        Self {
            files: vec![Arc::new(AtomicRefCell::new(file))],
            package_roots: Vec::new(),
        }
    }

    pub fn set_package_roots(&mut self, roots: impl IntoIterator<Item = (u32, PathBuf)>) {
        self.package_roots = roots.into_iter().collect();
    }

    /// Insert `updated`, overwriting in place the file that shares its id path
    /// and appending it when the path is new. A re-add must keep its slot so
    /// that the order of the set does not depend on how often a file was
    /// loaded.
    pub fn add_or_update_file(&mut self, updated: SourceFile) {
        let path = updated.file.to_id_path();
        let known = self
            .files
            .iter_mut()
            .find(|file| file.borrow().file.to_id_path() == path);
        match known {
            Some(slot) => *slot.borrow_mut() = updated,
            None => self.files.push(Arc::new(AtomicRefCell::new(updated))),
        }
    }

    pub fn get_line(&self, line_col: &LineCol) -> Option<String> {
        let file = self.file_for_linecol(line_col)?;
        file.borrow()
            .contents
            .lines()
            .nth(line_col.line)
            .map(str::to_string)
    }

    pub fn file_for_linecol(&self, line_col: &LineCol) -> Option<&Arc<AtomicRefCell<SourceFile>>> {
        self.files
            .iter()
            .find(|file| file.borrow().file.to_id_path() == line_col.file.as_str())
    }

    /// A copy without the test-only files. Test-only means the path
    /// convention: a file with a `tests` component below the root of the
    /// package that owns it (sims live in `<pkg>/tests/sims/`, unit tests in
    /// `<pkg>/tests/`). Production builds ask for this copy so that an error
    /// in a test-only file cannot break the application, while the test, sim,
    /// lint and lsp paths keep working on the full set.
    pub fn production_only(&self, project_root: &Path) -> Result<SourceFiles, CompilerError> {
        let mut files = Vec::with_capacity(self.files.len());
        for source in &self.files {
            let file = source.borrow();
            let path = file.file.to_id_path();
            let (root, tagged) = match file.package.as_ref() {
                Some(tag) => (self.package_root(tag.id, &path), true),
                None => (project_root, false),
            };
            if !Self::is_test_only_path_for_source(&path, root, tagged)? {
                files.push(source.clone());
            }
        }
        Ok(SourceFiles {
            files,
            package_roots: self.package_roots.clone(),
        })
    }

    /// The recorded root of one package. A tagged file whose package has no
    /// recorded root breaks the invariant that the loader stamps both together,
    /// and classification must not guess a root for it.
    fn package_root(&self, id: u32, path: &str) -> &Path {
        match self.package_roots.iter().find(|(known, _)| *known == id) {
            Some((_, root)) => root.as_path(),
            None => panic!(
                "SourceFiles invariant: tagged source `{path}` has no package root for package id {id}"
            ),
        }
    }

    /// Whether a package-owned path sits under a `tests` component of `root`.
    /// Path components above `root` are ignored, and a path outside `root` is
    /// an error rather than a guess. Unowned sources may legitimately live
    /// outside the project root, so production filtering calls the private
    /// helper with `tagged = false` for them.
    pub fn is_test_only_path(path: &str, root: &Path) -> Result<bool, CompilerError> {
        Self::is_test_only_path_for_source(path, root, true)
    }

    fn is_test_only_path_for_source(
        path: &str,
        root: &Path,
        tagged: bool,
    ) -> Result<bool, CompilerError> {
        let canonical_root = root.canonicalize().map_err(|error| {
            CompilerError::new_no_location(format!(
                "cannot classify source `{path}` against root `{}`: cannot canonicalize root: {error}",
                root.display()
            ))
        })?;
        let canonical_path = match Path::new(path).canonicalize() {
            Ok(canonical) => canonical,
            Err(_) if !tagged => {
                // Untagged stdlib and loose sources may use non-filesystem ids.
                return Ok(false);
            }
            Err(error) => {
                return Err(CompilerError::new_no_location(format!(
                    "cannot classify source `{path}` against root `{}`: cannot canonicalize source: {error}",
                    root.display()
                )));
            }
        };
        let relative = match canonical_path.strip_prefix(&canonical_root) {
            Ok(relative) => relative,
            Err(_) if !tagged => {
                // Untagged stdlib and loose files are not owned by this root.
                return Ok(false);
            }
            Err(_) => {
                return Err(CompilerError::new_no_location(format!(
                    "production source `{path}` is outside package root `{}`",
                    root.display()
                )));
            }
        };
        Ok(Self::is_test_only_relative_path(relative))
    }

    /// The check shared by production discovery and watch-event filtering,
    /// once the caller has resolved which package owns the file.
    pub fn is_test_only_relative_path(relative: &Path) -> bool {
        relative
            .components()
            .any(|component| component.as_os_str() == "tests")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(path: &str) -> SourceFile {
        crate::source_code::source_file_from_path_and_contents(
            path.to_string(),
            "source.rock".to_string(),
            "fn main() {}\n".to_string(),
        )
    }

    #[test]
    fn production_only_keeps_files_when_project_root_is_under_tests() {
        let temp_root = std::env::temp_dir().join(format!(
            "rock_core_source_files_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp_root);
        let project_root = temp_root.join("tests/project");
        std::fs::create_dir_all(project_root.join("tests")).unwrap();
        let main_path = project_root.join("main.rock");
        let broken_path = project_root.join("tests/broken.rock");
        std::fs::write(&main_path, "fn main() {}\n").unwrap();
        std::fs::write(&broken_path, "fn broken() {}\n").unwrap();
        let main_path = main_path.to_string_lossy().into_owned();
        let broken_path = broken_path.to_string_lossy().into_owned();
        let sources = SourceFiles {
            files: vec![
                std::sync::Arc::new(atomic_refcell::AtomicRefCell::new(source(
                    &main_path,
                ))),
                std::sync::Arc::new(atomic_refcell::AtomicRefCell::new(source(
                    &broken_path,
                ))),
            ],
            package_roots: Vec::new(),
        };

        let production = sources.production_only(&project_root).unwrap();
        let paths: Vec<_> = production
            .files
            .iter()
            .map(|file| file.borrow().file.to_id_path())
            .collect();

        assert_eq!(paths, [main_path.clone()]);
        assert!(!SourceFiles::is_test_only_path(&main_path, &project_root).unwrap());
        assert!(SourceFiles::is_test_only_path(&broken_path, &project_root).unwrap());
        std::fs::remove_dir_all(temp_root).unwrap();
    }
}
