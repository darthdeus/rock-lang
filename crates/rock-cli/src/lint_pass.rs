//! Run build lints and apply the invocation's reporting policy.

use bone_core::prelude::{FxHashMap, FxHashSet, Ustr};
use bone_semantic::inference::Workspace;
use rock_core::{CompilerError, SourceFiles};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

/// How a build reports lint findings. Applies to `Warning`/`Advice` only --
/// `Error` findings always render in full and always fail the build, which is
/// what makes a correctness lint non-optional.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub(crate) enum LintReport {
    /// Render every finding spanned against its source, like a type error.
    #[default]
    Full,
    /// One line per lint name with a count, plus a pointer to `rock lint`.
    Summary,
    /// Don't run the Warning/Advice tiers at all (`--no-lints`).
    Off,
}

static POLICY: AtomicU8 = AtomicU8::new(POLICY_UNSET);

const POLICY_UNSET: u8 = 0;
const POLICY_FULL: u8 = 1;
const POLICY_SUMMARY: u8 = 2;
const POLICY_OFF: u8 = 3;

/// Install the invocation's reporting policy from the parsed flags. Called from
/// the dispatcher; the build call sites are deep in the backend and have no
/// `Args` to thread down.
pub(crate) fn set_policy(report: LintReport) {
    POLICY.store(
        match report {
            LintReport::Full => POLICY_FULL,
            LintReport::Summary => POLICY_SUMMARY,
            LintReport::Off => POLICY_OFF,
        },
        Ordering::Relaxed,
    );
}

pub(crate) fn set_corpus_mode() {
    set_policy(LintReport::Off);
}

pub fn policy() -> LintReport {
    match POLICY.load(Ordering::Relaxed) {
        POLICY_SUMMARY => LintReport::Summary,
        POLICY_OFF => LintReport::Off,
        _ => LintReport::Full,
    }
}

pub(crate) fn run_build_lints_cached(
    cache: &mut bone_lints::IncrementalLintCache,
    ws: &Workspace,
    sources: &SourceFiles,
    root: &Path,
    scope: Scope,
    change: &bone_lints::LintCacheUpdate,
) -> Vec<CompilerError> {
    let found = with_source_texts(sources, |texts| {
        let warm = skull::pipeline::warm_module_ids(ws, texts);
        let is_bin = scope == Scope::WholeProject && rock_host::stick::StickConfig::is_bin_at(root);
        let owned = owned_files(texts, root);
        let owned_sources: FxHashMap<Ustr, &str> = texts
            .iter()
            .filter(|(file, _)| owned.contains(*file))
            .map(|(&file, &text)| (file, text))
            .collect();
        cache.update(
            ws,
            &warm,
            is_bin,
            &owned_sources,
            &bone_lints::LintOptions::default(),
            change,
        )
    });
    report(found, sources, root, policy())
}

fn report(
    found: Vec<bone_lints::Finding>,
    sources: &SourceFiles,
    root: &Path,
    mode: LintReport,
) -> Vec<CompilerError> {
    if found.is_empty() {
        return Vec::new();
    }
    let (errors, soft) = skull::pipeline::split_findings(found);
    present_soft(&soft, sources, root, mode);
    errors
}

/// Report the soft tiers of a pipeline build per the invocation's policy;
/// the pipeline itself refuses on the `Error` findings.
pub(crate) fn report_soft(soft: &[bone_lints::Finding], sources: &SourceFiles, root: &Path) {
    if soft.is_empty() {
        return;
    }
    present_soft(soft, sources, root, policy());
}

fn present_soft(
    soft: &[bone_lints::Finding],
    sources: &SourceFiles,
    root: &Path,
    mode: LintReport,
) {
    match mode {
        LintReport::Off => {}
        LintReport::Full => {
            // One batch render: the ariadne source cache copies every file in
            // the project, so a per-finding render is ~1.6ms EACH on a game.
            let errs: Vec<CompilerError> = soft.iter().map(|f| f.error.clone()).collect();
            eprintln!("{}", rock_core::errors::render_all(&errs, sources));
        }
        LintReport::Summary => {
            if !soft.is_empty() {
                eprintln!("{}", summary_line(soft, root));
            }
        }
    }
}

/// Only a whole project can run closed-world lints: production builds omit test roots.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    WholeProject,
    /// Production sources only -- test/sim files were dropped before checking.
    ProductionOnly,
}

/// The `SourceFiles` and lint root of a single-file build. `file` must be the
/// same path the module was parsed under, or every span lookup misses and the
/// findings render without their snippet.
pub(crate) fn single_file_scope(file: &str, src: &str) -> (SourceFiles, std::path::PathBuf) {
    let sources = crate::common::single_source(file, src);
    let root = Path::new(file)
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    (sources, root)
}

/// Every finding in the catalog for this build, in workspace order.
pub fn findings(
    ws: &Workspace,
    sources: &SourceFiles,
    root: &Path,
    scope: Scope,
) -> Vec<bone_lints::Finding> {
    findings_with(
        ws,
        sources,
        root,
        scope,
        &bone_lints::LintOptions::default(),
    )
}

/// [`findings`] for an explicit lint selection. `rock lint --lints=<names>`
/// only: a BUILD always runs the default tier, so no invocation flag can drop
/// a lint the compiler considers non-optional.
pub fn findings_with(
    ws: &Workspace,
    sources: &SourceFiles,
    root: &Path,
    scope: Scope,
    sel: &bone_lints::LintOptions,
) -> Vec<bone_lints::Finding> {
    with_source_texts(sources, |texts| {
        let warm = skull::pipeline::warm_module_ids(ws, texts);
        let is_bin = scope == Scope::WholeProject && rock_host::stick::StickConfig::is_bin_at(root);
        // The REPORT set: this project's own files. A `stick.toml` DEPENDENCY
        // (`engine/engine` under a game) is scanned but never reported on -- its code
        // is not the author's to fix here, and a consumer must not fail a build on a
        // dependency's findings. Scanned rather than warm: an engine body calling
        // into game code is a real reachability edge, and dropping it would
        // false-flag the game's callbacks as dead.
        let owned = owned_files(texts, root);
        let report: FxHashMap<Ustr, &str> = texts
            .iter()
            .filter(|(file, _)| owned.contains(*file))
            .map(|(&file, &text)| (file, text))
            .collect();
        bone_lints::lint_workspace(ws, &warm, is_bin, &report, sel)
    })
}

fn owned_files(texts: &FxHashMap<Ustr, &str>, root: &Path) -> FxHashSet<Ustr> {
    let owned = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    texts
        .keys()
        .copied()
        .filter(|f| Path::new(f.as_str()).starts_with(&owned))
        .collect()
}

/// `12 lint warnings (8 unused_var, 4 unused_fn) -- run `rock lint <root>` for detail`.
fn summary_line(soft: &[bone_lints::Finding], root: &Path) -> String {
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    for f in soft {
        match counts.iter_mut().find(|(name, _)| *name == f.lint) {
            Some((_, n)) => *n += 1,
            None => counts.push((f.lint, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let detail = counts
        .iter()
        .map(|(name, n)| format!("{n} {name}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{} lint warning(s) ({detail}) -- run `rock lint {}` for detail",
        soft.len(),
        root.display()
    )
}

/// Judge the library's reachability in one consumer. The union pass intersects these findings.
pub(crate) fn unused_fns_under(
    ws: &Workspace,
    sources: &SourceFiles,
    consumer: &Path,
    report_root: &Path,
) -> Vec<bone_lints::Finding> {
    with_source_texts(sources, |texts| {
        let warm = skull::pipeline::warm_module_ids(ws, texts);
        let is_bin = rock_host::stick::StickConfig::is_bin_at(consumer);
        let report: FxHashMap<Ustr, &str> = texts
            .iter()
            .filter(|(file, _)| Path::new(file.as_str()).starts_with(report_root))
            .map(|(&file, &text)| (file, text))
            .collect();
        let only = bone_lints::LintOptions::only([bone_lints::UNUSED_FN.to_owned()]);
        bone_lints::lint_workspace(ws, &warm, is_bin, &report, &only)
    })
}

fn with_source_texts<R>(
    sources: &SourceFiles,
    use_texts: impl FnOnce(&FxHashMap<Ustr, &str>) -> R,
) -> R {
    let files = sources
        .files
        .iter()
        .map(|file| file.borrow())
        .collect::<Vec<_>>();
    let texts = files
        .iter()
        .map(|file| (Ustr::from(&file.file.to_id_path()), file.contents.as_str()))
        .collect::<FxHashMap<_, _>>();
    use_texts(&texts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bone_core::prelude::Span;
    use rock_core::CompilerError;
    use rock_core::errors::CompilerErrorLevel;

    fn finding(lint: &'static str, level: CompilerErrorLevel) -> bone_lints::Finding {
        bone_lints::Finding {
            lint,
            error: CompilerError::new_with_label(Span::unknown(), format!("{lint} fired"))
                .with_level(level),
            line: 0,
        }
    }

    fn empty_sources() -> SourceFiles {
        SourceFiles::empty()
    }

    #[test]
    fn error_findings_gate_the_build_under_every_report_mode() {
        // The load-bearing property: `--no-lints` and `--lint-summary` change
        // what is PRINTED, never whether an Error-severity finding fails the
        // build. `entity_across_yield`'s no-suppression argument rests on this.
        for mode in [LintReport::Full, LintReport::Summary, LintReport::Off] {
            let found = vec![
                finding("unused_var", CompilerErrorLevel::Warning),
                finding("entity_across_yield", CompilerErrorLevel::Error),
            ];
            let errs = report(found, &empty_sources(), Path::new("."), mode);
            assert_eq!(errs.len(), 1, "{mode:?} must return the Error finding");
            assert!(errs[0].message.contains("entity_across_yield"));
        }
    }

    #[test]
    fn soft_findings_never_gate_the_build() {
        let found = vec![
            finding("unused_var", CompilerErrorLevel::Warning),
            finding("could_be_value", CompilerErrorLevel::Advice),
        ];
        assert!(report(found, &empty_sources(), Path::new("."), LintReport::Full).is_empty());
    }

    #[test]
    fn summary_line_counts_per_lint_most_frequent_first() {
        let soft = vec![
            finding("unused_var", CompilerErrorLevel::Warning),
            finding("unused_fn", CompilerErrorLevel::Warning),
            finding("unused_fn", CompilerErrorLevel::Warning),
        ];
        let line = summary_line(&soft, Path::new("example"));
        assert_eq!(
            line,
            "3 lint warning(s) (2 unused_fn, 1 unused_var) -- run `rock lint example` for detail"
        );
    }

    #[test]
    fn policy_flags_map_to_report_modes() {
        set_policy(LintReport::Summary);
        assert_eq!(policy(), LintReport::Summary);
        set_corpus_mode();
        assert_eq!(
            policy(),
            LintReport::Off,
            "a corpus run silences the soft tiers"
        );
        set_policy(LintReport::Full);
        assert_eq!(policy(), LintReport::Full);
    }
}
