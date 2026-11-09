use crate::stick::EngineConfigExt;
use anyhow::Result;
use rock_visual_bank as visual_bank;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use rock_host::stick::StickConfig;

pub const ASSET_OVERLAYS_ENV: &str = "ROCK_ASSET_OVERLAYS";

/// Every file the bank's sheets draw from, as `assets/.eat/<dir>/<name>` keys
/// mapped to the first line that names them. Identical sheets share a
/// directory, so a key may belong to several lines.
pub fn bank_files(lines: &[visual_bank::Line]) -> BTreeMap<String, &visual_bank::Line> {
    let mut files = BTreeMap::new();
    for line in lines {
        let Some(dir) = line.dir() else { continue };
        for name in visual_bank::sheet_files(line) {
            files
                .entry(format!("assets/{}/{dir}/{name}", visual_bank::SHEET_ROOT))
                .or_insert(line);
        }
    }
    files
}

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

/// Select the shipped-key set for native packaging.
/// `config` is the already-parsed StickConfig; neither caller reparses TOML.
#[cfg(test)]
pub(crate) fn select_assets(
    project_dir: &Path,
    game_key: &str,
    config: Option<&StickConfig>,
    stone_dir: Option<&Path>,
) -> Result<AssetSelection> {
    let overlays = configured_asset_overlays(project_dir, config)?;
    select_assets_with_overlays(project_dir, game_key, config, stone_dir, &overlays)
}

pub fn asset_overlay_roots(
    project_dir: &Path,
    config: Option<&StickConfig>,
) -> Result<Vec<PathBuf>> {
    configured_asset_overlays(project_dir, config)
}

pub fn sound_project_path(
    project_dir: &Path,
    config: Option<&StickConfig>,
) -> Result<Option<PathBuf>> {
    let Some(sound) = config.and_then(|config| config.sound.as_ref()) else {
        return Ok(None);
    };
    let relative = &sound.project;
    let path = Path::new(relative);
    if relative.trim().is_empty()
        || path.is_absolute()
        || relative.starts_with('~')
        || relative.contains('\\')
        || relative.as_bytes().get(1) == Some(&b':')
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        anyhow::bail!(
            "[sound].project must be a non-empty relative path without traversal: {relative:?}"
        );
    }
    let project_path = project_dir.join(path);
    if !project_path.is_file() {
        anyhow::bail!(
            "declared sound project does not exist: {}",
            project_path.display()
        );
    }
    Ok(Some(project_path))
}

fn configured_asset_overlays(
    project_dir: &Path,
    config: Option<&StickConfig>,
) -> Result<Vec<PathBuf>> {
    let env = std::env::var_os(ASSET_OVERLAYS_ENV);
    configured_asset_overlays_from(project_dir, config, env.as_deref())
}

pub fn configured_asset_overlays_from(
    project_dir: &Path,
    config: Option<&StickConfig>,
    env: Option<&OsStr>,
) -> Result<Vec<PathBuf>> {
    configured_asset_overlays_from_with_vault(project_dir, config, env, &crate::art_vault::root)
}

pub fn configured_asset_overlays_from_with_vault(
    project_dir: &Path,
    config: Option<&StickConfig>,
    env: Option<&OsStr>,
    vault: &dyn Fn() -> Result<PathBuf>,
) -> Result<Vec<PathBuf>> {
    if let Some(value) = env {
        return parse_asset_overlay_paths(value);
    }
    let defaults = match config {
        Some(config) => config.asset_overlay_defaults_with_vault(project_dir, vault)?,
        None => Vec::new(),
    };
    validate_asset_overlay_paths(defaults, "stick.toml [assets].overlays")
}

fn parse_asset_overlay_paths(value: &OsStr) -> Result<Vec<PathBuf>> {
    validate_asset_overlay_paths(
        std::env::split_paths(value)
            .filter(|path| !path.as_os_str().is_empty())
            .collect(),
        ASSET_OVERLAYS_ENV,
    )
}

fn validate_asset_overlay_paths(paths: Vec<PathBuf>, source: &str) -> Result<Vec<PathBuf>> {
    paths
        .into_iter()
        .map(|path| {
            if !path.is_absolute() {
                anyhow::bail!(
                    "{source} entries must resolve to absolute directories: {}",
                    path.display()
                );
            }
            let canonical = fs_err::canonicalize(&path).map_err(|e| {
                anyhow::anyhow!(
                    "{source} directory {} cannot be resolved: {e}",
                    path.display()
                )
            })?;
            if !canonical.is_dir() {
                anyhow::bail!("{source} entry is not a directory: {}", canonical.display());
            }
            Ok(canonical)
        })
        .collect()
}

pub fn select_assets_with_overlays(
    project_dir: &Path,
    game_key: &str,
    config: Option<&StickConfig>,
    stone_dir: Option<&Path>,
    overlays: &[PathBuf],
) -> Result<AssetSelection> {
    validate_relative_key(game_key, "game key")?;
    let names = match config {
        Some(config) => config.release_assets(project_dir)?,
        None => default_asset_names(),
    };

    let mut by_key = BTreeMap::<String, PathBuf>::new();
    let mut missing = Vec::new();
    for name in &names {
        let source = project_dir.join(name);
        if !source.exists() {
            missing.push(source);
            continue;
        }
        add_path(&mut by_key, game_key, &source, project_dir, &source, false)?;
    }

    // A game with a compiled worktree visual bank admits overlay files
    // exactly: sound and visual dependencies plus declared loose assets, each
    // resolved directly by key. Walking the whole shared overlay tree here
    // only to discard it in narrowing wastes tens of seconds on slow overlay
    // disks. Bankless games keep the legacy whole-overlay staging.
    if !project_dir.join(visual_bank::BANK_PATH).is_file() {
        for overlay in overlays.iter().rev() {
            let source = overlay.join("assets");
            if !source.exists() {
                continue;
            }
            add_path(&mut by_key, game_key, &source, overlay, &source, true)?;
        }
    }

    if let Some(stone_dir) = stone_dir {
        let source = stone_dir.join("engine/shaders");
        if source.exists() {
            add_path(&mut by_key, "stone", &source, stone_dir, &source, false)?;
        } else {
            missing.push(source);
        }
        let assets = stone_dir.join("engine/assets");
        if assets.exists() {
            add_path(&mut by_key, "stone", &assets, stone_dir, &assets, false)?;
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

pub fn resolve_asset_with_overlays(
    project_dir: &Path,
    game_key: &str,
    overlays: &[PathBuf],
    logical_key: &str,
) -> Result<Option<ReleaseAsset>> {
    validate_relative_key(logical_key, "asset key")?;
    if !logical_key.starts_with("assets/") || logical_key.contains('\\') {
        anyhow::bail!("asset key must be a canonical assets-relative key: {logical_key:?}");
    }
    let mut candidates = Vec::with_capacity(overlays.len() + 1);
    candidates.push(project_dir.join(logical_key));
    candidates.extend(
        overlays
            .iter()
            .rev()
            .map(|overlay| overlay.join(logical_key)),
    );
    for candidate in candidates.into_iter().rev() {
        if candidate.is_file() {
            return Ok(Some(ReleaseAsset {
                key: key_for(game_key, logical_key),
                source: candidate,
            }));
        }
    }
    Ok(None)
}

pub fn resolve_worktree_asset(
    project_dir: &Path,
    game_key: &str,
    logical_key: &str,
) -> Result<Option<ReleaseAsset>> {
    validate_relative_key(logical_key, "asset key")?;
    if !logical_key.starts_with("assets/") || logical_key.contains('\\') {
        anyhow::bail!("asset key must be a canonical assets-relative key: {logical_key:?}");
    }
    let candidate = project_dir.join(logical_key);
    if candidate.is_file() {
        return Ok(Some(ReleaseAsset {
            key: key_for(game_key, logical_key),
            source: candidate,
        }));
    }
    Ok(None)
}

#[cfg(test)]
mod worktree_bank_tests {
    use super::*;

    #[test]
    fn worktree_bank_resolution_prefers_the_project_over_an_overlay() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("demo");
        let overlay = root.path().join("vault");
        std::fs::create_dir_all(project.join("assets")).unwrap();
        std::fs::create_dir_all(overlay.join("assets")).unwrap();
        std::fs::write(project.join(visual_bank::BANK_PATH), "project").unwrap();
        std::fs::write(overlay.join(visual_bank::BANK_PATH), "overlay").unwrap();

        let selected = resolve_worktree_asset(&project, "demo", visual_bank::BANK_PATH)
            .unwrap()
            .unwrap();
        assert_eq!(selected.source, project.join(visual_bank::BANK_PATH));

        let ordinary = resolve_asset_with_overlays(
            &project,
            "demo",
            std::slice::from_ref(&overlay),
            visual_bank::BANK_PATH,
        )
        .unwrap()
        .unwrap();
        assert_eq!(ordinary.source, overlay.join(visual_bank::BANK_PATH));
    }

    #[test]
    fn missing_worktree_bank_does_not_fall_back_to_an_overlay() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("demo");
        let overlay = root.path().join("vault");
        std::fs::create_dir_all(project.join("assets")).unwrap();
        std::fs::create_dir_all(overlay.join("assets")).unwrap();
        std::fs::write(overlay.join(visual_bank::BANK_PATH), "overlay").unwrap();

        let selected = resolve_worktree_asset(&project, "demo", visual_bank::BANK_PATH).unwrap();
        assert!(selected.is_none());
    }
}

/// Insert an overlay-first-resolved asset, overwriting any worktree-selected
/// same-key entry. Bank inserts leave the selection unsorted; the caller sorts
/// once after all of them instead of paying a sort per insertion.
pub fn replace_release_asset(selection: &mut AssetSelection, asset: ReleaseAsset) {
    if let Some(existing) = selection
        .assets
        .iter_mut()
        .find(|existing| existing.key == asset.key)
    {
        *existing = asset;
    } else {
        selection.assets.push(asset);
    }
}

fn is_runtime_data_asset(relative: &Path) -> bool {
    let mut components = relative.components();
    matches!(
        (components.next(), components.next()),
        (
            Some(std::path::Component::Normal(asset_root)),
            Some(std::path::Component::Normal(cache_dir))
        ) if asset_root == OsStr::new("assets") && cache_dir == OsStr::new(".runtime-data")
    )
}

fn add_path(
    by_key: &mut BTreeMap<String, PathBuf>,
    prefix: &str,
    source: &Path,
    root: &Path,
    display_source: &Path,
    overwrite: bool,
) -> Result<()> {
    if source.is_dir() {
        for entry in walkdir::WalkDir::new(source)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|entry| {
                entry
                    .path()
                    .strip_prefix(root)
                    .map_or(true, |relative| !is_runtime_data_asset(relative))
            })
        {
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
                insert_asset(by_key, key, entry.path().to_path_buf(), overwrite);
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
        if is_runtime_data_asset(rel) {
            return Ok(());
        }
        let key = key_for(prefix, &rel.to_string_lossy().replace('\\', "/"));
        insert_asset(by_key, key, source.to_path_buf(), overwrite);
    }
    Ok(())
}

fn insert_asset(
    by_key: &mut BTreeMap<String, PathBuf>,
    key: String,
    source: PathBuf,
    overwrite: bool,
) {
    if overwrite {
        if let Some(previous) = by_key.insert(key.clone(), source.clone()) {
            log::info!(
                target: "ASSETS",
                "overlay {} replaces {} for {}",
                source.display(),
                previous.display(),
                key
            );
        }
    } else {
        by_key.entry(key).or_insert(source);
    }
}

/// Sibling marker proving `output_dir` is a tree this module generated and
/// may therefore clear. Kept beside the directory, not inside it, so the
/// staged tree itself holds only shipped keys.
pub(crate) fn staging_marker_path(output_dir: &Path) -> PathBuf {
    let name = output_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    output_dir.with_file_name(format!("{name}.rock-staging"))
}

/// Clear a generated staging tree, then copy the selected files into it.
///
/// Only a tree this fn generated (marker present) or an empty directory may
/// be cleared: `--output` is caller-controlled, and an unowned `assets/`
/// child (a game's own source tree, say) must reject rather than be deleted.
pub fn stage_assets(selection: &AssetSelection, output_dir: &Path) -> Result<StagedAssets> {
    let marker = staging_marker_path(output_dir);
    if output_dir.exists() {
        let is_empty = std::fs::read_dir(output_dir)?.next().is_none();
        if !marker.is_file() && !is_empty {
            anyhow::bail!(
                "refusing to clear {}: not a staging tree this tool generated \
                 (missing {}). If it is a stale staging tree, delete it yourself first.",
                output_dir.display(),
                marker.display()
            );
        }
        std::fs::remove_dir_all(output_dir)?;
    }
    let staged = copy_assets(selection, output_dir)?;
    std::fs::create_dir_all(output_dir)?;
    std::fs::write(
        &marker,
        b"generated by rock package asset staging; safe to delete\n",
    )?;
    Ok(staged)
}

pub(crate) fn copy_assets(selection: &AssetSelection, output_dir: &Path) -> Result<StagedAssets> {
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
    Ok(StagedAssets {
        file_count: keys.len(),
        keys,
    })
}

fn validate_relative_key(value: &str, label: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || path.has_root()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
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

fn default_asset_names() -> Vec<String> {
    vec!["assets".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rock_host::stick::StickConfig;

    #[derive(serde::Deserialize)]
    struct ReleaseAssetOracle {
        release: ReleaseAssetRootsOracle,
    }

    #[derive(serde::Deserialize)]
    struct ReleaseAssetRootsOracle {
        assets: Vec<String>,
        predefined_levels: Vec<String>,
    }

    fn independent_keys_for_roots(
        project: &Path,
        game_key: &str,
        roots: impl IntoIterator<Item = String>,
    ) -> Vec<String> {
        let mut keys = BTreeMap::new();
        for root in roots {
            let source = project.join(root);
            if source.is_file() {
                let relative = source.strip_prefix(project).unwrap();
                keys.insert(
                    format!(
                        "{game_key}/{}",
                        relative.to_string_lossy().replace('\\', "/")
                    ),
                    (),
                );
            } else if source.is_dir() {
                for entry in walkdir::WalkDir::new(source)
                    .into_iter()
                    .filter_map(Result::ok)
                {
                    if entry.file_type().is_file() {
                        let relative = entry.path().strip_prefix(project).unwrap();
                        keys.insert(
                            format!(
                                "{game_key}/{}",
                                relative.to_string_lossy().replace('\\', "/")
                            ),
                            (),
                        );
                    }
                }
            }
        }
        keys.into_keys().collect()
    }

    fn independent_release_keys(project: &Path, game_key: &str, config: &str) -> Vec<String> {
        let parsed: ReleaseAssetOracle = basic_toml::from_str(config).unwrap();
        let roots = parsed
            .release
            .assets
            .into_iter()
            .chain(parsed.release.predefined_levels);
        independent_keys_for_roots(project, game_key, roots)
    }

    #[test]
    fn configured_predefined_levels_match_selected_and_packaged_keys() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("games/configured-levels");
        std::fs::create_dir_all(project.join("assets")).unwrap();
        std::fs::create_dir_all(project.join("levels")).unwrap();
        std::fs::write(project.join("assets/ui.txt"), "ui").unwrap();
        std::fs::write(project.join("levels/index.json"), "catalog").unwrap();
        std::fs::write(project.join("levels/crypt.json"), "level").unwrap();
        std::fs::write(project.join("levels/editor.json"), "editor-only").unwrap();
        std::fs::write(project.join("level.txt"), "legacy").unwrap();
        let config_text = concat!(
            "[package]\nname = \"configured-levels\"\n",
            "[release]\nassets = [\"assets\"]\n",
            "predefined_levels = [\"levels/index.json\", \"levels/crypt.json\"]\n",
        );
        let config = StickConfig::new_from_str(config_text)
            .expect("release configuration must parse named predefined levels");
        let expected = independent_release_keys(&project, "games/configured-levels", config_text);

        let native = select_assets_with_overlays(
            &project,
            "games/configured-levels",
            Some(&config),
            Some(&root.path().join("stone")),
            &[],
        )
        .unwrap();
        let keys = |selection: &AssetSelection| {
            selection
                .assets
                .iter()
                .map(|asset| asset.key.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys(&native),
            expected,
            "native selector follows configured release roots"
        );

        let staged = stage_assets(&native, &root.path().join("package/assets")).unwrap();
        assert_eq!(
            staged.keys, expected,
            "packaged keys follow the same configured roots"
        );
        assert!(
            staged
                .keys
                .iter()
                .any(|key| key.ends_with("levels/crypt.json"))
        );
        assert!(!staged.keys.iter().any(|key| key.ends_with("level.txt")));
        assert!(
            !staged
                .keys
                .iter()
                .any(|key| key.ends_with("levels/editor.json"))
        );

        let config_without_release =
            StickConfig::new_from_str("[package]\nname = \"defaults\"\n").unwrap();
        let default_expected =
            independent_keys_for_roots(&project, "games/configured-levels", ["assets".to_string()]);
        let no_config = select_assets_with_overlays(
            &project,
            "games/configured-levels",
            None,
            Some(&root.path().join("stone")),
            &[],
        )
        .unwrap();
        let config_without_release = select_assets_with_overlays(
            &project,
            "games/configured-levels",
            Some(&config_without_release),
            Some(&root.path().join("stone")),
            &[],
        )
        .unwrap();
        assert_eq!(
            keys(&no_config),
            default_expected,
            "unconfigured native default is assets only"
        );
        assert_eq!(
            keys(&config_without_release),
            default_expected,
            "StickConfig default is assets only"
        );
    }

    #[test]
    fn selection_and_staging_share_game_engine_and_fallback_assets() {
        let root = tempfile::tempdir().unwrap();
        let engine = tempfile::tempdir().unwrap();
        let project = root.path().join("games/demo");
        std::fs::create_dir_all(project.join("assets/nested")).unwrap();
        std::fs::create_dir_all(project.join("saves")).unwrap();
        std::fs::create_dir_all(engine.path().join("engine/shaders/common")).unwrap();
        std::fs::write(project.join("assets/nested/a.txt"), "a").unwrap();
        std::fs::write(project.join("saves/level.json"), "level").unwrap();
        std::fs::write(engine.path().join("engine/shaders/common/a.glsl"), "shader").unwrap();
        std::fs::create_dir_all(engine.path().join("engine/assets/fonts")).unwrap();
        std::fs::write(
            engine.path().join("engine/assets/fonts/default.ttf"),
            "font",
        )
        .unwrap();
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"demo\"\n[release]\nassets = [\"assets\", \"saves/level.json\"]\n",
        )
        .unwrap();

        let selected =
            select_assets(&project, "games/demo", Some(&cfg), Some(engine.path())).unwrap();
        let keys: Vec<_> = selected
            .assets
            .iter()
            .map(|asset| asset.key.as_str())
            .collect();
        assert!(keys.contains(&"games/demo/assets/nested/a.txt"));
        assert!(keys.contains(&"games/demo/saves/level.json"));
        assert!(keys.contains(&"stone/engine/shaders/common/a.glsl"));
        assert!(keys.contains(&"stone/engine/assets/fonts/default.ttf"));

        let out = root.path().join("staged");
        let staged = stage_assets(&selected, &out).unwrap();
        assert_eq!(
            std::fs::read(out.join("stone/engine/assets/fonts/default.ttf")).unwrap(),
            b"font"
        );
        assert_eq!(staged.file_count, keys.len());
        assert_eq!(
            std::fs::read(out.join("games/demo/assets/nested/a.txt")).unwrap(),
            b"a"
        );
        assert_eq!(
            std::fs::read(out.join("stone/engine/shaders/common/a.glsl")).unwrap(),
            b"shader"
        );
    }

    #[test]
    fn selected_asset_directories_exclude_runtime_metadata_cache() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("demo");
        std::fs::create_dir_all(project.join("assets/ui")).unwrap();
        std::fs::create_dir_all(project.join("assets/.runtime-data")).unwrap();
        std::fs::write(project.join("assets/ui/hud.png"), b"hud").unwrap();
        std::fs::write(project.join("assets/.runtime-data/boot.bin"), b"metadata").unwrap();
        std::fs::write(
            project.join("assets/.runtime-data/explicit.bin"),
            b"metadata",
        )
        .unwrap();
        let config = StickConfig::new_from_str(concat!(
            "[package]\nname = \"demo\"\n",
            "[release]\nassets = [\"assets\", ",
            "\"assets/.runtime-data/explicit.bin\"]\n",
        ))
        .unwrap();

        let selection = select_assets(
            &project,
            "demo",
            Some(&config),
            Some(&root.path().join("stone")),
        )
        .unwrap();
        let keys: Vec<_> = selection
            .assets
            .iter()
            .map(|asset| asset.key.as_str())
            .collect();
        assert!(keys.contains(&"demo/assets/ui/hud.png"));
        assert!(!keys.contains(&"demo/assets/.runtime-data/boot.bin"));
        assert!(!keys.contains(&"demo/assets/.runtime-data/explicit.bin"));

        let output = root.path().join("staged/assets");
        let staged = stage_assets(&selection, &output).unwrap();
        assert_eq!(staged.file_count, 1);
        assert!(!output.join("demo/assets/.runtime-data/boot.bin").exists());
        assert!(
            !output
                .join("demo/assets/.runtime-data/explicit.bin")
                .exists()
        );
    }

    #[test]
    fn staging_refuses_a_directory_it_did_not_generate() {
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("assets");
        std::fs::create_dir_all(out.join("art")).unwrap();
        std::fs::write(out.join("art/original.png"), "irreplaceable").unwrap();

        let selection = AssetSelection::default();
        let err = stage_assets(&selection, &out).expect_err("unowned directory must reject");
        assert!(
            format!("{err}").contains("staging"),
            "error names the staging-tree refusal: {err}"
        );
        assert_eq!(
            std::fs::read(out.join("art/original.png")).unwrap(),
            b"irreplaceable",
            "refusal must not delete anything"
        );
    }

    #[test]
    fn restaging_a_generated_tree_clears_and_recopies() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("demo");
        std::fs::create_dir_all(project.join("assets")).unwrap();
        std::fs::write(project.join("assets/a.txt"), "a").unwrap();
        let selection = AssetSelection {
            assets: vec![ReleaseAsset {
                key: "demo/assets/a.txt".to_string(),
                source: project.join("assets/a.txt"),
            }],
            missing: vec![],
        };

        let out = root.path().join("staged/assets");
        stage_assets(&selection, &out).unwrap();
        std::fs::write(out.join("stale.txt"), "stale").unwrap();
        let staged = stage_assets(&selection, &out).unwrap();
        assert_eq!(staged.file_count, 1);
        assert!(
            !out.join("stale.txt").exists(),
            "restaging clears the generated tree"
        );
        assert_eq!(std::fs::read(out.join("demo/assets/a.txt")).unwrap(), b"a");
    }

    #[test]
    fn staging_claims_an_empty_directory() {
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("assets");
        std::fs::create_dir_all(&out).unwrap();
        stage_assets(&AssetSelection::default(), &out).unwrap();
        assert!(
            staging_marker_path(&out).is_file(),
            "claimed dir carries the marker"
        );
    }

    #[test]
    fn selection_rejects_absolute_game_keys() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("demo");
        std::fs::create_dir_all(&project).unwrap();
        let mut keys = vec!["/tmp/demo", "../demo", "demo/../other"];
        if cfg!(windows) {
            keys.extend([r"\demo", r"C:demo", r"C:\demo", r"\\server\share\demo"]);
        }
        for key in keys {
            let err = select_assets(&project, key, None, None)
                .expect_err("non-relative game key must reject");
            assert!(
                format!("{err}").contains("relative"),
                "error explains key restriction for {key}: {err}"
            );
        }
    }

    #[test]
    fn sound_project_path_resolves_the_stick_declaration_without_allowing_escape() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("game");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("sounds.json"), "{}").unwrap();
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"game\"\n[sound]\nproject = \"sounds.json\"\n",
        )
        .unwrap();

        assert_eq!(
            sound_project_path(&project, Some(&cfg)).unwrap(),
            Some(project.join("sounds.json"))
        );

        let escaped = StickConfig::new_from_str(
            "[package]\nname = \"game\"\n[sound]\nproject = \"../sounds.json\"\n",
        )
        .unwrap();
        let error = sound_project_path(&project, Some(&escaped))
            .expect_err("a declared sound project cannot escape its game");
        assert!(format!("{error}").contains("without traversal"));
    }

    #[test]
    fn overlay_path_lists_ignore_empty_segments() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let value = std::env::join_paths([
            Path::new(""),
            first.as_path(),
            Path::new(""),
            second.as_path(),
            Path::new(""),
        ])
        .unwrap();

        assert_eq!(
            parse_asset_overlay_paths(&value).unwrap(),
            vec![
                first.canonicalize().unwrap(),
                second.canonicalize().unwrap()
            ]
        );
    }

    #[test]
    fn overlays_merge_assets_in_declared_priority_without_shadowing_other_release_files() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("demo");
        let high = root.path().join("high");
        let low = root.path().join("low");
        for dir in [&project, &high, &low] {
            std::fs::create_dir_all(dir.join("assets/sfx")).unwrap();
            std::fs::create_dir_all(dir.join("saves")).unwrap();
        }
        std::fs::create_dir_all(root.path().join("stone/engine/shaders")).unwrap();

        std::fs::write(project.join("assets/sfx/shared.ogg"), "base").unwrap();
        std::fs::write(project.join("assets/sfx/base.ogg"), "base-only").unwrap();
        std::fs::write(project.join("saves/level.json"), "base-level").unwrap();
        std::fs::write(low.join("assets/sfx/shared.ogg"), "low").unwrap();
        std::fs::write(low.join("assets/sfx/low.ogg"), "low-only").unwrap();
        std::fs::write(high.join("assets/sfx/shared.ogg"), "high").unwrap();
        std::fs::write(high.join("assets/sfx/high.ogg"), "high-only").unwrap();
        std::fs::create_dir_all(high.join("assets/fx")).unwrap();
        std::fs::write(high.join("assets/fx/overlay-only.png"), "fx-only").unwrap();
        std::fs::write(high.join("saves/level.json"), "overlay-level").unwrap();

        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"demo\"\n[release]\nassets = [\"assets/sfx\", \"saves/level.json\"]\n",
        )
        .unwrap();
        let selected = select_assets_with_overlays(
            &project,
            "demo",
            Some(&cfg),
            Some(&root.path().join("stone")),
            &[high.clone(), low.clone()],
        )
        .unwrap();
        let by_key: BTreeMap<_, _> = selected
            .assets
            .iter()
            .map(|asset| (asset.key.as_str(), asset.source.as_path()))
            .collect();

        assert_eq!(
            by_key["demo/assets/sfx/shared.ogg"],
            high.join("assets/sfx/shared.ogg")
        );
        assert_eq!(
            by_key["demo/assets/sfx/base.ogg"],
            project.join("assets/sfx/base.ogg")
        );
        assert_eq!(
            by_key["demo/assets/sfx/low.ogg"],
            low.join("assets/sfx/low.ogg")
        );
        assert_eq!(
            by_key["demo/assets/sfx/high.ogg"],
            high.join("assets/sfx/high.ogg")
        );
        assert_eq!(
            by_key["demo/assets/fx/overlay-only.png"],
            high.join("assets/fx/overlay-only.png")
        );
        assert_eq!(
            by_key["demo/saves/level.json"],
            project.join("saves/level.json")
        );
    }

    #[test]
    fn explicit_overlay_environment_replaces_stick_defaults_including_with_empty() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("game");
        let configured = root.path().join("configured");
        let explicit = root.path().join("explicit");
        for path in [&project, &configured, &explicit] {
            std::fs::create_dir_all(path).unwrap();
        }
        let cfg = StickConfig::new_from_str(&format!(
            "[package]\nname = \"game\"\n[assets]\noverlays = ['{}']\n",
            configured.display()
        ))
        .unwrap();

        assert_eq!(
            configured_asset_overlays_from(&project, Some(&cfg), None).unwrap(),
            vec![configured.canonicalize().unwrap()]
        );
        let explicit_list = std::env::join_paths([explicit.as_path()]).unwrap();
        assert_eq!(
            configured_asset_overlays_from(&project, Some(&cfg), Some(&explicit_list)).unwrap(),
            vec![explicit.canonicalize().unwrap()]
        );
        assert!(
            configured_asset_overlays_from(&project, Some(&cfg), Some(OsStr::new("")))
                .unwrap()
                .is_empty(),
            "an explicitly empty environment disables configured defaults"
        );
    }
}

#[cfg(test)]
mod visual_bank_file_tests {
    use super::*;

    fn dir(c: char) -> String {
        c.to_string().repeat(64)
    }

    #[test]
    fn bank_files_name_every_sheet_file_once() {
        let (a, b, c, d, e, f) = (dir('a'), dir('b'), dir('c'), dir('d'), dir('e'), dir('f'));
        let basis = "[0,0,0,1,0,0,0,1,0,0,1,0]";
        let bases = [basis; 8].join(",");
        let prop = |id: &str, dir: &str, masks: &str| {
            format!(
                r#"{{"kind":"prop","id":"{id}","dir":"{dir}","frame_size":[8,8],"pivot":[0.5,0.5],"orientations":1,"rows":1,"masks":{masks},"spin":false}}"#
            )
        };
        let piece = r#"{"id":"wall","role":"wall","anchor":"cell_center","anchor_point":[0,0],"pivot":[0.5,0.5],"sprite_scale":[1,1],"orientation_map":[0],"variants":[{"index":0,"frames":[{"x":0,"y":0,"w":8,"h":8,"orientation":0}]}]}"#;
        let text = [
            format!(r#"{{"kind":"animation","character":"bear","label":"idle","dir":"{a}","size":[16,16],"cell":[8,8],"pivot":[0.5,0.5],"frames":4,"frame_time":0.1,"skin_mask":false}}"#),
            format!(r#"{{"kind":"architecture","id":"walls","dir":"{e}","size":[16,16],"tile_size":[8,8],"sprite_scale":[1,1],"masks":["cap"],"pieces":[{piece}]}}"#),
            format!(r#"{{"kind":"character","id":"bear","draw_size":4.0,"padding_px":0.0,"camera":{{"ground_y":0.0,"bases":[{bases}]}}}}"#),
            format!(r#"{{"kind":"icon","id":"ui/body","dir":"{c}","frame_size":[8,8],"pivot":[0.5,0.5],"skin":true}}"#),
            prop("world/crate", &f, "[]"),
            prop("world/well", &b, r#"["paint","water"]"#),
            prop("world/well_copy", &b, r#"["paint","water"]"#),
            format!(r#"{{"kind":"texture","id":"cobble","dir":"{d}","size":[16,16],"normal":false,"material":true}}"#),
        ]
        .map(|line| line + "\n")
        .concat();
        let (lines, errors) = visual_bank::parse_bank(&text);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(lines.len(), 8);

        let files = bank_files(&lines);
        let mut expected: Vec<String> = [
            (&a, "albedo.png"),
            (&a, "depth.png"),
            (&a, "material.png"),
            (&a, "normal.png"),
            (&a, "sheet.json"),
            (&b, "albedo.png"),
            (&b, "mask-paint.png"),
            (&b, "mask-water.png"),
            (&b, "material.png"),
            (&b, "normal.png"),
            (&c, "color.png"),
            (&c, "skin-albedo.png"),
            (&c, "skin-diffuse.png"),
            (&c, "skin-specular.png"),
            (&d, "color.png"),
            (&d, "material.png"),
            (&e, "color.png"),
            (&e, "mask-cap.png"),
            (&f, "albedo.png"),
            (&f, "material.png"),
            (&f, "normal.png"),
        ]
        .map(|(dir, name)| format!("assets/.eat/{dir}/{name}"))
        .into();
        expected.sort();
        assert_eq!(files.keys().cloned().collect::<Vec<_>>(), expected);
        assert_eq!(
            files[&format!("assets/.eat/{b}/albedo.png")].describe(),
            r#"prop "world/well""#
        );
    }
}
