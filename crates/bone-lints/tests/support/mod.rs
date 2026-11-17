use bone_lints::{ActionLayerPolicy, Finding, LintOptions};
use bone_semantic::prelude::*;

pub fn parse(file: &str, source: &str) -> skull_parser::Ast {
    let mut module = skull_parser::parse_module_clean(source, Ustr::from(file)).expect("parse");
    module.package = Some(skull_parser::PackageTag {
        id: 1,
        self_name: "@/<lint>".to_string(),
        identity_storage: skull_parser::IdentityStorage::Exempt,
        root: String::new(),
    });
    module
}

pub fn workspace(modules: Vec<skull_parser::Ast>) -> Workspace {
    let mut modules = skull_parser::stdlib::ModuleSet::with_stdlib(modules).expect("stdlib");
    bone_semantic::semantic_passes::semantic_pass_without_closure_gate(
        &mut modules,
        &Default::default(),
    )
    .expect("semantic pass")
}

pub fn warm(ws: &Workspace) -> FxHashSet<ModuleId> {
    ws.modules
        .iter()
        .filter(|module| bone_core::modules::is_stdlib_module_file(module.file.as_str()))
        .map(|module| module.id)
        .collect()
}

pub fn setup<'a>(files: &[(&str, &'a str)]) -> (Workspace, FxHashMap<Ustr, &'a str>) {
    let modules = files.iter().map(|(file, src)| parse(file, src)).collect();
    let sources = files
        .iter()
        .map(|&(file, src)| (Ustr::from(file), src))
        .collect();
    (workspace(modules), sources)
}

pub fn checked_lint(files: &[(&str, &str)], options: &LintOptions) -> Vec<Finding> {
    let (ws, sources) = setup(files);
    let warm = warm(&ws);
    for module in ws.user_modules(&warm) {
        assert!(
            module.interface_errors.is_empty(),
            "{:?}",
            module.interface_errors
        );
        assert!(
            module.lowering_errors.values().all(Vec::is_empty),
            "{:?}",
            module.lowering_errors
        );
        if let Some(inference) = ws.inference(module.id) {
            assert!(
                inference.errors.values().all(Vec::is_empty),
                "{:?}",
                inference.errors
            );
        }
    }
    bone_lints::lint_workspace(&ws, &warm, true, &sources, options)
}

/// Lint `files` as a bin package, selecting only `lint`.
pub fn lint_only(files: &[(&str, &str)], lint: &str) -> Vec<Finding> {
    crate::lint(files, &LintOptions::only([lint.to_owned()]), true)
}

/// The first backtick-quoted name in a finding's message.
pub fn quoted(finding: &Finding) -> String {
    let msg = &finding.error.message;
    let start = msg.find('`').expect("a quoted name") + 1;
    let len = msg[start..].find('`').expect("a closing backtick");
    msg[start..start + len].to_owned()
}

// Legacy project policy for the existing action-layer fixtures.
pub const RAW_INPUT_CLASSES: &[&str] = &[
    "editor_tool_debug",
    "deliberate_pointer",
    "splash_skip",
    "test_only",
];

const RAW_INPUT_NAMES: &[&str] = &[
    "is_key_down",
    "is_key_pressed",
    "is_key_released",
    "is_scancode_down",
    "is_scancode_pressed",
    "is_scancode_released",
    "is_mouse_down",
    "is_mouse_pressed",
    "is_mouse_released",
    "mouse_screen",
    "host_get_mouse_position",
    "host_get_global_mouse_position",
    "is_pad_down",
    "is_pad_pressed",
    "is_pad_released",
    "pad_axis_value",
];

pub static POLICY: std::sync::LazyLock<ActionLayerPolicy> =
    std::sync::LazyLock::new(|| ActionLayerPolicy {
        production_root: "app".into(),
        production_label: "App".into(),
        action_prefix: "ACT_".into(),
        raw_input_names: RAW_INPUT_NAMES.iter().map(|s| (*s).to_owned()).collect(),
        raw_input_classes: RAW_INPUT_CLASSES.iter().map(|s| (*s).to_owned()).collect(),
        registration_functions: ["refresh_action_defaults", "action_new"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
    });
