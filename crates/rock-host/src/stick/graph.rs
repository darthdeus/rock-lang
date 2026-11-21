//! The one package graph for a build.

use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rock_core::package_aliases::{NsAliases, PackageAliases};

use super::{StickConfig, dependency_matches_channel};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackageId(pub u32);

#[derive(Debug, Clone)]
pub struct PackageEdge {
    pub alias: String,
    pub declared_key: String,
    pub target: PackageId,
}

#[derive(Debug, Clone)]
pub struct PackageNode {
    pub id: PackageId,
    pub root: PathBuf,
    pub manifest_name: String,
    /// Manifest name with `-` normalized to `_`, case preserved:
    /// `name = "Demo"` yields `Demo`, never an invented `demo`.
    pub self_name: String,
    pub global_policy: rock_core::source_code::GlobalPolicy,
    pub identity_storage: rock_core::source_code::IdentityStorage,
    pub dependencies: Vec<PackageEdge>,
    pub source_groups: Vec<SourceGroup>,
}

impl PackageNode {
    pub fn tag(&self) -> rock_core::source_code::PackageTag {
        rock_core::source_code::PackageTag {
            id: self.id.0,
            self_name: self.self_name.clone(),
            identity_storage: self.identity_storage.clone(),
            root: self.root.to_string_lossy().into_owned(),
        }
    }

    pub fn stamp(&self, source: &mut rock_core::source_code::SourceFile) {
        source.package = Some(self.tag());
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
    load_order: Vec<PackageId>,
}

const RESERVED_ROOTS: [&str; 3] = ["std", "self", "super"];

struct Builder {
    packages: Vec<Option<PackageNode>>,
    by_root: HashMap<PathBuf, PackageId>,
    stack: Vec<(PathBuf, String)>,
    load_order: Vec<PackageId>,
}

impl Builder {
    fn node(&self, id: PackageId) -> &PackageNode {
        self.packages[id.0 as usize]
            .as_ref()
            .expect("dependency finished before its dependent validated edges")
    }

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
            let (dep_manifest_dir, package_name) = match dep {
                super::Dependency::Path(path) => (manifest_dir.join(path), None),
                super::Dependency::Detailed(detailed) => match &detailed.source {
                    super::DependencySource::Path(path) => (manifest_dir.join(path), None),
                    super::DependencySource::Package(name) => (
                        rock_stdlib::package_dir(name)
                            .map_err(|e| anyhow!("dependency `{key}`: {e}"))?,
                        Some(name),
                    ),
                },
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
            });
        }
        self.validate_dependency_aliases(config, &dependencies)?;

        self.stack.pop();
        self.load_order.push(id);
        self.packages[id.0 as usize] = Some(PackageNode {
            id,
            root,
            manifest_name: config.package.name.clone(),
            self_name: config.package.name.replace('-', "_"),
            global_policy: config.global_policy(),
            identity_storage: config.identity_storage(),
            dependencies,
            source_groups,
        });
        Ok(id)
    }

    fn validate_dependency_aliases(
        &self,
        config: &StickConfig,
        dependencies: &[PackageEdge],
    ) -> Result<()> {
        // `a-b` and `a_b` normalize alike: two packages behind one alias are ambiguous,
        // two aliases reaching one package are fine.
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

        Ok(())
    }

    fn finish(self) -> Result<PackageGraph> {
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

/// `-` becomes `_`, case is preserved. Rejects empty, non-identifier and reserved keys
/// so a manifest typo fails here.
pub(crate) fn normalize_dep_key(key: &str) -> Result<String> {
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
    pub(crate) fn find_from_dir(start_dir: &Path) -> Result<Option<(PathBuf, Self)>> {
        let Some((manifest, config)) = StickConfig::find_from_dir(start_dir)? else {
            return Ok(None);
        };
        let directory = manifest.parent().unwrap_or(&manifest);
        let channel = super::resolved_channel_for(&config);
        let graph = Self::build_with_channel(directory, &config, &channel)?;
        Ok(Some((directory.to_path_buf(), graph)))
    }

    pub fn build(manifest_dir: &Path) -> Result<PackageGraph> {
        let manifest_path = manifest_dir.join("stick.toml");
        let config = StickConfig::from_path(&manifest_path)?;
        let channel = super::resolved_channel_for(&config);
        Self::build_with_channel(manifest_dir, &config, &channel)
    }

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

    pub fn packages(&self) -> &[PackageNode] {
        &self.packages
    }

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

    /// Postorder (providers first, root last): the order source discovery loads packages in.
    pub fn load_order(&self) -> &[PackageId] {
        &self.load_order
    }

    /// Deepest package wins on nesting; callers resolve filesystem aliases first. An
    /// unresolvable path has no owner.
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
        self.package_of_canonical_file(&path)
    }

    fn package_of_canonical_file(&self, path: &Path) -> Option<PackageId> {
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

    /// Inactive source groups vanish before parsing and contribute nothing.
    pub fn includes_file(&self, owner: PackageId, path: &Path) -> bool {
        self.includes(owner, path, |path| self.package_of_file(path))
    }

    fn includes(
        &self,
        owner: PackageId,
        path: &Path,
        package_of: impl FnOnce(&Path) -> Option<PackageId>,
    ) -> bool {
        let node = self.package(owner);
        path.starts_with(&node.root)
            && package_of(path) == Some(owner)
            && !node
                .source_groups
                .iter()
                .any(|group| path.starts_with(&group.directory) && !group.active)
    }

    pub fn selected_rock_files(&self, owner: PackageId, directory: &Path) -> Result<Vec<PathBuf>> {
        Ok(self
            .select(owner, directory)?
            .into_iter()
            .map(|(walked, _)| walked)
            .collect())
    }

    pub fn selected_canonical_rock_files(
        &self,
        owner: PackageId,
        directory: &Path,
    ) -> Result<Vec<PathBuf>> {
        Ok(self
            .select(owner, directory)?
            .into_iter()
            .map(|(walked, canonical)| canonical.unwrap_or(walked))
            .collect())
    }

    /// Each selected file as walked, with its canonical path where that is
    /// another one. Resolving a path costs several system calls, and only a
    /// symlinked file needs it.
    fn select(
        &self,
        owner: PackageId,
        directory: &Path,
    ) -> Result<Vec<(PathBuf, Option<PathBuf>)>> {
        let mut selected = Vec::new();
        for file in crate::project::walk_rock_files(directory)? {
            let walked = PathBuf::from(file.path);
            let resolved = file
                .symlink
                .then(|| fs_err::canonicalize(&walked).ok())
                .flatten();
            let canonical = if file.symlink {
                resolved.as_deref()
            } else {
                Some(walked.as_path())
            };
            let package_of = |_: &Path| self.package_of_canonical_file(canonical?);
            if self.includes(owner, &walked, package_of) {
                selected.push((walked, resolved));
            }
        }
        Ok(selected)
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

    const GROUP_CHANNELS: &str = "\n[language]\nmodules = \"v1\"\n\n\
        [build]\nchannels = [\"dev\", \"demo\"]\n";

    fn group_package(groups: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("app");
        for dir in ["dev_tools/inner", "demo"] {
            fs::create_dir_all(root.join(dir)).expect("create group dir");
        }
        for file in ["main.rock", "dev_tools/inner/probe.rock", "demo/intro.rock"] {
            fs::write(root.join(file), "fn f() {}\n").expect("write source");
        }
        write_manifest(&root, "app", &format!("{GROUP_CHANNELS}{groups}"));
        (tmp, root)
    }

    #[test]
    fn source_groups_keep_their_package_and_follow_the_channel() {
        let (_tmp, root) = group_package(
            "\n[source_groups]\ntools = { directory = \"dev_tools\", channels = [\"dev\"] }\n\
             intro = { directory = \"demo\", channels = [\"demo\"] }\n",
        );
        let config =
            StickConfig::new_from_str(&fs::read_to_string(root.join("stick.toml")).unwrap())
                .expect("manifest");
        for (channel, tools, intro) in [("dev", true, false), ("demo", false, true)] {
            let graph = PackageGraph::build_with_channel(&root, &config, channel).expect("graph");
            let app = graph.root();
            let probe = fs::canonicalize(root.join("dev_tools/inner/probe.rock")).unwrap();
            let demo = fs::canonicalize(root.join("demo/intro.rock")).unwrap();
            let main = fs::canonicalize(root.join("main.rock")).unwrap();
            assert_eq!(graph.package_of_file(&probe), Some(app));
            assert_eq!(graph.package_of_file(&demo), Some(app));
            assert!(graph.includes_file(app, &main), "{channel}");
            assert_eq!(graph.includes_file(app, &probe), tools, "{channel}");
            assert_eq!(graph.includes_file(app, &demo), intro, "{channel}");
        }
    }

    #[test]
    fn nested_source_groups_are_rejected() {
        let (_tmp, root) = group_package(
            "\n[source_groups]\ntools = { directory = \"dev_tools\", channels = [\"dev\"] }\n\
             probe = { directory = \"dev_tools/inner\", channels = [\"demo\"] }\n",
        );
        let err = PackageGraph::build(&root).expect_err("overlapping groups must reject");
        assert!(
            err.to_string().contains("overlaps another source group"),
            "{err}"
        );
    }

    #[test]
    fn channel_scoped_dependencies_resolve_only_on_matching_channels() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("app");
        write_manifest(&tmp.path().join("leaf"), "leaf", "");
        write_manifest(&tmp.path().join("tool"), "tool", "");
        write_manifest(
            &tmp.path().join("mid"),
            "mid",
            "\n[dependencies]\nleaf = { path = \"../leaf\", channels = [\"dev\"] }\n",
        );
        write_manifest(
            &root,
            "app",
            "\n[dependencies]\nmid = \"../mid\"\ntool = { path = \"../tool\", channels = [\"dev\"] }\n",
        );
        let config =
            StickConfig::new_from_str(&fs::read_to_string(root.join("stick.toml")).unwrap())
                .expect("manifest");
        for (channel, want) in [
            ("dev", vec!["app", "leaf", "mid", "tool"]),
            ("steam", vec!["app", "mid"]),
        ] {
            let graph = PackageGraph::build_with_channel(&root, &config, channel).expect("graph");
            let mut names: Vec<&str> = graph
                .packages()
                .iter()
                .map(|p| p.manifest_name.as_str())
                .collect();
            names.sort();
            assert_eq!(names, want, "{channel}");
        }
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
    #[cfg(unix)]
    fn selection_resolves_symlinked_files_and_follows_no_symlinked_directory() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("app");
        let inner = root.join("lib");
        write_manifest(&inner, "inner", "\n[language]\nmodules = \"v1\"\n");
        write_manifest(
            &root,
            "app",
            "\n[language]\nmodules = \"v1\"\n\n[dependencies]\ninner = { path = \"lib\" }\n",
        );
        fs::create_dir_all(root.join("sub")).expect("sub directory");
        for file in ["main.rock", "library.rock", "sub/deep.rock", "lib/lib.rock"] {
            fs::write(root.join(file), "fn f() {}\n").expect("source");
        }
        let alias = tmp.path().join("alias");
        symlink(&root, &alias).expect("symlinked package root");
        symlink(inner.join("lib.rock"), root.join("linked.rock")).expect("link into dependency");
        symlink(root.join("library.rock"), root.join("same.rock")).expect("link within package");
        symlink(root.join("sub"), root.join("linked_dir")).expect("symlinked directory");
        symlink("absent.rock", root.join("dangling.rock")).expect("dangling source link");
        symlink("loop.txt", root.join("loop.txt")).expect("non-source loop");
        fs::write(tmp.path().join("outside.rock"), "fn outside() {}\n").unwrap();
        symlink(tmp.path().join("outside.rock"), root.join("outside.rock"))
            .expect("link outside package");

        let graph = PackageGraph::build(&alias).expect("graph through symlink");
        let real = fs::canonicalize(&root).expect("canonical root");
        let walked = graph
            .selected_rock_files(graph.root(), &alias)
            .expect("selection");
        let expected: Vec<PathBuf> = ["library.rock", "main.rock", "same.rock", "sub/deep.rock"]
            .iter()
            .map(|file| real.join(file))
            .collect();
        assert_eq!(walked, expected);

        let canonical = graph
            .selected_canonical_rock_files(graph.root(), &alias)
            .expect("canonical selection");
        let resolved: Vec<PathBuf> = walked
            .iter()
            .map(|path| fs::canonicalize(path).expect("canonical source"))
            .collect();
        assert_eq!(canonical, resolved);
        assert_eq!(canonical[2], real.join("library.rock"));

        let bad_source = real.join("loop.rock");
        symlink("loop.rock", &bad_source).expect("source loop");
        let error = graph
            .selected_canonical_rock_files(graph.root(), &alias)
            .expect_err("source metadata failure must surface");
        assert!(
            format!("{error:#}").contains(bad_source.to_str().unwrap()),
            "{error:#}"
        );
    }
}
