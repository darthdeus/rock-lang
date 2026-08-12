use anyhow::Result;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::stick::StickConfig;

#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    pub key: String,
    pub source: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct AssetSelection {
    pub assets: Vec<ReleaseAsset>,
    pub missing: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedAssets {
    pub file_count: usize,
    pub keys: Vec<String>,
}

/// Select the same shipped-key set for native packaging and the web bundle.
/// `config` is the already-parsed StickConfig; neither caller reparses TOML.
pub fn select_assets(
    project_dir: &Path,
    game_key: &str,
    config: Option<&StickConfig>,
    workspace_root: &Path,
    include_web_font: bool,
) -> Result<AssetSelection> {
    validate_relative_key(game_key, "game key")?;
    let names = match config {
        Some(config) => config.release_assets(project_dir)?,
        None => default_asset_names(project_dir),
    };

    let mut by_key = BTreeMap::<String, PathBuf>::new();
    let mut missing = Vec::new();
    for name in names {
        let source = project_dir.join(&name);
        if !source.exists() {
            missing.push(source);
            continue;
        }
        add_path(
            &mut by_key,
            game_key,
            &source,
            project_dir,
            &source,
        )?;
    }

    for relative_root in ["stone/engine/shaders", "stone/assets"] {
        let source = workspace_root.join(relative_root);
        if !source.exists() {
            missing.push(source);
            continue;
        }
        add_path(
            &mut by_key,
            relative_root,
            &source,
            workspace_root,
            &source,
        )?;
    }

    if include_web_font {
        let source = workspace_root.join("stone-rs/fonts/DroidSans.ttf");
        if source.is_file() {
            by_key
                .entry(format!("{game_key}/assets/fonts/DroidSans.ttf"))
                .or_insert(source);
        } else {
            missing.push(source);
        }
    }

    Ok(AssetSelection {
        assets: by_key
            .into_iter()
            .map(|(key, source)| ReleaseAsset { key, source })
            .collect(),
        missing,
    })
}

fn add_path(
    by_key: &mut BTreeMap<String, PathBuf>,
    prefix: &str,
    source: &Path,
    root: &Path,
    display_source: &Path,
) -> Result<()> {
    if source.is_dir() {
        for entry in walkdir::WalkDir::new(source).sort_by_file_name() {
            let entry = entry?;
            if entry.file_type().is_file() {
                let rel = entry.path().strip_prefix(root).map_err(|_| {
                    anyhow::anyhow!(
                        "release asset {} escaped its root {}",
                        entry.path().display(),
                        root.display()
                    )
                })?;
                let key = key_for(prefix, &rel.to_string_lossy().replace('\\', "/"));
                by_key.entry(key).or_insert_with(|| entry.path().to_path_buf());
            }
        }
    } else if source.is_file() {
        let rel = source.strip_prefix(root).map_err(|_| {
            anyhow::anyhow!(
                "release asset {} escaped its root {}",
                display_source.display(),
                root.display()
            )
        })?;
        let key = key_for(prefix, &rel.to_string_lossy().replace('\\', "/"));
        by_key.entry(key).or_insert_with(|| source.to_path_buf());
    }
    Ok(())
}

/// Clear a generated staging tree, then copy the selected files through the
/// same copier the web directory path uses.
pub fn stage_assets(selection: &AssetSelection, output_dir: &Path) -> Result<StagedAssets> {
    if output_dir.exists() {
        std::fs::remove_dir_all(output_dir)?;
    }
    copy_assets(selection, output_dir)
}

pub fn copy_assets(selection: &AssetSelection, output_dir: &Path) -> Result<StagedAssets> {
    let mut keys = Vec::with_capacity(selection.assets.len());
    for asset in &selection.assets {
        let destination = output_dir.join(&asset.key);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&asset.source, &destination).map_err(|e| {
            anyhow::anyhow!(
                "copying release asset {} to {}: {e}",
                asset.source.display(),
                destination.display()
            )
        })?;
        keys.push(asset.key.clone());
    }
    Ok(StagedAssets { file_count: keys.len(), keys })
}

fn validate_relative_key(value: &str, label: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        anyhow::bail!("release {label} must be a non-empty relative path: {value:?}");
    }
    Ok(())
}

fn normalize_key(value: &str) -> String {
    value.trim_start_matches("./").replace('\\', "/")
}

fn key_for(prefix: &str, relative: &str) -> String {
    let relative = normalize_key(relative);
    if relative == prefix || relative.starts_with(&format!("{prefix}/")) {
        relative
    } else {
        format!("{prefix}/{relative}")
    }
}

fn default_asset_names(project_dir: &Path) -> Vec<String> {
    let mut assets = vec!["assets".to_string()];
    if project_dir.join("level.txt").exists() {
        assets.push("level.txt".to_string());
    }
    assets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stick::StickConfig;

    #[test]
    fn selection_and_staging_share_game_engine_and_fallback_assets() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("games/demo");
        std::fs::create_dir_all(project.join("assets/nested")).unwrap();
        std::fs::create_dir_all(project.join("saves")).unwrap();
        std::fs::create_dir_all(root.path().join("stone/engine/shaders/common")).unwrap();
        std::fs::create_dir_all(root.path().join("stone/assets")).unwrap();
        std::fs::create_dir_all(root.path().join("stone-rs/fonts")).unwrap();
        std::fs::write(project.join("assets/nested/a.txt"), "a").unwrap();
        std::fs::write(project.join("saves/level.json"), "level").unwrap();
        std::fs::write(root.path().join("stone/engine/shaders/common/a.glsl"), "shader").unwrap();
        std::fs::write(root.path().join("stone/assets/sound.wav"), "sound").unwrap();
        std::fs::write(root.path().join("stone-rs/fonts/DroidSans.ttf"), "font").unwrap();
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"demo\"\n[release]\nassets = [\"assets\", \"saves/level.json\"]\n",
        )
        .unwrap();

        let selected = select_assets(&project, "games/demo", Some(&cfg), root.path(), true).unwrap();
        let keys: Vec<_> = selected.assets.iter().map(|asset| asset.key.as_str()).collect();
        assert!(keys.contains(&"games/demo/assets/nested/a.txt"));
        assert!(keys.contains(&"games/demo/saves/level.json"));
        assert!(keys.contains(&"stone/engine/shaders/common/a.glsl"));
        assert!(keys.contains(&"stone/assets/sound.wav"));
        assert!(keys.contains(&"games/demo/assets/fonts/DroidSans.ttf"));

        let out = root.path().join("staged");
        let staged = stage_assets(&selected, &out).unwrap();
        assert_eq!(staged.file_count, keys.len());
        assert_eq!(std::fs::read(out.join("games/demo/assets/nested/a.txt")).unwrap(), b"a");
        assert_eq!(std::fs::read(out.join("stone/assets/sound.wav")).unwrap(), b"sound");
    }

    #[test]
    fn selection_rejects_absolute_game_keys() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("demo");
        std::fs::create_dir_all(&project).unwrap();
        let err = select_assets(&project, "/tmp/demo", None, root.path(), false)
            .expect_err("absolute game key must reject");
        assert!(format!("{err}").contains("relative"), "error explains key restriction: {err}");
    }
}
