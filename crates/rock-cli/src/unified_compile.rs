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

/// Discover sources based on source specification
pub fn discover_sources(source_spec: &SourceSpec) -> Result<SourceFiles> {
    match source_spec {
        SourceSpec::Directory(project_dir) => {
            // Check for stick.toml to pick up project settings
            if let Some((config_path, stick_config)) = StickConfig::find_from_dir(project_dir)? {
                // Load project files first
                let mut sources = SourceFiles::from_dir_preprocessed(project_dir)?;

                // Resolve and load dependencies
                if !stick_config.dependencies.is_empty() {
                    let root_path = config_path.parent().unwrap_or(&config_path).to_path_buf();
                    let mut resolver = rock_host::stick::resolver::DependencyResolver::new(root_path);
                    let resolved_deps = resolver.resolve(&stick_config)?;

                    // Load files from each dependency
                    for dep in resolved_deps {
                        let dep_sources = SourceFiles::from_dir_preprocessed(&dep.path)?;
                        // Merge dependency sources into main sources, excluding script files
                        for file in dep_sources.files {
                            // Skip files with #script directive (test/example files in library dependencies)
                            if !file.borrow().is_script {
                                sources.files.push(file);
                            }
                        }

                    }
                }

                Ok(sources)
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

/// Build a package map (package_name, absolute_path) from the project's stick.toml.
/// The main project's own name and path are included first, followed by each dependency.
/// Returns an empty Vec if there is no stick.toml or no dependencies.
pub fn build_package_map(source_spec: &SourceSpec) -> Vec<(String, PathBuf)> {
    let project_dir = match source_spec {
        SourceSpec::Directory(dir) => dir.clone(),
        SourceSpec::Files(files) => {
            if let Some(f) = files.first() {
                f.parent().unwrap_or(f).to_path_buf()
            } else {
                return Vec::new();
            }
        }
    };

    // No stick.toml → empty map is the expected outcome. Any other
    // error (parse failure, IO) means the codegen's package grouping
    // will be wrong; surface to stderr the same way the resolver
    // failure below does. Don't propagate -- `discover_sources` ran
    // earlier and already propagated any real failures.
    let (config_path, stick_config) = match StickConfig::find_from_dir(&project_dir) {
        Ok(Some(pair)) => pair,
        Ok(None) => return Vec::new(),
        Err(e) => {
            log::warn!(target: "COMPILE",
                "build_package_map: stick.toml lookup failed: {}. \
                 Package grouping in _G.__ROCK_GLOBALS will be incomplete.",
                e
            );
            return Vec::new();
        }
    };

    let root_path = config_path.parent().unwrap_or(&config_path).to_path_buf();
    // Canonicalize to absolute path so it matches the absolute ModuleId paths in the assembler.
    let abs_root = root_path.canonicalize().unwrap_or_else(|_| root_path.clone());
    let mut map = Vec::new();

    // Main project first
    map.push((stick_config.package.name.to_lowercase(), abs_root.clone()));

    // Each resolved dependency. `discover_sources` runs the same
    // resolver earlier in the same compile and propagates errors, so
    // by the time we get here the resolve has already succeeded once.
    // If it fails now (transient FS race, stick.toml rewritten mid-
    // build), surface it -- the package map feeds _G.__ROCK_GLOBALS
    // grouping in codegen, and silently dropping deps would produce
    // a subtly-wrong runtime.
    if !stick_config.dependencies.is_empty() {
        let mut resolver = rock_host::stick::resolver::DependencyResolver::new(abs_root);
        match resolver.resolve(&stick_config) {
            Ok(resolved_deps) => {
                for dep in resolved_deps {
                    let abs_dep = dep.path.canonicalize().unwrap_or(dep.path);
                    map.push((dep.name, abs_dep));
                }
            }
            Err(e) => {
                log::warn!(target: "COMPILE",
                    "build_package_map: dependency resolver failed: {}. \
                     Package grouping in _G.__ROCK_GLOBALS will be incomplete.",
                    e
                );
            }
        }
    }

    map
}
