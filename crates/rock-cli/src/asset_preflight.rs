//! Declared runtime files must exist before a sim or a cached gate can pass.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub fn check_project(project: &Path) -> Result<()> {
    configured_check(project, false)
}

pub(crate) fn check_sim_project(project: &Path) -> Result<()> {
    configured_check(project, true)
}

fn configured_check(project: &Path, mag_sim_only: bool) -> Result<()> {
    let config = rock_host::stick::StickConfig::from_path_optional(&project.join("stick.toml"))?;
    if mag_sim_only && !config.as_ref().is_some_and(|config| config.package.name == "mag") {
        return Ok(());
    }
    let overlays = rock_host::release::asset_overlay_roots(project, config.as_ref())?;
    check_with_roots(project, config.as_ref(), &overlays)
}

fn content_keys(value: &serde_json::Value, keys: &mut std::collections::BTreeMap<String, bool>) {
    match value {
        serde_json::Value::String(key) if loose_runtime_path(key) => {
            keys.entry(key.clone()).or_insert(true);
        }
        serde_json::Value::Array(values) => {
            for value in values { content_keys(value, keys); }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() { content_keys(value, keys); }
        }
        _ => {}
    }
}

fn loose_runtime_path(key: &str) -> bool {
    ["particles", "sfx", "ui", "vfx", "music"].iter()
        .any(|directory| key.starts_with(&format!("assets/{directory}/")))
        && !key.contains('{') && Path::new(key).extension().is_some()
}

fn check_with_roots(
    project: &Path,
    config: Option<&rock_host::stick::StickConfig>,
    overlays: &[PathBuf],
) -> Result<()> {
    let mut keys = std::collections::BTreeMap::new();
    if let Some(config) = config {
        for key in config.release_overlay_assets()? { keys.insert(key, true); }
    }
    let sound_path = rock_host::release::sound_project_path(project, config)?;
    if let Some(path) = sound_path.as_ref() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("asset preflight: read {}", path.display()))?;
        let sound = sound_core::parse_project(&text)
            .with_context(|| format!("asset preflight: parse {}", path.display()))?;
        for event in sound.events {
            for step in event.steps {
                for layer in step.layers {
                    for key in layer.sources {
                        let overlay_only = loose_runtime_path(&key);
                        keys.entry(key).or_insert(overlay_only);
                    }
                }
            }
        }
    }
    // Assets are data-driven. Inspect concrete content references as well as
    // release declarations; test snippets and generated assets are not inputs.
    let pattern = regex_lite::Regex::new(r#""(assets/(?:particles|sfx|ui|vfx|music)/[^"{}]+)""#)?;
    for entry in walkdir::WalkDir::new(project).into_iter().filter_entry(|entry| {
        entry.depth() == 0 || !entry.file_type().is_dir() ||
            !matches!(entry.file_name().to_str(), Some("assets" | "tests" | "tmp" | "target" | "saves"))
                && !entry.file_name().to_string_lossy().starts_with('.')
                && !overlays.iter().any(|root| entry.path() == root)
    }) {
        let entry = entry.with_context(|| format!("asset preflight: scan {}", project.display()))?;
        if !entry.file_type().is_file() { continue; }
        let path = entry.path();
        if Some(path) == sound_path.as_deref() { continue; }
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("json") => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("asset preflight: read {}", path.display()))?;
                let value = serde_json::from_str(&text)
                    .with_context(|| format!("asset preflight: parse {}", path.display()))?;
                content_keys(&value, &mut keys);
            }
            Some("rock") if !path.file_stem().unwrap_or_default().to_string_lossy().ends_with("_test") => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("asset preflight: read {}", path.display()))?;
                for line in text.lines() {
                    let code = line.split("//").next().unwrap_or_default();
                    for capture in pattern.captures_iter(code) {
                        let key = &capture[1];
                        if loose_runtime_path(key) { keys.entry(key.to_owned()).or_insert(true); }
                    }
                }
            }
            _ => {}
        }
    }
    let mut missing = Vec::new();
    for (key, overlay_only) in keys {
        let found = rock_host::release::resolve_asset_with_overlays(project, "", overlays, &key)?;
        let available = found.is_some_and(|asset| !overlay_only ||
            overlays.iter().any(|root| asset.source.starts_with(root)));
        if !available {
            missing.push(format!("  {key}{}", if overlay_only { " (required overlay asset)" } else { " (sound/content reference)" }));
        }
    }
    if !missing.is_empty() {
        anyhow::bail!("asset preflight: {} missing runtime files in {}:\n{}\nSync the game-art-vault or set ROCK_ASSET_OVERLAYS to current overlay roots.",
            missing.len(), project.display(), missing.join("\n"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, rock_host::stick::StickConfig) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("stick.toml"),
            "[package]\nname = \"fixture\"\n[release]\noverlay_assets = [\"assets/ui/frame.png\"]\n[sound]\nproject = \"sound_project.json\"\n").unwrap();
        std::fs::write(tmp.path().join("sound_project.json"),
            r#"{"version":1,"events":[{"id":"cue","steps":[{"layers":[{"sources":["assets/sfx/cue.ogg"]}]}]}]}"#).unwrap();
        std::fs::write(tmp.path().join("content.json"),
            r#"{"image":"assets/vfx/blast.png"}"#).unwrap();
        std::fs::create_dir_all(tmp.path().join("store/assets/ui")).unwrap();
        std::fs::create_dir_all(tmp.path().join("store/assets/sfx")).unwrap();
        std::fs::create_dir_all(tmp.path().join("store/assets/vfx")).unwrap();
        for key in ["assets/ui/frame.png", "assets/sfx/cue.ogg", "assets/vfx/blast.png"] {
            std::fs::write(tmp.path().join("store").join(key), b"fixture").unwrap();
        }
        let config = rock_host::stick::StickConfig::from_path_optional(&tmp.path().join("stick.toml")).unwrap().unwrap();
        (tmp, config)
    }

    #[test]
    fn declared_overlay_cannot_fall_back_to_source() {
        let (tmp, config) = fixture();
        std::fs::create_dir_all(tmp.path().join("assets/ui")).unwrap();
        let key = config.release_overlay_assets().unwrap().remove(0);
        std::fs::write(tmp.path().join(&key), b"source fallback").unwrap();
        let error = check_with_roots(tmp.path(), Some(&config), &[]).unwrap_err().to_string();
        assert!(error.contains(&key), "{error}");
    }

    #[test]
    fn missing_sound_source_names_the_catalog_path() {
        let (tmp, config) = fixture();
        let project = sound_core::parse_project(&std::fs::read_to_string(tmp.path().join("sound_project.json")).unwrap()).unwrap();
        let key = &project.events[0].steps[0].layers[0].sources[0];
        std::fs::remove_file(tmp.path().join("store").join(key)).unwrap();
        std::fs::create_dir_all(tmp.path().join(key).parent().unwrap()).unwrap();
        std::fs::write(tmp.path().join(key), b"source fallback").unwrap();
        let error = check_with_roots(tmp.path(), Some(&config), &[tmp.path().join("store")]).unwrap_err().to_string();
        assert!(error.contains(key), "{error}");
    }

    #[test]
    fn missing_content_asset_names_the_content_path() {
        let (tmp, config) = fixture();
        let catalog: serde_json::Value = serde_json::from_slice(&std::fs::read(tmp.path().join("content.json")).unwrap()).unwrap();
        let key = catalog["image"].as_str().unwrap();
        std::fs::remove_file(tmp.path().join("store").join(key)).unwrap();
        std::fs::create_dir_all(tmp.path().join(key).parent().unwrap()).unwrap();
        std::fs::write(tmp.path().join(key), b"source fallback").unwrap();
        let error = check_with_roots(tmp.path(), Some(&config), &[tmp.path().join("store")]).unwrap_err().to_string();
        assert!(error.contains(key), "{error}");
    }

    #[test]
    fn missing_source_asset_names_the_catalog_path() {
        let (tmp, config) = fixture();
        let catalog: serde_json::Value = serde_json::from_slice(&std::fs::read(tmp.path().join("content.json")).unwrap()).unwrap();
        let key = catalog["image"].as_str().unwrap();
        std::fs::write(tmp.path().join("content.rock"), format!("const IMAGE: string = \"{key}\"\n")).unwrap();
        std::fs::remove_file(tmp.path().join("content.json")).unwrap();
        std::fs::remove_file(tmp.path().join("store").join(key)).unwrap();
        std::fs::create_dir_all(tmp.path().join(key).parent().unwrap()).unwrap();
        std::fs::write(tmp.path().join(key), b"source fallback").unwrap();
        let error = check_with_roots(tmp.path(), Some(&config), &[tmp.path().join("store")]).unwrap_err().to_string();
        assert!(error.contains(key), "{error}");
    }

    #[test]
    fn complete_declarations_resolve_across_overlay_roots() {
        let (tmp, config) = fixture();
        let first = tmp.path().join("first");
        std::fs::create_dir_all(&first).unwrap();
        check_with_roots(tmp.path(), Some(&config), &[first, tmp.path().join("store")]).unwrap();
    }
}
