//! The mag-content layer boundary.
//!
//! `mag-content/` is a `type = "lib"` stick package holding authored,
//! runtime-neutral level data: the document, its codec, and the catalogs the
//! codec resolves ids against (planning doc section 3, `mag-content`).
//!
//! The direction of the layer is enforced by the package graph, not here.
//! mag-content does not depend on mag, Rock resolves names over the dependency
//! closure only, and a content file that names `player` or `world_mobs` fails
//! to compile. What is left to this file is what the graph cannot say:
//!
//! 1. mag-content must keep NOT depending on mag. One line in a stick.toml
//!    would dissolve the whole boundary silently, so the manifest is asserted.
//! 2. mag-content does depend on stone, for IVec2, Color and math. That makes
//!    stone's runtime surface -- drawing, audio, the filesystem -- reachable
//!    again, so the subset of the old lint that covers it is kept.
//!
//! When the module system lands, 2 is replaced by real visibility rules.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn content_dir() -> PathBuf {
    workspace_root().join("mag-content")
}

fn content_sources() -> Vec<(PathBuf, String)> {
    let dir = content_dir();
    let mut out = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) == Some("rock") {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            out.push((path, text));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// A comment may name a banned symbol while explaining why it is banned; only
/// code counts.
fn is_code(line: &str) -> bool {
    let t = line.trim_start();
    !t.is_empty() && !t.starts_with("//")
}

/// The `[dependencies]` table of a stick manifest, as bare package names.
fn stick_dependencies(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_deps = false;
    for line in manifest.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = t == "[dependencies]";
            continue;
        }
        if !in_deps || t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = t.split_once('=') {
            names.push(name.trim().to_string());
        }
    }
    names
}

/// The layer is a package, and its direction is the package graph. This is the
/// one edit that would erase that without any other test noticing.
#[test]
fn mag_content_does_not_depend_on_the_game() {
    let manifest_path = content_dir().join("stick.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
    let deps = stick_dependencies(&manifest);
    assert!(
        !deps.is_empty(),
        "mag-content/stick.toml declares no dependencies at all -- this test would \
         pass vacuously if the manifest moved or changed shape"
    );
    for banned in ["mag", "Mag", "mag-agent", "lighting3d"] {
        assert!(
            !deps.iter().any(|d| d == banned),
            "mag-content must not depend on `{banned}`. The content layer is a leaf: \
             the game depends on the document, never the other way round. Declared: {deps:?}"
        );
    }
}

/// mag-content depends on stone, so the package graph alone no longer keeps a
/// window, a renderer, an audio device or the filesystem out of the layer.
#[test]
fn mag_content_stays_free_of_the_engine_runtime() {
    let sources = content_sources();
    assert!(
        !sources.is_empty(),
        "mag-content/ has no .rock files -- this gate would pass vacuously"
    );

    // Substrings: prefixes that name a whole subsystem of stone the content
    // layer must not reach, plus the bank it must never load itself.
    const BANNED_PREFIXES: &[&str] = &[
        "draw_", "sfx_", "vfx_", "audio_", "play_sound", "presentation_random",
        "read_file", "write_file", "file_read", "file_write", "load_image",
        // Loading a bank is the game's job; the catalog arrives installed.
        "MAG_VISUAL_BANK", "VisualBankData",
        // stone's live session state. `world_frozen()` is engine-side, so the
        // package graph does not keep it out, and a document that branched on
        // whether the game is paused would not be a document.
        "WORLD_MODE", "world_frozen", "editor_mode_enabled", "game_paused",
        "menu_active",
    ];

    let mut violations = Vec::new();
    for (path, text) in &sources {
        let rel = path.strip_prefix(workspace_root()).unwrap_or(path);
        for (n, line) in text.lines().enumerate() {
            if !is_code(line) {
                continue;
            }
            for p in BANNED_PREFIXES {
                if line.contains(p) {
                    violations.push(format!("{}:{}: {p} -- {}", rel.display(), n + 1, line.trim()));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "mag-content/ must stay decodable without a window, a renderer, an audio \
         device or the filesystem. A catalog it needs is INSTALLED by whoever has \
         one (mag_content_install_props / mag_content_install_floor_materials); it \
         is never loaded here:\n{}",
        violations.join("\n")
    );
}
