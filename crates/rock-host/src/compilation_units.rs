use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use atomic_refcell::AtomicRefCell;
use rock_core::SourceFiles;

use crate::source_files::source_file_load;

fn project_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let is_tests_directory = dir.file_name().is_some_and(|name| name == "tests");
    if is_tests_directory || !dir.join("stick.toml").exists() {
        return Ok(Vec::new());
    }
    let Some((_, graph)) = crate::stick::graph::PackageGraph::find_from_dir(dir)? else {
        return Ok(Vec::new());
    };
    graph.selected_rock_files(graph.root(), dir)
}

pub fn load_sources_for_file(file_path: impl AsRef<Path>) -> Result<SourceFiles> {
    let path = file_path.as_ref();
    let files = match crate::stick::StickConfig::find_from_dir(path.parent().unwrap_or(path))? {
        Some((stick_path, _)) => project_files(stick_path.parent().unwrap_or(&stick_path))?,
        None => vec![path.to_path_buf()],
    };
    load_project_sources(&files, files.first().and_then(|file| file.parent()))
}

pub fn load_sources_for_project(project_dir: &Path) -> Result<SourceFiles> {
    load_project_sources(&project_files(project_dir)?, Some(project_dir))
}

pub fn source_package<'g>(
    graph: &'g crate::stick::graph::PackageGraph,
    path: &Path,
) -> Option<&'g crate::stick::graph::PackageNode> {
    let owner = graph.package_of_file(path)?;
    graph
        .includes_file(owner, path)
        .then(|| graph.package(owner))
}

fn load_project_sources(files: &[PathBuf], project_dir: Option<&Path>) -> Result<SourceFiles> {
    let mut all_files = Vec::new();
    let mut graph = None;

    if let Some(project_dir) = project_dir {
        if let Some((_, resolved)) = crate::stick::graph::PackageGraph::find_from_dir(project_dir)?
        {
            for id in resolved.load_order() {
                if *id == resolved.root() {
                    continue;
                }
                let node = resolved.package(*id);
                all_files.extend(resolved.selected_rock_files(*id, &node.root)?);
            }
            graph = Some(resolved);
        }
    }

    // Module-load effects from dependency initializers must be
    // ready before a consuming project's global initializers.
    all_files.extend_from_slice(files);

    let mut sources = load_sources_from_paths(&all_files)?;

    if let Some(graph) = graph.as_ref() {
        sources.set_package_roots(
            graph
                .packages()
                .iter()
                .map(|node| (node.id.0, node.root.clone())),
        );
        for file in &sources.files {
            let path = file.borrow().file.to_id_path();
            if let Some(node) = source_package(graph, Path::new(&path)) {
                node.stamp(&mut file.borrow_mut());
            }
        }
    }

    Ok(sources)
}

fn load_sources_from_paths(paths: &[PathBuf]) -> Result<SourceFiles> {
    let mut sources = Vec::new();

    for path in paths {
        sources.push(Arc::new(AtomicRefCell::new(source_file_load(path)?)));
    }

    Ok(SourceFiles {
        files: sources,
        package_roots: Vec::new(),
        ns_aliases: None,
    })
}
