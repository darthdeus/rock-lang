use super::{Module, STONE_WINDOW};

pub const STONE_RENDERING: Module = Module {
    package: "stone-rendering",
    library: "stone",
    opaque_pointers: &[STONE_WINDOW],
    layouts: &[],
};

pub const STONE_RENDERING_SOURCES: &[(&str, &str)] = &[
    ("canvas.rs", "canvas"),
    ("cursor.rs", "cursor"),
    ("frame_stream.rs", "frame_stream"),
    ("gl_state.rs", "gl_state"),
    ("graphics_capture.rs", "graphics_capture"),
    ("imgui.rs", "imgui"),
    ("input.rs", "input"),
    ("mesh.rs", "mesh"),
    ("mesh_capture.rs", "mesh_capture"),
    ("native_ledger.rs", "native_ledger"),
    ("native_renderer.rs", "native_renderer"),
    ("pixel_readback.rs", "pixel_readback"),
    ("screenshot.rs", "screenshot"),
    ("shader.rs", "shader"),
    ("sprite.rs", "sprite"),
    ("text.rs", "text"),
    ("ui_submission_probe.rs", "ui_submission_probe"),
    ("uniform_block.rs", "uniform_block"),
];

pub const STONE_RENDERING_LAYOUT_FILES: Option<(&str, &str)> = None;
