use std::path::Path;

use anyhow::Result;
use rock_core::SourceFiles;

use crate::source_files::SourceFilesExt;
use crate::stick::StickConfig;
use crate::stick::graph::{PackageGraph, PackageNode};

pub struct ProjectSources {
    pub sources: SourceFiles,
    pub graph: Option<PackageGraph>,
}

pub fn load_sources(path: &Path) -> Result<SourceFiles> {
    if path.is_dir() {
        load_sources_for_project(path)
    } else {
        load_sources_for_file(path)
    }
}

pub fn load_sources_for_file(file_path: impl AsRef<Path>) -> Result<SourceFiles> {
    Ok(load_file(file_path.as_ref())?.sources)
}

pub fn load_file(path: &Path) -> Result<ProjectSources> {
    match StickConfig::find_from_dir(path.parent().unwrap_or(path))? {
        Some((manifest_path, config)) => {
            let manifest_dir =
                fs_err::canonicalize(manifest_path.parent().unwrap_or(&manifest_path))?;
            load_package_sources(&manifest_dir, &manifest_dir, &config)
        }
        None => load_standalone_file(path),
    }
}

/// The root package loads only the files under `project_dir`, which may be a
/// subdirectory of its manifest root; dependencies always load whole.
pub fn load_sources_for_project(project_dir: &Path) -> Result<SourceFiles> {
    Ok(load_project(project_dir)?.sources)
}

pub fn load_project(project_dir: &Path) -> Result<ProjectSources> {
    if let Some((manifest_path, config)) = StickConfig::find_from_dir(project_dir)? {
        let manifest_dir = manifest_path.parent().unwrap_or(&manifest_path);
        return load_package_sources(manifest_dir, project_dir, &config);
    }
    let rock_files = crate::project::find_rock_files_at_path(project_dir)?;
    match rock_files.as_slice() {
        [file] => load_standalone_file(Path::new(file)),
        [] => Err(anyhow::anyhow!(
            "No .rock files found in directory: {}",
            project_dir.display()
        )),
        _ => Err(anyhow::anyhow!(
            "Directory contains {} .rock files but no stick.toml. Please specify a single file or add a stick.toml",
            rock_files.len()
        )),
    }
}

fn load_standalone_file(file: &Path) -> Result<ProjectSources> {
    Ok(ProjectSources {
        sources: SourceFiles::from_single_path(file)?,
        graph: None,
    })
}

/// Dependencies load before the root package because global initializers are
/// module-load effects: a library must be ready before its consumer's.
fn load_package_sources(
    manifest_dir: &Path,
    root_dir: &Path,
    config: &StickConfig,
) -> Result<ProjectSources> {
    let graph = PackageGraph::build_with_channel(
        manifest_dir,
        config,
        &crate::stick::resolved_channel_for(config),
    )?;
    let loaded: Vec<Result<SourceFiles>> = {
        use rayon::prelude::*;
        graph
            .load_order()
            .par_iter()
            .map(|id| {
                let load_dir = if *id == graph.root() {
                    root_dir
                } else {
                    graph.package(*id).root.as_path()
                };
                SourceFiles::from_canonical_paths(
                    &graph.selected_canonical_rock_files(*id, load_dir)?,
                )
            })
            .collect()
    };

    let mut files = Vec::new();
    for (id, package_sources) in graph.load_order().iter().zip(loaded) {
        let node = graph.package(*id);
        for file in package_sources?.files {
            node.stamp(&mut file.borrow_mut());
            files.push(file);
        }
    }
    let sources = SourceFiles {
        files,
        package_roots: graph
            .packages()
            .iter()
            .map(|node| (node.id.0, node.root.clone()))
            .collect(),
        ns_aliases: Some(graph.ns_aliases()),
    };
    Ok(ProjectSources {
        sources,
        graph: Some(graph),
    })
}

pub fn source_package<'g>(graph: &'g PackageGraph, path: &Path) -> Option<&'g PackageNode> {
    let owner = graph.package_of_file(path)?;
    graph
        .includes_file(owner, path)
        .then(|| graph.package(owner))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs_err::create_dir_all(path.parent().unwrap()).unwrap();
        fs_err::write(path, text).unwrap();
    }

    fn loaded(sources: &SourceFiles) -> Vec<(String, Option<u32>)> {
        sources
            .files
            .iter()
            .map(|file| {
                let file = file.borrow();
                (
                    file.file.to_id_path(),
                    file.package.as_ref().map(|tag| tag.id),
                )
            })
            .collect()
    }

    #[test]
    fn a_file_and_its_project_directory_load_the_same_package_sources() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs_err::canonicalize(dir.path()).unwrap();
        write(
            &root.join("app/stick.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n[dependencies]\nlib = { path = \"../lib\" }\n",
        );
        write(&root.join("app/main.rock"), "fn main() {}\n");
        write(
            &root.join("lib/stick.toml"),
            "[package]\nname = \"lib\"\nversion = \"0.1.0\"\n",
        );
        write(&root.join("lib/lib.rock"), "pub fn f() {}\n");

        let graph = crate::stick::graph::PackageGraph::build(&root.join("app")).unwrap();
        let from_dir = load_sources_for_project(&root.join("app")).unwrap();
        let from_file = load_sources_for_file(root.join("app/main.rock")).unwrap();

        let lib = root.join("lib/lib.rock").to_string_lossy().into_owned();
        let main = root.join("app/main.rock").to_string_lossy().into_owned();
        assert_eq!(loaded(&from_dir), [(lib, Some(1)), (main, Some(0))]);
        assert_eq!(loaded(&from_file), loaded(&from_dir));
        for sources in [&from_dir, &from_file] {
            assert_eq!(sources.ns_aliases, Some(graph.ns_aliases()));
            assert_eq!(sources.package_roots.len(), 2);
        }
    }
}
