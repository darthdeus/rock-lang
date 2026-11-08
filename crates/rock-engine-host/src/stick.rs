//! Runtime interpretation of the compiler's parsed project configuration.
use anyhow::Result;
use rock_host::stick::StickConfig;
use std::path::{Path, PathBuf};

/// Private engine and asset policy; the public parser only supplies data.
pub trait EngineConfigExt {
    fn release_assets(&self, _project_dir: &Path) -> Result<Vec<String>>;
    fn release_overlay_assets(&self) -> Result<Vec<String>>;
    fn asset_overlay_defaults(&self, project_dir: &Path) -> Result<Vec<PathBuf>>;
    fn asset_overlay_defaults_with_vault(
        &self,
        project_dir: &Path,
        vault: &dyn Fn() -> Result<PathBuf>,
    ) -> Result<Vec<PathBuf>>;
}

impl EngineConfigExt for StickConfig {
    /// Resolve the release roots once from the already-parsed StickConfig.
    /// Predefined levels are explicit inputs, independent of the general asset roots.
    fn release_assets(&self, _project_dir: &Path) -> Result<Vec<String>> {
        let mut assets = self
            .release
            .as_ref()
            .and_then(|release| release.assets.clone())
            .unwrap_or_else(|| vec!["assets".to_string()]);
        if let Some(levels) = self
            .release
            .as_ref()
            .and_then(|release| release.predefined_levels.as_ref())
        {
            assets.extend(levels.iter().cloned());
        }
        for rel in &assets {
            let path = Path::new(rel);
            if rel.is_empty()
                || path.is_absolute()
                || path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                anyhow::bail!(
                    "stick.toml: [release].assets entry {:?} must be a non-empty relative path inside the project",
                    rel
                );
            }
        }
        Ok(assets)
    }

    fn release_overlay_assets(&self) -> Result<Vec<String>> {
        let assets = self
            .release
            .as_ref()
            .and_then(|release| release.overlay_assets.clone())
            .unwrap_or_default();
        for rel in &assets {
            let path = Path::new(rel);
            if !rel.starts_with("assets/")
                || rel.contains('\\')
                || path.is_absolute()
                || path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                anyhow::bail!(
                    "stick.toml: [release].overlay_assets entry {:?} must be a canonical assets-relative file",
                    rel
                );
            }
        }
        Ok(assets)
    }

    fn asset_overlay_defaults(&self, project_dir: &Path) -> Result<Vec<PathBuf>> {
        self.asset_overlay_defaults_with_vault(project_dir, &crate::art_vault::root)
    }

    fn asset_overlay_defaults_with_vault(
        &self,
        project_dir: &Path,
        vault: &dyn Fn() -> Result<PathBuf>,
    ) -> Result<Vec<PathBuf>> {
        let Some(assets) = &self.assets else {
            return Ok(Vec::new());
        };
        let base = project_dir.canonicalize().unwrap_or_else(|_| {
            if project_dir.is_absolute() {
                project_dir.to_path_buf()
            } else {
                std::env::current_dir()
                    .unwrap_or_default()
                    .join(project_dir)
            }
        });
        assets
            .overlays
            .iter()
            .map(|raw| {
                if raw.is_empty() {
                    anyhow::bail!("stick.toml: [assets].overlays entries must not be empty");
                }
                let vault_path = crate::art_vault::expand_token(raw, vault).map_err(|e| {
                    anyhow::anyhow!("stick.toml: cannot expand [assets].overlays entry {raw:?}: {e}")
                })?;
                let expanded = if let Some(path) = vault_path {
                    path
                } else if raw == "~" || raw.starts_with("~/") || raw.starts_with("~\\") {
                    let home = std::env::var_os("HOME")
                        .or_else(|| std::env::var_os("USERPROFILE"))
                        .ok_or_else(|| anyhow::anyhow!(
                            "stick.toml: cannot expand [assets].overlays entry {raw:?}: no home directory environment"
                        ))?;
                    let tail = raw.trim_start_matches('~').trim_start_matches(['/', '\\']);
                    PathBuf::from(home).join(tail)
                } else if raw.starts_with('~') {
                    anyhow::bail!(
                        "stick.toml: [assets].overlays entry {raw:?} uses unsupported user-home syntax; use ~/path"
                    );
                } else {
                    PathBuf::from(raw)
                };
                Ok(if expanded.is_absolute() { expanded } else { base.join(expanded) })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_section_parses_assets_through_stick_config() {
        let cfg = StickConfig::new_from_str(concat!(
            "[package]\nname = \"x\"\n",
            "[release]\nassets = [\"assets\", \"saves/level.json\"]\n",
            "overlay_assets = [\"assets/particles/blood.png\"]\n",
        ))
        .expect("release section must parse");
        assert_eq!(
            cfg.release.as_ref().unwrap().assets.as_deref(),
            Some(&["assets".to_string(), "saves/level.json".to_string()][..])
        );
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            cfg.release_assets(dir.path()).unwrap(),
            vec!["assets", "saves/level.json"]
        );
        assert_eq!(
            cfg.release_overlay_assets().unwrap(),
            vec!["assets/particles/blood.png"]
        );
    }

    #[test]
    fn release_section_rejects_unknown_keys_and_escape_paths() {
        let err =
            StickConfig::new_from_str("[package]\nname = \"x\"\n[release]\nasset = [\"assets\"]\n")
                .expect_err("unknown release key must reject");
        assert!(
            format!("{err}").contains("asset"),
            "error names the bad release key: {err}"
        );

        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[release]\nassets = [\"../outside\"]\n",
        )
        .expect("path validation happens when selecting assets");
        let dir = tempfile::tempdir().unwrap();
        let err = cfg
            .release_assets(dir.path())
            .expect_err("asset escape must reject");
        assert!(
            format!("{err}").contains("outside"),
            "error names the escape: {err}"
        );

        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[release]\noverlay_assets = [\"../outside.png\"]\n",
        )
        .expect("overlay path validation happens when selecting assets");
        let err = cfg
            .release_overlay_assets()
            .expect_err("overlay asset escape must reject");
        assert!(
            format!("{err}").contains("outside.png"),
            "error names the escape: {err}"
        );
    }

    #[test]
    fn asset_overlay_defaults_resolve_relative_and_home_paths() {
        let project = tempfile::tempdir().unwrap();
        let project_path = project.path().canonicalize().unwrap();
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[assets]\noverlays = [\"../shared\", \"~/rock-assets\"]\n",
        )
        .unwrap();
        let resolved = cfg.asset_overlay_defaults(project.path()).unwrap();
        assert_eq!(resolved[0], project_path.join("../shared"));
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .unwrap();
        assert_eq!(resolved[1], PathBuf::from(home).join("rock-assets"));
    }

    #[test]
    fn asset_overlay_defaults_expand_the_vault_token() {
        let project = tempfile::tempdir().unwrap();
        let vault = tempfile::tempdir().unwrap();
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[assets]\noverlays = [\"$GAME_ART_VAULT/demo\"]\n",
        )
        .unwrap();
        let resolved = cfg
            .asset_overlay_defaults_with_vault(project.path(), &|| Ok(vault.path().to_path_buf()))
            .unwrap();
        assert_eq!(resolved, vec![vault.path().join("demo")]);
        let err = cfg
            .asset_overlay_defaults_with_vault(project.path(), &|| {
                anyhow::bail!("game-art vault not found")
            })
            .unwrap_err();
        // The CLI prints only the outermost message, so the cause must be in it.
        assert!(
            err.to_string().contains("game-art vault not found"),
            "{err}"
        );
    }
}
