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

pub struct LiveProject {
    session: bone_incremental::session::Session,
    dir: String,
    sources: SourceFiles,
    entry_path: String,
    include_tests: bool,
}

impl LiveProject {
    pub fn init(dir: &str, include_tests: bool) -> Result<Self> {
        let spec = crate::unified_compile::SourceSpec::Directory(PathBuf::from(dir));
        let discovered = crate::unified_compile::discover_sources(&spec)?;
        let sources = if include_tests {
            discovered
        } else {
            discovered.production_only()
        };
        let entry_path = entry_path(&sources)?;
        let files = source_pairs(&sources);
        let session = crate::bone_backend::bone_session(files).map_err(|e| {
            anyhow::anyhow!("{}", crate::skull_backend::render_project_err(&sources, &e))
        })?;
        let project = Self {
            session,
            dir: dir.to_string(),
            sources,
            entry_path,
            include_tests,
        };
        project.verdict("in initial build")?;
        Ok(project)
    }

    pub fn recompile(&mut self, changed: &[PathBuf]) -> Result<LiveUpdate> {
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

    pub fn apply_edit(&mut self, path: &Path, new_text: &str) -> Result<LiveUpdate> {
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

    pub fn verdict(&self, context: &str) -> Result<()> {
        let diagnostics = self.session.diagnostics();
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
        let lint_errs = crate::lint_pass::run_build_lints(
            self.session.workspace(),
            &self.sources,
            Path::new(&self.dir),
            scope,
        );
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
            discovered.production_only()
        };
        self.entry_path = entry_path(&self.sources)?;
        Ok(())
    }

    fn rebuild_session(&mut self) -> Result<()> {
        self.session =
            crate::bone_backend::bone_session(source_pairs(&self.sources)).map_err(|error| {
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
        let mut full = false;
        let mut changed = false;
        for (path, text) in feed {
            let update = self.session.update_file(path, text).map_err(|e| {
                anyhow::anyhow!("{}", crate::skull_backend::render_project_err(&sources, &e))
            })?;
            match update.tier {
                bone_incremental::session::UpdateTier::NoChange => {}
                bone_incremental::session::UpdateTier::BodyOnly => {
                    changed = true;
                    reinferred.extend(update.reinferred);
                    if let Some(edit) = update.body_update.edit {
                        span_edits.push(skull::instance_graph::SpanEdit {
                            file: edit.file,
                            at: edit.at,
                            byte_delta: edit.byte_delta,
                            line_delta: edit.line_delta,
                        });
                    }
                }
                bone_incremental::session::UpdateTier::Interface
                | bone_incremental::session::UpdateTier::FullUser => {
                    changed = true;
                    full = true;
                }
            }
        }
        self.verdict("- serving last good output")?;
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

fn source_pairs(sources: &SourceFiles) -> Vec<(String, String)> {
    sources
        .files
        .iter()
        .map(|source| {
            let source = source.borrow();
            (source.file.to_id_path(), source.contents.clone())
        })
        .collect()
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
}
