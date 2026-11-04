use crate::support;
use bone_lints::{
    lint_workspace, Finding, LintOptions, ACTION_CALLERS, RAW_INPUT_CLASSIFICATION,
    RAW_INPUT_MIGRATION,
};
use bone_semantic::prelude::*;

fn findings(file: &str, src: &str, lint: &str) -> Vec<Finding> {
    let ast = skull_parser::parse_module_clean(src, Ustr::from(file)).expect("parse");
    let ws = semantic_pass(&mut [ast]).expect("clean program typechecks");
    let mut sources = FxHashMap::default();
    sources.insert(Ustr::from(file), src);
    lint_workspace(
        &ws,
        &FxHashSet::default(),
        true,
        &sources,
        &LintOptions::only([lint.to_owned()]).with_action_policy(Some(&support::POLICY)),
    )
}

fn quoted_after(line: &str, marker: &str) -> Option<String> {
    let start = line.find(marker)? + marker.len();
    let end = line[start..].find('"')?;
    Some(line[start..start + end].to_owned())
}

/// One ACT declaration line as its lowercased bare name and registered key.
fn action_declaration(line: &str) -> Option<(String, String)> {
    let rest = ["global ", "const "]
        .iter()
        .find_map(|keyword| line.strip_prefix(keyword))?;
    let name = rest.strip_prefix("ACT_")?.split_once(':')?.0.to_lowercase();
    Some((name, quoted_after(line, "= app_action(\"")?))
}

#[test]
fn the_action_census_reads_both_declaration_spellings() {
    assert_eq!(
        action_declaration("global ACT_MOVE: AppAction = app_action(\"move\")"),
        Some(("move".to_owned(), "move".to_owned()))
    );
    assert_eq!(
        action_declaration("const ACT_MOVE: AppAction = app_action(\"move\")"),
        Some(("move".to_owned(), "move".to_owned())),
        "a const ACT_* declaration is still an action declaration"
    );
    assert_eq!(action_declaration("global OTHER: int = 1"), None);
    assert_eq!(action_declaration("    global ACT_MOVE: AppAction = app_action(\"move\")"), None);
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
fn removing_one_production_read_is_a_load_bearing_mutation() {
    let src = r#"
global ACT_USED: int = 1

fn gameplay() {
    let value = ACT_USED
}

fn main() { gameplay() }
"#;
    assert!(findings("app/player.rock", src, ACTION_CALLERS).is_empty());

    let mutated = src.replace("let value = ACT_USED", "let value = 0");
    let found = findings("app/player.rock", &mutated, ACTION_CALLERS);
    assert_eq!(found.len(), 1, "mutated caller census: {found:?}");
    assert!(found[0].error.message.contains("ACT_USED"));
}

#[test]
fn caller_census_rejects_an_empty_production_declaration_population() {
    let found = findings("app/empty.rock", "fn main() {}", ACTION_CALLERS);
    assert_eq!(
        found.len(),
        1,
        "empty population must not be 0/0: {found:?}"
    );
    assert!(found[0]
        .error
        .message
        .contains("no production App ACT_* declarations"));
}

#[test]
fn raw_input_classification_is_explicit_and_does_not_hide_the_live_census() {
    let src = r#"
extern fn is_key_pressed(key: string) -> bool

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
fn raw_input_classification_rejects_unknown_classes() {
    let src = r#"
extern fn is_key_pressed(key: string) -> bool

fn gameplay() {
    // action-layer: gameplay_exception
    if is_key_pressed("q") { }
}

fn main() { gameplay() }
"#;
    let found = findings("app/player.rock", src, RAW_INPUT_CLASSIFICATION);
    assert_eq!(found.len(), 1, "unknown class must fail closed: {found:?}");
}

#[test]
fn raw_input_classification_rejects_multiple_queries_hidden_by_one_marker() {
    let src = r#"
extern fn is_key_down(key: string) -> bool

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
