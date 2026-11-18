use crate::{Result, SourceFiles};
use bone_core::prelude::Instant;
use bone_semantic::hir::BodyOwner;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use rock_core::CompilerError;
use std::path::{Path, PathBuf};

pub(crate) enum LiveWatchError {
    Discovery(anyhow::Error),
    SourceSelection(anyhow::Error),
    Session(SourceFiles, CompilerError),
    Update(SourceFiles, Vec<CompilerError>),
    Diagnostics(SourceFiles, Vec<CompilerError>),
    Lints(SourceFiles, Vec<CompilerError>),
}

fn watch_sources(dir: &str, is_game: bool) -> std::result::Result<SourceFiles, LiveWatchError> {
    let discovered = crate::unified_compile::discover_project_sources(Path::new(dir))
        .map_err(LiveWatchError::Discovery)?;
    if is_game {
        rock_host::source_files::production_only(&discovered, Path::new(dir))
            .map_err(|error| LiveWatchError::SourceSelection(anyhow::anyhow!(error.to_string())))
    } else {
        Ok(discovered)
    }
}

#[derive(Default)]
struct SessionChanges {
    reinferred: Vec<(ModuleId, BodyOwner)>,
    span_edits: Vec<skull::instance_graph::TextEditDelta>,
    lint_changed_files: Vec<ustr::Ustr>,
    full: bool,
    changed: bool,
    errors: Vec<CompilerError>,
}

#[derive(Clone, Copy)]
enum UpdateErrorPolicy {
    Stop,
    Continue,
}

fn update_session(
    session: &mut bone_incremental::session::Session,
    feed: &[(String, String)],
    removed: &[String],
    policy: UpdateErrorPolicy,
) -> SessionChanges {
    use bone_incremental::session::UpdateTier;
    let mut changes = SessionChanges::default();
    for (path, text) in feed {
        let update = match session.update_file(path, text) {
            Ok(update) => update,
            Err(error) => {
                changes.errors.push(error);
                if matches!(policy, UpdateErrorPolicy::Stop) {
                    break;
                }
                continue;
            }
        };
        changes.reinferred.extend(update.reinferred.iter().copied());
        match update.tier {
            UpdateTier::NoChange => {}
            UpdateTier::BodyOnly => {
                changes.changed = true;
                changes
                    .lint_changed_files
                    .push(ustr::Ustr::from(path.as_str()));
                if let Some(edit) = update.body_update.edit {
                    changes.span_edits.push(edit);
                }
            }
            UpdateTier::Interface | UpdateTier::Scope | UpdateTier::FullUser => {
                changes.changed = true;
                changes.full = true;
            }
        }
    }
    if changes.errors.is_empty() || matches!(policy, UpdateErrorPolicy::Continue) {
        for path in removed {
            changes.changed = true;
            changes.full = true;
            if let Err(error) = session.remove_file(path) {
                changes.errors.push(error);
                if matches!(policy, UpdateErrorPolicy::Stop) {
                    break;
                }
            }
        }
    }
    changes
}

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
    comptime_inputs: bone_semantic::inference::ComptimeInputs,
    aliases: bone_incremental::session::NsAliases,
    sources: SourceFiles,
    entry_path: String,
    previous_contents: Vec<(String, String)>,
}

pub struct LiveProject {
    session: bone_incremental::session::Session,
    dir: String,
    sources: SourceFiles,
    entry_path: String,
    include_tests: bool,
    lint_cache: bone_lints::IncrementalLintCache,
}

/// The sources a live project of `dir` compiles and accepts edits to: every
/// discovered file, or without `include_tests` the production build's.
pub(crate) fn project_sources(dir: &str, include_tests: bool) -> Result<SourceFiles> {
    let discovered = crate::unified_compile::discover_project_sources(Path::new(dir))?;
    if include_tests {
        return Ok(discovered);
    }
    rock_host::source_files::production_only(&discovered, Path::new(dir))
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

impl LiveProject {
    pub fn init(dir: &str, include_tests: bool) -> Result<Self> {
        Self::init_from_sources(dir, include_tests, project_sources(dir, include_tests)?)
    }

    pub(crate) fn init_from_sources(
        dir: &str,
        include_tests: bool,
        sources: SourceFiles,
    ) -> Result<Self> {
        let entry = entry_path(&sources)?;
        let mut project =
            Self::create(dir, include_tests, sources, entry).map_err(|(sources, error)| {
                anyhow::anyhow!(
                    "{}",
                    crate::skull_backend::render_project_err(&sources, &error)
                )
            })?;
        project.verdict("in initial build")?;
        Ok(project)
    }

    pub(crate) fn init_for_watch(
        dir: &str,
        is_game: bool,
    ) -> std::result::Result<Self, LiveWatchError> {
        let sources = watch_sources(dir, is_game)?;
        let entry = entry_path(&sources).unwrap_or_default();
        Self::create(dir, !is_game, sources, entry)
            .map_err(|(sources, error)| LiveWatchError::Session(sources, error))
    }

    fn create(
        dir: &str,
        include_tests: bool,
        sources: SourceFiles,
        entry_path: String,
    ) -> std::result::Result<Self, (SourceFiles, CompilerError)> {
        // Build-var commands run only when constructing or rebuilding the session.
        let session = match crate::bone_backend::bone_session_for_emission(
            crate::bone_backend::session_files(&sources),
            Path::new(dir),
        ) {
            Ok(session) => session,
            Err(error) => return Err((sources, error)),
        };
        Ok(Self {
            session,
            dir: dir.to_string(),
            sources,
            entry_path,
            include_tests,
            lint_cache: Default::default(),
        })
    }

    pub(crate) fn snapshot(&mut self, changed: &[PathBuf]) -> LiveProjectSnapshot {
        let mut previous_contents = Vec::new();
        for path in changed {
            let Ok(canonical) = std::fs::canonicalize(path) else {
                continue;
            };
            let id = canonical.to_string_lossy().into_owned();
            let Some(source) = self
                .sources
                .files
                .iter()
                .find(|source| source.borrow().file.to_id_path() == id)
            else {
                continue;
            };
            if previous_contents.iter().any(|(old_id, _)| old_id == &id) {
                continue;
            }
            previous_contents.push((id, source.borrow().contents.clone()));
        }
        LiveProjectSnapshot {
            comptime_inputs: self.session.ws.comptime_inputs.clone(),
            aliases: self.session.aliases.clone(),
            sources: self.sources.clone(),
            entry_path: self.entry_path.clone(),
            previous_contents,
        }
    }

    pub(crate) fn restore(&mut self, snapshot: LiveProjectSnapshot) -> Result<()> {
        self.sources = snapshot.sources;
        self.entry_path = snapshot.entry_path;
        for (id, contents) in snapshot.previous_contents {
            if let Some(source) = self
                .sources
                .files
                .iter()
                .find(|source| source.borrow().file.to_id_path() == id)
            {
                source.borrow_mut().contents = contents;
            }
        }
        // Rollback uses the package facts of the last good build: the current
        // manifest may be the input that made the candidate fail.
        self.session = bone_incremental::session::Session::new(
            self.session.warm.clone(),
            crate::bone_backend::session_files(&self.sources),
            &snapshot.aliases,
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "{}",
                crate::skull_backend::render_project_err(&self.sources, &e)
            )
        })?;
        self.session.ws.comptime_inputs = snapshot.comptime_inputs;
        crate::lint_pass::rollback_build_lints_cached(
            &mut self.lint_cache,
            self.session.workspace(),
            &self.sources,
            Path::new(&self.dir),
            if self.include_tests {
                crate::lint_pass::Scope::WholeProject
            } else {
                crate::lint_pass::Scope::ProductionOnly
            },
        )
        .map_err(|error| anyhow::anyhow!("lint-cache rollback failed: {error}"))?;
        self.verdict_with_change(
            "after failed publication rollback",
            &bone_lints::LintCacheUpdate::body_only([], [], []),
        )
    }

    pub(crate) fn commit_body_transaction(&mut self) {
        self.lint_cache.commit_body_transaction();
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
        self.transaction(changed, "compile error", |project| {
            project.recompile_inner(changed)
        })
    }

    pub(crate) fn recompile_for_watch(
        &mut self,
        is_game: bool,
    ) -> std::result::Result<(bool, Vec<(ModuleId, BodyOwner)>), LiveWatchError> {
        self.include_tests = !is_game;
        self.sources = watch_sources(&self.dir, is_game)?;
        self.entry_path = entry_path(&self.sources).unwrap_or_default();
        let files = crate::bone_backend::session_files(&self.sources);
        let feed = files
            .iter()
            .map(|file| (file.path.clone(), file.text.clone()))
            .collect::<Vec<_>>();
        let tracked = self.session.tracked_paths();
        let removed = tracked
            .into_iter()
            .filter(|path| !files.iter().any(|file| file.path == *path))
            .map(|path| path.to_string())
            .collect::<Vec<_>>();
        let changes = update_session(
            &mut self.session,
            &feed,
            &removed,
            UpdateErrorPolicy::Continue,
        );
        if !changes.errors.is_empty() {
            return Err(LiveWatchError::Update(self.sources.clone(), changes.errors));
        }
        self.watch_verdict()?;
        Ok((changes.full, changes.reinferred))
    }

    pub(crate) fn watch_verdict(&mut self) -> std::result::Result<(), LiveWatchError> {
        let (lint, errors) = self.build_errors(crate::lint_pass::Scope::WholeProject, None);
        if errors.is_empty() {
            Ok(())
        } else if lint {
            Err(LiveWatchError::Lints(self.sources.clone(), errors))
        } else {
            Err(LiveWatchError::Diagnostics(self.sources.clone(), errors))
        }
    }

    /// Run `change` against a snapshot of `touched`, restoring the snapshot
    /// when it fails.
    fn transaction(
        &mut self,
        touched: &[PathBuf],
        failure: &str,
        change: impl FnOnce(&mut Self) -> Result<LiveUpdate>,
    ) -> Result<(LiveUpdate, LiveProjectSnapshot)> {
        let snapshot = self.snapshot(touched);
        let result = change(self);
        let error = match result {
            Ok(update) => return Ok((update, snapshot)),
            Err(error) => error,
        };
        let restored = self.restore(snapshot);
        match restored {
            Ok(()) => Err(error),
            Err(restore) => Err(anyhow::anyhow!(
                "live project rollback failed after {failure}: {restore}; original error: {error}"
            )),
        }
    }

    fn recompile_inner(&mut self, changed: &[PathBuf]) -> Result<LiveUpdate> {
        let started = Instant::now();
        let mut need_full = changed.is_empty();
        let mut feed = Vec::new();
        if !need_full {
            for path in changed {
                let Ok(canonical) = std::fs::canonicalize(path) else {
                    need_full = true;
                    break;
                };
                let id = canonical.to_string_lossy().into_owned();
                let Some(source) = self
                    .sources
                    .files
                    .iter()
                    .find(|source| source.borrow().file.to_id_path() == id)
                else {
                    need_full = true;
                    break;
                };
                let Ok(text) = fs_err::read_to_string(&canonical) else {
                    need_full = true;
                    break;
                };
                source.borrow_mut().contents = text.clone();
                feed.push((id, text));
            }
        }
        if need_full {
            self.rediscover()?;
            self.rebuild_session()?;
            return Ok(LiveUpdate::Full {
                cook_ms: started.elapsed().as_secs_f64() * 1000.0,
            });
        }
        self.update(&feed, started)
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
        self.transaction(&[path.to_path_buf()], "in-memory edit error", |project| {
            project.apply_edit_inner(path, new_text)
        })
    }

    fn apply_edit_inner(&mut self, path: &Path, new_text: &str) -> Result<LiveUpdate> {
        let started = Instant::now();
        let canonical = std::fs::canonicalize(path).map_err(|e| {
            anyhow::anyhow!("apply_edit: cannot canonicalize `{}`: {e}", path.display())
        })?;
        let id = canonical.to_string_lossy().into_owned();
        let Some(source) = self
            .sources
            .files
            .iter()
            .find(|source| source.borrow().file.to_id_path() == id)
        else {
            anyhow::bail!(
                "apply_edit: `{id}` is not a discovered source of `{}`",
                self.dir
            );
        };
        source.borrow_mut().contents = new_text.to_string();
        self.update(&[(id, new_text.to_string())], started)
    }

    pub(crate) fn source_text(&self, path: &Path) -> Option<String> {
        let id = std::fs::canonicalize(path)
            .ok()?
            .to_string_lossy()
            .into_owned();
        self.sources
            .files
            .iter()
            .find(|source| source.borrow().file.to_id_path() == id)
            .map(|source| source.borrow().contents.clone())
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

    pub fn entry_module(&self) -> ModuleId {
        self.session.workspace().module_ids[&ustr::Ustr::from(&self.entry_path)]
    }

    pub fn verdict(&mut self, context: &str) -> Result<()> {
        self.verdict_with_change(context, &bone_lints::LintCacheUpdate::full())
    }

    fn verdict_with_change(
        &mut self,
        context: &str,
        change: &bone_lints::LintCacheUpdate,
    ) -> Result<()> {
        let scope = if self.include_tests {
            crate::lint_pass::Scope::WholeProject
        } else {
            crate::lint_pass::Scope::ProductionOnly
        };
        let (lint, errors) = self.build_errors(scope, Some(change));
        for error in &errors {
            eprintln!(
                "{}",
                crate::skull_backend::render_project_err(&self.sources, error)
            );
        }
        if errors.is_empty() {
            Ok(())
        } else {
            let kind = if lint { "lint" } else { "type" };
            anyhow::bail!("skull: {} {kind} error(s) {context}", errors.len())
        }
    }

    fn build_errors(
        &mut self,
        scope: crate::lint_pass::Scope,
        change: Option<&bone_lints::LintCacheUpdate>,
    ) -> (bool, Vec<CompilerError>) {
        let errors = self.session.diagnostics();
        if !errors.is_empty() {
            return (false, errors);
        }
        let errors = match change {
            Some(change) => crate::lint_pass::run_build_lints_cached(
                &mut self.lint_cache,
                self.session.workspace(),
                &self.sources,
                Path::new(&self.dir),
                scope,
                change,
            ),
            None => crate::lint_pass::run_build_lints(
                self.session.workspace(),
                &self.sources,
                Path::new(&self.dir),
                scope,
            ),
        };
        (true, errors)
    }

    fn rediscover(&mut self) -> Result<()> {
        let sources = project_sources(&self.dir, self.include_tests)?;
        let entry_path = entry_path(&sources)?;
        self.sources = sources;
        self.entry_path = entry_path;
        Ok(())
    }

    fn rebuild_session(&mut self) -> Result<()> {
        self.lint_cache.clear();
        self.session = crate::bone_backend::bone_session_for_emission(
            crate::bone_backend::session_files(&self.sources),
            Path::new(&self.dir),
        )
        .map_err(|error| {
            anyhow::anyhow!(
                "{}",
                crate::skull_backend::render_project_err(&self.sources, &error)
            )
        })?;
        self.verdict("- serving last good output")
    }

    fn update(&mut self, feed: &[(String, String)], started: Instant) -> Result<LiveUpdate> {
        let changes = update_session(&mut self.session, feed, &[], UpdateErrorPolicy::Stop);
        if let Some(error) = changes.errors.first() {
            return Err(anyhow::anyhow!(
                "{}",
                crate::skull_backend::render_project_err(&self.sources, error)
            ));
        }
        let SessionChanges {
            reinferred,
            span_edits,
            lint_changed_files,
            mut full,
            changed,
            ..
        } = changes;
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
                |file| {
                    self.session
                        .files
                        .get(file)
                        .map(|source| source.text.clone())
                },
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
            bone_lints::LintCacheUpdate::body_only(
                lint_changed_files,
                reinferred.iter().copied(),
                span_edits.iter().copied(),
            )
        };
        if changed && !full {
            self.lint_cache.begin_body_transaction(&lint_change);
        }
        self.verdict_with_change("- serving last good output", &lint_change)?;
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

fn entry_path(sources: &SourceFiles) -> Result<String> {
    let mut first = None;
    let mut main = None;
    for source in &sources.files {
        let path = source.borrow().file.to_id_path();
        first.get_or_insert_with(|| path.clone());
        if path.ends_with("/main.rock") || path.ends_with("\\main.rock") || path == "main.rock" {
            main = Some(path);
        }
    }
    main.or(first)
        .ok_or_else(|| anyhow::anyhow!("bone: project has no .rock files"))
}

#[cfg(test)]
mod tests {
    use super::{LiveProject, LiveUpdate};

    #[test]
    fn a_rejected_lint_edit_keeps_the_running_program_and_allows_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rock");
        let main = dir.path().join("main.lua");
        let overlay = dir.path().join("incremental.lua");
        let old = "fn maybe_int() -> int? { 1 }\nfn always_int() -> int { 1 }\n@nomangle fn value() -> int { let _value: int = cast(always_int()) 3 }\n";
        std::fs::write(&source, old).unwrap();
        let mut project = crate::skull_backend::IncrementalProject::init(
            dir.path().to_str().unwrap(),
            &main,
            &overlay,
        )
        .unwrap();
        let before = std::fs::read_to_string(&main).unwrap();
        let error = project
            .apply_edit(
                &source,
                &old.replace("cast(always_int())", "cast(maybe_int())"),
                &main,
                &overlay,
            )
            .unwrap_err();
        assert!(error.to_string().contains("lint error(s)"), "{error}");
        assert_eq!(std::fs::read_to_string(&main).unwrap(), before);
        project.force_full(&main, &overlay).unwrap();
        let script = format!(
            "{}\nassert(value() == 3)",
            std::fs::read_to_string(&main).unwrap()
        );
        let run = crate::compile_cmd::run_lua_code_with_timeout(&script, None).unwrap();
        assert!(run.success, "{}", run.combined());
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
