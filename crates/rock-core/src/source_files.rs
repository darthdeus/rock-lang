use crate::errors::CompilerError;
use crate::prelude::*;
use atomic_refcell::AtomicRefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct SourceFiles {
    pub files: Vec<Arc<AtomicRefCell<SourceFile>>>,
    pub package_roots: Vec<(u32, PathBuf)>,
    pub ns_aliases: Option<crate::package_aliases::NsAliases>,
}

impl SourceFiles {
    pub fn empty() -> Self {
        Self {
            files: Vec::new(),
            package_roots: Vec::new(),
            ns_aliases: None,
        }
    }

    pub fn from_single_file(file: SourceFile) -> Self {
        Self {
            files: vec![Arc::new(AtomicRefCell::new(file))],
            package_roots: Vec::new(),
            ns_aliases: None,
        }
    }

    pub fn set_package_roots(&mut self, roots: impl IntoIterator<Item = (u32, PathBuf)>) {
        self.package_roots = roots.into_iter().collect();
    }

    // A re-add keeps its slot so the set order does not depend on how often a file was loaded.
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

    pub fn production_filter<'a>(&'a self, project_root: &'a Path) -> ProductionFilter<'a> {
        let canonical = |root: &Path| root.canonicalize().map_err(|error| error.to_string());
        ProductionFilter {
            sources: self,
            project_root: (project_root, canonical(project_root)),
            package_roots: self
                .package_roots
                .iter()
                .map(|(_, root)| canonical(root))
                .collect(),
        }
    }

    pub fn retaining(&self, keep: &[bool]) -> SourceFiles {
        SourceFiles {
            files: self
                .files
                .iter()
                .zip(keep)
                .filter(|(_, keep)| **keep)
                .map(|(file, _)| file.clone())
                .collect(),
            package_roots: self.package_roots.clone(),
            ns_aliases: self.ns_aliases.clone(),
        }
    }

    fn package_root_index(&self, id: u32, path: &str) -> usize {
        match self
            .package_roots
            .iter()
            .position(|(known, _)| *known == id)
        {
            Some(index) => index,
            None => panic!(
                "SourceFiles invariant: tagged source `{path}` has no package root for package id {id}"
            ),
        }
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
        plain_dirs: &mut std::collections::HashSet<PathBuf>,
    ) -> Result<bool, CompilerError> {
        let canonical_path =
            match Self::canonical_under(Path::new(path), canonical_root, plain_dirs) {
                Some(canonical) => canonical,
                None => match Self::canonicalize_source(path, root, tagged)? {
                    Some(canonical) => canonical,
                    None => return Ok(false),
                },
            };
        let relative = match canonical_path.strip_prefix(canonical_root) {
            Ok(relative) => relative,
            Err(_) if !tagged => {
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

    fn canonical_under(
        path: &Path,
        canonical_root: &Path,
        plain_dirs: &mut std::collections::HashSet<PathBuf>,
    ) -> Option<PathBuf> {
        use std::path::Component;
        let relative = path.strip_prefix(canonical_root).ok()?;
        let mut probe = canonical_root.to_path_buf();
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                return None;
            };
            probe.push(name);
            let dir = components.peek().is_some();
            if dir && plain_dirs.contains(&probe) {
                continue;
            }
            if std::fs::symlink_metadata(&probe).ok()?.is_symlink() {
                return None;
            }
            if dir {
                plain_dirs.insert(probe.clone());
            }
        }
        Some(probe)
    }

    fn canonicalize_source(
        path: &str,
        root: &Path,
        tagged: bool,
    ) -> Result<Option<PathBuf>, CompilerError> {
        match Path::new(path).canonicalize() {
            Ok(canonical) => Ok(Some(canonical)),
            Err(_) if !tagged => Ok(None),
            Err(error) => Err(CompilerError::new_no_location(format!(
                "cannot classify source `{path}` against root `{}`: cannot canonicalize source: {error}",
                root.display()
            ))),
        }
    }

    pub fn is_test_only_relative_path(relative: &Path) -> bool {
        relative
            .components()
            .any(|component| component.as_os_str() == "tests")
    }
}

pub struct ProductionFilter<'a> {
    sources: &'a SourceFiles,
    project_root: (&'a Path, Result<PathBuf, String>),
    package_roots: Vec<Result<PathBuf, String>>,
}

impl ProductionFilter<'_> {
    pub fn keeps(
        &self,
        source: &SourceFile,
        plain_dirs: &mut std::collections::HashSet<PathBuf>,
    ) -> Result<bool, CompilerError> {
        let path = source.file.to_id_path();
        let (root, canonical_root, tagged) = match source.package.as_ref() {
            Some(tag) => {
                let index = self.sources.package_root_index(tag.id, &path);
                let root = self.sources.package_roots[index].1.as_path();
                (root, &self.package_roots[index], true)
            }
            None => (self.project_root.0, &self.project_root.1, false),
        };
        let canonical_root = SourceFiles::canonical_root_for(&path, root, canonical_root)?;
        let test_only =
            SourceFiles::is_test_only_path_under(&path, root, canonical_root, tagged, plain_dirs)?;
        Ok(!test_only)
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

        let sources = SourceFiles::empty();
        let filter = sources.production_filter(&project_root);
        let keeps = |path: &Path| {
            filter
                .keeps(&source(path.to_str().unwrap()), &mut Default::default())
                .unwrap()
        };
        assert!(!keeps(&through_link));
        assert!(keeps(&out_of_tests));
        assert!(keeps(&plain));
        std::fs::remove_dir_all(temp_root).unwrap();
    }
}
