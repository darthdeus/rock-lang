use crate::*;
use std::path::PathBuf;

/// Specification of sources to compile
#[derive(Clone)]
pub enum SourceSpec {
    /// Compile a directory (with optional stick.toml)
    Directory(PathBuf),
    /// Compile specific files
    Files(Vec<PathBuf>),
}

fn stamp_package(file: &Arc<AtomicRefCell<SourceFile>>, node: &PackageNode) {
    let mut f = file.borrow_mut();
    f.package = Some(rock_core::source_code::PackageTag {
        id: node.id.0,
        self_name: node.self_name.clone(),
    });
    f.dialect = node.dialect;
    f.global_policy = node.global_policy;
}

/// Discover sources based on source specification
pub fn discover_sources(source_spec: &SourceSpec) -> Result<SourceFiles> {
    match source_spec {
        SourceSpec::Directory(project_dir) => {
            // Check for stick.toml to pick up project settings
            if let Some((config_path, stick_config)) = StickConfig::find_from_dir(project_dir)? {
                let manifest_dir = config_path.parent().unwrap_or(&config_path).to_path_buf();
                let graph = PackageGraph::build_with_channel(
                    &manifest_dir,
                    &stick_config,
                    &rock_host::stick::resolved_channel_for(&stick_config),
                )?;
                let mut dependency_files = Vec::new();

                // Load each dependency package's files in graph load order
                // (providers before consumers), then the root package's files
                // last. Global initializers are module-load effects, so a
                // library must be ready before a consuming game's global
                // initializer on both emitters.
                for id in graph.load_order() {
                    let node = graph.package(*id);
                    // The root package loads from the INVOKED directory, which
                    // can be a subdirectory of its manifest root; dependencies
                    // always load their whole root.
                    let load_dir = if *id == graph.root() {
                        project_dir.clone()
                    } else {
                        node.root.clone()
                    };
                    let selected = graph.selected_rock_files(*id, &load_dir)?;
                    let package_sources = SourceFiles::from_multiple_paths(&selected, true)?;
                    for file in package_sources.files {
                        // Skip files with #script directive (test/example files in library dependencies)
                        if !file.borrow().is_script || *id == graph.root() {
                            stamp_package(&file, node);
                            dependency_files.push(file);
                        }
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
                })
            } else {
                // No stick.toml - check if there's exactly one .rock file
                let rock_files = rock_host::project::find_rock_files_at_path(project_dir)?;
                if rock_files.len() == 1 {
                    SourceFiles::from_single_path_preprocessed(&rock_files[0])
                } else if rock_files.is_empty() {
                    Err(anyhow::anyhow!("No .rock files found in directory: {}", project_dir.display()))
                } else {
                    Err(anyhow::anyhow!(
                        "Directory contains {} .rock files but no stick.toml. Please specify a single file or add a stick.toml",
                        rock_files.len()
                    ))
                }
            }
        }
        SourceSpec::Files(file_paths) => {
            // Check if this is a script file with dependencies
            if file_paths.len() == 1 {
                let compilation_unit = rock_host::compilation_units::determine_compilation_unit_for_file(&file_paths[0])?;
                rock_host::compilation_units::load_sources_for_compilation_unit(&compilation_unit)
            } else {
                SourceFiles::from_multiple_paths(file_paths, true)
            }
        }
    }
}
