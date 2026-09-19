use crate::prelude::*;
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
    pub fn production_only(&self, project_root: &Path) -> SourceFiles {
        SourceFiles {
            files: self
                .files
                .iter()
                .filter(|f| {
                    let file = f.borrow();
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
                    !Self::is_test_only_path_for_source(&file.file.to_id_path(), root, tagged)
                })
                .cloned()
                .collect(),
            package_roots: self.package_roots.clone(),
        }
    }

    /// Path-convention test-only check: true if a `tests` component is below
    /// `root`. Components in the absolute path before `root` are ignored. The
    /// caller supplies a package-owned path, so an outside path is an
    /// invariant violation rather than a non-test result.
    pub fn is_test_only_path(path: &str, root: &Path) -> bool {
        Self::is_test_only_path_for_source(path, root, true)
    }

    fn is_test_only_path_for_source(path: &str, root: &Path, tagged: bool) -> bool {
        let canonical_root = root.canonicalize().unwrap_or_else(|error| {
            panic!(
                "SourceFiles invariant: cannot canonicalize source root `{}`: {error}",
                root.display()
            )
        });
        let canonical_path = match Path::new(path).canonicalize() {
            Ok(path) => path,
            Err(error) if tagged => {
                panic!("SourceFiles invariant: cannot canonicalize tagged source `{path}`: {error}")
            }
            Err(_) => {
                // Untagged stdlib and loose sources may use non-filesystem ids.
                return false;
            }
        };
        let relative = match canonical_path.strip_prefix(&canonical_root) {
            Ok(relative) => relative,
            Err(_) if tagged => {
                panic!(
                    "SourceFiles invariant: tagged source `{}` is outside package root `{}`",
                    canonical_path.display(),
                    canonical_root.display()
                )
            }
            Err(_) => {
                // Untagged stdlib and loose files are not project-owned; admit them outside this root.
                return false;
            }
        };
        relative
            .components()
            .any(|c| c.as_os_str() == "tests")
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

        let production = sources.production_only(&project_root);
        let paths: Vec<_> = production
            .files
            .iter()
            .map(|file| file.borrow().file.to_id_path())
            .collect();

        assert_eq!(paths, [main_path.clone()]);
        assert!(!SourceFiles::is_test_only_path(&main_path, &project_root));
        assert!(SourceFiles::is_test_only_path(&broken_path, &project_root));
        std::fs::remove_dir_all(temp_root).unwrap();
    }
}
