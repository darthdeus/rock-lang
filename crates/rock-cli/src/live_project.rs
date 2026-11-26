use crate::lint_pass::Scope;
use crate::{Result, SourceFiles};
use bone_core::prelude::Instant;
use bone_semantic::hir::BodyOwner;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use rock_core::CompilerError;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum LiveUpdate {
    NoChange,
    BodyOnly {
        reinferred: Vec<(ModuleId, BodyOwner)>,
        span_edits: Vec<skull::instance_graph::TextEditDelta>,
        cook_ms: f64,
    },
    Full {
        cook_ms: f64,
    },
}

pub(crate) struct LiveProjectSnapshot {
    warm: std::sync::Arc<[skull_parser::Ast]>,
    comptime_inputs: bone_semantic::inference::ComptimeInputs,
    sources: SourceFiles,
    previous_contents: Vec<(usize, String)>,
}

struct Edit {
    index: usize,
    id: String,
    text: String,
}

pub struct LiveProject {
    session: bone_incremental::session::Session,
    dir: String,
    stdlib: rock_stdlib::StdlibSource,
    sources: SourceFiles,
    include_tests: bool,
    lint_cache: bone_lints::IncrementalLintCache,
    cache_needs_full_build: bool,
}

/// The sources a live project of `dir` compiles and accepts edits to: every
/// discovered file, or without `include_tests` the production build's.
pub(crate) fn project_sources(dir: &str, include_tests: bool) -> Result<SourceFiles> {
    let discovered = rock_host::compilation_units::load_sources_for_project(Path::new(dir))?;
    if include_tests {
        return Ok(discovered);
    }
    rock_host::source_files::production_only(&discovered, Path::new(dir))
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn load_live_sources(
    dir: &str,
    include_tests: bool,
    stdlib: &rock_stdlib::StdlibSource,
) -> Result<SourceFiles> {
    let mut sources = project_sources(dir, include_tests)?;
    anyhow::ensure!(
        !sources.files.is_empty(),
        "bone: project has no .rock files"
    );
    for file in rock_stdlib::embedded_files() {
        let id = rock_stdlib::stdlib_file_id(file.name);
        let (path, contents) = match stdlib {
            rock_stdlib::StdlibSource::Dir(dir) => {
                let path = dir.join(file.name);
                let contents = fs_err::read_to_string(&path)?;
                (path.to_string_lossy().into_owned(), contents)
            }
            rock_stdlib::StdlibSource::Embedded => {
                (file.display_path.to_string(), file.contents.to_string())
            }
        };
        sources.add_or_update_file(rock_core::source_file_from_path_and_contents(
            id, path, contents,
        ));
    }
    Ok(sources)
}

impl LiveProject {
    pub fn init(dir: &str, include_tests: bool) -> Result<Self> {
        let stdlib = rock_stdlib::stdlib_origin();
        let sources = load_live_sources(dir, include_tests, &stdlib)?;
        let mut project = Self {
            session: new_session(dir, &sources)?,
            dir: dir.to_string(),
            stdlib,
            sources,
            include_tests,
            lint_cache: Default::default(),
            cache_needs_full_build: false,
        };
        project.verdict("in initial build")?;
        Ok(project)
    }

    fn source_index(&self, id: &str) -> Option<usize> {
        self.sources
            .files
            .iter()
            .position(|source| source.borrow().file.to_id_path() == id)
    }

    fn edit_id(&self, path: &Path) -> String {
        if let rock_stdlib::StdlibSource::Dir(dir) = &self.stdlib {
            for &name in rock_stdlib::STDLIB_NAMES {
                if dir.join(name).canonicalize().is_ok_and(|p| p == path) {
                    return rock_stdlib::stdlib_file_id(name);
                }
            }
        }
        path.to_string_lossy().into_owned()
    }

    fn source_at_path(&self, canonical: &Path) -> Option<(String, usize)> {
        let path = canonical.to_string_lossy();
        if let Some(index) = self.source_index(&path) {
            return Some((path.into_owned(), index));
        }
        let id = self.edit_id(canonical);
        self.source_index(&id).map(|index| (id, index))
    }

    fn snapshot(&self, edits: &[Edit]) -> LiveProjectSnapshot {
        LiveProjectSnapshot {
            warm: self.session.warm.clone(),
            comptime_inputs: self.session.ws.comptime_inputs.clone(),
            sources: self.sources.clone(),
            previous_contents: edits
                .iter()
                .map(|edit| {
                    (
                        edit.index,
                        self.sources.files[edit.index].borrow().contents.clone(),
                    )
                })
                .collect(),
        }
    }

    fn restore(&mut self, snapshot: LiveProjectSnapshot) -> Result<()> {
        self.sources = snapshot.sources;
        for (index, contents) in snapshot.previous_contents {
            self.sources.files[index].borrow_mut().contents = contents;
        }
        // Rollback uses the package facts of the last good build: the current
        // manifest may be the input that made the candidate fail.
        self.session = bone_incremental::session::Session::new(
            snapshot.warm,
            crate::bone_backend::session_files(&self.sources),
            self.sources
                .ns_aliases
                .as_ref()
                .unwrap_or(&Default::default()),
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "{}",
                crate::skull_backend::render_project_err(&self.sources, &e)
            )
        })?;
        self.session.ws.comptime_inputs = snapshot.comptime_inputs;
        // Fresh inference has a new generation even for sources that rolled
        // back to the same text. Existing MIR plans hold the old type tables.
        self.cache_needs_full_build = true;
        self.verdict("after failed publication rollback")
    }

    pub(crate) fn finish_publication<T>(
        &mut self,
        snapshot: LiveProjectSnapshot,
        result: Result<T>,
        edit: &str,
        restore_cache: impl FnOnce(&Workspace) -> std::result::Result<(), CompilerError>,
    ) -> Result<T> {
        match result {
            Ok(output) => Ok(output),
            Err(error) => {
                self.restore(snapshot).map_err(|restore| {
                    anyhow::anyhow!(
                        "skull publication rollback failed after {edit}: {restore}; original error: {error}"
                    )
                })?;
                restore_cache(self.workspace()).map_err(|restore| {
                    anyhow::anyhow!(
                        "skull cache rollback failed after {edit}: {restore}; original error: {error}"
                    )
                })?;
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn recompile(&mut self, changed: &[PathBuf]) -> Result<LiveUpdate> {
        self.recompile_for_publication(changed)
            .map(|(update, _snapshot)| update)
    }

    pub(crate) fn recompile_for_publication(
        &mut self,
        changed: &[PathBuf],
    ) -> Result<(LiveUpdate, LiveProjectSnapshot)> {
        let started = Instant::now();
        let edits: Option<Vec<_>> = changed
            .iter()
            .map(|path| {
                let path = fs_err::canonicalize(path).ok()?;
                let (id, index) = self.source_at_path(&path)?;
                let text = fs_err::read_to_string(&path).ok()?;
                Some(Edit { index, id, text })
            })
            .collect();
        if let Some(edits) = edits.filter(|edits| !edits.is_empty()) {
            return self.transaction(edits, "compile error", |project, edits| {
                project.update(edits, started)
            });
        }
        let sources = load_live_sources(&self.dir, self.include_tests, &self.stdlib)?;
        self.transaction(Vec::new(), "compile error", |project, _| {
            project.rebuild(sources, started)
        })
    }

    fn transaction(
        &mut self,
        edits: Vec<Edit>,
        failure: &str,
        change: impl FnOnce(&mut Self, Vec<Edit>) -> Result<LiveUpdate>,
    ) -> Result<(LiveUpdate, LiveProjectSnapshot)> {
        let snapshot = self.snapshot(&edits);
        match change(self, edits) {
            Ok(update) => Ok((update, snapshot)),
            Err(error) => {
                self.restore(snapshot).map_err(|restore| anyhow::anyhow!(
                    "live project rollback failed after {failure}: {restore}; original error: {error}"
                ))?;
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn apply_edit(&mut self, path: &Path, new_text: &str) -> Result<LiveUpdate> {
        self.apply_edit_for_publication(path, new_text)
            .map(|(update, _snapshot)| update)
    }

    pub(crate) fn apply_edit_for_publication(
        &mut self,
        path: &Path,
        new_text: &str,
    ) -> Result<(LiveUpdate, LiveProjectSnapshot)> {
        let started = Instant::now();
        let canonical = std::fs::canonicalize(path).map_err(|e| {
            anyhow::anyhow!("apply_edit: cannot canonicalize `{}`: {e}", path.display())
        })?;
        let (id, index) = self.source_at_path(&canonical).ok_or_else(|| {
            anyhow::anyhow!(
                "apply_edit: `{}` is not a discovered source of `{}`",
                canonical.display(),
                self.dir
            )
        })?;
        let edit = Edit {
            index,
            id,
            text: new_text.to_string(),
        };
        self.transaction(vec![edit], "in-memory edit error", |project, edits| {
            project.update(edits, started)
        })
    }

    pub(crate) fn source_text(&self, path: &Path) -> Option<String> {
        let canonical = std::fs::canonicalize(path).ok()?;
        let (_, index) = self.source_at_path(&canonical)?;
        Some(self.sources.files[index].borrow().contents.clone())
    }

    pub fn workspace(&self) -> &Workspace {
        self.session.workspace()
    }

    pub fn sources(&self) -> &SourceFiles {
        &self.sources
    }

    pub fn dir(&self) -> &str {
        &self.dir
    }

    pub(crate) fn cache_rebuilt(&mut self) {
        self.cache_needs_full_build = false;
    }

    pub fn verdict(&mut self, context: &str) -> Result<()> {
        self.verdict_with_change(context, &bone_lints::LintCacheUpdate::full())
    }

    fn verdict_with_change(
        &mut self,
        context: &str,
        change: &bone_lints::LintCacheUpdate,
    ) -> Result<()> {
        let mut errors = self.session.diagnostics();
        let kind = if errors.is_empty() {
            errors = crate::lint_pass::run_build_lints_cached(
                &mut self.lint_cache,
                self.session.workspace(),
                &self.sources,
                Path::new(&self.dir),
                if self.include_tests {
                    Scope::WholeProject
                } else {
                    Scope::ProductionOnly
                },
                change,
            );
            "lint"
        } else {
            "type"
        };
        if errors.is_empty() {
            return Ok(());
        }
        for error in &errors {
            eprintln!(
                "{}",
                crate::skull_backend::render_project_err(&self.sources, error)
            );
        }
        anyhow::bail!("skull: {} {kind} error(s) {context}", errors.len())
    }

    fn rebuild(&mut self, sources: SourceFiles, started: Instant) -> Result<LiveUpdate> {
        self.sources = sources;
        self.lint_cache.clear();
        self.session = new_session(&self.dir, &self.sources)?;
        self.verdict("during rebuild")?;
        Ok(LiveUpdate::Full {
            cook_ms: started.elapsed().as_secs_f64() * 1000.0,
        })
    }

    fn update(&mut self, edits: Vec<Edit>, started: Instant) -> Result<LiveUpdate> {
        use bone_incremental::session::UpdateTier;
        let mut reinferred = Vec::new();
        let mut span_edits = Vec::new();
        let mut lint_changed_files = Vec::new();
        let mut full = self.cache_needs_full_build;
        let mut changed = false;
        let mut stdlib_changed = false;
        for edit in &edits {
            let mut source = self.sources.files[edit.index].borrow_mut();
            stdlib_changed |=
                rock_stdlib::is_stdlib_module_file(&edit.id) && source.contents != edit.text;
            source.contents.clone_from(&edit.text);
        }
        if stdlib_changed {
            return self.rebuild(self.sources.clone(), started);
        }
        let render_error = |error: CompilerError| {
            anyhow::anyhow!(
                "{}",
                crate::skull_backend::render_project_err(&self.sources, &error)
            )
        };
        for Edit { id, text, .. } in &edits {
            if rock_stdlib::is_stdlib_module_file(id) {
                continue;
            }
            let update = match self.session.update_file(id, text) {
                Ok(update) => update,
                Err(_) if edits.len() > 1 => {
                    // One intermediate state may be invalid even though the
                    // watcher batch is valid as a unit.
                    self.session
                        .replace_files(
                            edits
                                .iter()
                                .map(|edit| (edit.id.as_str(), edit.text.as_str())),
                        )
                        .map_err(render_error)?;
                    full = true;
                    changed = true;
                    break;
                }
                Err(error) => return Err(render_error(error)),
            };
            reinferred.extend(update.reinferred.iter().copied());
            match update.tier {
                UpdateTier::NoChange => continue,
                UpdateTier::BodyOnly => {
                    lint_changed_files.push(ustr::Ustr::from(id.as_str()));
                    span_edits.extend(update.text_edit);
                }
                UpdateTier::Interface | UpdateTier::Scope | UpdateTier::FullUser => full = true,
            }
            changed = true;
        }
        if changed
            && !self
                .session
                .ws
                .comptime_inputs
                .source_fingerprints
                .is_empty()
        {
            let inputs = crate::comptime::resolve_checked_inputs(
                &self.session.ws,
                self.session.ws.comptime_inputs.clone(),
                |file| crate::bone_backend::checked_source_text(&self.sources, &self.session, file),
            )?;
            if inputs.source_fingerprints.keys().any(|name| {
                inputs.vars.get(name).map(|value| &value.value)
                    != self
                        .session
                        .ws
                        .comptime_inputs
                        .vars
                        .get(name)
                        .map(|value| &value.value)
            }) {
                full = true;
            }
            self.session.ws.comptime_inputs = inputs;
        }
        let lint_change = if full {
            bone_lints::LintCacheUpdate::full()
        } else {
            bone_lints::LintCacheUpdate::body_only(lint_changed_files, reinferred.iter().copied())
        };
        self.verdict_with_change("during rebuild", &lint_change)?;
        if !changed {
            return Ok(LiveUpdate::NoChange);
        }
        let cook_ms = started.elapsed().as_secs_f64() * 1000.0;
        if full {
            Ok(LiveUpdate::Full { cook_ms })
        } else {
            Ok(LiveUpdate::BodyOnly {
                reinferred,
                span_edits,
                cook_ms,
            })
        }
    }
}

fn new_session(dir: &str, sources: &SourceFiles) -> Result<bone_incremental::session::Session> {
    crate::bone_backend::bone_session_for_emission(sources, Path::new(dir)).map_err(|error| {
        anyhow::anyhow!(
            "{}",
            crate::skull_backend::render_project_err(sources, &error)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{LiveProject, LiveUpdate};

    #[test]
    fn a_nested_manifest_does_not_replace_the_selecting_package() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("stick.toml"),
            "[package]\nname = \"app\"\ntype = \"bin\"\n",
        )
        .unwrap();
        let nested = root.join("src");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("stick.toml"), "invalid manifest\n").unwrap();
        std::fs::write(nested.join("main.rock"), "fn main() {}\n").unwrap();

        LiveProject::init(root.to_str().unwrap(), false)
            .expect("the manifest that selected the sources supplies their package facts");
    }

    #[test]
    fn malformed_manifest_keeps_the_last_good_session_and_allows_recovery() {
        let (dir, main, _a, mut project) = v1_live_project();
        let manifest = dir.path().join("stick.toml");
        let valid_manifest = std::fs::read_to_string(&manifest).unwrap();
        let previous_source = project.source_text(&main).unwrap();
        std::fs::write(
            &manifest,
            format!("unknown_reload_key = true\n{valid_manifest}"),
        )
        .unwrap();

        for _ in 0..2 {
            let error = project
                .recompile_for_publication(std::slice::from_ref(&manifest))
                .err()
                .expect("a malformed manifest must reject the candidate")
                .to_string();
            assert!(
                error.contains("unknown key `unknown_reload_key`"),
                "{error}"
            );
            assert!(!error.contains("rollback failed"), "{error}");
            assert_eq!(
                project.source_text(&main).as_deref(),
                Some(previous_source.as_str())
            );
            assert!(diagnostic_messages(&project).is_empty());
        }

        let error = project
            .apply_edit(&main, "fn main() { missing() }\n")
            .unwrap_err()
            .to_string();
        assert!(!error.contains("rollback failed"), "{error}");
        assert_eq!(
            project.source_text(&main).as_deref(),
            Some(previous_source.as_str())
        );
        project
            .apply_edit(&main, "fn main() { print(a::bump(2)) }\n")
            .expect("the last good package facts must still resolve v1 names");

        std::fs::write(&manifest, valid_manifest).unwrap();
        assert!(matches!(
            project.recompile(std::slice::from_ref(&manifest)).unwrap(),
            LiveUpdate::Full { .. }
        ));
    }

    #[test]
    fn build_var_commands_run_once_for_incremental_updates() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rock");
        let count = dir.path().join("count");
        std::fs::write(
            dir.path().join("stick.toml"),
            format!(
                "[package]\nname = \"cadence\"\n[build]\n[build.vars]\nCOUNT = {{ cmd = \"n=$(cat {} 2>/dev/null || echo 0); echo $((n + 1)) > {}; echo counted\" }}\n",
                count.display(), count.display()
            ),
        )
        .unwrap();
        std::fs::write(
            &source,
            "fn value() -> int { 1 }\nfn main() { print(build_var(\"COUNT\")) }\n",
        )
        .unwrap();

        let mut project = LiveProject::init(dir.path().to_str().unwrap(), false).unwrap();
        assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "1");

        std::fs::write(
            &source,
            "fn value() -> int { 2 }\nfn main() { print(build_var(\"COUNT\")) }\n",
        )
        .unwrap();
        project.recompile(std::slice::from_ref(&source)).unwrap();
        assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "1");

        project.recompile(&[]).unwrap();
        assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "2");
    }

    fn diagnostic_messages(project: &LiveProject) -> Vec<String> {
        project
            .session
            .diagnostics()
            .iter()
            .map(|d| d.message.clone())
            .collect()
    }

    fn v1_live_project() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        LiveProject,
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("stick.toml"),
            "[package]\nname = \"livev1\"\ntype = \"bin\"\n\n[language]\nmodules = \"v1\"\n",
        )
        .unwrap();
        let main = dir.path().join("main.rock");
        std::fs::write(&main, "fn main() {\n    print(\"n={a::bump(1)}\")\n}\n").unwrap();
        let a = dir.path().join("a.rock");
        std::fs::write(&a, "mod a;\n\npub fn bump(n: int) -> int {\n    n + 1\n}\n").unwrap();
        let project = LiveProject::init(dir.path().to_str().unwrap(), false)
            .expect("a v1 package must build through a session");
        (dir, main, a, project)
    }
}
