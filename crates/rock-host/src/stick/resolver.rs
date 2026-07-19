use super::*;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct DependencyResolver {
    root_path: PathBuf,
    resolved: HashMap<String, ResolvedDependency>,
}

#[derive(Debug, Clone)]
pub struct ResolvedDependency {
    pub name: String,
    pub version: String,
    pub path: PathBuf,
    pub dependencies: Vec<String>,
}

impl DependencyResolver {
    pub fn new(root_path: PathBuf) -> Self {
        Self {
            root_path,
            resolved: HashMap::new(),
        }
    }

    pub fn resolve(&mut self, config: &StickConfig) -> Result<Vec<ResolvedDependency>> {
        // Start with direct dependencies. `config.dependencies` is a HashMap, so
        // iterate its keys sorted -- the resolution/concatenation order flows into
        // `ws.modules` (type_id numbering + function-chunk emission order), and an
        // unsorted HashMap walk makes the emitted `.lua` non-reproducible run-to-run
        // (per-process RandomState) and across builds (unfuck-backlog #58).
        let mut direct: Vec<(&String, &Dependency)> = config.dependencies.iter().collect();
        direct.sort_by(|a, b| a.0.cmp(b.0));
        for (name, dep) in direct {
            self.resolve_dependency(name.clone(), dep, false)?;
        }

        // Collect all resolved dependencies in dependency order. Seed the DFS from
        // the resolved names sorted (same reproducibility rationale as above).
        let mut result = Vec::new();
        let mut visited = HashSet::new();
        let mut visiting = HashSet::new();

        let mut names: Vec<String> = self.resolved.keys().cloned().collect();
        names.sort();
        for name in &names {
            self.collect_ordered(name, &mut result, &mut visited, &mut visiting)?;
        }

        Ok(result)
    }

    fn resolve_dependency(
        &mut self,
        name: String,
        dep: &Dependency,
        _is_transitive: bool,
    ) -> Result<()> {
        if self.resolved.contains_key(&name) {
            // Already resolved
            return Ok(());
        }

        let path = match dep {
            Dependency::Path(path) => path.as_str(),
            Dependency::Detailed(detailed) => detailed.path.as_str(),
        };

        let resolved = self.resolve_path_dependency(&name, Path::new(path))?;
        self.resolved.insert(name, resolved);
        Ok(())
    }

    fn resolve_path_dependency(&self, name: &str, path: &Path) -> Result<ResolvedDependency> {
        let absolute_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root_path.join(path)
        };

        // Check if the path exists and has a stick.toml
        let stick_path = absolute_path.join("stick.toml");
        if !stick_path.exists() {
            anyhow::bail!(
                "Dependency '{}' at path '{}' does not contain a stick.toml",
                name,
                absolute_path.display()
            );
        }

        // Load the dependency's stick.toml to get its version and transitive deps
        let dep_config = StickConfig::from_path(&stick_path)?;

        // TODO: Resolve transitive dependencies
        let transitive_deps = dep_config.dependencies.keys().cloned().collect();

        Ok(ResolvedDependency {
            name: name.to_string(),
            version: dep_config.package.version().to_string(),
            path: absolute_path,
            dependencies: transitive_deps,
        })
    }

    fn collect_ordered(
        &self,
        name: &str,
        result: &mut Vec<ResolvedDependency>,
        visited: &mut HashSet<String>,
        visiting: &mut HashSet<String>,
    ) -> Result<()> {
        if visited.contains(name) {
            return Ok(());
        }

        if visiting.contains(name) {
            anyhow::bail!("Circular dependency detected involving '{}'", name);
        }

        visiting.insert(name.to_string());

        if let Some(dep) = self.resolved.get(name) {
            // First, process all dependencies
            for dep_name in &dep.dependencies {
                self.collect_ordered(dep_name, result, visited, visiting)?;
            }

            // Then add this dependency
            result.push(dep.clone());
        }

        visiting.remove(name);
        visited.insert(name.to_string());

        Ok(())
    }
}

impl ResolvedDependency {
    pub fn to_lock_entry(&self) -> StickLockDependency {
        StickLockDependency {
            name: self.name.clone(),
            version: self.version.clone(),
            source: format!("path+file://{}", self.path.display()),
            checksum: None,
            dependencies: self.dependencies.clone(),
        }
    }
}