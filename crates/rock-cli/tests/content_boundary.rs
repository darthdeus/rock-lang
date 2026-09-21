//! The mag-content layer boundary.
//!
//! `mag-content/` is a `type = "lib"` stick package holding authored,
//! runtime-neutral level data: the document, its codec, and the catalogs the
//! codec resolves ids against (planning doc section 3, `mag-content`).
//!
//! The direction of the layer is enforced by the package graph, not here.
//! mag-content does not depend on mag, Rock resolves names over the dependency
//! closure only, and a content file that names `player` or `world_mobs` fails
//! to compile. Its one dependency is stone-core -- the shared value types
//! without the engine runtime -- so there is no window, renderer, audio device
//! or filesystem in the closure either.
//!
//! What is left to this file is what the graph cannot say: the two manifest
//! edits that would dissolve the boundary without any other test noticing.
//! Adding `mag` to mag-content reverses the layer. Swapping stone-core back for
//! stone reopens the whole engine runtime, which is why stone-core carries the
//! purity test the closure rests on.

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

fn simulation_dir() -> PathBuf {
    workspace_root().join("mag-simulation")
}

fn stone_core_dir() -> PathBuf {
    workspace_root().join("stone-core")
}

fn rock_sources(dir: PathBuf) -> Vec<(PathBuf, String)> {
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
    assert!(
        !deps.iter().any(|d| d == "stone"),
        "mag-content must depend on stone-core, not stone. stone brings the window, \
         the renderer, the audio device and the filesystem back into the closure, and \
         the layer would then rest on a lint again. Declared: {deps:?}"
    );
    assert!(
        deps.iter().any(|d| d == "stone-core"),
        "mag-content is expected to reach its value types through stone-core; if that \
         edge is gone, the test above no longer means what it says. Declared: {deps:?}"
    );
}

#[test]
fn mag_simulation_has_a_renderer_free_package_closure() {
    let manifest_path = simulation_dir().join("stick.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
    let mut deps = stick_dependencies(&manifest);
    deps.sort();
    assert_eq!(
        deps,
        ["mag-content", "stone-core"],
        "each new simulation dependency needs a renderer-free closure review"
    );

    let game_manifest = std::fs::read_to_string(workspace_root().join("mag/stick.toml"))
        .expect("read MAG manifest");
    assert!(
        stick_dependencies(&game_manifest).iter().any(|d| d == "mag-simulation"),
        "MAG must consume the simulation package"
    );
}

/// The purity mag-content's closure rests on. stone-core is the shared value
/// types -- IVec2, Color, the math over them -- and nothing that needs a
/// session. Every package that wants those types without the engine depends on
/// it, so runtime code landing here would reopen the door for all of them at
/// once, silently.
#[test]
fn stone_core_stays_free_of_the_engine_runtime() {
    let sources = rock_sources(stone_core_dir());
    assert!(
        !sources.is_empty(),
        "stone-core/ has no .rock files -- this gate would pass vacuously"
    );

    // Substrings: prefixes that name a whole subsystem a value type must not
    // reach, plus the FFI and session state that would drag one in.
    const BANNED_PREFIXES: &[&str] = &[
        "draw_", "sfx_", "vfx_", "audio_", "play_sound", "_play_sound",
        "read_file", "write_file", "file_read", "file_write", "load_image",
        "@extern", "inline(c)", "WORLD_MODE", "world_frozen",
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
        "stone-core must stay usable without a window, a renderer, an audio \
         device or the filesystem. Code that needs a live session belongs in \
         stone, which depends on this package:\n{}",
        violations.join("\n")
    );
}
