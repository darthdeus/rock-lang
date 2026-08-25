use bone_lints::{LintOptions, lint_workspace_with_census};
use bone_semantic::prelude::*;

const FILE: &str = "<lint-census>";

#[test]
fn census_has_nonzero_requests_for_empty_and_nonempty_families() {
    let src = r#"
fn helper() {
    let unread = 1
}

fn main() { helper() }
"#;
    let ast = skull_parser::parse_module_clean(src, Ustr::from(FILE)).expect("parse");
    let ws = semantic_pass(&mut [ast]).expect("semantic pass");
    let mut sources = FxHashMap::default();
    sources.insert(Ustr::from(FILE), src);

    let (findings, census) = lint_workspace_with_census(
        &ws,
        &FxHashSet::default(),
        true,
        &sources,
        &LintOptions::default(),
    );

    assert!(findings.iter().any(|f| f.lint == bone_lints::UNUSED_VAR));
    assert_eq!(census.full_workspace_passes, 1);
    assert!(census.flat.requests > 0);
    assert!(census.flat.modules_examined > 0);
    assert!(census.flat.bodies_examined > 0);
    assert!(census.inference.requests > 0);
    assert!(census.inference.modules_examined > 0);
    assert!(census.inference.bodies_examined > 0);
    assert!(census.suppression.requests > 0);
    assert!(census.backend_sibling.requests > 0);
    assert!(census.unused_fn.requests > 0);
    assert!(census.unused_fn.modules_examined > 0);
}

#[test]
fn census_counts_suppressed_findings_separately() {
    let src = r#"
fn main() {
    // rock: allow(unused_var)
    let unread = 1
}
"#;
    let ast = skull_parser::parse_module_clean(src, Ustr::from(FILE)).expect("parse");
    let ws = semantic_pass(&mut [ast]).expect("semantic pass");
    let mut sources = FxHashMap::default();
    sources.insert(Ustr::from(FILE), src);

    let (findings, census) = lint_workspace_with_census(
        &ws,
        &FxHashSet::default(),
        true,
        &sources,
        &LintOptions::default(),
    );

    assert!(findings.iter().all(|f| f.lint != bone_lints::UNUSED_VAR));
    assert!(census.suppression.requests > 0);
    assert!(census.suppression.findings_derived > 0);
    assert!(census.suppression.findings_suppressed > 0);
}
