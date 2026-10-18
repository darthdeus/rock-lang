//! Under modules v1 a definition's name is its DefKey (`<pkg>::<module>::<name>`).
//! Lints that key on a declared name (`Task`, `main`) must read the declared
//! segment, or they go silent (or noisy) the moment a package converts.

mod support;
use bone_lints::{lint_workspace, task_null_abandon, unused_fns, LintOptions, ACTION_CALLERS};
use bone_semantic::prelude::*;

fn ws_v1(src: &str) -> Workspace {
    let mut module = skull_parser::parse_module_clean_with_dialect(
        src,
        Ustr::from("main.rock"),
        skull_parser::Dialect::V1,
    )
    .expect("parse");
    module.package = Some(skull_parser::PackageTag { id: 0, self_name: "lintv1".to_string() });
    semantic_pass(&mut [module]).expect("clean program typechecks")
}

#[test]
fn v1_task_null_abandon_still_fires() {
    let ws = ws_v1(
        r#"
struct Task<R = ()> { id: int }
global SLOT: Task? = null
fn abandon() { SLOT = null }
fn main() { SLOT = Task { id: 1 }; abandon() }
"#,
    );
    assert_eq!(task_null_abandon(&ws).len(), 1, "a v1 `Task?` slot set to null must be flagged");
}

#[test]
fn v1_main_is_a_reachability_root() {
    let ws = ws_v1("fn main() { used() }\nfn used() {}\nfn dead() {}");
    let mut names: Vec<String> = unused_fns(&ws).iter().map(|f| f.name.to_string()).collect();
    names.sort();
    assert_eq!(names, vec!["lintv1::dead".to_string()], "only `dead` is unused");
}

fn action_gaps(src: &str, dialect: skull_parser::Dialect) -> Vec<String> {
    let file = Ustr::from("app/actions.rock");
    let mut module = skull_parser::parse_module_clean_with_dialect(src, file, dialect).expect("parse");
    if dialect == skull_parser::Dialect::V1 {
        module.package = Some(skull_parser::PackageTag { id: 0, self_name: "lintv1".to_string() });
    }
    let ws = semantic_pass(&mut [module]).expect("clean program typechecks");
    let mut sources = FxHashMap::default();
    sources.insert(file, src);
    lint_workspace(
        &ws,
        &FxHashSet::default(),
        true,
        &sources,
        &LintOptions::only([ACTION_CALLERS.to_owned()]).with_action_policy(Some(&support::POLICY)),
    )
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

#[test]
fn legacy_action_registration_reads_are_not_callers() {
    assert_registration_is_not_a_caller(action_gaps(ACTION_REGISTRATION, skull_parser::Dialect::Legacy));
}

#[test]
fn v1_action_registration_reads_are_not_callers() {
    assert_registration_is_not_a_caller(action_gaps(ACTION_REGISTRATION, skull_parser::Dialect::V1));
}
