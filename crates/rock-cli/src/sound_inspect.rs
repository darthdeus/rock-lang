use anyhow::{Context, Result};
use std::process::Command;

pub fn run(project: &str) -> Result<()> {
    let target = crate::package::prepare_sound_inspection_target(project)?;
    let inspector = target.root.join("tools/sound-bank-inspector");
    if !inspector.join("stick.toml").is_file() {
        anyhow::bail!(
            "sound-inspect: inspector project is missing: {}",
            inspector.display(),
        );
    }
    let overlays = std::env::join_paths(&target.overlays)
        .context("sound-inspect: cannot encode asset overlay paths")?;
    let status = Command::new(std::env::current_exe()?)
        .current_dir(&target.root)
        .arg("stone")
        .arg(&inspector)
        .arg("--")
        .arg(&target.game_key)
        .env("GAME_SOURCE_DIR", &target.project)
        .env(rock_host::release::ASSET_OVERLAYS_ENV, overlays)
        .status()
        .context("sound-inspect: failed to launch inspector")?;
    if !status.success() {
        anyhow::bail!("sound-inspect: inspector exited with {status}");
    }
    Ok(())
}
