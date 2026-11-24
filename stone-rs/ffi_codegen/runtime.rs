use super::{Layout, Module, STONE_WINDOW};

pub const STONE_ENGINE: Module = Module {
    package: "stone",
    library: "stone",
    opaque_pointers: &[STONE_WINDOW, ("StoneEvent", "crate::StoneEvent")],
    layouts: &[
        Layout {
            rock: "GraphicsStats",
            rust: "GraphicsStats",
        },
        Layout {
            rock: "ImageLoadStats",
            rust: "ImageLoadStats",
        },
    ],
};

pub const STONE_ENGINE_SOURCES: &[(&str, &str)] = &[
    ("lib.rs", ""),
    ("audio.rs", "audio"),
    ("asset_watcher.rs", "asset_watcher"),
    ("file_jobs.rs", "file_jobs"),
    ("filesystem.rs", "filesystem"),
    ("image_data.rs", "image_data"),
    ("image_publication.rs", "image_publication"),
    ("image_texture.rs", "image_texture"),
    ("jit_health.rs", "jit_health"),
    ("log.rs", "log"),
    ("system_report.rs", "system_report"),
];

pub const STONE_ENGINE_LAYOUT_FILES: Option<(&str, &str)> =
    Some(("wire_layouts.rock", "tests/ffi_layouts.rock"));
