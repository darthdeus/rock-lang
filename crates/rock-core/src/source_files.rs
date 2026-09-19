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
                    let root = file
                        .package
                        .as_ref()
                        .and_then(|tag| {
                            self.package_roots
                                .iter()
                                .find(|(id, _)| *id == tag.id)
                                .map(|(_, root)| root.as_path())
                        })
                        .unwrap_or(project_root);
                    !Self::is_test_only_path(&file.file.to_id_path(), root)
                })
                .cloned()
                .collect(),
            package_roots: self.package_roots.clone(),
        }
    }

    /// Path-convention test-only check: true if a `tests` component is below
    /// `root`. Components in the absolute path before `root` are ignored.
    pub fn is_test_only_path(path: &str, root: &Path) -> bool {
        let root = root
            .canonicalize()
            .unwrap_or_else(|_| root.to_path_buf());
        let Ok(relative) = Path::new(path).strip_prefix(root) else {
            return false;
        };
        relative
            .components()
            .any(|c| c.as_os_str() == "tests")
    }

    /// Whether a path contains a `tests` component anywhere. This is only for
    /// identifying package roots placed under a repository test fixture tree;
    /// file admission uses `is_test_only_path` with an owning root.
    pub fn contains_tests_component(path: &Path) -> bool {
        path.components().any(|c| c.as_os_str() == "tests")
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
        let sources = SourceFiles {
            files: vec![
                std::sync::Arc::new(atomic_refcell::AtomicRefCell::new(source(
                    "/tmp/tests/project/main.rock",
                ))),
                std::sync::Arc::new(atomic_refcell::AtomicRefCell::new(source(
                    "/tmp/tests/project/tests/broken.rock",
                ))),
            ],
            package_roots: Vec::new(),
        };

        let project_root = Path::new("/tmp/tests/project");
        let production = sources.production_only(project_root);
        let paths: Vec<_> = production
            .files
            .iter()
            .map(|file| file.borrow().file.to_id_path())
            .collect();

        assert_eq!(paths, ["/tmp/tests/project/main.rock"]);
        assert!(!SourceFiles::is_test_only_path(
            "/tmp/tests/project/main.rock",
            project_root
        ));
        assert!(SourceFiles::is_test_only_path(
            "/tmp/tests/project/tests/broken.rock",
            project_root
        ));
    }
}
