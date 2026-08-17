//! The Phase 6 raw-input census must inspect typechecked HIR call sites, not a
//! markdown or source-text inventory.

use bone_lints::{Finding, LintOptions, RAW_INPUT_MIGRATION, lint_workspace};
use bone_semantic::prelude::*;

const FILE: &str = "<action_layer_raw_input>";

fn findings(src: &str, options: &LintOptions) -> Vec<Finding> {
    let ast = skull_parser::parse_module_clean(src, Ustr::from(FILE)).expect("parse");
    let ws = semantic_pass(&mut [ast]).expect("clean program typechecks");
    let mut sources = FxHashMap::default();
    sources.insert(Ustr::from(FILE), src);
    lint_workspace(&ws, &FxHashSet::default(), true, &sources, options)
}

fn census(src: &str) -> Vec<Finding> {
    findings(src, &LintOptions::only([RAW_INPUT_MIGRATION.to_string()]))
}

#[test]
fn finds_typechecked_raw_keyboard_and_mouse_sites() {
    let src = r#"
extern fn is_key_pressed(key: string) -> bool
extern fn is_mouse_down(button: int) -> bool

fn gameplay() {
    if is_key_pressed("q") && is_mouse_down(1) { }
}

fn main() { gameplay() }
"#;
    let found = census(src);
    assert_eq!(found.len(), 2, "raw census findings: {found:?}");
    assert!(found.iter().any(|f| f.error.message.contains("is_key_pressed")));
    assert!(found.iter().any(|f| f.error.message.contains("is_mouse_down")));
}

#[test]
fn action_queries_are_the_negative_control_and_default_is_silent() {
    let src = r#"
struct Action { }
fn Action.down(a) -> bool { true }
fn gameplay(a: Action) {
    if a.down() { }
}
fn main() { gameplay(Action {}) }
"#;
    assert!(census(src).is_empty(), "action query must not be a raw site");
    let default_findings = findings(src, &LintOptions::default());
    assert!(default_findings.iter().all(|f| f.lint != RAW_INPUT_MIGRATION));
}

#[test]
fn an_allow_comment_suppresses_only_the_census_site() {
    let src = r#"
extern fn is_key_pressed(key: string) -> bool
fn gameplay() {
    // rock: allow(action_layer_raw_input)
    if is_key_pressed("q") { }
}
fn main() { gameplay() }
"#;
    assert!(census(src).is_empty());
}
