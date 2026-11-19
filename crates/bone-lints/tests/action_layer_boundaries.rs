use crate::support;
use bone_lints::{
    ACTION_CALLERS, Finding, LintOptions, RAW_INPUT_CLASSIFICATION, RAW_INPUT_MIGRATION,
};

fn findings(file: &str, src: &str, lint: &str) -> Vec<Finding> {
    let options = LintOptions::only([lint.to_owned()]).with_action_policy(Some(&support::POLICY));
    crate::lint(&[(file, src)], &options, true)
}

#[test]
fn action_caller_census_has_a_nonzero_population_and_exact_zero_names() {
    let src = r#"
global ACT_USED: int = 1
global ACT_MISSING: int = 2

fn gameplay() {
    let value = ACT_USED
}

fn main() { gameplay() }
"#;
    let found = findings("app/player.rock", src, ACTION_CALLERS);
    assert_eq!(found.len(), 1, "caller gaps: {found:?}");
    assert!(found[0].error.message.contains("ACT_MISSING"));
    assert!(!found[0].error.message.contains("ACT_USED"));
}

#[test]
fn caller_census_rejects_an_empty_production_declaration_population() {
    let found = findings("app/empty.rock", "fn main() {}", ACTION_CALLERS);
    assert_eq!(
        found.len(),
        1,
        "empty population must not be 0/0: {found:?}"
    );
    assert!(
        found[0]
            .error
            .message
            .contains("no production App ACT_* declarations")
    );
}

#[test]
fn raw_input_classification_is_explicit_and_does_not_hide_the_live_census() {
    let src = r#"
extern(lua) fn is_key_pressed(key: string) -> bool

fn gameplay() {
    // action-layer: editor_tool_debug
    if is_key_pressed("q") { }
}

fn main() { gameplay() }
"#;
    let classified = findings("app/editor_tools.rock", src, RAW_INPUT_CLASSIFICATION);
    assert!(classified.is_empty(), "classified site: {classified:?}");

    let census = findings("app/editor_tools.rock", src, RAW_INPUT_MIGRATION);
    assert_eq!(
        census.len(),
        1,
        "the live census remains unsuppressed: {census:?}"
    );

    let mutated = src.replace("// action-layer: editor_tool_debug\n", "");
    let found = findings("app/editor_tools.rock", &mutated, RAW_INPUT_CLASSIFICATION);
    assert_eq!(found.len(), 1, "removed classification marker: {found:?}");
}

#[test]
fn raw_input_classification_rejects_multiple_queries_hidden_by_one_marker() {
    let src = r#"
extern(lua) fn is_key_down(key: string) -> bool

fn gameplay() {
    // action-layer: editor_tool_debug
    if is_key_down("q") || is_key_down("w") { }
}

fn main() { gameplay() }
"#;
    let found = findings("app/editor_tools.rock", src, RAW_INPUT_CLASSIFICATION);
    assert_eq!(
        found.len(),
        2,
        "one marker cannot classify two sites: {found:?}"
    );
}
