//! The one package graph for a build.
//!
//! Resolves a project's stick.toml dependency tree recursively and
//! manifest-relative (each dependency's own manifest resolves its deps
//! relative to its own directory), dedupes packages by canonicalized root
//! path, and validates the v1/legacy edges (`allow_legacy`), alias
//! normalization (`-` to `_`), and file-ownership overlaps. Every consumer
//! that used to resolve dependencies on its own (CLI source discovery, the
//! compilation-unit loader, watch, global directory metadata) reads facts from
//! here instead of re-walking manifests.

use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rock_core::package_aliases::{NsAliases, PackageAliases};
use rock_core::source_code::Dialect;

use super::{StickConfig, dependency_matches_channel};

/// Dense package id, stable within one graph build. Ids are assigned in
/// DFS discovery order (root = 0, then each dependency subtree in declared
/// key order); [`PackageGraph::load_order`] is the order sources load in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackageId(pub u32);

/// One dependency edge in the graph: the declaring package's view of a
/// provider. `alias` is the normalized source name (`-` to `_`, case
/// preserved); `declared_key` is the key as written in the consumer's
/// manifest, kept for legacy metadata parity (global directory package strings).
#[derive(Debug, Clone)]
pub struct PackageEdge {
    pub alias: String,
    pub declared_key: String,
    pub target: PackageId,
    pub allow_legacy: bool,
}

#[derive(Debug, Clone)]
pub struct PackageNode {
    pub id: PackageId,
    /// Canonicalized package root (the directory containing stick.toml).
    pub root: PathBuf,
    pub manifest_name: String,
    /// Manifest name with `-` normalized to `_`, case preserved:
    /// `name = "Demo"` yields `Demo`, never an invented `demo`.
    pub self_name: String,
    pub dialect: Dialect,
    /// The package's mutable-global policy (`[language] globals`).
    pub global_policy: rock_core::source_code::GlobalPolicy,
    /// The package's identity-storage migration state
    /// (`[language] identity_storage`).
    pub identity_storage: rock_core::source_code::IdentityStorage,
    /// Channel-filtered edges, sorted by declared key.
    pub dependencies: Vec<PackageEdge>,
    /// Source directories selected before parsing, under this same owner.
    pub source_groups: Vec<SourceGroup>,
}

impl PackageNode {
    /// The tag every source file of this package is stamped with.
    pub fn tag(&self) -> rock_core::source_code::PackageTag {
        rock_core::source_code::PackageTag {
            id: self.id.0,
            self_name: self.self_name.clone(),
            identity_storage: self.identity_storage.clone(),
        }
    }

    /// Stamp this package's facts onto one of its source files.
    pub fn stamp(&self, source: &mut rock_core::source_code::SourceFile) {
        source.package = Some(self.tag());
        source.dialect = self.dialect;
        source.global_policy = self.global_policy;
    }
}

#[derive(Debug, Clone)]
pub struct SourceGroup {
    pub name: String,
    pub directory: PathBuf,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct PackageGraph {
    packages: Vec<PackageNode>,
    /// Postorder dependency order (providers first), root last: the order
    /// source discovery loads packages in.
    load_order: Vec<PackageId>,
}

/// Reserved path roots no dependency alias may claim.
const RESERVED_ROOTS: [&str; 3] = ["std", "self", "super"];

struct Builder {
    /// Filled in postorder; indexed by PackageId with `None` at ids whose
    /// subtree is still being visited.
    packages: Vec<Option<PackageNode>>,
    by_root: HashMap<PathBuf, PackageId>,
    /// `(canonical root, manifest name)` of the packages currently being
    /// visited, for cycle chain reporting.
    stack: Vec<(PathBuf, String)>,
    load_order: Vec<PackageId>,
}

impl Builder {
    fn node(&self, id: PackageId) -> &PackageNode {
        self.packages[id.0 as usize]
            .as_ref()
            .expect("dependency finished before its dependent validated edges")
    }

    /// Visit one package: dedupe by canonical root, recurse into its
    /// channel-filtered dependencies (sorted by declared key), then assign
    /// its id. Postorder ids make `load_order` a prefix of the id sequence.
    fn visit(
        &mut self,
        manifest_dir: &Path,
        config: &StickConfig,
        channel: &str,
    ) -> Result<PackageId> {
        let root = fs_err::canonicalize(manifest_dir).map_err(|e| {
            anyhow!(
                "failed to resolve package root `{}`: {e}",
                manifest_dir.display()
            )
        })?;
        if let Some((_, name)) = self
            .stack
            .iter()
            .find(|(root_on_stack, _)| root_on_stack == &root)
        {
            let mut chain: Vec<&str> = self.stack.iter().map(|(_, n)| n.as_str()).collect();
            chain.push(name.as_str());
            return Err(anyhow!(
                "circular package dependency detected: {}",
                chain.join(" -> "),
            ));
        }
        if let Some(&id) = self.by_root.get(&root) {
            return Ok(id);
        }

        let id = PackageId(self.packages.len() as u32);
        self.packages.push(None);
        self.by_root.insert(root.clone(), id);
        self.stack.push((root.clone(), config.package.name.clone()));

        let dialect = config.dialect();
        let mut source_groups = Vec::new();
        let mut declared_groups: Vec<_> = config.source_groups.iter().collect();
        declared_groups.sort_by_key(|(name, _)| *name);
        for (name, group) in declared_groups {
            let relative = Path::new(&group.directory);
            if relative.as_os_str().is_empty()
                || !relative
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
            {
                return Err(anyhow!(
                    "source group `{name}` directory must be a nonempty package-relative path"
                ));
            }
            if group.channels.is_empty() {
                return Err(anyhow!(
                    "source group `{name}` must select at least one channel"
                ));
            }
            for selected in &group.channels {
                if !config
                    .build
                    .as_ref()
                    .is_some_and(|b| b.channels.contains(selected))
                {
                    return Err(anyhow!(
                        "source group `{name}` names undeclared channel `{selected}`"
                    ));
                }
            }
            let directory = fs_err::canonicalize(root.join(relative)).map_err(|e| {
                anyhow!(
                    "source group `{name}` directory `{}` cannot be resolved: {e}",
                    group.directory
                )
            })?;
            if directory == root || !directory.starts_with(&root) || !directory.is_dir() {
                return Err(anyhow!(
                    "source group `{name}` directory must be a directory below its package root"
                ));
            }
            if source_groups.iter().any(|other: &SourceGroup| {
                directory.starts_with(&other.directory) || other.directory.starts_with(&directory)
            }) {
                return Err(anyhow!(
                    "source group `{name}` overlaps another source group"
                ));
            }
            source_groups.push(SourceGroup {
                name: name.clone(),
                directory,
                active: group.channels.iter().any(|selected| selected == channel),
            });
        }
        let mut dependencies = Vec::new();
        let mut direct: Vec<(&String, &super::Dependency)> = config
            .dependencies
            .iter()
            .filter(|(_, dep)| dependency_matches_channel(dep, channel))
            .collect();
        direct.sort_by(|a, b| a.0.cmp(b.0));
        for (key, dep) in direct {
            let (dep_manifest_dir, dep_allow_legacy, package_name) = match dep {
                super::Dependency::Path(path) => (manifest_dir.join(path), false, None),
                super::Dependency::Detailed(detailed) => {
                    let (dir, name) = match &detailed.source {
                        super::DependencySource::Path(path) => (manifest_dir.join(path), None),
                        super::DependencySource::Package(name) => (
                            rock_stdlib::package_dir(name)
                                .map_err(|e| anyhow!("dependency `{key}`: {e}"))?,
                            Some(name),
                        ),
                    };
                    (dir, detailed.allow_legacy, name)
                }
            };
            let dep_manifest = dep_manifest_dir.join("stick.toml");
            if !dep_manifest.exists() {
                return Err(anyhow!(
                    "dependency `{key}` at path `{}` does not contain a stick.toml",
                    dep_manifest_dir.display()
                ));
            }
            let dep_config = StickConfig::from_path(&dep_manifest)
                .with_context(|| format!("dependency `{key}` at `{}`", dep_manifest.display()))?;
            if let Some(name) = package_name {
                if dep_config.package.name != *name {
                    return Err(anyhow!(
                        "package `{name}` at `{}` declares name `{}`",
                        dep_manifest.display(),
                        dep_config.package.name
                    ));
                }
            }
            let target = self.visit(&dep_manifest_dir, &dep_config, channel)?;
            dependencies.push(PackageEdge {
                alias: normalize_dep_key(key)?,
                declared_key: key.clone(),
                target,
                allow_legacy: dep_allow_legacy,
            });
        }
        dependencies.sort_by(|a, b| a.declared_key.cmp(&b.declared_key));
        self.validate_edges(&root, config, dialect, &dependencies)?;

        self.stack.pop();
        self.load_order.push(id);
        self.packages[id.0 as usize] = Some(PackageNode {
            id,
            root,
            manifest_name: config.package.name.clone(),
            self_name: config.package.name.replace('-', "_"),
            dialect,
            global_policy: config.global_policy(),
            identity_storage: config.identity_storage(),
            dependencies,
            source_groups,
        });
        Ok(id)
    }

    fn validate_edges(
        &self,
        root: &Path,
        config: &StickConfig,
        dialect: Dialect,
        dependencies: &[PackageEdge],
    ) -> Result<()> {
        // Normalized aliases must be unique within one package's dependency
        // table: `a-b` and `a_b` would otherwise be two indistinguishable
        // root bindings. Two aliases reaching one package are
        // fine; two packages behind one normalized alias are not.
        let mut seen: HashMap<&str, &str> = HashMap::new();
        for edge in dependencies {
            if let Some(first_key) = seen.get(edge.alias.as_str()) {
                let other = self.node(edge.target).manifest_name.clone();
                let same_target = dependencies
                    .iter()
                    .any(|e| e.declared_key == *first_key && e.target == edge.target);
                let detail = if same_target {
                    format!("both point at `{other}`; drop one or rename the keys")
                } else {
                    format!(
                        "they point at different packages (`{first_key}` also resolves here, `{other}` is distinct)"
                    )
                };
                return Err(anyhow!(
                    "package `{}` declares colliding dependency keys `{first_key}` and `{}` (normalize to `{}`): {detail}",
                    config.package.name,
                    edge.declared_key,
                    edge.alias,
                ));
            }
            seen.insert(edge.alias.as_str(), edge.declared_key.as_str());
        }

        let _ = root;

        // A v1 consumer of a legacy provider needs the explicit opt-in; the
        // flag on a v1 provider is a removable warning so its upgrade does
        // not force simultaneous manifest edits in every consumer.
        for edge in dependencies {
            let target = self.node(edge.target);
            if dialect == Dialect::V1 && target.dialect == Dialect::Legacy && !edge.allow_legacy {
                return Err(anyhow!(
                    "package `{}` (language.modules = \"v1\") depends on legacy package `{}`; \
                     add `allow_legacy = true` to the `{}` dependency if the legacy boundary is intended",
                    config.package.name,
                    target.manifest_name,
                    edge.declared_key,
                ));
            }
            if dialect == Dialect::V1 && target.dialect == Dialect::V1 && edge.allow_legacy {
                eprintln!(
                    "rock: warning: package `{}`: `allow_legacy` on dependency `{}` is redundant; \
                     `{}` already selects modules = \"v1\" and the flag can be removed",
                    config.package.name, edge.declared_key, target.manifest_name,
                );
            }
        }
        Ok(())
    }

    /// Postorder the visited packages and check the file-ownership overlaps
    /// that a mixed graph would introduce.
    fn finish(self) -> Result<PackageGraph> {
        // A legacy package's discovery includes every file under its root,
        // including any nested package's sources. A nested v1 package also
        // owns those files, so the graph would compile one physical source
        // as two owners with different rule sets. Pure-legacy
        // nesting keeps today's silent inclusion.
        for outer in self.packages.iter().flatten() {
            if outer.dialect != Dialect::Legacy {
                continue;
            }
            for inner in self.packages.iter().flatten() {
                if inner.id != outer.id
                    && inner.dialect == Dialect::V1
                    && inner.root.starts_with(&outer.root)
                {
                    return Err(anyhow!(
                        "file ownership overlap: legacy package `{}` contains v1 package `{}` ({}); \
                         a mixed graph must not compile the same source under two rule sets",
                        outer.manifest_name,
                        inner.manifest_name,
                        inner.root.display(),
                    ));
                }
            }
        }
        Ok(PackageGraph {
            packages: self
                .packages
                .into_iter()
                .map(|slot| slot.expect("every visited package finished"))
                .collect(),
            load_order: self.load_order,
        })
    }
}

/// Normalize a dependency key into a source alias: `-` to `_`, case
/// preserved. Rejects empty keys, non-identifier spellings, and reserved
/// roots, so a manifest typo fails here instead of inventing an unusable
/// namespace name.
pub fn normalize_dep_key(key: &str) -> Result<String> {
    let alias = key.replace('-', "_");
    let mut chars = alias.chars();
    let valid = match chars.next() {
        Some(c) => {
            (c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    };
    if !valid {
        return Err(anyhow!(
            "dependency key `{key}` normalizes to invalid source identifier `{alias}`"
        ));
    }
    if RESERVED_ROOTS.contains(&alias.as_str()) {
        return Err(anyhow!(
            "dependency key `{key}` resolves to reserved root `{alias}`"
        ));
    }
    Ok(alias)
}

impl PackageGraph {
    /// Build from the project directory that contains the root stick.toml,
    /// reading the invocation's resolved channel for edge filtering.
    pub fn build(manifest_dir: &Path) -> Result<PackageGraph> {
        let manifest_path = manifest_dir.join("stick.toml");
        let config = StickConfig::from_path(&manifest_path)?;
        let channel = super::resolved_channel_for(&config);
        Self::build_with_channel(manifest_dir, &config, &channel)
    }

    /// Build with the manifest already loaded and the channel selected, so
    /// callers that resolved the manifest once do not resolve it twice.
    pub fn build_with_channel(
        manifest_dir: &Path,
        config: &StickConfig,
        channel: &str,
    ) -> Result<PackageGraph> {
        let mut builder = Builder {
            packages: Vec::new(),
            by_root: HashMap::new(),
            stack: Vec::new(),
            load_order: Vec::new(),
        };
        builder.visit(manifest_dir, config, channel)?;
        builder.finish()
    }

    pub fn root(&self) -> PackageId {
        self.packages[0].id
    }

    pub fn package(&self, id: PackageId) -> &PackageNode {
        &self.packages[id.0 as usize]
    }

    /// All packages in id order.
    pub fn packages(&self) -> &[PackageNode] {
        &self.packages
    }

    /// Every package's self name and declared dependency aliases, keyed by
    /// package id: the table bone's namespace resolver reads. The batch
    /// compile and the incremental session both build from this one method.
    pub fn ns_aliases(&self) -> NsAliases {
        NsAliases {
            packages: self
                .packages
                .iter()
                .map(|node| PackageAliases {
                    id: node.id.0,
                    self_name: ustr::Ustr::from(node.self_name.as_str()),
                    aliases: node
                        .dependencies
                        .iter()
                        .map(|edge| (ustr::Ustr::from(edge.alias.as_str()), edge.target.0))
                        .collect(),
                })
                .collect(),
        }
    }

    /// Postorder dependency order (providers first), root last: the order
    /// source discovery loads packages in.
    pub fn load_order(&self) -> &[PackageId] {
        &self.load_order
    }

    /// The package whose root contains `path`, deepest first on nesting.
    /// Resolve filesystem aliases before comparing against canonical roots.
    /// An unresolvable path has no owner.
    pub fn package_of_file(&self, path: &Path) -> Option<PackageId> {
        let path = match fs_err::canonicalize(path) {
            Ok(path) => path,
            Err(error) => {
                log::debug!(
                    "cannot resolve package owner for {}: {error}",
                    path.display()
                );
                return None;
            }
        };
        let mut best: Option<(PackageId, usize)> = None;
        for node in &self.packages {
            if path.starts_with(&node.root) {
                let len = node.root.as_os_str().len();
                if best.map_or(true, |(_, best_len)| len > best_len) {
                    best = Some((node.id, len));
                }
            }
        }
        best.map(|(id, _)| id)
    }

    /// Whether `owner` contributes `path` to the compile set. V1 packages own
    /// exactly the files under their root that no nested package owns; legacy
    /// packages keep today's inclusion (everything under the root, including
    /// nested package directories).
    pub fn owns_file(&self, owner: PackageId, path: &Path) -> bool {
        let node = self.package(owner);
        if !path.starts_with(&node.root) {
            return false;
        }
        if node.dialect == Dialect::V1 {
            self.package_of_file(path) == Some(owner)
        } else {
            true
        }
    }

    /// Selected source membership. Inactive groups vanish before parsing and
    /// never contribute definitions, initializers or lifecycle hooks.
    pub fn includes_file(&self, owner: PackageId, path: &Path) -> bool {
        if !self.owns_file(owner, path) {
            return false;
        }
        !self
            .package(owner)
            .source_groups
            .iter()
            .any(|group| path.starts_with(&group.directory) && !group.active)
    }

    pub fn selected_rock_files(&self, owner: PackageId, directory: &Path) -> Result<Vec<PathBuf>> {
        Ok(crate::project::find_rock_files_at_path(directory)?
            .into_iter()
            .map(PathBuf::from)
            .filter(|path| self.includes_file(owner, path))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_manifest(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).expect("create package dir");
        fs::write(
            dir.join("stick.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n{body}"),
        )
        .expect("write manifest");
    }

    #[test]
    fn ns_aliases_keep_package_ids_self_names_and_declared_aliases() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("app");
        let lib = tmp.path().join("lib");
        write_manifest(&lib, "shared-lib", "\n[language]\nmodules = \"v1\"\n");
        write_manifest(
            &root,
            "app",
            "\n[language]\nmodules = \"v1\"\n\n[dependencies]\nmy-lib = { path = \"../lib\" }\n",
        );

        let graph = PackageGraph::build(&root).expect("graph builds");
        let aliases = graph.ns_aliases();
        let expected = NsAliases {
            packages: vec![
                PackageAliases {
                    id: 0,
                    self_name: "app".into(),
                    aliases: vec![("my_lib".into(), 1)],
                },
                PackageAliases {
                    id: 1,
                    self_name: "shared_lib".into(),
                    aliases: vec![],
                },
            ],
        };
        assert_eq!(aliases, expected);
    }

    #[test]
    fn unknown_language_modules_value_is_load_error() {
        let err = StickConfig::new_from_str(
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[language]\nmodules = \"v2\"\n",
        )
        .expect_err("unknown modules value must reject");
        assert!(
            err.to_string()
                .contains("unknown `[language] modules` value")
        );
    }

    #[test]
    fn language_modules_v1_and_legacy_select_dialect() {
        let v1 = StickConfig::new_from_str(
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[language]\nmodules = \"v1\"\n",
        )
        .expect("v1 manifest parses");
        assert_eq!(v1.dialect(), Dialect::V1);

        let legacy = StickConfig::new_from_str(
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[language]\nmodules = \"legacy\"\n",
        )
        .expect("legacy manifest parses");
        assert_eq!(legacy.dialect(), Dialect::Legacy);

        let absent = StickConfig::new_from_str("[package]\nname = \"x\"\nversion = \"0.1.0\"\n")
            .expect("manifest without [language] parses");
        assert_eq!(absent.dialect(), Dialect::Legacy);
    }

    #[test]
    fn v1_consumer_of_legacy_dependency_requires_allow_legacy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("app");
        let lib = tmp.path().join("lib");
        write_manifest(&lib, "legacy-lib", "");
        write_manifest(
            &root,
            "app",
            "\n[language]\nmodules = \"v1\"\n\n[dependencies]\nlib = { path = \"../lib\" }\n",
        );

        let err = PackageGraph::build(&root).expect_err("missing allow_legacy must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("allow_legacy"),
            "error should name the fix: {msg}"
        );
        assert!(
            msg.contains("legacy-lib"),
            "error should name the provider: {msg}"
        );

        write_manifest(
            &root,
            "app",
            "\n[language]\nmodules = \"v1\"\n\n[dependencies]\nlib = { path = \"../lib\", allow_legacy = true }\n",
        );
        let graph = PackageGraph::build(&root).expect("flagged edge builds");
        let lib_node = graph
            .packages()
            .iter()
            .find(|p| p.manifest_name == "legacy-lib")
            .expect("provider in graph");
        assert_eq!(lib_node.dialect, Dialect::Legacy);
    }

    #[test]
    fn allow_legacy_on_v1_provider_builds_without_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("app");
        let lib = tmp.path().join("lib");
        write_manifest(&lib, "v1-lib", "\n[language]\nmodules = \"v1\"\n");
        write_manifest(
            &root,
            "app",
            "\n[language]\nmodules = \"v1\"\n\n[dependencies]\nlib = { path = \"../lib\", allow_legacy = true }\n",
        );
        let graph = PackageGraph::build(&root).expect("redundant flag is a warning, not an error");
        assert_eq!(graph.packages().len(), 2);
    }

    #[test]
    fn alias_normalization_collision_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("app");
        let p1 = tmp.path().join("p1");
        let p2 = tmp.path().join("p2");
        write_manifest(&p1, "p1", "");
        write_manifest(&p2, "p2", "");
        write_manifest(
            &root,
            "app",
            "\n[dependencies]\na-b = { path = \"../p1\" }\na_b = { path = \"../p2\" }\n",
        );
        let err = PackageGraph::build(&root).expect_err("normalized collision must reject");
        assert!(
            err.to_string().contains("a_b"),
            "error names the alias: {err}"
        );
    }

    #[test]
    fn reserved_alias_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("app");
        let std_like = tmp.path().join("fake-std");
        write_manifest(&std_like, "fake-std", "");
        write_manifest(
            &root,
            "app",
            "\n[dependencies]\nstd = { path = \"../fake-std\" }\n",
        );
        let err = PackageGraph::build(&root).expect_err("reserved root must reject");
        assert!(
            err.to_string().contains("reserved"),
            "error explains: {err}"
        );
    }

    #[test]
    fn two_aliases_one_package_share_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("app");
        let lib = tmp.path().join("some-other-name");
        write_manifest(&lib, "real-lib", "");
        write_manifest(
            &root,
            "app",
            "\n[dependencies]\ngeo = { path = \"../some-other-name\" }\ngeometry = { path = \"../some-other-name\" }\n",
        );
        let graph = PackageGraph::build(&root).expect("aliases to one package are fine");
        assert_eq!(graph.packages().len(), 2, "one root + one shared package");
        let targets: Vec<_> = graph
            .package(graph.root())
            .dependencies
            .iter()
            .map(|e| e.target)
            .collect();
        assert_eq!(
            targets[0], targets[1],
            "both aliases resolve to one package"
        );
        let shared = graph.package(targets[0]);
        assert_eq!(shared.manifest_name, "real-lib");
        assert_eq!(shared.self_name, "real_lib");
    }

    #[test]
    fn transitive_dependency_resolves_manifest_relative() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("app");
        let child = tmp.path().join("child");
        let grandchild = child.join("nested").join("grandchild");
        write_manifest(&grandchild, "grandchild", "");
        write_manifest(
            &child,
            "child",
            "\n[dependencies]\ng = { path = \"nested/grandchild\" }\n",
        );
        write_manifest(
            &root,
            "app",
            "\n[dependencies]\nc = { path = \"../child\" }\n",
        );

        let graph = PackageGraph::build(&root).expect("transitive build");
        assert_eq!(graph.packages().len(), 3, "root + child + grandchild");
        let names: Vec<&str> = graph
            .load_order()
            .iter()
            .map(|id| graph.package(*id).manifest_name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["grandchild", "child", "app"],
            "providers load first"
        );
    }

    #[test]
    fn cycle_reports_the_edge_chain() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        write_manifest(&a, "a", "\n[dependencies]\nb = { path = \"../b\" }\n");
        write_manifest(&b, "b", "\n[dependencies]\na = { path = \"../a\" }\n");

        let err = PackageGraph::build(&a).expect_err("cycle must reject");
        let msg = err.to_string();
        assert!(msg.contains("circular"), "error names the cycle: {msg}");
        assert!(
            msg.contains("a -> b -> a"),
            "chain is the edge path, not just the letter a: {msg}"
        );
    }

    #[test]
    fn legacy_three_package_load_order_is_sorted_postorder_root_last() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("app");
        let b_child = tmp.path().join("b-child");
        let a_child = tmp.path().join("a-child");
        write_manifest(&b_child, "b-child", "");
        write_manifest(&a_child, "a-child", "");
        // Declared b first; resolution orders by key, so a-child loads first.
        write_manifest(
            &root,
            "app",
            "\n[dependencies]\nb = { path = \"../b-child\" }\na = { path = \"../a-child\" }\n",
        );

        let graph = PackageGraph::build(&root).expect("build");
        let names: Vec<&str> = graph
            .load_order()
            .iter()
            .map(|id| graph.package(*id).manifest_name.as_str())
            .collect();
        assert_eq!(names, vec!["a-child", "b-child", "app"]);
    }

    #[test]
    fn nested_v1_package_inside_legacy_root_is_overlap_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("app");
        let inner = root.join("inner");
        write_manifest(&inner, "inner-v1", "\n[language]\nmodules = \"v1\"\n");
        write_manifest(
            &root,
            "app",
            "\n[dependencies]\ninner = { path = \"inner\" }\n",
        );

        let err = PackageGraph::build(&root).expect_err("mixed ownership overlap must reject");
        assert!(
            err.to_string().contains("inner-v1"),
            "error names the nested package: {err}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn package_of_file_resolves_symlinked_paths_before_selecting_owner() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("app");
        let inner = root.join("lib");
        write_manifest(&inner, "inner", "\n[language]\nmodules = \"v1\"\n");
        write_manifest(
            &root,
            "app",
            "\n[language]\nmodules = \"v1\"\n\n[dependencies]\ninner = { path = \"lib\" }\n",
        );
        fs::write(root.join("main.rock"), "fn main() {}\n").expect("root source");
        fs::write(root.join("library.rock"), "fn sibling() {}\n").expect("sibling source");
        fs::write(inner.join("lib.rock"), "fn inner() {}\n").expect("dependency source");
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).expect("symlinked package root");
        std::os::unix::fs::symlink(inner.join("lib.rock"), root.join("linked.rock"))
            .expect("symlink into dependency");

        let graph = PackageGraph::build(&alias).expect("graph through symlink");
        let root_id = graph.root();
        let inner_id = graph.package(root_id).dependencies[0].target;
        for path in [alias.join("main.rock"), alias.join("library.rock")] {
            assert_eq!(
                graph.package_of_file(&path),
                Some(root_id),
                "{}",
                path.display()
            );
        }
        for path in [alias.join("lib/lib.rock"), alias.join("linked.rock")] {
            assert_eq!(
                graph.package_of_file(&path),
                Some(inner_id),
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn v1_ownership_excludes_nested_package_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("app");
        let inner = root.join("inner");
        write_manifest(&inner, "inner-v1", "\n[language]\nmodules = \"v1\"\n");
        write_manifest(
            &root,
            "app",
            "\n[language]\nmodules = \"v1\"\n\n[dependencies]\ninner = { path = \"inner\" }\n",
        );
        fs::write(root.join("main.rock"), "fn main() {}\n").expect("write root source");
        fs::create_dir_all(&inner).expect("inner dir");
        fs::write(inner.join("lib.rock"), "fn inner_fn() {}\n").expect("write inner source");

        let graph = PackageGraph::build(&root).expect("pure-v1 nesting builds");
        let root_id = graph.root();
        let main = fs::canonicalize(root.join("main.rock")).expect("canonical main");
        let inner_lib = fs::canonicalize(inner.join("lib.rock")).expect("canonical inner");
        assert!(graph.owns_file(root_id, &main));
        assert!(
            !graph.owns_file(root_id, &inner_lib),
            "inner's file is not the root's"
        );
        let inner_id = graph
            .package_of_file(&inner_lib)
            .expect("inner owns its file");
        assert_eq!(graph.package(inner_id).manifest_name, "inner-v1");
    }
}
