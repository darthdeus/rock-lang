use crate::*;
use std::path::Path;

pub(crate) fn discover_sources(path: &Path) -> Result<SourceFiles> {
    if path.is_dir() {
        return discover_project_sources(path);
    }
    rock_host::compilation_units::load_sources_for_file(path)
}

pub(crate) fn discover_project_sources(project_dir: &Path) -> Result<SourceFiles> {
    // Check for stick.toml to pick up project settings
    if let Some((config_path, stick_config)) = StickConfig::find_from_dir(project_dir)? {
        let manifest_dir = config_path.parent().unwrap_or(&config_path).to_path_buf();
        let graph = PackageGraph::build_with_channel(
            &manifest_dir,
            &stick_config,
            &rock_host::stick::resolved_channel_for(&stick_config),
        )?;
        let mut dependency_files = Vec::new();

        let loaded: Vec<Result<SourceFiles>> = {
            use rayon::prelude::*;
            graph
                .load_order()
                .par_iter()
                .map(|id| {
                    let load_dir = if *id == graph.root() {
                        project_dir
                    } else {
                        graph.package(*id).root.as_path()
                    };
                    let selected = graph.selected_canonical_rock_files(*id, load_dir)?;
                    SourceFiles::from_canonical_paths(&selected)
                })
                .collect()
        };

        for (id, package_sources) in graph.load_order().iter().zip(loaded) {
            let node = graph.package(*id);
            let package_sources = package_sources?;
            for file in package_sources.files {
                node.stamp(&mut file.borrow_mut());
                dependency_files.push(file);
            }
        }

        let package_roots = graph
            .packages()
            .iter()
            .map(|node| (node.id.0, node.root.clone()))
            .collect();
        Ok(SourceFiles {
            files: dependency_files,
            package_roots,
            ns_aliases: Some(graph.ns_aliases()),
        })
    } else {
        // No stick.toml - check if there's exactly one .rock file
        let rock_files = rock_host::project::find_rock_files_at_path(project_dir)?;
        if rock_files.len() == 1 {
            SourceFiles::from_single_path(&rock_files[0])
        } else if rock_files.is_empty() {
            Err(anyhow::anyhow!(
                "No .rock files found in directory: {}",
                project_dir.display()
            ))
        } else {
            Err(anyhow::anyhow!(
                "Directory contains {} .rock files but no stick.toml. Please specify a single file or add a stick.toml",
                rock_files.len()
            ))
        }
    }
}
