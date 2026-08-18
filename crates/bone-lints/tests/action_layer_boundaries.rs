use bone_lints::{
    lint_workspace, Finding, LintOptions, ACTION_CALLERS, RAW_INPUT_CLASSIFICATION,
    RAW_INPUT_MIGRATION,
};
use bone_semantic::prelude::*;
use std::fs;
use std::path::Path;

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
        &LintOptions::only([lint.to_owned()]),
    )
}

fn quoted_after(line: &str, marker: &str) -> Option<String> {
    let start = line.find(marker)? + marker.len();
    let end = line[start..].find('"')?;
    Some(line[start..start + end].to_owned())
}

#[test]
fn mag_action_globals_are_keyed_to_the_canonical_specs() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../mag/actions.rock");
    let source = fs::read_to_string(path).expect("mag action source");
    let specs: Vec<String> = source
        .lines()
        .filter_map(|line| quoted_after(line, "MagActionSpec { name: \""))
        .collect();
    let globals: Vec<(String, String)> = source
        .lines()
        .filter(|line| line.starts_with("global ACT_"))
        .filter_map(|line| {
            let global = line
                .strip_prefix("global ACT_")?
                .split_once(':')?
                .0
                .to_lowercase();
            Some((global, quoted_after(line, "= mag_action(\"")?))
        })
        .collect();

    assert_eq!(specs.len(), 43, "canonical spec population must be exact");
    assert_eq!(
        globals.len(),
        43,
        "production global population must be exact"
    );
    assert_eq!(specs.iter().collect::<FxHashSet<_>>().len(), 43);
    assert_eq!(
        globals
            .iter()
            .map(|(_, key)| key)
            .collect::<FxHashSet<_>>()
            .len(),
        43
    );
    let mut reordered_specs = specs.clone();
    reordered_specs.reverse();
    for (global_name, key) in &globals {
        assert_eq!(
            global_name, key,
            "global identity must be keyed by its action name"
        );
        assert!(
            reordered_specs.contains(key),
            "global key is not a canonical spec: {key}"
        );
    }
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
    let found = findings("mag/player.rock", src, ACTION_CALLERS);
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
    assert!(findings("mag/player.rock", src, ACTION_CALLERS).is_empty());

    let mutated = src.replace("let value = ACT_USED", "let value = 0");
    let found = findings("mag/player.rock", &mutated, ACTION_CALLERS);
    assert_eq!(found.len(), 1, "mutated caller census: {found:?}");
    assert!(found[0].error.message.contains("ACT_USED"));
}

#[test]
fn caller_census_rejects_an_empty_production_declaration_population() {
    let found = findings("mag/empty.rock", "fn main() {}", ACTION_CALLERS);
    assert_eq!(
        found.len(),
        1,
        "empty population must not be 0/0: {found:?}"
    );
    assert!(found[0]
        .error
        .message
        .contains("no production Mag ACT_* declarations"));
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
    let classified = findings("mag/editor_tools.rock", src, RAW_INPUT_CLASSIFICATION);
    assert!(classified.is_empty(), "classified site: {classified:?}");

    let census = findings("mag/editor_tools.rock", src, RAW_INPUT_MIGRATION);
    assert_eq!(
        census.len(),
        1,
        "the live census remains unsuppressed: {census:?}"
    );

    let mutated = src.replace("// action-layer: editor_tool_debug\n", "");
    let found = findings("mag/editor_tools.rock", &mutated, RAW_INPUT_CLASSIFICATION);
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
    let found = findings("mag/player.rock", src, RAW_INPUT_CLASSIFICATION);
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
    let found = findings("mag/editor_tools.rock", src, RAW_INPUT_CLASSIFICATION);
    assert_eq!(
        found.len(),
        2,
        "one marker cannot classify two sites: {found:?}"
    );
}
