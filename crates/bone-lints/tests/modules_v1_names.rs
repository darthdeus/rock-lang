//! Under modules v1 a definition's name is its DefKey (`<pkg>::<module>::<name>`).
//! Lints that key on a declared name (`Task`, `main`) must read the declared
//! segment, or they go silent (or noisy) the moment a package converts.

use crate::support;
use bone_lints::{ACTION_CALLERS, Finding, LintOptions, UNUSED_FN};
use bone_semantic::prelude::*;

fn lint(src: &str, file: &str, options: &LintOptions) -> Vec<Finding> {
    let mut module = skull_parser::parse_module_clean(src, Ustr::from(file)).expect("parse");
    module.package = Some(skull_parser::PackageTag {
        id: 0,
        self_name: "lintv1".to_string(),
        identity_storage: Default::default(),
        root: String::new(),
    });
    crate::lint_asts(&mut [module], &[(file, src)], options, true)
}

fn action_gaps(src: &str) -> Vec<String> {
    let options =
        LintOptions::only([ACTION_CALLERS.to_owned()]).with_action_policy(Some(&support::POLICY));
    lint(src, "app/actions.rock", &options)
        .iter()
        .map(|finding| finding.error.message.clone())
        .collect()
}

// The configured `action_new` and `refresh_action_defaults` register actions;
// reading an `ACT_*` there is not a production caller.
const ACTION_REGISTRATION: &str = r#"
global ACT_REGISTERED: int = 1
global ACT_REFRESHED: int = 2
global ACT_USED: int = 3
fn action_new() { let v = ACT_REGISTERED }
fn refresh_action_defaults() { let v = ACT_REFRESHED }
fn gameplay() { let v = ACT_USED }
fn main() { action_new(); refresh_action_defaults(); gameplay() }
"#;

fn assert_registration_is_not_a_caller(gaps: Vec<String>) {
    let mut gaps = gaps;
    gaps.sort();
    assert_eq!(gaps.len(), 2, "gaps: {gaps:?}");
    assert!(gaps[0].contains("ACT_REFRESHED`"), "{gaps:?}");
    assert!(gaps[1].contains("ACT_REGISTERED`"), "{gaps:?}");
}
