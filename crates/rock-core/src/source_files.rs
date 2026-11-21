use crate::errors::CompilerError;
use crate::prelude::*;
use atomic_refcell::AtomicRefCell;
use std::collections::HashSet;
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
            ..Self::empty()
        }
    }

    // A re-add keeps its slot so the set order does not depend on how often a file was loaded.
    pub fn add_or_update_file(&mut self, updated: SourceFile) {
        let path = updated.file.id_path();
        let known = self
            .files
            .iter()
            .find(|file| file.borrow().file.id_path() == path);
        match known {
            Some(slot) => *slot.borrow_mut() = updated,
            None => self.files.push(Arc::new(AtomicRefCell::new(updated))),
        }
    }

    pub fn production_filter<'a>(&'a self, project_root: &'a Path) -> ProductionFilter<'a> {
        ProductionFilter {
            sources: self,
            project_root: (project_root, project_root.canonicalize()),
            package_roots: self
                .package_roots
                .iter()
                .map(|(_, root)| root.canonicalize())
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

    pub fn is_test_only_relative_path(relative: &Path) -> bool {
        relative
            .components()
            .any(|component| component.as_os_str() == "tests")
    }
}

fn canonical_under(
    path: &Path,
    canonical_root: &Path,
    plain_dirs: &mut HashSet<PathBuf>,
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

pub struct ProductionFilter<'a> {
    sources: &'a SourceFiles,
    project_root: (&'a Path, std::io::Result<PathBuf>),
    package_roots: Vec<std::io::Result<PathBuf>>,
}

impl ProductionFilter<'_> {
    pub fn keeps(
        &self,
        source: &SourceFile,
        plain_dirs: &mut HashSet<PathBuf>,
    ) -> Result<bool, CompilerError> {
        let path = source.file.id_path();
        let (root, canonical_root, tagged) = if let Some(tag) = &source.package {
            let ((_, root), canonical) = self
                .sources
                .package_roots
                .iter()
                .zip(&self.package_roots)
                .find(|((id, _), _)| *id == tag.id)
                .unwrap_or_else(|| {
                    panic!(
                        "SourceFiles invariant: tagged source `{path}` has no package root for package id {}",
                        tag.id
                    )
                });
            (root.as_path(), canonical, true)
        } else {
            (self.project_root.0, &self.project_root.1, false)
        };
        let canonical_root = canonical_root.as_deref().map_err(|error| {
            CompilerError::new_no_location(format!(
                "cannot classify source `{path}` against root `{}`: cannot canonicalize root: {error}",
                root.display()
            ))
        })?;
        let source_path = Path::new(path);
        let canonical = match canonical_under(source_path, canonical_root, plain_dirs) {
            Some(canonical) => canonical,
            None => match source_path.canonicalize() {
                Ok(canonical) => canonical,
                Err(_) if !tagged => return Ok(true),
                Err(error) => {
                    return Err(CompilerError::new_no_location(format!(
                        "cannot classify source `{path}` against root `{}`: cannot canonicalize source: {error}",
                        root.display()
                    )));
                }
            },
        };
        match canonical.strip_prefix(canonical_root) {
            Ok(relative) => Ok(!SourceFiles::is_test_only_relative_path(relative)),
            Err(_) if !tagged => Ok(true),
            Err(_) => Err(CompilerError::new_no_location(format!(
                "production source `{path}` is outside package root `{}`",
                root.display()
            ))),
        }
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

    #[cfg(unix)]
    #[test]
    fn production_classification_resolves_symlinks_and_package_roots() {
        let temp_root = std::env::temp_dir().join(format!(
            "rock_core_source_files_links_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp_root);
        let project_root = temp_root.join("project");
        std::fs::create_dir_all(project_root.join("tests")).unwrap();
        std::fs::create_dir_all(project_root.join("src")).unwrap();
        for path in ["tests/helper.rock", "src/main.rock"] {
            std::fs::write(project_root.join(path), "fn main() {}\n").unwrap();
        }
        std::os::unix::fs::symlink("tests", project_root.join("linked")).unwrap();
        std::os::unix::fs::symlink("../src", project_root.join("tests/production")).unwrap();
        let project_root = project_root.canonicalize().unwrap();
        let through_link = project_root.join("linked/helper.rock");
        let out_of_tests = project_root.join("tests/production/main.rock");
        let plain = project_root.join("src/main.rock");

        let package_root = project_root.join("tests");
        let mut sources = SourceFiles::empty();
        sources.package_roots = vec![(0, project_root.join("src")), (7, package_root.clone())];
        let filter = sources.production_filter(&project_root);
        let keeps = |path: &Path| {
            filter
                .keeps(&source(path.to_str().unwrap()), &mut Default::default())
                .unwrap()
        };
        assert!(!keeps(&through_link));
        assert!(keeps(&out_of_tests));
        assert!(keeps(&plain));

        let package = PackageTag {
            id: 7,
            self_name: "pkg".to_string(),
            root: package_root.to_str().unwrap().to_string(),
            identity_storage: IdentityStorage::default(),
        };
        let mut owned = source(through_link.to_str().unwrap());
        owned.package = Some(package);
        assert!(filter.keeps(&owned, &mut HashSet::new()).unwrap());
        owned.file = source(out_of_tests.to_str().unwrap()).file;
        let error = filter.keeps(&owned, &mut HashSet::new()).unwrap_err();
        assert!(error.message.contains("outside package root"), "{error}");
        std::fs::remove_dir_all(temp_root).unwrap();
    }
}
