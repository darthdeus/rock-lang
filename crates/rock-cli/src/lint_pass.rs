//! The lint pass every build runs, and the `rock lint` driver behind it.
//!
//! Lints belong to the compiler, not to the editor: a finding that only appears
//! in the LSP is one an author can ignore indefinitely and CI can never enforce.
//! So the same `crates/bone-lints` engine the LSP drives also runs as a pass over
//! bone's already-typechecked output on every `compile`/`run`/`test`/`stone`
//! build. `Error`-severity findings FAIL the build; `Warning`/`Advice` report
//! only. Plan: `docs/lint-future-plan.md`.

use bone_core::prelude::{FxHashMap, FxHashSet, ModuleId, Ustr};
use bone_semantic::inference::Workspace;
use rock_core::errors::CompilerErrorLevel;
use rock_core::{CompilerError, SourceFiles, source_file_from_path_and_contents};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

/// How a build reports lint findings. Applies to `Warning`/`Advice` only --
/// `Error` findings always render in full and always fail the build, which is
/// what makes a correctness lint non-optional.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum LintReport {
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
pub fn set_policy(report: LintReport) {
    POLICY.store(
        match report {
            LintReport::Full => POLICY_FULL,
            LintReport::Summary => POLICY_SUMMARY,
            LintReport::Off => POLICY_OFF,
        },
        Ordering::Relaxed,
    );
}

/// Silence the soft tiers for a CORPUS run (`rock test <dir>`), which compiles
/// hundreds of unrelated files in parallel. Per-file findings there are noise --
/// the files are deliberate compiler-shape probes, not a project -- and the
/// parallel runner would interleave them into unreadable output anyway. `Error`
/// findings still gate every file; `rock lint <dir>` is the detail surface.
pub fn set_corpus_mode() {
    set_policy(LintReport::Off);
}

pub fn policy() -> LintReport {
    match POLICY.load(Ordering::Relaxed) {
        POLICY_SUMMARY => LintReport::Summary,
        POLICY_OFF => LintReport::Off,
        _ => LintReport::Full,
    }
}

/// Run the lint catalog over a checked workspace, report the `Warning`/`Advice`
/// tiers per the invocation's policy, and RETURN the `Error`-severity findings
/// for the caller to render and fail on. Returning them (rather than printing
/// here) lets each build surface fail through the error path it already has, so
/// a lint error reads exactly like a type error.
///
/// `sources` are the USER files of this build: everything else in `ws` (the
/// stdlib) is warm and never reported on. `root` selects the `stick.toml` whose
/// package type gates the closed-world lints.
///
/// Under `--no-lints` the pass still RUNS and still returns `Error` findings;
/// the flag silences the noisy tiers, it is not an escape hatch from a
/// correctness lint.
pub fn run_build_lints(
    ws: &Workspace,
    sources: &SourceFiles,
    root: &Path,
    scope: Scope,
) -> Vec<CompilerError> {
    report(findings(ws, sources, root, scope), sources, root, policy())
}

/// Split findings by severity, emit the soft tiers per `mode`, and hand back the
/// `Error`s. Pure apart from the printing, so the build gate is unit-testable
/// without a workspace. `transmute_of_nullable` is the catalog's `Error` today;
/// the point of build integration is that such a finding refuses the program
/// rather than printing at an author who can ignore it.
fn report(
    found: Vec<bone_lints::Finding>,
    sources: &SourceFiles,
    root: &Path,
    mode: LintReport,
) -> Vec<CompilerError> {
    if found.is_empty() {
        return Vec::new();
    }
    let (errors, soft): (Vec<_>, Vec<_>) =
        found.into_iter().partition(|f| f.level() == CompilerErrorLevel::Error);
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
                eprintln!("{}", summary_line(&soft, root));
            }
        }
    }
    errors.into_iter().map(|f| f.error).collect()
}

/// Whether this build's sources are the author's WHOLE project. A game build
/// drops `tests/` (`SourceFiles::production_only`), so `@sim`/`@test` roots and
/// every call they make are missing from the workspace -- the closed-world lints
/// (`unused_fn`) would report a sim-only helper as dead. They run only for a
/// [`Scope::WholeProject`] source set; the per-body lints run either way.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    WholeProject,
    /// Production sources only -- test/sim files were dropped before checking.
    ProductionOnly,
}

/// [`run_build_lints`] for a single-file build, which has no `SourceFiles` of
/// its own. `file` must be the same path the module was parsed under, or every
/// span lookup misses and the findings render without their snippet.
pub fn run_build_lints_single(ws: &Workspace, file: &str, src: &str) -> Vec<CompilerError> {
    let mut sources = SourceFiles { files: vec![] };
    sources.add_or_update_file(source_file_from_path_and_contents(
        file.to_string(),
        file.to_string(),
        src.to_string(),
    ));
    let root = Path::new(file).parent().unwrap_or(Path::new(".")).to_path_buf();
    run_build_lints(ws, &sources, &root, Scope::WholeProject)
}

/// Every finding in the catalog for this build, in workspace order.
pub fn findings(
    ws: &Workspace,
    sources: &SourceFiles,
    root: &Path,
    scope: Scope,
) -> Vec<bone_lints::Finding> {
    findings_with(ws, sources, root, scope, &bone_lints::LintSelection::default())
}

/// [`findings`] for an explicit lint selection. `rock lint --lints=<names>`
/// only: a BUILD always runs the default tier, so no invocation flag can drop
/// a lint the compiler considers non-optional.
pub fn findings_with(
    ws: &Workspace,
    sources: &SourceFiles,
    root: &Path,
    scope: Scope,
    sel: &bone_lints::LintSelection,
) -> Vec<bone_lints::Finding> {
    let texts = source_texts(sources);
    let warm = warm_module_ids(ws, &texts);
    let is_bin =
        scope == Scope::WholeProject && rock_host::stick::StickConfig::is_bin_at(root);
    // The REPORT set: this project's own files. A `stick.toml` DEPENDENCY
    // (`stone/engine` under a game) is scanned but never reported on -- its code
    // is not the author's to fix here, and a consumer must not fail a build on a
    // dependency's findings. Scanned rather than warm: an engine body calling
    // into game code is a real reachability edge, and dropping it would
    // false-flag the game's callbacks as dead. Restricting the set also keeps
    // the pass off the compile hot path -- a dependency's bodies are never
    // walked for the per-body lints.
    let owned = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let report: FxHashMap<Ustr, &str> = texts
        .iter()
        .filter(|(file, _)| Path::new(file.as_str()).starts_with(&owned))
        .map(|(&file, text)| (file, text.as_str()))
        .collect();
    bone_lints::lint_workspace(ws, &warm, is_bin, &report, sel)
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
    let detail =
        counts.iter().map(|(name, n)| format!("{n} {name}")).collect::<Vec<_>>().join(", ");
    format!(
        "{} lint warning(s) ({detail}) -- run `rock lint {}` for detail",
        soft.len(),
        root.display()
    )
}

/// The modules of `ws` that are NOT part of this build's user sources: the
/// stdlib, plus anything else warmed in. Read-only references to the author, so
/// no lint reports on them.
/// The `unused_fn` findings for files under `report_root`, judged against
/// `consumer`'s closed world. The union pass's per-consumer probe: it inverts
/// the usual report set (report on the DEPENDENCY, not the project) because the
/// question is what this consumer fails to reach in a library it depends on.
/// One consumer's answer is never a finding on its own -- see `lint_union`.
pub fn unused_fns_under(
    ws: &Workspace,
    sources: &SourceFiles,
    consumer: &Path,
    report_root: &Path,
) -> Vec<bone_lints::Finding> {
    let texts = source_texts(sources);
    let warm = warm_module_ids(ws, &texts);
    let is_bin = rock_host::stick::StickConfig::is_bin_at(consumer);
    let by_file = bone_lints::workspace_findings_by_file(ws, &warm, is_bin);
    let mut out = Vec::new();
    for (file, findings) in by_file {
        if !Path::new(file.as_str()).starts_with(report_root) {
            continue;
        }
        let src = texts.get(&file).map(String::as_str).unwrap_or("");
        out.extend(
            bone_lints::filter_suppressed_findings(findings, src)
                .into_iter()
                .filter(|f| f.lint == bone_lints::UNUSED_FN),
        );
    }
    out
}

fn warm_module_ids(ws: &Workspace, texts: &FxHashMap<Ustr, String>) -> FxHashSet<ModuleId> {
    ws.modules
        .iter()
        .filter(|m| m.id != ModuleId::builtin() && !texts.contains_key(&m.file))
        .map(|m| m.id)
        .collect()
}

fn source_texts(sources: &SourceFiles) -> FxHashMap<Ustr, String> {
    sources
        .files
        .iter()
        .map(|f| {
            let f = f.borrow();
            (Ustr::from(&f.file.to_id_path()), f.contents.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rock_core::CompilerError;
    use bone_core::prelude::Span;

    fn finding(lint: &'static str, level: CompilerErrorLevel) -> bone_lints::Finding {
        bone_lints::Finding {
            lint,
            error: CompilerError::new_with_label(Span::unknown(), format!("{lint} fired"))
                .with_level(level),
            line: 0,
        }
    }

    fn empty_sources() -> SourceFiles {
        SourceFiles { files: vec![] }
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
        let line = summary_line(&soft, Path::new("mag"));
        assert_eq!(
            line,
            "3 lint warning(s) (2 unused_fn, 1 unused_var) -- run `rock lint mag` for detail"
        );
    }

    #[test]
    fn policy_flags_map_to_report_modes() {
        set_policy(LintReport::Summary);
        assert_eq!(policy(), LintReport::Summary);
        set_corpus_mode();
        assert_eq!(policy(), LintReport::Off, "a corpus run silences the soft tiers");
        set_policy(LintReport::Full);
        assert_eq!(policy(), LintReport::Full);
    }
}
