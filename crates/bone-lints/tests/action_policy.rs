use bone_lints::{
    ACTION_CALLERS, ActionLayerPolicy, Finding, IncrementalLintCache, LintCacheUpdate, LintOptions,
    RAW_INPUT_CLASSIFICATION, RAW_INPUT_MIGRATION, lint_workspace,
};
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

const FILES: [(&str, &str); 2] = [
    ("game/player.rock", "fn main() {}\n"),
    ("game/input.rock", "fn input() {}\n"),
];

fn policy_workspace() -> Workspace {
    let mut asts = FILES
        .map(|(file, source)| skull_parser::parse_module_clean(source, Ustr::from(file)).unwrap());
    semantic_pass(&mut asts).unwrap()
}

#[test]
fn missing_policy_notice_is_once_per_workspace_lint() {
    let ws = policy_workspace();
    let sources = FILES
        .map(|(file, source)| (Ustr::from(file), source))
        .into_iter()
        .collect();
    let lints = [
        ACTION_CALLERS,
        RAW_INPUT_MIGRATION,
        RAW_INPUT_CLASSIFICATION,
    ];
    for selection in [&lints[..1], &lints[1..2], &lints[2..], &lints[..]] {
        let options = LintOptions::only(selection.iter().map(|lint| (*lint).to_owned()));
        let found = lint_workspace(&ws, &FxHashSet::default(), true, &sources, &options);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].error.message, "no action-layer policy configured");
    }
}

#[test]
fn incremental_missing_policy_notice_survives_updates_and_report_changes() {
    let ws = policy_workspace();
    let all_sources: FxHashMap<_, _> = FILES
        .map(|(file, source)| (Ustr::from(file), source))
        .into_iter()
        .collect();
    let mut cache = IncrementalLintCache::default();
    let plain = LintOptions::only(
        [
            ACTION_CALLERS,
            RAW_INPUT_MIGRATION,
            RAW_INPUT_CLASSIFICATION,
        ]
        .map(str::to_owned),
    );
    let configured = plain.clone().with_action_policy(Some(&POLICY));
    let disabled = LintOptions::only([]);
    let mut sources = all_sources.clone();
    for (options, change, report_second_only) in [
        (&plain, LintCacheUpdate::full(), false),
        (&plain, LintCacheUpdate::body_only([], [], []), false),
        (
            &plain,
            LintCacheUpdate::body_only([Ustr::from(FILES[0].0)], [], []),
            false,
        ),
        (&plain, LintCacheUpdate::body_only([], [], []), true),
        (&configured, LintCacheUpdate::body_only([], [], []), false),
        (&plain, LintCacheUpdate::body_only([], [], []), false),
        (&disabled, LintCacheUpdate::body_only([], [], []), false),
        (&plain, LintCacheUpdate::body_only([], [], []), false),
    ] {
        sources.clone_from(&all_sources);
        if report_second_only {
            sources.remove(&Ustr::from(FILES[0].0));
        }
        let (cached, _) =
            cache.update(&ws, &FxHashSet::default(), true, &sources, options, &change);
        let fresh = lint_workspace(&ws, &FxHashSet::default(), true, &sources, options);
        assert_eq!(format!("{cached:?}"), format!("{fresh:?}"));
        let notices = cached
            .iter()
            .filter(|finding| finding.error.message == "no action-layer policy configured")
            .count();
        assert_eq!(notices, usize::from(options == &plain), "{cached:?}");
        let per_file: usize = FILES
            .iter()
            .map(|(file, _)| cache.findings_for_file(Ustr::from(file)).len())
            .sum();
        assert_eq!(per_file, cached.len());
    }
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

#[test]
fn raw_input_policy_controls_query_population_and_classes() {
    let file = "game/player.rock";
    let source = "extern(lua) fn read_button() -> bool\nextern(lua) fn other_button() -> bool\nfn main() {\n    // action-layer: pointer\n    if read_button() {}\n    if other_button() {}\n}\n";
    let options = LintOptions::only([
        RAW_INPUT_MIGRATION.to_owned(),
        RAW_INPUT_CLASSIFICATION.to_owned(),
    ])
    .with_action_policy(Some(&POLICY));
    let found = lint_one(file, source, &options);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].lint, RAW_INPUT_MIGRATION);
    let unclassified = source.replace("action-layer: pointer", "action-layer: unknown");
    let found = lint_one(file, &unclassified, &options);
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

#[test]
fn allow_comments_never_hide_the_missing_policy_notice_on_either_path() {
    let file = "game/player.rock";
    let source =
        format!("// rock: allow({ACTION_CALLERS})\nglobal DO_MOVE: int = 1\nfn main() {{}}\n");
    let source = source.as_str();
    let ws = workspace(file, source);
    let sources = [(Ustr::from(file), source)].into_iter().collect();
    let options = LintOptions::only([ACTION_CALLERS.to_owned()]);
    let fresh = lint_workspace(&ws, &FxHashSet::default(), true, &sources, &options);
    let (cached, _) = IncrementalLintCache::default().update(
        &ws,
        &FxHashSet::default(),
        true,
        &sources,
        &options,
        &LintCacheUpdate::full(),
    );
    for found in [&fresh, &cached] {
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].error.message, "no action-layer policy configured");
    }
}
