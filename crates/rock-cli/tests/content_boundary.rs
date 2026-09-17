//! The mag-content layer boundary.
//!
//! `mag/content/` holds authored, runtime-neutral level data: the document
//! codec that the simulation, the authoring tools, the lint and every external
//! reader decode the same way (planning doc section 3, `mag-content`).
//!
//! Rock has one namespace and no imports today -- the module system is at M4 --
//! so nothing in the language stops a content file from reaching into the live
//! world. Without this test the directory is a diagram, not a boundary, and it
//! would rot within a month. When modules land this is replaced by real
//! visibility rules and the boundary will already be known to hold.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn content_sources() -> Vec<(PathBuf, String)> {
    let dir = workspace_root().join("mag").join("content");
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
    out
}

/// A comment may name a banned symbol while explaining why it is banned; only
/// code counts.
fn is_code(line: &str) -> bool {
    let t = line.trim_start();
    !t.is_empty() && !t.starts_with("//")
}

fn word_hit(line: &str, word: &str) -> bool {
    line.match_indices(word).any(|(i, _)| {
        let before = line[..i].chars().next_back();
        let after = line[i + word.len()..].chars().next();
        let boundary = |c: Option<char>| !matches!(c, Some(c) if c.is_alphanumeric() || c == '_');
        boundary(before) && boundary(after)
    })
}

#[test]
fn mag_content_layer_stays_runtime_free() {
    let sources = content_sources();
    assert!(
        !sources.is_empty(),
        "mag/content/ has no .rock files -- this gate would pass vacuously"
    );

    // Whole words, so `player` is caught but `player_spawn_names` as part of a
    // longer identifier is judged on its own.
    const BANNED_WORDS: &[&str] = &["player", "camera", "drawable_size", "mag_world_state"];
    // Substrings: prefixes that name a whole subsystem the content layer must
    // not reach.
    const BANNED_PREFIXES: &[&str] = &[
        "editor_", "EDITOR_", "world_mobs", "world_doors", "world_walls", "world_spaces",
        "world_fuses", "world_corpses", "draw_", "sfx_", "vfx_", "presentation_random",
        "read_file", "write_file", "file_read", "file_write",
    ];

    let mut violations = Vec::new();
    for (path, text) in &sources {
        let rel = path.strip_prefix(workspace_root()).unwrap_or(path);
        for (n, line) in text.lines().enumerate() {
            if !is_code(line) {
                continue;
            }
            for w in BANNED_WORDS {
                if word_hit(line, w) {
                    violations.push(format!("{}:{}: {w} -- {}", rel.display(), n + 1, line.trim()));
                }
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
        "mag/content/ must stay decodable without a live world, a camera, an editor \
         or the filesystem. Move the offending code to the layer that owns it rather \
         than widening this list:\n{}",
        violations.join("\n")
    );
}

/// A ratchet, not a ban. Decoding a tile still resolves a prop id through the
/// catalog, and the catalog is filled from the compiled visual bank
/// (`mag/props.rock`), so it only exists inside a running game. That is the one
/// reason `mag/content/` cannot yet be read by an offline tool, and it is the
/// next slice's work. This test fails if the debt grows rather than shrinks.
#[test]
fn mag_content_prop_catalog_dependency_does_not_grow() {
    const KNOWN: &[&str] = &[
        "prop_index_by_id",
        "prop_def_at",
        "prop_orient_wrap",
        // Reads `def.wall_mount` off the catalog to re-derive a mounted prop's
        // facing (mag/props.rock:334-340).
        "prop_wall_mount_reorient_in_asset",
    ];

    let mut found = Vec::new();
    for (path, text) in &content_sources() {
        let rel = path.strip_prefix(workspace_root()).unwrap_or(path);
        for (n, line) in text.lines().enumerate() {
            if !is_code(line) {
                continue;
            }
            // Calls and globals only. A local named `prop_id` is this layer's
            // own business; reaching `prop_index_by_id(...)` or `PROP_DEFS` is
            // the dependency being ratcheted.
            for (i, _) in line.match_indices("prop_").chain(line.match_indices("PROP_")) {
                let before = line[..i].chars().next_back();
                if matches!(before, Some(c) if c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let rest = &line[i..];
                let end = rest
                    .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .unwrap_or(rest.len());
                let name = &rest[..end];
                let is_call = rest[end..].starts_with('(');
                let is_global = name.starts_with("PROP_");
                if (is_call || is_global) && !KNOWN.contains(&name) {
                    found.push(format!("{}:{}: {name}", rel.display(), n + 1));
                }
            }
        }
    }

    assert!(
        found.is_empty(),
        "mag/content/ reaches further into the prop catalog than the known debt {KNOWN:?}. \
         The catalog comes from the compiled visual bank and does not exist outside a \
         running game, so every addition here moves an offline reader further away:\n{}",
        found.join("\n")
    );
}
