use bone_lints::{ACTION_CALLERS, ActionLayerPolicy, Finding, LintOptions, lint_workspace};
use bone_semantic::prelude::*;

static POLICY: std::sync::LazyLock<ActionLayerPolicy> =
    std::sync::LazyLock::new(|| ActionLayerPolicy {
        production_root: "game".into(),
        production_label: "Example".into(),
        action_prefix: "DO_".into(),
        raw_input_names: ["read_button"].iter().map(|s| (*s).to_owned()).collect(),
        raw_input_classes: ["pointer"].iter().map(|s| (*s).to_owned()).collect(),
        registration_functions: ["register_controls"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
    });

fn workspace(file: &str, source: &str) -> Workspace {
    let ast = skull_parser::parse_module_clean(source, Ustr::from(file)).unwrap();
    semantic_pass(&mut [ast]).unwrap()
}

fn lint_one(file: &str, source: &str, options: &LintOptions) -> Vec<Finding> {
    let ws = workspace(file, source);
    let sources = [(Ustr::from(file), source)].into_iter().collect();
    lint_workspace(&ws, &FxHashSet::default(), true, &sources, options)
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
        let found = lint_one(file, source, &options);
        assert_eq!(found.len(), expected, "{file}: {found:?}");
        if expected == 1 {
            assert_eq!(
                found[0].error.message,
                "production Example action `DO_MOVE` has no production caller"
            );
        }
    }
}
