use crate::errors::CompilerError;
use crate::prelude::*;
use atomic_refcell::AtomicRefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

    /// A copy without the test-only files. Test-only means the path
    /// convention: a file with a `tests` component below the root of the
    /// package that owns it (sims live in `<pkg>/tests/sims/`, unit tests in
    /// `<pkg>/tests/`). Production builds ask for this copy so that an error
    /// in a test-only file cannot break the application, while the test, sim,
    /// lint and lsp paths keep working on the full set.
    pub fn production_only(&self, project_root: &Path) -> Result<SourceFiles, CompilerError> {
        let mut files = Vec::with_capacity(self.files.len());
        // Every file shares one of a few roots; canonicalize each root once.
        let mut canonical_roots: Vec<(&Path, Result<PathBuf, String>)> = Vec::new();
        for source in &self.files {
            let file = source.borrow();
            let path = file.file.to_id_path();
            let (root, tagged) = match file.package.as_ref() {
                Some(tag) => (self.package_root(tag.id, &path), true),
                None => (project_root, false),
            };
            let index = match canonical_roots.iter().position(|(known, _)| *known == root) {
                Some(index) => index,
                None => {
                    let canonical = root.canonicalize().map_err(|error| error.to_string());
                    canonical_roots.push((root, canonical));
                    canonical_roots.len() - 1
                }
            };
            let canonical_root = Self::canonical_root_for(&path, root, &canonical_roots[index].1)?;
            if !Self::is_test_only_path_under(&path, root, canonical_root, tagged)? {
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
        let canonical_root = root.canonicalize().map_err(|error| error.to_string());
        let canonical_root = Self::canonical_root_for(path, root, &canonical_root)?;
        Self::is_test_only_path_under(path, root, canonical_root, tagged)
    }

    fn canonical_root_for<'a>(
        path: &str,
        root: &Path,
        canonical_root: &'a Result<PathBuf, String>,
    ) -> Result<&'a Path, CompilerError> {
        canonical_root.as_deref().map_err(|error| {
            CompilerError::new_no_location(format!(
                "cannot classify source `{path}` against root `{}`: cannot canonicalize root: {error}",
                root.display()
            ))
        })
    }

    fn is_test_only_path_under(
        path: &str,
        root: &Path,
        canonical_root: &Path,
        tagged: bool,
    ) -> Result<bool, CompilerError> {
        let canonical_path = match Self::canonical_under(Path::new(path), canonical_root) {
            Some(canonical) => canonical,
            None => match Self::canonicalize_source(path, root, tagged)? {
                Some(canonical) => canonical,
                None => return Ok(false),
            },
        };
        let relative = match canonical_path.strip_prefix(canonical_root) {
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

    /// `path` itself when it is already canonical below `canonical_root`:
    /// the root's own spelling, then plain names with no symlink among them.
    /// Discovery hands over canonical paths, so this answers for nearly every
    /// source with one `lstat` per component below the root instead of a
    /// `realpath` walk from `/`.
    fn canonical_under(path: &Path, canonical_root: &Path) -> Option<PathBuf> {
        use std::path::Component;
        let relative = path.strip_prefix(canonical_root).ok()?;
        let mut probe = canonical_root.to_path_buf();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return None;
            };
            probe.push(name);
            if std::fs::symlink_metadata(&probe).ok()?.is_symlink() {
                return None;
            }
        }
        Some(probe)
    }

    /// The filesystem's canonical spelling of `path`, or `None` for an
    /// untagged source the filesystem does not know.
    fn canonicalize_source(
        path: &str,
        root: &Path,
        tagged: bool,
    ) -> Result<Option<PathBuf>, CompilerError> {
        match Path::new(path).canonicalize() {
            Ok(canonical) => Ok(Some(canonical)),
            Err(_) if !tagged => {
                // Untagged stdlib and loose sources may use non-filesystem ids.
                Ok(None)
            }
            Err(error) => Err(CompilerError::new_no_location(format!(
                "cannot classify source `{path}` against root `{}`: cannot canonicalize source: {error}",
                root.display()
            ))),
        }
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
    fn an_empty_set_holds_no_files_and_no_package_roots() {
        let sources = SourceFiles::empty();
        assert!(sources.files.is_empty());
        assert!(sources.package_roots.is_empty());
    }

    #[test]
    fn a_single_file_set_holds_exactly_that_file() {
        let sources = SourceFiles::from_single_file(source("main.rock"));
        assert_eq!(sources.files.len(), 1);
        assert_eq!(sources.files[0].borrow().file.to_id_path(), "main.rock");
        assert!(sources.package_roots.is_empty());
    }

    #[test]
    fn a_known_file_is_replaced_in_place_and_a_new_one_is_appended() {
        let mut sources = SourceFiles::empty();
        sources.add_or_update_file(source("main.rock"));
        sources.add_or_update_file(source("other.rock"));
        assert_eq!(sources.files.len(), 2);

        let mut updated = source("main.rock");
        updated.contents = "fn main() { changed(); }\n".to_string();
        sources.add_or_update_file(updated);
        assert_eq!(sources.files.len(), 2);
        assert_eq!(sources.files[0].borrow().file.to_id_path(), "main.rock");
        assert_eq!(
            sources.files[0].borrow().contents,
            "fn main() { changed(); }\n"
        );
    }

    #[test]
    fn package_roots_are_stored_keyed_by_package_id() {
        let mut sources = SourceFiles::empty();
        sources.set_package_roots([(7, std::path::PathBuf::from("/tmp/pkg"))]);
        assert_eq!(sources.package_roots.len(), 1);
        assert_eq!(sources.package_roots[0].0, 7);
    }

    #[test]
    fn a_tests_component_anywhere_below_the_root_marks_a_relative_path_test_only() {
        assert!(SourceFiles::is_test_only_relative_path(Path::new(
            "tests/main.rock"
        )));
        assert!(SourceFiles::is_test_only_relative_path(Path::new(
            "sims/tests/a.rock"
        )));
        assert!(SourceFiles::is_test_only_relative_path(Path::new("tests")));
        assert!(!SourceFiles::is_test_only_relative_path(Path::new(
            "src/main.rock"
        )));
        assert!(!SourceFiles::is_test_only_relative_path(Path::new(
            "src/testing.rock"
        )));
    }

    #[test]
    fn production_only_keeps_files_when_project_root_is_under_tests() {
        let temp_root =
            std::env::temp_dir().join(format!("rock_core_source_files_{}", std::process::id()));
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
                std::sync::Arc::new(atomic_refcell::AtomicRefCell::new(source(&main_path))),
                std::sync::Arc::new(atomic_refcell::AtomicRefCell::new(source(&broken_path))),
            ],
            package_roots: Vec::new(),
        };

        let production = sources.production_only(&project_root).unwrap();
        let paths: Vec<_> = production
            .files
            .iter()
            .map(|file| file.borrow().file.to_id_path())
            .collect();

        assert_eq!(paths.as_slice(), std::slice::from_ref(&main_path));
        assert!(!SourceFiles::is_test_only_path(&main_path, &project_root).unwrap());
        assert!(SourceFiles::is_test_only_path(&broken_path, &project_root).unwrap());
        std::fs::remove_dir_all(temp_root).unwrap();
    }

    /// A path spelled below the root is classified by where it really is: a
    /// symlink below the root into a `tests` directory is test-only, and one
    /// out of a `tests` directory into production is not.
    #[cfg(unix)]
    #[test]
    fn production_classification_follows_symlinks_below_the_root() {
        let temp_root = std::env::temp_dir().join(format!(
            "rock_core_source_files_links_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp_root);
        let project_root = temp_root.join("project");
        std::fs::create_dir_all(project_root.join("tests")).unwrap();
        std::fs::create_dir_all(project_root.join("src")).unwrap();
        std::fs::write(
            project_root.join("tests/helper.rock"),
            "fn helper() {}
",
        )
        .unwrap();
        std::fs::write(
            project_root.join("src/main.rock"),
            "fn main() {}
",
        )
        .unwrap();
        std::os::unix::fs::symlink("tests", project_root.join("linked")).unwrap();
        std::os::unix::fs::symlink("../src", project_root.join("tests/production")).unwrap();
        let project_root = project_root.canonicalize().unwrap();
        let through_link = project_root.join("linked/helper.rock");
        let out_of_tests = project_root.join("tests/production/main.rock");
        let plain = project_root.join("src/main.rock");

        assert!(
            SourceFiles::is_test_only_path(through_link.to_str().unwrap(), &project_root).unwrap()
        );
        assert!(
            !SourceFiles::is_test_only_path(out_of_tests.to_str().unwrap(), &project_root).unwrap()
        );
        assert!(!SourceFiles::is_test_only_path(plain.to_str().unwrap(), &project_root).unwrap());
        std::fs::remove_dir_all(temp_root).unwrap();
    }
}
