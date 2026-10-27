use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use atomic_refcell::AtomicRefCell;
use rock_core::{SourceFile, SourceFiles};

use crate::source_files::{
    source_file_new, source_file_new_preprocessed, SourceFilesExt,
};

/// Represents different types of compilation units based on file directives
#[derive(Debug, Clone)]
pub enum CompilationUnit {
    /// Regular project compilation: all files without directives
    Project(Vec<PathBuf>),

    /// Script compilation: one file with optional package dependencies
    Script {
        file: PathBuf,
        used_packages: Vec<String>,
    },

    /// Test compilation: project + test-utils + one test file
    Test {
        project_files: Vec<PathBuf>,
        test_util_files: Vec<PathBuf>,
        test_file: PathBuf,
    },

    /// Library compilation: project + additional file
    Lib {
        lib_project_dir: PathBuf,
        file: PathBuf,
    },
}

/// Analyzes files in a directory and categorizes them by directive type
pub fn analyze_files_in_directory(dir_path: impl AsRef<Path>) -> Result<FileAnalysis> {
    let mut project_files = Vec::new();
    let mut script_files = Vec::new();
    let mut test_files = Vec::new();
    let mut test_util_files = Vec::new();

    let dir_path_ref = dir_path.as_ref();
    let rock_files = if let Some((manifest, config)) = crate::stick::StickConfig::find_from_dir(dir_path_ref)? {
        let root = manifest.parent().unwrap_or(&manifest);
        let graph = crate::stick::graph::PackageGraph::build_with_channel(
            root, &config, &crate::stick::resolved_channel_for(&config),
        )?;
        graph.selected_rock_files(graph.root(), dir_path_ref)?
            .into_iter().map(|path| path.to_string_lossy().into_owned()).collect()
    } else {
        crate::project::find_rock_files_at_path(dir_path_ref)?
    };

    // Check if this is a tests directory or a directory without a project
    let is_tests_directory = dir_path_ref
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name == "tests")
        .unwrap_or(false);

    let has_stick_toml = dir_path_ref.join("stick.toml").exists();

    for file_path in rock_files {
        let path = PathBuf::from(&file_path);
        let source_file = source_file_new(&path)?;

        if source_file.is_script {
            script_files.push(path);
        } else if source_file.is_test {
            test_files.push(path);
        } else if source_file.is_test_util {
            test_util_files.push(path);
        } else {
            // Handle files without directives based on context
            if is_tests_directory {
                // In tests/ directory, treat files without directives as test files
                test_files.push(path);
            } else if has_stick_toml {
                // In project directories, treat them as project files
                project_files.push(path);
            }
            // Otherwise, ignore files without directives
        }
    }

    Ok(FileAnalysis {
        project_files,
        script_files,
        test_files,
        test_util_files,
    })
}

/// Result of analyzing files in a directory
#[derive(Debug, Clone)]
pub struct FileAnalysis {
    pub project_files: Vec<PathBuf>,
    pub script_files: Vec<PathBuf>,
    pub test_files: Vec<PathBuf>,
    pub test_util_files: Vec<PathBuf>,
}

impl FileAnalysis {
    /// Get compilation unit for running/compiling the entire project
    pub fn project_compilation_unit(&self) -> CompilationUnit {
        CompilationUnit::Project(self.project_files.clone())
    }

    /// Get compilation unit for a specific test file
    pub fn test_compilation_unit(&self, test_file: PathBuf) -> CompilationUnit {
        CompilationUnit::Test {
            project_files: self.project_files.clone(),
            test_util_files: self.test_util_files.clone(),
            test_file,
        }
    }

}

/// Parse the lib directive from a file's contents
/// This is public so the LSP can reuse it
/// Supports both `//! lib:` and legacy `//! love-lib:` syntax
pub fn parse_lib_directive(contents: &str) -> Option<String> {
    // Look for //! lib: or //! love-lib: path directive at the beginning of the file
    for line in contents.lines().take(10) {
        // Only check first 10 lines
        let trimmed = line.trim_start();
        if trimmed.starts_with("//! lib:") || trimmed.starts_with("//! love-lib:") {
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() >= 2 {
                return Some(parts[1..].join(":").trim().to_string());
            }
        }
    }
    None
}

/// Resolve a lib path to the actual project directory
/// This is public so the LSP can reuse it
pub fn resolve_lib_path(lib_path: &str, file_dir: &Path) -> PathBuf {
    let lib_project_path = file_dir.join(lib_path);

    // If it's a stick.toml file, get its parent directory
    if lib_project_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == "stick.toml")
        .unwrap_or(false)
    {
        lib_project_path
            .parent()
            .unwrap_or(&lib_project_path)
            .to_path_buf()
    } else {
        lib_project_path
    }
}

/// Determines the appropriate compilation unit for a given file
pub fn determine_compilation_unit_for_file(file_path: impl AsRef<Path>) -> Result<CompilationUnit> {
    let path = file_path.as_ref();
    let source_file = source_file_new(path)?;

    // Check for lib directive first
    if let Some(lib_path) = parse_lib_directive(&source_file.contents) {
        // Resolve the lib path relative to the file's directory
        let file_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let lib_project_dir = resolve_lib_path(&lib_path, file_dir);

        return Ok(CompilationUnit::Lib {
            lib_project_dir,
            file: path.to_path_buf(),
        });
    }

    if source_file.is_script {
        Ok(CompilationUnit::Script {
            file: path.to_path_buf(),
            used_packages: source_file.used_packages.clone(),
        })
    } else if source_file.is_test {
        // For a test file, we need to find all project and test-util files
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let analysis = analyze_files_in_directory(dir)?;
        Ok(analysis.test_compilation_unit(path.to_path_buf()))
    } else if source_file.is_test_util {
        Err(anyhow::anyhow!("Cannot run test-util files directly. Test utilities are meant to be used by test files."))
    } else {
        // Regular file - find the project directory and compile the whole project
        if let Some((stick_path, _)) =
            crate::stick::StickConfig::find_from_dir(path.parent().unwrap_or(path))?
        {
            // Found stick.toml, get the project directory
            let project_dir = stick_path.parent().unwrap_or(&stick_path);
            let analysis = analyze_files_in_directory(project_dir)?;
            Ok(analysis.project_compilation_unit())
        } else {
            // No project found, just compile this file
            Ok(CompilationUnit::Project(vec![path.to_path_buf()]))
        }
    }
}

/// The owning package and source facts `graph` gives the file at the canonical
/// `path`: what the loader stamps on every file it loads, and what a host
/// declares for a file that enters an incremental session after its load
/// (the editor opening a new file). `None` when no package owns the path: a
/// standalone source, legacy and unowned.
pub fn source_facts(
    graph: &crate::stick::graph::PackageGraph,
    path: &Path,
) -> Option<(
    rock_core::source_code::PackageTag,
    rock_core::source_code::Dialect,
    rock_core::source_code::GlobalPolicy,
)> {
    let owner = graph.package_of_file(path)?;
    if !graph.includes_file(owner, path) {
        return None;
    }
    let node = graph.package(owner);
    Some((
        rock_core::source_code::PackageTag { id: node.id.0, self_name: node.self_name.clone() },
        node.dialect,
        node.global_policy,
    ))
}

/// Load SourceFiles based on a compilation unit
pub fn load_sources_for_compilation_unit(unit: &CompilationUnit) -> Result<SourceFiles> {
    match unit {
        CompilationUnit::Project(files) => {
            // Load stick.toml dependencies for project compilation
            let mut all_files = Vec::new();
            let mut graph = None;

            // Try to find stick.toml in the project directory
            if let Some(first_file) = files.first() {
                if let Some(project_dir) = first_file.parent() {
                    if let Some((stick_path, stick_config)) = crate::stick::StickConfig::find_from_dir(project_dir)? {
                        // Resolve every package through the one graph, so the
                        // LSP's file set matches a real compile on the same
                        // channel: recursive, manifest-relative, channel-filtered.
                        let channel = crate::stick::resolved_channel_for(&stick_config);
                        let stick_dir = stick_path.parent().unwrap_or(&stick_path);
                        let resolved = crate::stick::graph::PackageGraph::build_with_channel(
                            stick_dir,
                            &stick_config,
                            &channel,
                        )?;

                        for id in resolved.load_order() {
                            if *id == resolved.root() {
                                continue;
                            }
                            let node = resolved.package(*id);
                            let dep_files = resolved.selected_rock_files(*id, &node.root)?;

                            // Filter out script files
                            for path in dep_files {
                                let source = source_file_new(&path)?;
                                if !source.is_script {
                                    all_files.push(path);
                                }
                            }
                        }
                        graph = Some(resolved);
                    }
                }
            }

            // Module-load effects from dependency initializers must be
            // ready before a consuming project's global initializers.
            all_files.extend(files.clone());

            let mut sources = load_sources_from_paths(&all_files)?;

            // Stamp package and dialect facts from the graph onto every file
            // it owns, so downstream consumers (parser dialect selection,
            // module scoping) never infer ownership from inclusion order.
            if let Some(graph) = graph.as_ref() {
                sources.set_package_roots(
                    graph
                        .packages()
                        .iter()
                        .map(|node| (node.id.0, node.root.clone())),
                );
                for file in &sources.files {
                    let path = file.borrow().file.to_id_path();
                    if let Some((package, dialect, global_policy)) = source_facts(graph, Path::new(&path)) {
                        let mut f = file.borrow_mut();
                        f.package = Some(package);
                        f.dialect = dialect;
                        f.global_policy = global_policy;
                    }
                }
            }

            Ok(sources)
        }
        CompilationUnit::Script { file, used_packages } => {
            if used_packages.is_empty() {
                // No dependencies, just load the script file
                SourceFiles::from_single_path_preprocessed(file)
            } else {
                // Load the script file and all files from used packages
                let mut all_files = vec![];

                // First add files from dependencies
                for package_name in used_packages {
                    // Resolve package path relative to the script file's directory
                    let script_dir = file.parent().unwrap_or_else(|| Path::new("."));
                    let package_path = script_dir.join(package_name);

                    if package_path.exists() && package_path.is_dir() {
                        // Load all .rock files from the package directory
                        let package_files = crate::project::find_rock_files_at_path(&package_path)?;

                        // Filter out files marked with #script directive - they should not be loaded via #use
                        for package_file_path in package_files {
                            let path = PathBuf::from(&package_file_path);
                            let source = source_file_new(&path)?;
                            if !source.is_script {
                                all_files.push(path);
                            }
                        }
                    } else {
                        eprintln!("Warning: Package '{}' not found at {:?}", package_name, package_path);
                    }
                }

                // Add the script file itself
                all_files.push(file.clone());

                load_sources_from_paths(&all_files)
            }
        }
        CompilationUnit::Test {
            project_files,
            test_util_files,
            test_file,
        } => {
            let mut all_files = project_files.clone();
            all_files.extend(test_util_files.clone());
            all_files.push(test_file.clone());
            load_sources_from_paths(&all_files)
        }
        CompilationUnit::Lib {
            lib_project_dir,
            file,
        } => {
            // Load all library project files
            let mut sources = SourceFiles::from_dir_preprocessed(lib_project_dir)?;

            // Add the additional file
            let additional_file = source_file_new_preprocessed(file)?;
            sources.add_or_update_file(additional_file);

            Ok(sources)
        }
    }
}

/// Helper function to load SourceFiles from a list of paths.
///
/// Files starting with `//! @no_check` are silently skipped — they
/// exist on disk for non-compiler tooling (e.g. the bone test harness's
/// section-based fixtures), but the LSP / compiler shouldn't try to
/// type-check them. Without this, the bone fixtures fail with phantom
/// duplicate-`fn` errors because each section reuses the same name.
fn load_sources_from_paths(paths: &[PathBuf]) -> Result<SourceFiles> {
    let mut sources = Vec::new();

    for path in paths {
        let source_file: SourceFile = source_file_new_preprocessed(path)?;
        if has_no_check_pragma(&source_file.contents) {
            continue;
        }
        sources.push(Arc::new(AtomicRefCell::new(source_file)));
    }

    Ok(SourceFiles {
        files: sources,
        package_roots: Vec::new(),
    })
}

/// True iff the file's first non-blank line is `//! @no_check`.
fn has_no_check_pragma(src: &str) -> bool {
    for line in src.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return trimmed == "//! @no_check";
    }
    false
}

#[cfg(test)]
mod no_check_tests {
    use super::has_no_check_pragma;

    #[test]
    fn pragma_first_line() {
        assert!(has_no_check_pragma("//! @no_check\n//- foo\n"));
        assert!(has_no_check_pragma("   //! @no_check   \n"));
    }

    #[test]
    fn pragma_after_blank_lines() {
        assert!(has_no_check_pragma("\n\n//! @no_check\n"));
    }

    #[test]
    fn pragma_absent() {
        assert!(!has_no_check_pragma("fn f() {}\n"));
        assert!(!has_no_check_pragma("// regular comment\n//! @no_check\n"));
        assert!(!has_no_check_pragma("//! @other_pragma\n"));
    }
}
