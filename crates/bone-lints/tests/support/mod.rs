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
    "stone_get_mouse_position",
    "stone_get_global_mouse_position",
    "is_pad_down",
    "is_pad_pressed",
    "is_pad_released",
    "pad_axis_value",
];

pub static POLICY: ActionLayerPolicy = ActionLayerPolicy {
    production_root: "app",
    production_label: "App",
    action_prefix: "ACT_",
    raw_input_names: RAW_INPUT_NAMES,
    raw_input_classes: RAW_INPUT_CLASSES,
    registration_functions: &["refresh_action_defaults", "action_new"],
};
