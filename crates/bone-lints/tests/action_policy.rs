use bone_lints::{
    ACTION_CALLERS, ActionLayerPolicy, IncrementalLintCache, LintCacheUpdate, LintOptions,
    RAW_INPUT_CLASSIFICATION, RAW_INPUT_MIGRATION, lint_workspace,
};
use bone_semantic::prelude::*;

static POLICY: ActionLayerPolicy = ActionLayerPolicy {
    production_root: "game",
    production_label: "Example",
    action_prefix: "DO_",
    raw_input_names: &["read_button"],
    raw_input_classes: &["pointer"],
    registration_functions: &["register_controls"],
};

fn workspace(file: &str, source: &str) -> Workspace {
    let ast = skull_parser::parse_module_clean(source, Ustr::from(file)).unwrap();
    semantic_pass(&mut [ast]).unwrap()
}

#[test]
fn caller_policy_controls_root_prefix_registration_and_diagnostic() {
    let source = "global DO_MOVE: int = 1\nglobal OTHER: int = 2\nfn register_controls() { let value = DO_MOVE }\nfn main() { register_controls() }\n";
    let options = LintOptions::only([ACTION_CALLERS.to_owned()]).with_action_policy(Some(&POLICY));
    for (file, expected) in [
        ("game/player.rock", 1),
        ("other/player.rock", 0),
        ("game/tests/player.rock", 0),
        ("endgame/player.rock", 0),
    ] {
        let ws = workspace(file, source);
        let sources = [(Ustr::from(file), source)].into_iter().collect();
        let found = lint_workspace(&ws, &FxHashSet::default(), true, &sources, &options);
        assert_eq!(found.len(), expected, "{file}: {found:?}");
        if expected == 1 {
            assert_eq!(
                found[0].error.message,
                "production Example action `DO_MOVE` has no production caller"
            );
        }
    }
}

#[test]
fn raw_input_policy_controls_query_population_and_classes() {
    let file = "game/player.rock";
    let source = "extern fn read_button() -> bool\nextern fn other_button() -> bool\nfn main() {\n    // action-layer: pointer\n    if read_button() {}\n    if other_button() {}\n}\n";
    let ws = workspace(file, source);
    let sources = [(Ustr::from(file), source)].into_iter().collect();
    let options = LintOptions::only([
        RAW_INPUT_MIGRATION.to_owned(),
        RAW_INPUT_CLASSIFICATION.to_owned(),
    ])
    .with_action_policy(Some(&POLICY));
    let found = lint_workspace(&ws, &FxHashSet::default(), true, &sources, &options);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].lint, RAW_INPUT_MIGRATION);
    let unclassified = source.replace("action-layer: pointer", "action-layer: unknown");
    let ws = workspace(file, &unclassified);
    let sources = [(Ustr::from(file), unclassified.as_str())]
        .into_iter()
        .collect();
    let found = lint_workspace(&ws, &FxHashSet::default(), true, &sources, &options);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found
            .iter()
            .any(|finding| finding.lint == RAW_INPUT_CLASSIFICATION
                && finding.error.message.ends_with("marker: pointer"))
    );
}

#[test]
fn policy_changes_invalidate_incremental_action_findings() {
    let file = "game/player.rock";
    let source = "global DO_MOVE: int = 1\nfn main() {}\n";
    let ws = workspace(file, source);
    let sources = [(Ustr::from(file), source)].into_iter().collect();
    let plain = LintOptions::only([ACTION_CALLERS.to_owned()]);
    let configured = plain.clone().with_action_policy(Some(&POLICY));
    let mut cache = IncrementalLintCache::default();
    for options in [&configured, &plain, &configured] {
        let (cached, _) = cache.update(
            &ws,
            &FxHashSet::default(),
            true,
            &sources,
            options,
            &LintCacheUpdate::body_only([], [], []),
        );
        let fresh = lint_workspace(&ws, &FxHashSet::default(), true, &sources, options);
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].error.message, fresh[0].error.message);
        if options == &plain {
            assert_eq!(cached[0].error.message, "no action-layer policy configured");
        } else {
            assert_eq!(
                cached[0].error.message,
                "production Example action `DO_MOVE` has no production caller"
            );
        }
    }
}
