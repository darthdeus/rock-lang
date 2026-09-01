//! The catalog runner: the surface-facing entry points every consumer (LSP,
//! `rock lint`, the build pass) drives, so no surface assembles the lint list
//! itself. Harness mirrors unused_var.rs.

use bone_lints::{lint_workspace, Finding, LintOptions, KNOWN_LINTS};
use bone_semantic::prelude::*;
use rock_core::errors::CompilerErrorLevel;

const FILE: &str = "<catalog>";

fn parse(src: &str) -> skull_parser::Ast {
    skull_parser::parse_module_clean(src, Ustr::from(FILE)).expect("parse")
}

fn findings(src: &str) -> Vec<Finding> {
    findings_with(src, &LintOptions::default())
}

fn findings_with(src: &str, options: &LintOptions) -> Vec<Finding> {
    let ws = semantic_pass(&mut [parse(src)]).expect("clean program typechecks");
    let mut sources = FxHashMap::default();
    sources.insert(Ustr::from(FILE), src);
    lint_workspace(&ws, &FxHashSet::default(), true, &sources, options)
}

fn lints_of(src: &str) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = findings(src).iter().map(|f| f.lint).collect();
    names.sort_unstable();
    names
}

#[test]
fn runs_every_cost_class_in_one_call() {
    // One program carrying a flat-HIR finding (unused_var) and a whole-workspace
    // one (unused_fn). A runner that drives only one cost class misses the other.
    let src = "fn dead() -> int { 1 }\nfn main() {\n    let unread = 5\n}";
    assert_eq!(lints_of(src), vec!["unused_fn", "unused_var"]);
}

#[test]
fn findings_carry_their_lint_name_and_file() {
    let src = "fn main() {\n    let unread = 5\n}";
    let fs = findings(src);
    assert_eq!(fs.len(), 1, "one finding, got {fs:?}");
    assert_eq!(fs[0].lint, "unused_var");
    assert_eq!(fs[0].file().as_str(), FILE, "finding is attributed to its source file");
    assert!(KNOWN_LINTS.contains(&fs[0].lint), "every finding's name is in the registry");
}

#[test]
fn this_fixture_produces_only_soft_findings() {
    // This fixture reaches unused_fn and unused_var only. Error-level catalog
    // coverage belongs to each correctness lint's own discriminating fixture.
    let src = "fn dead() -> int { 1 }\nfn main() {\n    let unread = 5\n}";
    for f in findings(src) {
        assert!(
            matches!(f.level(), CompilerErrorLevel::Warning | CompilerErrorLevel::Advice),
            "{} is {:?}",
            f.lint,
            f.level()
        );
    }
}

#[test]
fn suppression_applies_to_every_cost_class() {
    // `allow` on the line above each finding; both the per-file (unused_var) and
    // the whole-workspace (unused_fn) class must honour it through the runner.
    let src = "// rock: allow(unused_fn)\nfn dead() -> int { 1 }\n\
               fn main() {\n    // rock: allow(unused_var)\n    let unread = 5\n}";
    assert!(findings(src).is_empty(), "got {:?}", lints_of(src));
}

#[test]
fn suppression_is_matched_per_lint_name() {
    // The wrong lint's `allow` must not silence a finding -- a name-agnostic
    // line filter would swallow both.
    let src = "// rock: allow(unused_var)\nfn dead() -> int { 1 }\nfn main() {}";
    assert_eq!(lints_of(src), vec!["unused_fn"]);
}

#[test]
fn unused_fn_is_gated_on_bin_packages() {
    let src = "fn dead() -> int { 1 }\nfn main() {}";
    let ws = semantic_pass(&mut [parse(src)]).expect("typechecks");
    let mut sources = FxHashMap::default();
    sources.insert(Ustr::from(FILE), src);
    let lib = lint_workspace(&ws, &FxHashSet::default(), false, &sources, &Default::default());
    assert!(lib.is_empty(), "a lib's public fn has no in-project caller, got {lib:?}");
}

#[test]
fn backend_sibling_mismatch_is_opt_in() {
    let src = "inline(lua) fn probe(x: int) -> int = \"return x\"\ninline(js) fn probe(x: float) -> int = \"return x;\"\nfn main() {}";
    let default_findings = findings(src);
    assert!(
        default_findings.iter().all(|f| f.lint != bone_lints::BACKEND_SIBLING_MISMATCH),
        "default lint selection reached backend_sibling_mismatch: {default_findings:?}"
    );
    let explicit = LintOptions::only([bone_lints::BACKEND_SIBLING_MISMATCH.to_owned()]);
    assert!(
        findings_with(src, &explicit)
            .iter()
            .any(|f| f.lint == bone_lints::BACKEND_SIBLING_MISMATCH),
        "explicit backend_sibling_mismatch selection produced no finding"
    );
}

#[test]
fn warm_modules_are_not_reported_on() {
    // A stdlib module is a read-only reference: scanned where a lint needs it,
    // never reported. Marking the only module warm must yield nothing.
    let src = "fn dead() -> int { 1 }\nfn main() {\n    let unread = 5\n}";
    let ws = semantic_pass(&mut [parse(src)]).expect("typechecks");
    let warm: FxHashSet<ModuleId> =
        ws.modules.iter().filter(|m| m.id != ModuleId::builtin()).map(|m| m.id).collect();
    let mut sources = FxHashMap::default();
    sources.insert(Ustr::from(FILE), src);
    assert!(lint_workspace(&ws, &warm, true, &sources, &Default::default()).is_empty());
}

#[test]
fn per_class_entry_points_skip_a_warm_module() {
    // The LSP calls the cost classes directly for ONE file (the user opened a
    // stdlib file), bypassing `lint_workspace`'s user-module filter, so each
    // class carries its own warm guard.
    let src = "fn main() {\n    let unread = 5\n}";
    let ws = semantic_pass(&mut [parse(src)]).expect("typechecks");
    let mid = ws
        .modules
        .iter()
        .map(|m| m.id)
        .find(|&id| id != ModuleId::builtin())
        .expect("one user module");
    assert_eq!(
        bone_lints::flat_findings(&ws, &FxHashSet::default(), mid, src).len(),
        1,
        "cold: the unused local is reported"
    );
    let warm: FxHashSet<ModuleId> = std::iter::once(mid).collect();
    assert!(bone_lints::flat_findings(&ws, &warm, mid, src).is_empty());
    assert!(bone_lints::inference_findings(&ws, &warm, mid, src).is_empty());
}
