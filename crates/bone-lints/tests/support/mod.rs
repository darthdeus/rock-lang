// Legacy project policy for the existing action-layer fixtures.
use bone_lints::ActionLayerPolicy;
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
