use crate::{Result, SourceFiles};
use bone_semantic::hir::BodyOwner;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug)]
pub enum LiveUpdate {
    NoChange,
    BodyOnly {
        reinferred: Vec<(ModuleId, BodyOwner)>,
        span_edits: Vec<skull::instance_graph::SpanEdit>,
        cook_ms: f64,
    },
    Full {
        cook_ms: f64,
    },
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LiveRouteTiming {
    pub session_snapshot_ns: u128,
    pub source_lookup_feed_ns: u128,
    pub session_update_ns: u128,
    pub diagnostics_ns: u128,
    pub lint_total_ns: u128,
    pub local_lint_derivation_ns: u128,
    pub global_lint_dependency_ns: u128,
    pub global_lint_derivation_ns: u128,
    pub suppression_filtering_ns: u128,
    pub suppression_filter_calls: usize,
    pub report_prepare_ns: u128,
    pub summary_sort_ns: u128,
    pub render_ns: u128,
    pub finding_aggregation_ns: u128,
    pub rollback_ns: u128,
    pub live_update_ns: u128,
}

pub(crate) struct LiveProjectSnapshot {
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
    route_profile_enabled: bool,
    last_route_timing: LiveRouteTiming,
}

impl LiveProject {
    pub fn init(dir: &str, include_tests: bool) -> Result<Self> {
        let spec = crate::unified_compile::SourceSpec::Directory(PathBuf::from(dir));
        let discovered = crate::unified_compile::discover_sources(&spec)?;
        let sources = if include_tests {
            discovered
        } else {
            discovered
                .production_only(Path::new(dir))
                .map_err(|error| anyhow::anyhow!(error.to_string()))?
        };
        let entry_path = entry_path(&sources)?;
        let files = crate::bone_backend::session_files(&sources);
        // bone_session_for_emission resolves [build.vars] at construction, so
        // var commands run once per session build -- init and every
        // `rebuild_session` -- never per incremental update (the Session
        // mutates its ws in place, so the stamp survives `update_file`).
        let session = crate::bone_backend::bone_session_for_emission(files, Path::new(dir))
            .map_err(|e| {
                anyhow::anyhow!("{}", crate::skull_backend::render_project_err(&sources, &e))
            })?;
        let mut project = Self {
            session,
            dir: dir.to_string(),
            sources,
            entry_path,
            include_tests,
            lint_cache: Default::default(),
            route_profile_enabled: std::env::var_os("ROCK_SKULL_ROUTE_PROFILE").is_some(),
            last_route_timing: LiveRouteTiming::default(),
        };
        project.verdict("in initial build")?;
        Ok(project)
    }

    pub(crate) fn snapshot(&mut self, changed: &[PathBuf]) -> LiveProjectSnapshot {
        let started = self.route_profile_enabled.then(Instant::now);
        let mut previous_contents = Vec::new();
        for path in changed {
            let Ok(canonical) = std::fs::canonicalize(path) else { continue };
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
        if let Some(stats) = self
            .lint_cache
            .snapshot_stats(crate::lint_pass::census_enabled())
        {
            crate::lint_pass::record_lint_cache_snapshot(stats);
        }
        let snapshot = LiveProjectSnapshot {
            sources: self.sources.clone(),
            entry_path: self.entry_path.clone(),
            previous_contents,
        };
        if let Some(started) = started {
            self.last_route_timing.session_snapshot_ns += started.elapsed().as_nanos();
        }
        snapshot
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
        self.session = crate::bone_backend::bone_session(crate::bone_backend::session_files(&self.sources)).map_err(|e| {
            anyhow::anyhow!(
                "{}",
                crate::skull_backend::render_project_err(&self.sources, &e)
            )
        })?;
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

    pub(crate) fn enable_route_profile(&mut self) {
        self.route_profile_enabled = true;
    }

    pub(crate) fn take_route_timing(&mut self) -> LiveRouteTiming {
        std::mem::take(&mut self.last_route_timing)
    }

    pub(crate) fn commit_body_transaction(&mut self) {
        self.lint_cache.commit_body_transaction();
    }

    pub fn recompile(&mut self, changed: &[PathBuf]) -> Result<LiveUpdate> {
        self.recompile_for_publication(changed).map(|(update, _snapshot)| update)
    }

    pub(crate) fn recompile_for_publication(
        &mut self,
        changed: &[PathBuf],
    ) -> Result<(LiveUpdate, LiveProjectSnapshot)> {
        self.last_route_timing = LiveRouteTiming::default();
        let snapshot = self.snapshot(changed);
        let started = self.route_profile_enabled.then(Instant::now);
        match self.recompile_inner(changed) {
            Ok(update) => {
                if let Some(started) = started {
                    self.last_route_timing.live_update_ns += started.elapsed().as_nanos();
                }
                Ok((update, snapshot))
            }
            Err(error) => {
                if let Some(started) = started {
                    self.last_route_timing.live_update_ns += started.elapsed().as_nanos();
                }
                let rollback_started = self.route_profile_enabled.then(Instant::now);
                if let Err(restore) = self.restore(snapshot) {
                    if let Some(started) = rollback_started {
                        self.last_route_timing.rollback_ns += started.elapsed().as_nanos();
                    }
                    return Err(anyhow::anyhow!(
                        "live project rollback failed after compile error: {restore}; original error: {error}"
                    ));
                }
                if let Some(started) = rollback_started {
                    self.last_route_timing.rollback_ns += started.elapsed().as_nanos();
                }
                Err(error)
            }
        }
    }

    fn recompile_inner(&mut self, changed: &[PathBuf]) -> Result<LiveUpdate> {
        let started = Instant::now();
        let mut need_full = changed.is_empty();
        let mut feed = Vec::new();
        let feed_started = self.route_profile_enabled.then(Instant::now);
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
        if let Some(started) = feed_started {
            self.last_route_timing.source_lookup_feed_ns += started.elapsed().as_nanos();
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

    pub fn apply_edit(&mut self, path: &Path, new_text: &str) -> Result<LiveUpdate> {
        self.apply_edit_for_publication(path, new_text)
            .map(|(update, _snapshot)| update)
    }

    pub(crate) fn apply_edit_for_publication(
        &mut self,
        path: &Path,
        new_text: &str,
    ) -> Result<(LiveUpdate, LiveProjectSnapshot)> {
        self.last_route_timing = LiveRouteTiming::default();
        let snapshot = self.snapshot(&[path.to_path_buf()]);
        let started = self.route_profile_enabled.then(Instant::now);
        match self.apply_edit_inner(path, new_text) {
            Ok(update) => {
                if let Some(started) = started {
                    self.last_route_timing.live_update_ns += started.elapsed().as_nanos();
                }
                Ok((update, snapshot))
            }
            Err(error) => {
                if let Some(started) = started {
                    self.last_route_timing.live_update_ns += started.elapsed().as_nanos();
                }
                let rollback_started = self.route_profile_enabled.then(Instant::now);
                if let Err(restore) = self.restore(snapshot) {
                    if let Some(started) = rollback_started {
                        self.last_route_timing.rollback_ns += started.elapsed().as_nanos();
                    }
                    return Err(anyhow::anyhow!(
                        "live project rollback failed after in-memory edit error: {restore}; original error: {error}"
                    ));
                }
                if let Some(started) = rollback_started {
                    self.last_route_timing.rollback_ns += started.elapsed().as_nanos();
                }
                Err(error)
            }
        }
    }

    fn apply_edit_inner(&mut self, path: &Path, new_text: &str) -> Result<LiveUpdate> {
        let started = Instant::now();
        let feed_started = self.route_profile_enabled.then(Instant::now);
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
        if let Some(started) = feed_started {
            self.last_route_timing.source_lookup_feed_ns += started.elapsed().as_nanos();
        }
        self.update(&[(id, new_text.to_string())], started)
    }

    pub fn source_text(&self, path: &Path) -> Option<String> {
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
        let diagnostics_started = self.route_profile_enabled.then(Instant::now);
        let diagnostics = self.session.diagnostics();
        if let Some(started) = diagnostics_started {
            self.last_route_timing.diagnostics_ns += started.elapsed().as_nanos();
        }
        if !diagnostics.is_empty() {
            for diagnostic in &diagnostics {
                eprintln!(
                    "{}",
                    crate::skull_backend::render_project_err(&self.sources, diagnostic)
                );
            }
            anyhow::bail!("skull: {} type error(s) {context}", diagnostics.len())
        }
        // The build lint pass on the hot-reload path: a reload is a build, so a
        // finding surfaces the same way it does for `rock compile`. Only a
        // type-clean workspace is linted -- broken bodies would report garbage.
        let scope = if self.include_tests {
            crate::lint_pass::Scope::WholeProject
        } else {
            crate::lint_pass::Scope::ProductionOnly
        };
        let lint_started = self.route_profile_enabled.then(Instant::now);
        let mut lint_timing = crate::lint_pass::LintRouteTiming::default();
        let lint_errs = if self.route_profile_enabled {
            crate::lint_pass::run_build_lints_cached_timed(
                &mut self.lint_cache,
                self.session.workspace(),
                &self.sources,
                Path::new(&self.dir),
                scope,
                change,
                &mut lint_timing,
            )
        } else {
            crate::lint_pass::run_build_lints_cached(
                &mut self.lint_cache,
                self.session.workspace(),
                &self.sources,
                Path::new(&self.dir),
                scope,
                change,
            )
        };
        if let Some(started) = lint_started {
            self.last_route_timing.lint_total_ns += started.elapsed().as_nanos();
            self.last_route_timing.local_lint_derivation_ns += lint_timing.local_lint_derivation_ns;
            self.last_route_timing.global_lint_dependency_ns += lint_timing.global_lint_dependency_ns;
            self.last_route_timing.global_lint_derivation_ns += lint_timing.global_lint_derivation_ns;
            self.last_route_timing.suppression_filtering_ns += lint_timing.suppression_filtering_ns;
            self.last_route_timing.suppression_filter_calls += lint_timing.suppression_filter_calls;
            self.last_route_timing.report_prepare_ns += lint_timing.report_prepare_ns;
            self.last_route_timing.summary_sort_ns += lint_timing.summary_sort_ns;
            self.last_route_timing.render_ns += lint_timing.render_ns;
            self.last_route_timing.finding_aggregation_ns += lint_timing.finding_aggregation_ns;
        }
        if !lint_errs.is_empty() {
            for e in &lint_errs {
                eprintln!("{}", crate::skull_backend::render_project_err(&self.sources, e));
            }
            anyhow::bail!("skull: {} lint error(s) {context}", lint_errs.len())
        }
        Ok(())
    }

    fn rediscover(&mut self) -> Result<()> {
        let spec = crate::unified_compile::SourceSpec::Directory(PathBuf::from(&self.dir));
        let discovered = crate::unified_compile::discover_sources(&spec)?;
        self.sources = if self.include_tests {
            discovered
        } else {
            discovered
                .production_only(Path::new(&self.dir))
                .map_err(|error| anyhow::anyhow!(error.to_string()))?
        };
        self.entry_path = entry_path(&self.sources)?;
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
        let sources = self.sources.clone();
        let mut reinferred = Vec::new();
        let mut span_edits = Vec::new();
        let mut lint_changed_files = Vec::new();
        let mut lint_reinferred = Vec::new();
        let mut lint_edits = Vec::new();
        let mut full = false;
        let mut changed = false;
        for (path, text) in feed {
            let session_started = self.route_profile_enabled.then(Instant::now);
            let update = self.session.update_file(path, text).map_err(|e| {
                anyhow::anyhow!("{}", crate::skull_backend::render_project_err(&sources, &e))
            })?;
            if let Some(started) = session_started {
                self.last_route_timing.session_update_ns += started.elapsed().as_nanos();
            }
            match update.tier {
                bone_incremental::session::UpdateTier::NoChange => {}
                bone_incremental::session::UpdateTier::BodyOnly => {
                    changed = true;
                    lint_changed_files.push(ustr::Ustr::from(path.as_str()));
                    lint_reinferred.extend(update.reinferred.iter().copied());
                    reinferred.extend(update.reinferred);
                    if let Some(edit) = update.body_update.edit {
                        span_edits.push(skull::instance_graph::SpanEdit {
                            file: edit.file,
                            at: edit.at,
                            byte_delta: edit.byte_delta,
                            line_delta: edit.line_delta,
                        });
                        lint_edits.push(bone_lints::TextEditDelta {
                            file: edit.file,
                            at: edit.at,
                            byte_delta: edit.byte_delta,
                            line_delta: edit.line_delta,
                        });
                    }
                }
                bone_incremental::session::UpdateTier::Interface
                | bone_incremental::session::UpdateTier::Scope
                | bone_incremental::session::UpdateTier::FullUser => {
                    changed = true;
                    full = true;
                }
            }
        }
        let lint_change = if full {
            bone_lints::LintCacheUpdate::full()
        } else {
            bone_lints::LintCacheUpdate::body_only(
                lint_changed_files,
                lint_reinferred,
                lint_edits,
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
    fn classifies_body_and_interface_edits_from_one_warm_session() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rock");
        std::fs::write(
            &source,
            "fn value() -> int { 1 }\nfn main() { print(value()) }\n",
        )
        .unwrap();
        let mut project = LiveProject::init(dir.path().to_str().unwrap(), false).unwrap();

        std::fs::write(
            &source,
            "fn value() -> int { 2 }\nfn main() { print(value()) }\n",
        )
        .unwrap();
        assert!(matches!(
            project.recompile(std::slice::from_ref(&source)).unwrap(),
            LiveUpdate::BodyOnly { ref reinferred, .. } if !reinferred.is_empty()
        ));

        std::fs::write(
            &source,
            "fn value(x: int) -> int { x }\nfn main() { print(value(2)) }\n",
        )
        .unwrap();
        assert!(matches!(
            project.recompile(std::slice::from_ref(&source)).unwrap(),
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

    #[test]
    fn body_edit_updates_the_shared_lint_cache_and_failed_edit_restores_it() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rock");
        std::fs::write(&source, "fn main() { let stale = 1 }\n").unwrap();
        let mut project = LiveProject::init(dir.path().to_str().unwrap(), false).unwrap();
        assert!(project.lint_cache.is_initialized());
        let file = ustr::Ustr::from(
            std::fs::canonicalize(&source)
                .unwrap()
                .to_string_lossy()
                .as_ref(),
        );
        assert!(!project.lint_cache.findings_for_file(file).is_empty());

        assert!(project.apply_edit(&source, "fn main( {\n").is_err());
        assert!(!project.lint_cache.findings_for_file(file).is_empty());

        project.apply_edit(&source, "fn main() {}\n").unwrap();
        assert!(project.lint_cache.is_initialized());
        assert!(project.lint_cache.findings_for_file(file).is_empty());
    }

    #[test]
    fn lint_error_after_cache_update_rolls_back_sources_and_findings_together() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rock");
        let old = r#"
fn maybe_int() -> int? { return 1 }
fn always_int() -> int { return 1 }
fn main() -> int { let _value: int = cast(always_int()) 3 }
"#;
        let bad = old.replace("cast(always_int())", "cast(maybe_int())");
        std::fs::write(&source, old).unwrap();
        let mut project = LiveProject::init(dir.path().to_str().unwrap(), false).unwrap();
        let file = ustr::Ustr::from(
            std::fs::canonicalize(&source)
                .unwrap()
                .to_string_lossy()
                .as_ref(),
        );
        let before = project
            .lint_cache
            .findings_for_file(file)
            .into_iter()
            .map(|finding| format!("{}:{}", finding.lint, finding.error.message))
            .collect::<Vec<_>>();
        assert!(project.apply_edit(&source, &bad).is_err());
        assert_eq!(project.source_text(&source).as_deref(), Some(old));
        let after = project
            .lint_cache
            .findings_for_file(file)
            .into_iter()
            .map(|finding| format!("{}:{}", finding.lint, finding.error.message))
            .collect::<Vec<_>>();
        assert_eq!(after, before);
        assert!(after.iter().all(|finding| !finding.starts_with("transmute_of_nullable:")));
    }

    /// A session re-parses on every edit, so it has to KEEP each file's
    /// dialect and owning package rather than re-derive them -- and it was
    /// given neither. Every Session-backed route (`rock sim`, `rock stone`,
    /// `rock watch`, the LSP) therefore read a v1 package as legacy source:
    /// `pub fn` came back as `unsupported item start`, and the definitions it
    /// could still parse had no package, so nothing resolved. Converting
    /// `libs/chess-core` turned that into 830 type errors in `rock sim
    /// chesslings` while `rock compile games/chesslings` was clean.
    #[test]
    fn a_v1_package_keeps_its_dialect_and_package_through_a_session() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("stick.toml"),
            "[package]\nname = \"livev1\"\ntype = \"bin\"\n\n[language]\nmodules = \"v1\"\n",
        )
        .unwrap();
        let main = dir.path().join("main.rock");
        std::fs::write(&main, "fn main() {\n    print(\"n={a::bump(1)}\")\n}\n").unwrap();
        std::fs::write(
            dir.path().join("a.rock"),
            "mod a;\n\npub fn bump(n: int) -> int {\n    n + 1\n}\n",
        )
        .unwrap();
        let mut project = LiveProject::init(dir.path().to_str().unwrap(), false)
            .expect("a v1 package must build through a session");

        // And keeps them across a re-parse: an edit re-runs the parser from the
        // stored text, which is the half a one-shot stamp at init would miss.
        project
            .apply_edit(&main, "fn main() {\n    print(\"n={a::bump(2)}\")\n}\n")
            .expect("a v1 edit must re-parse in its own dialect");
    }

    fn v1_live_project() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf, LiveProject) {
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

    /// The namespace tree is built from the module set, so an edit that
    /// changes what a v1 module declares has to reach it: a function added to
    /// `a` is callable as `a::twice` from the next edit on.
    #[test]
    fn a_v1_definition_added_by_an_edit_is_resolvable() {
        let (_dir, main, a, mut project) = v1_live_project();
        project
            .apply_edit(
                &a,
                "mod a;\n\npub fn bump(n: int) -> int {\n    n + 1\n}\n\npub fn twice(n: int) -> int {\n    n * 2\n}\n",
            )
            .expect("adding a pub fn to a v1 module is a valid edit");
        project
            .apply_edit(&main, "fn main() {\n    print(\"n={a::twice(3)}\")\n}\n")
            .expect("a pub fn added by an earlier edit must resolve");
    }

    /// Visibility is read from the namespace tree too, so taking `pub` off a
    /// definition must stop a consumer in another module reaching it.
    #[test]
    fn a_v1_visibility_narrowed_by_an_edit_is_enforced() {
        let (_dir, _main, a, mut project) = v1_live_project();
        // `apply_edit_inner` skips the rollback, so the rejecting diagnostics
        // are still on the session to read.
        project
            .apply_edit_inner(&a, "mod a;\n\nfn bump(n: int) -> int {\n    n + 1\n}\n")
            .expect_err("main still calls a::bump, which is now private to `a`");
        let messages: Vec<String> =
            project.session.diagnostics().iter().map(|d| d.message.clone()).collect();
        assert!(
            messages.iter().any(|m| m.contains("`bump` is not accessible from this module")),
            "{messages:?}"
        );
    }

    /// A file's `mod` header is not an item, so an edit to it alone changes
    /// no item print. It still moves every definition in the file: `a::bump`
    /// stops existing when `a.rock` becomes `mod b;`.
    #[test]
    fn a_v1_module_header_edit_moves_its_definitions() {
        let (_dir, _main, a, mut project) = v1_live_project();
        project
            .apply_edit_inner(&a, "mod b;\n\npub fn bump(n: int) -> int {\n    n + 1\n}\n")
            .expect_err("main still calls a::bump, and `a` no longer exists");
        let messages: Vec<String> =
            project.session.diagnostics().iter().map(|d| d.message.clone()).collect();
        assert!(
            messages.iter().any(|m| m.contains("`a::bump` did not resolve")),
            "{messages:?}"
        );
    }
}
