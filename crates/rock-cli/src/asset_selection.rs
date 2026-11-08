use anyhow::{Context, Result};
use rayon::prelude::*;
use rock_engine_host::{
    release::{self, AssetSelection},
    stick::EngineConfigExt,
};
use sound_core::AssetResolver;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub(crate) struct PreparedAssets {
    pub assets: AssetSelection,
    pub sound: Option<SelectedSound>,
    pub visual: Option<SelectedVisual>,
}

pub(crate) struct SelectedSound {
    pub declaration: String,
    pub catalog: sound_core::PreparedSoundProject,
    pub resolver: sound_core::AssetMapResolver,
}

pub(crate) struct SelectedVisual {
    pub bank_key: String,
    pub dependencies: Vec<String>,
    lines: Vec<rock_visual_bank::Line>,
}

impl SelectedVisual {
    pub fn verify(&self, selection: &AssetSelection) -> Result<()> {
        let sources: BTreeMap<_, _> = selection
            .assets
            .iter()
            .map(|asset| (asset.key.as_str(), &asset.source))
            .collect();
        let prefix = self
            .bank_key
            .strip_suffix(rock_visual_bank::BANK_PATH)
            .expect("bank key");
        self.lines.par_iter().try_for_each(|line| -> Result<()> {
            let verify = || -> Result<()> {
                let Some(spec) = rock_visual_bank::sheet_spec(line)? else {
                    return Ok(());
                };
                let digest = line.dir().expect("sheet directory");
                let mut directory = None;
                for file in rock_visual_bank::sheet_files(line) {
                    let key = format!("{prefix}assets/.eat/{digest}/{file}");
                    let source = sources
                        .get(key.as_str())
                        .with_context(|| format!("missing selected file {key}"))?;
                    let parent = source.parent().context("sheet file has no parent")?;
                    if let Some(directory) = directory {
                        anyhow::ensure!(
                            directory == parent,
                            "generation {digest} is split across asset roots"
                        );
                    }
                    directory = Some(parent);
                }
                let hash = rock_visual_bank::check_sheet(directory.context("empty sheet")?, &spec)
                    .map_err(|problems| anyhow::anyhow!("{}", problems.join("\n")))?;
                anyhow::ensure!(hash == digest, "directory {digest} hashes to {hash}");
                Ok(())
            };
            verify().map_err(|error| anyhow::anyhow!("{}: {error:#}", line.describe()))
        })
    }
}

pub(crate) fn prepare(
    project: &Path,
    game_key: &str,
    config: Option<&rock_host::stick::StickConfig>,
    overlays: &[PathBuf],
) -> Result<PreparedAssets> {
    let stone_dir = rock_engine_host::project::configured_stone_dir(project)?;
    let mut assets = release::select_assets_with_overlays(
        project,
        game_key,
        config,
        stone_dir.as_deref(),
        overlays,
    )?;
    if project.join(rock_visual_bank::BANK_PATH).is_file() {
        assets.assets.retain(|asset| {
            !asset.key.contains("/assets/.eat/")
                && !overlays.iter().any(|root| asset.source.starts_with(root))
        });
    }
    let sound = prepare_sound(project, game_key, config, overlays, &mut assets, true)?;
    let visual = select_visual_bank(project, game_key, overlays, &mut assets)?;
    include_declared_overlay_assets(project, game_key, config, overlays, &mut assets)?;
    assets
        .assets
        .sort_by(|left, right| left.key.cmp(&right.key));
    Ok(PreparedAssets {
        assets,
        sound,
        visual,
    })
}

pub(crate) fn prepare_sound(
    project: &Path,
    game_key: &str,
    config: Option<&rock_host::stick::StickConfig>,
    overlays: &[PathBuf],
    selection: &mut AssetSelection,
    require_assets: bool,
) -> Result<Option<SelectedSound>> {
    let Some(sound) = config.and_then(|config| config.sound.as_ref()) else {
        return Ok(None);
    };
    let path = release::sound_project_path(project, config)?.expect("declared sound project");
    let prepare = || -> Result<_> {
        let catalog = sound_core::parse_project(&std::fs::read_to_string(&path)?)?;
        let resolver = ReleaseSoundResolver {
            project_dir: project,
            game_key,
            overlays,
        };
        Ok(sound_core::prepare_project(
            &catalog,
            &resolver,
            require_assets,
        )?)
    };
    let catalog =
        prepare().map_err(|error| anyhow::anyhow!("sound project {}: {error}", path.display()))?;
    let mut sources = BTreeMap::new();
    for key in &catalog.dependencies {
        if let Some(asset) = release::resolve_asset_with_overlays(project, game_key, overlays, key)?
        {
            sources.insert(key.clone(), asset.source.clone());
            release::replace_release_asset(selection, asset);
        } else if require_assets {
            anyhow::bail!(
                "sound project {}: selected source disappeared: {key}",
                sound.project
            );
        } else {
            log::warn!(target: "SOUND", "sound project {} references missing asset {}; plays using it will be silent", sound.project, key);
        }
    }
    Ok(Some(SelectedSound {
        declaration: sound.project.clone(),
        catalog,
        resolver: sound_core::AssetMapResolver::new(sources),
    }))
}

struct ReleaseSoundResolver<'a> {
    project_dir: &'a Path,
    game_key: &'a str,
    overlays: &'a [PathBuf],
}

impl AssetResolver for ReleaseSoundResolver<'_> {
    fn discover(&self, pattern: &str) -> Result<Vec<String>, String> {
        let roots = self
            .overlays
            .iter()
            .cloned()
            .chain(std::iter::once(self.project_dir.to_path_buf()))
            .collect();
        sound_core::FsAssetResolver::new(roots).discover(pattern)
    }

    fn resolve(&self, key: &str) -> Option<PathBuf> {
        rock_engine_host::release::resolve_asset_with_overlays(
            self.project_dir,
            self.game_key,
            self.overlays,
            key,
        )
        .ok()
        .flatten()
        .map(|asset| asset.source)
    }
}

fn include_declared_overlay_assets(
    project_dir: &Path,
    game_key: &str,
    config: Option<&rock_host::stick::StickConfig>,
    overlays: &[PathBuf],
    selection: &mut rock_engine_host::release::AssetSelection,
) -> Result<()> {
    let Some(config) = config else { return Ok(()) };
    let logical_keys = config.release_overlay_assets()?;
    for logical_key in logical_keys {
        let asset = rock_engine_host::release::resolve_asset_with_overlays(
            project_dir,
            game_key,
            overlays,
            &logical_key,
        )?
        .ok_or_else(|| {
            anyhow::anyhow!("package: declared overlay asset does not exist: {logical_key}")
        })?;
        if !overlays
            .iter()
            .any(|overlay| asset.source.starts_with(overlay))
        {
            anyhow::bail!(
                "package: declared overlay asset resolves outside configured overlays: {logical_key}"
            );
        }
        rock_engine_host::release::replace_release_asset(selection, asset);
    }
    Ok(())
}

fn select_visual_bank(
    project_dir: &Path,
    game_key: &str,
    overlays: &[PathBuf],
    selection: &mut rock_engine_host::release::AssetSelection,
) -> Result<Option<SelectedVisual>> {
    let bank_logical_key = rock_visual_bank::BANK_PATH;
    let Some(bank_asset) =
        rock_engine_host::release::resolve_worktree_asset(project_dir, game_key, bank_logical_key)?
    else {
        let bank_key = format!("{game_key}/{bank_logical_key}");
        selection.assets.retain(|asset| asset.key != bank_key);
        if rock_engine_host::release::resolve_asset_with_overlays(
            project_dir,
            game_key,
            overlays,
            bank_logical_key,
        )?
        .is_some()
        {
            anyhow::bail!("package: worktree visual bank is missing; refusing the overlay bank");
        }
        return Ok(None);
    };
    let bank_path = bank_asset.source.display().to_string();
    let text = std::fs::read_to_string(&bank_asset.source).map_err(|error| {
        anyhow::anyhow!("package: visual bank {bank_path} could not be read: {error}")
    })?;
    let (lines, mut errors) = rock_visual_bank::parse_bank(&text);
    errors.extend(rock_visual_bank::resolve(&lines).1);
    if !errors.is_empty() {
        anyhow::bail!(
            "package: visual bank {bank_path} has {} malformed or inconsistent lines:\n{}",
            errors.len(),
            errors.join("\n")
        );
    }

    rock_engine_host::release::replace_release_asset(selection, bank_asset);
    let files = rock_engine_host::release::bank_files(&lines);
    let mut dependencies = Vec::with_capacity(files.len());
    let mut missing = Vec::new();
    for (file, line) in &files {
        match rock_engine_host::release::resolve_asset_with_overlays(
            project_dir,
            game_key,
            overlays,
            file,
        )? {
            Some(asset) => {
                rock_engine_host::release::replace_release_asset(selection, asset);
                dependencies.push(format!("{game_key}/{file}"));
            }
            None => missing.push(format!("{}: {file}", line.describe())),
        }
    }
    if !missing.is_empty() {
        anyhow::bail!(
            "package: {} files of visual bank {bank_path} are missing from the worktree and overlays:\n{}",
            missing.len(),
            missing.join("\n")
        );
    }
    Ok(Some(SelectedVisual {
        bank_key: format!("{game_key}/{bank_logical_key}"),
        dependencies,
        lines,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_with_roots(
        project: &Path,
        config: Option<&rock_host::stick::StickConfig>,
        overlays: &[PathBuf],
    ) -> Result<()> {
        prepare(project, "game", config, overlays).map(|_| ())
    }

    fn fixture() -> (tempfile::TempDir, rock_host::stick::StickConfig) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("stick.toml"),
            "[package]\nname = \"fixture\"\n[release]\noverlay_assets = [\"assets/ui/frame.png\"]\n[sound]\nproject = \"sound_project.json\"\n").unwrap();
        std::fs::write(tmp.path().join("sound_project.json"),
            r#"{"version":2,"normalization_lufs":-18.0,"groups":{"fixture":{"gain_db":0,"bus":"sfx"}},"events":[{"id":"cue","group":"fixture","steps":[{"layers":[{"sources":["assets/sfx/cue.ogg"]}]}]}]}"#).unwrap();
        std::fs::write(
            tmp.path().join("content.json"),
            r#"{"image":"assets/vfx/blast.png"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("store/assets/ui")).unwrap();
        std::fs::create_dir_all(tmp.path().join("store/assets/sfx")).unwrap();
        std::fs::create_dir_all(tmp.path().join("store/assets/vfx")).unwrap();
        for key in [
            "assets/ui/frame.png",
            "assets/sfx/cue.ogg",
            "assets/vfx/blast.png",
        ] {
            std::fs::write(tmp.path().join("store").join(key), b"fixture").unwrap();
        }
        let config =
            rock_host::stick::StickConfig::from_path_optional(&tmp.path().join("stick.toml"))
                .unwrap()
                .unwrap();
        (tmp, config)
    }

    #[test]
    fn selected_visual_bank_requires_its_generated_sheets() {
        let (tmp, config) = fixture();
        let bank = rock_visual_bank::BANK_PATH;
        let digest = "a".repeat(64);
        let selected = format!(
            r#"{{"kind":"texture","id":"floor","dir":"{digest}","size":[16,16],"normal":false,"material":true}}"#
        );
        std::fs::create_dir_all(tmp.path().join("assets")).unwrap();
        std::fs::write(tmp.path().join(bank), selected + "\n").unwrap();
        // A stale overlay bank must not erase the worktree's dependencies.
        std::fs::write(tmp.path().join("store").join(bank), "").unwrap();
        let sheet = format!("assets/.eat/{digest}");
        let stale = tmp.path().join("assets/.eat/stale/color.png");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"stale").unwrap();
        let root = tmp.path().join("store").join(&sheet);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("color.png"), b"color").unwrap();
        let error = check_with_roots(tmp.path(), Some(&config), &[tmp.path().join("store")])
            .unwrap_err()
            .to_string();
        assert!(error.contains(&format!("{sheet}/material.png")), "{error}");
        std::fs::write(root.join("material.png"), b"material").unwrap();
        let selected = prepare(
            tmp.path(),
            "game",
            Some(&config),
            &[tmp.path().join("store")],
        )
        .unwrap();
        assert!(
            selected
                .assets
                .assets
                .iter()
                .any(|asset| asset.key == format!("game/{sheet}/material.png"))
        );
        assert!(
            !selected
                .assets
                .assets
                .iter()
                .any(|asset| asset.source == stale || asset.key.ends_with("assets/vfx/blast.png"))
        );
    }

    #[test]
    fn declared_overlay_cannot_fall_back_to_source() {
        let (tmp, config) = fixture();
        std::fs::create_dir_all(tmp.path().join("assets/ui")).unwrap();
        let key = config.release_overlay_assets().unwrap().remove(0);
        std::fs::rename(tmp.path().join("store").join(&key), tmp.path().join(&key)).unwrap();
        let error = check_with_roots(tmp.path(), Some(&config), &[tmp.path().join("store")])
            .unwrap_err()
            .to_string();
        assert!(error.contains(&key), "{error}");
    }

    #[test]
    fn missing_sound_source_names_the_catalog_path() {
        let (tmp, config) = fixture();
        let project = sound_core::parse_project(
            &std::fs::read_to_string(tmp.path().join("sound_project.json")).unwrap(),
        )
        .unwrap();
        let key = &project.events[0].steps[0].layers[0].sources[0];
        std::fs::remove_file(tmp.path().join("store").join(key)).unwrap();
        let error = check_with_roots(tmp.path(), Some(&config), &[tmp.path().join("store")])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(key)
                && error.contains("sound_project.json")
                && error.contains("sources[0]"),
            "{error}"
        );
    }

    #[test]
    fn sound_sources_use_project_fallback_unless_declared_overlay_only() {
        let (tmp, mut config) = fixture();
        let key = "assets/sfx/cue.ogg";
        std::fs::create_dir_all(tmp.path().join("assets/sfx")).unwrap();
        std::fs::rename(tmp.path().join("store").join(key), tmp.path().join(key)).unwrap();
        let overlays = [tmp.path().join("store")];
        check_with_roots(tmp.path(), Some(&config), &overlays).unwrap();
        config
            .release
            .as_mut()
            .unwrap()
            .overlay_assets
            .as_mut()
            .unwrap()
            .push(key.to_string());
        let error = check_with_roots(tmp.path(), Some(&config), &overlays)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(key) && error.contains("outside configured overlays"),
            "{error}"
        );
    }

    #[test]
    fn sound_catalog_contract_is_checked_before_launch() {
        let (tmp, config) = fixture();
        let path = tmp.path().join("sound_project.json");
        let original: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for (field, value, diagnostic) in [
            ("group", serde_json::json!("absent"), "unknown mix group"),
            ("steps", serde_json::json!([]), "at least one step"),
        ] {
            let mut catalog = original.clone();
            catalog["events"][0][field] = value;
            std::fs::write(&path, serde_json::to_vec(&catalog).unwrap()).unwrap();
            let error = check_with_roots(tmp.path(), Some(&config), &[tmp.path().join("store")])
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("sound_project.json") && error.contains(diagnostic),
                "{error}"
            );
        }
    }

    #[test]
    fn undeclared_source_strings_do_not_select_runtime_files() {
        let (tmp, config) = fixture();
        std::fs::write(
            tmp.path().join("content.rock"),
            "const UNUSED = \"assets/vfx/unselected.png\"\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("content.json"), "not an asset catalog").unwrap();
        check_with_roots(tmp.path(), Some(&config), &[tmp.path().join("store")]).unwrap();
    }

    #[test]
    fn simulation_checks_declared_assets_for_any_game() {
        let (tmp, _) = fixture();
        let path = tmp.path().join("stick.toml");
        let config = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            format!(
                "{config}\n[assets]\noverlays = [{}]\n",
                serde_json::to_string(tmp.path().join("store").to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();
        std::fs::remove_file(tmp.path().join("store/assets/sfx/cue.ogg")).unwrap();
        let error = crate::package::check_project_assets(tmp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("cue.ogg"), "{error}");
    }

    #[test]
    fn complete_declarations_resolve_across_overlay_roots() {
        let (tmp, config) = fixture();
        let path = tmp.path().join("sound_project.json");
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("assets/sfx/cue.ogg", "assets/sfx/cue*.ogg");
        std::fs::write(path, text).unwrap();
        let first = tmp.path().join("first");
        std::fs::create_dir_all(&first).unwrap();
        check_with_roots(
            tmp.path(),
            Some(&config),
            &[first, tmp.path().join("store")],
        )
        .unwrap();
    }
}
