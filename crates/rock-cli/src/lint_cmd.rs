//! `rock lint <path>` -- the author-facing surface of the lint engine.
//!
//! A thin driver over `rock-lang/crates/bone-lints`: type-check the file or project
//! through bone exactly as a build does, then run the same catalog the build
//! pass and the LSP run. One engine, several surfaces. Exits non-zero only on an
//! `Error`-severity finding, matching the build gate -- a `Warning` reports.

use crate::Result;
use crate::lint_pass::{self, LintReport, Scope};
use rock_core::SourceFiles;
use std::path::{Path, PathBuf};

/// Lint a single `.rock` file or a whole project directory. Unlike a build this
/// keeps `tests/` sources: a sim is code the author maintains, and `@sim` /
/// `@test` decorators are reachability roots, so dropping them would report
/// their callees as dead.
pub fn do_lint(
    path: &str,
    lints: Option<&str>,
    only_types: Option<&str>,
    ignore_types: Option<&str>,
) -> Result<()> {
    let sel = match lints {
        // A typo would run nothing and report success, so an unknown name is a
        // rejection, not a silent no-op.
        Some(list) => {
            let sel = bone_lints::LintOptions::only(
                list.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
            );
            let unknown = sel.unknown();
            if !unknown.is_empty() {
                anyhow::bail!(
                    "unknown lint(s): {} -- known: {}",
                    unknown.join(", "),
                    bone_lints::KNOWN_LINTS.join(", ")
                );
            }
            sel
        }
        None => crate::registry::lint_options(),
    };
    let only = type_names(only_types);
    let ignored = type_names(ignore_types);
    let mut sel = sel.with_action_policy(crate::registry::registered().action_layer_policy).ignoring_types(ignored.iter().copied());
    if only_types.is_some() {
        sel = sel.over_types(only.iter().copied());
    }
    let p = Path::new(path);
    let (ws, sources, root) = if p.is_dir() {
        let sources = crate::unified_compile::discover_sources(
            &crate::unified_compile::SourceSpec::Directory(PathBuf::from(path)),
        )?;
        let mut modules = match crate::skull_backend::skull_assemble_project(&sources) {
            Ok((modules, _entry)) => modules,
            Err(errs) => return Err(reject(&sources, &errs)),
        };
        let ws = match crate::bone_backend::check_modules(&mut modules) {
            Ok(ws) => ws,
            Err(errs) => return Err(reject(&sources, &errs)),
        };
        (ws, sources, PathBuf::from(path))
    } else {
        let src = fs_err::read_to_string(path)?;
        let module = match skull_parser::parse_module(&src, ustr::Ustr::from(path)) {
            Ok(m) => m,
            Err(e) => return Err(reject(&single_source(path, &src), &[e])),
        };
        let ws = match crate::bone_backend::check_with_stdlib([module]) {
            Ok(ws) => ws,
            Err(errs) => return Err(reject(&single_source(path, &src), &errs)),
        };
        let root = p.parent().unwrap_or(Path::new(".")).to_path_buf();
        (ws, single_source(path, &src), root)
    };

    // The two lists fail in opposite directions, so a bad name is handled in
    // opposite ways. `--only-types` fails CLOSED: a typo drops the type from
    // the sweep and the run reports a project that looks converted, so it
    // rejects. `--ignore-types` fails OPEN: a typo filters nothing and the type
    // keeps appearing, which is visible -- naming it beats failing the run.
    if !only.is_empty() || !ignored.is_empty() {
        let warm = lint_pass::warm_modules(&ws, &sources);
        let unknown_only = bone_lints::unknown_type_names(&ws, &warm, &only);
        if !unknown_only.is_empty() {
            anyhow::bail!("--only-types names no type in this project: {}", join(&unknown_only));
        }
        let unknown_ignored = bone_lints::unknown_type_names(&ws, &warm, &ignored);
        if !unknown_ignored.is_empty() {
            log::warn!(target: "LINT", "--ignore-types names no type in this project: {}",
                join(&unknown_ignored));
        }
    }

    let findings = lint_pass::findings_with(&ws, &sources, &root, Scope::WholeProject, &sel);

    // `row_escape` is a CENSUS, so it reports the population it examined
    // alongside the findings. A bare zero cannot be read: a project with no
    // `Arena` at all scores the same as one whose every row stays put, and the
    // build-or-delete decision this feeds (the compiler contract P6)
    // turns on telling those apart.
    if sel.enabled(bone_lints::ROW_ESCAPE) {
        let warm = lint_pass::warm_modules(&ws, &sources);
        let report = lint_pass::report_files(&sources, &root);
        let pop = bone_lints::row_population(&ws, &warm, &report);
        log::info!(target: "LINT", "{}: examined {} row source(s), {} row binding(s) in {} body(ies)",
            path, pop.sources, pop.bindings, pop.bodies);
    }

    if findings.is_empty() {
        log::info!(target: "LINT", "{}: no findings", path);
        return Ok(());
    }

    // `--lint-summary` compacts the soft tiers here too; errors always render.
    let summarize = lint_pass::policy() == LintReport::Summary;
    let errors = findings
        .iter()
        .filter(|f| f.level() == rock_core::errors::CompilerErrorLevel::Error)
        .count();
    // One batch render (the ariadne cache copies every source file, so a
    // per-finding render is ~1.6ms each on a game-sized project).
    let shown: Vec<rock_core::CompilerError> = findings
        .iter()
        .filter(|f| !summarize || f.level() == rock_core::errors::CompilerErrorLevel::Error)
        .map(|f| f.error.clone())
        .collect();
    if !shown.is_empty() {
        eprintln!("{}", rock_core::errors::render_all(&shown, &sources));
    }
    log::info!(target: "LINT", "{}: {} finding(s) ({})", path, findings.len(), tally(&findings));
    if errors > 0 {
        anyhow::bail!("{errors} lint error(s)");
    }
    Ok(())
}

/// A comma-separated `--only-types` / `--ignore-types` list.
fn type_names(list: Option<&str>) -> Vec<ustr::Ustr> {
    list.into_iter()
        .flat_map(|l| l.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ustr::Ustr::from)
        .collect()
}

fn join(names: &[ustr::Ustr]) -> String {
    names.iter().map(|u| u.as_str()).collect::<Vec<_>>().join(", ")
}

/// `8 unused_var, 4 unused_fn`, most frequent first.
fn tally(findings: &[bone_lints::Finding]) -> String {
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    for f in findings {
        match counts.iter_mut().find(|(name, _)| *name == f.lint) {
            Some((_, n)) => *n += 1,
            None => counts.push((f.lint, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    counts.iter().map(|(name, n)| format!("{n} {name}")).collect::<Vec<_>>().join(", ")
}

/// A program that doesn't type-check can't be linted: its HIR is missing or
/// stale, so every finding would be noise. Report the real errors instead.
fn reject(sources: &SourceFiles, errs: &[rock_core::CompilerError]) -> anyhow::Error {
    eprintln!("{}", crate::bone_backend::render_errs(sources, errs));
    anyhow::anyhow!("lint: source does not type-check ({} error(s))", errs.len())
}

fn single_source(path: &str, src: &str) -> SourceFiles {
    let mut sources = SourceFiles::empty();
    sources.add_or_update_file(rock_core::source_file_from_path_and_contents(
        path.to_string(),
        path.to_string(),
        src.to_string(),
    ));
    sources
}
