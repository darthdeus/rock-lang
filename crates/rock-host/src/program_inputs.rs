//! The package facts a program is checked with, loaded the same way for the
//! command line and the editor. A manifest that does not resolve is an error
//! for both: checking with an empty alias table instead would type-check a
//! different program from the one a build compiles.

use std::path::{Path, PathBuf};

use rock_core::CompilerError;
use rock_core::package_aliases::NsAliases;
use rock_core::source_code::PackageTag;

use crate::stick::graph::PackageGraph;

pub fn package_graph(manifest_dir: &Path) -> Result<Option<PackageGraph>, CompilerError> {
    if !manifest_dir.join("stick.toml").is_file() {
        return Ok(None);
    }
    PackageGraph::build(manifest_dir)
        .map(Some)
        .map_err(|error| {
            CompilerError::new_no_location(format!(
                "cannot build the package graph of `{}`: {error:#}",
                manifest_dir.display()
            ))
        })
}

pub fn ns_aliases(graph: Option<&PackageGraph>) -> NsAliases {
    graph.map(PackageGraph::ns_aliases).unwrap_or_default()
}

pub fn ns_aliases_for_sources<'a>(
    sources: impl IntoIterator<Item = (&'a str, Option<&'a PackageTag>)>,
) -> Result<NsAliases, CompilerError> {
    ns_aliases_for_selected_sources(sources, None)
}

pub fn ns_aliases_for_selected_sources<'a>(
    sources: impl IntoIterator<Item = (&'a str, Option<&'a PackageTag>)>,
    selecting_graph: Option<&NsAliases>,
) -> Result<NsAliases, CompilerError> {
    let mut root = None;
    for (file, package) in sources {
        let Some(package) = package else { continue };
        if package.id == 0 && root.is_none() {
            root = Some(file);
        }
    }
    let Some(root) = root else {
        return Ok(selecting_graph.cloned().unwrap_or_default());
    };
    match selecting_graph {
        Some(aliases) => Ok(aliases.clone()),
        None => ns_aliases_for_root_file(Path::new(root)),
    }
}

pub(crate) fn ns_aliases_for_root_file(root_file: &Path) -> Result<NsAliases, CompilerError> {
    let Some(manifest_dir) = manifest_dir_above(root_file) else {
        return Err(CompilerError::new_no_location(format!(
            "cannot find stick.toml for the root package source `{}`",
            root_file.display()
        )));
    };
    Ok(ns_aliases(package_graph(&manifest_dir)?.as_ref()))
}

fn manifest_dir_above(file: &Path) -> Option<PathBuf> {
    file.ancestors()
        .skip(1)
        .find(|dir| dir.join("stick.toml").is_file())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_with_no_manifest_has_no_graph() {
        let dir = tempfile::tempdir().unwrap();
        assert!(package_graph(dir.path()).unwrap().is_none());
    }

    #[test]
    fn a_manifest_that_does_not_resolve_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("stick.toml"),
            "[package]\nname = \"app\"\n\n[dependencies]\nmissing = { path = \"../missing\" }\n",
        )
        .unwrap();
        let error = package_graph(dir.path()).unwrap_err();
        assert!(error.message.contains("package graph"), "{}", error.message);
        let file = dir.path().join("src").join("main.rock");
        assert!(ns_aliases_for_root_file(&file).is_err());
    }

    #[test]
    fn standalone_modules_use_empty_aliases() {
        let aliases = ns_aliases_for_sources([("scratch.rock", None)]).unwrap();
        assert_eq!(aliases, NsAliases::default());
    }

    #[test]
    fn package_sources_without_a_root_file_use_empty_aliases() {
        let package = PackageTag {
            id: 1,
            self_name: "dependency".to_string(),
            identity_storage: Default::default(),
            root: "/dependency".to_string(),
        };
        let aliases = ns_aliases_for_sources([("/dependency/lib.rock", Some(&package))]).unwrap();
        assert_eq!(aliases, NsAliases::default());
    }

    #[test]
    fn a_root_file_with_no_manifest_above_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = ns_aliases_for_root_file(&dir.path().join("main.rock")).unwrap_err();
        assert!(error.message.contains("stick.toml"), "{}", error.message);
    }
}
