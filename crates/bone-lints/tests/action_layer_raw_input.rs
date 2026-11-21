use crate::support;
use bone_lints::{Finding, LintOptions, RAW_INPUT_CLASSIFICATION, RAW_INPUT_MIGRATION};

const FILE: &str = "<action_layer_raw_input>";

fn findings(src: &str, options: &LintOptions) -> Vec<Finding> {
    let options = options.clone().with_action_policy(Some(&support::POLICY));
    crate::lint(&[(FILE, src)], &options, true)
}

fn census(src: &str) -> Vec<Finding> {
    findings(src, &LintOptions::only([RAW_INPUT_MIGRATION.to_string()]))
}

#[test]
fn an_allow_comment_suppresses_only_the_census_site() {
    let src = r#"
extern(lua) fn is_key_pressed(key: string) -> bool
fn gameplay() {
    // rock: allow(action_layer_raw_input)
    if is_key_pressed("q") { }
}
fn main() { gameplay() }
"#;
    assert!(census(src).is_empty());
    let both =
        LintOptions::only([RAW_INPUT_MIGRATION, RAW_INPUT_CLASSIFICATION].map(str::to_owned));
    let found = findings(src, &both);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].lint, RAW_INPUT_CLASSIFICATION);
    assert_eq!(found[0].line, 4);
}
