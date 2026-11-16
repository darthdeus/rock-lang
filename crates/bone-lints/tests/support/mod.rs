use bone_lints::{ActionLayerPolicy, Finding, LintOptions, lint_workspace};
use bone_semantic::prelude::*;

/// What `lint_workspace` reports for `lint` alone over `files` (name, source),
/// linted as a bin package with no warm module.
pub fn lint_only(files: &[(&str, &str)], lint: &str) -> Vec<Finding> {
    let mut asts: Vec<skull_parser::Ast> = files
        .iter()
        .map(|(name, src)| skull_parser::parse_module_clean(src, Ustr::from(*name)).expect("parse"))
        .collect();
    let ws = semantic_pass(&mut asts).expect("clean program typechecks");
    let sources: FxHashMap<Ustr, &str> = files
        .iter()
        .map(|(name, src)| (Ustr::from(*name), *src))
        .collect();
    lint_workspace(
        &ws,
        &FxHashSet::default(),
        true,
        &sources,
        &LintOptions::only([lint.to_owned()]),
    )
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
