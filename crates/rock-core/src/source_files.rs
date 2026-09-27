use crate::prelude::*;
use crate::errors::CompilerError;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use atomic_refcell::AtomicRefCell;

#[derive(Clone, Debug)]
pub struct SourceFiles {
    pub files: Vec<Arc<AtomicRefCell<SourceFile>>>,
    /// Canonical package roots for graph-loaded files, keyed by PackageTag id.
    /// Empty for standalone and synthetic source sets.
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

    pub fn add_or_update_file(&mut self, file: SourceFile) {
        if let Some(existing) = self
            .files
            .iter_mut()
            .find(|f| f.borrow().file.to_id_path() == file.file.to_id_path())
        {
            *existing.borrow_mut() = file;
        } else {
            self.files.push(Arc::new(AtomicRefCell::new(file)));
        }
    }

    pub fn get_line(&self, line_col: &LineCol) -> Option<String> {
        let file = self.file_for_linecol(line_col)?;
        let file = file.borrow();
        file.contents
            .lines()
            .nth(line_col.line)
            .map(|s| s.to_string())
    }

    pub fn file_for_linecol(&self, line_col: &LineCol) -> Option<&Arc<AtomicRefCell<SourceFile>>> {
        self.files
            .iter()
            .find(|f| *f.borrow().file.to_id_path() == *line_col.file)
    }

    /// A copy with every test-only file removed. Test-only is the path
    /// convention: any file under a `tests/` directory component below its
    /// owning package root (sims live in `<pkg>/tests/sims/`, unit tests in
    /// `<pkg>/tests/`). Game builds (`rock run/stone/frame/perf/compile <game>`)
    /// use this so a compile error in a sim/test can't break the running game.
    /// Test/sim/lint/lsp paths use the full set. See `docs/sim-layering.md`.
    pub fn production_only(&self, project_root: &Path) -> Result<SourceFiles, CompilerError> {
        let mut files = Vec::with_capacity(self.files.len());
        for source in &self.files {
            let file = source.borrow();
            let (root, tagged) = match file.package.as_ref() {
                Some(tag) => {
                    let root = self
                        .package_roots
                        .iter()
                        .find(|(id, _)| *id == tag.id)
                        .unwrap_or_else(|| {
                            panic!(
                                "SourceFiles invariant: tagged source `{}` has no package root for package id {}",
                                file.file.to_id_path(),
                                tag.id
                            )
                        });
                    (root.1.as_path(), true)
                }
                None => (project_root, false),
            };
            if !Self::is_test_only_path_for_source(&file.file.to_id_path(), root, tagged)? {
                files.push(source.clone());
            }
        }
        Ok(SourceFiles {
            files,
            package_roots: self.package_roots.clone(),
        })
    }

    /// Path-convention test-only check: true if a `tests` component is below
    /// `root`. Components in the absolute path before `root` are ignored.
    /// The caller supplies a package-owned path; an outside path is an error.
    /// Production filtering uses the private helper with `tagged = false` for
    /// unowned sources that may legitimately sit outside the project root.
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
            Ok(path) => path,
            Err(error) if tagged => {
                return Err(CompilerError::new_no_location(format!(
                    "cannot classify source `{path}` against root `{}`: cannot canonicalize source: {error}",
                    root.display()
                )));
            }
            Err(_) => {
                // Untagged stdlib and loose sources may use non-filesystem ids.
                return Ok(false);
            }
        };
        let relative = match canonical_path.strip_prefix(&canonical_root) {
            Ok(relative) => relative,
            Err(_) if tagged => {
                return Err(CompilerError::new_no_location(format!(
                    "production source `{path}` is outside package root `{}`",
                    root.display()
                )));
            }
            Err(_) => {
                // Untagged stdlib and loose files are not project-owned; admit them outside this root.
                return Ok(false);
            }
        };
        Ok(Self::is_test_only_relative_path(relative))
    }

    /// Shared by production discovery and watch-event filtering after each
    /// caller has resolved the owning package root.
    pub fn is_test_only_relative_path(relative: &Path) -> bool {
        relative.components().any(|c| c.as_os_str() == "tests")
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
