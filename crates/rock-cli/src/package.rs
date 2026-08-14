//! Native release preparation for `rock package <game>`.
//!
//! This owns the work that must happen before Fire's Cargo build: declared
//! input resolution, release asset selection/staging, LuaJIT bytecode baking,
//! and the metadata manifest. Fire remains responsible for archive packing,
//! encryption, linking, and executable assembly.

use anyhow::Result;
use bone_semantic::inference::ComptimeInputs;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const PACKAGE_MANIFEST_NAME: &str = "package-manifest.json";

pub fn run_package(project: &str, output: Option<&str>) -> Result<()> {
    rock_host::project::ensure_workspace_root()?;
    let root = rock_host::project::find_workspace_root(&std::env::current_dir()?)
        .ok_or_else(|| anyhow::anyhow!("package: cannot find the Rock workspace root"))?;
    let project_path = PathBuf::from(project);
    let project_abs = project_path
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("package: cannot resolve project {}: {e}", project_path.display()))?;
    let root_abs = root.canonicalize()?;
    let game_key = project_abs
        .strip_prefix(&root_abs)
        .map_err(|_| anyhow::anyhow!("package: project must be inside the Rock workspace: {}", project_abs.display()))?
        .to_string_lossy()
        .replace('\\', "/");
    if game_key.is_empty() {
        anyhow::bail!("package: expected a game directory, got the workspace root");
    }

    let config = rock_host::stick::StickConfig::from_path_optional(&project_abs.join("stick.toml"))?;
    if let Some(config) = config.as_ref() {
        config.validate_build()?;
    }
    let selection = rock_host::release::select_assets(
        &project_abs,
        &game_key,
        config.as_ref(),
        &root_abs,
        false,
    )?;
    for missing in &selection.missing {
        log::warn!(target: "PACKAGE", "release asset {} does not exist, skipping", missing.display());
    }

    let output_dir = output
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("crates/fire/build"));
    if output_dir.exists() && !output_dir.is_dir() {
        anyhow::bail!("package: output {} is not a directory", output_dir.display());
    }
    std::fs::create_dir_all(&output_dir)?;
    let lua_path = output_dir.join("main.lua");
    let bytecode_path = output_dir.join("main.bc");
    let manifest_path = output_dir.join(PACKAGE_MANIFEST_NAME);
    remove_if_present(&lua_path)?;
    remove_if_present(&bytecode_path)?;
    remove_if_present(&manifest_path)?;
    let assets_dir = output_dir.join("assets");
    let staged = rock_host::release::stage_assets(&selection, &assets_dir)?;

    // Resolve once for the package and hand the same resolved map to the
    // compiler. In particular, a secret command is never run a second time
    // merely to create the manifest.
    let inputs = match config.as_ref() {
        Some(config) => crate::comptime::inputs_for_project_with_config(&project_abs, config)?,
        None => crate::comptime::inputs_for_project_dir(&project_abs)?,
    };
    crate::skull_backend::skull_compile_project_with_inputs(
        &project_abs.to_string_lossy(),
        Some(lua_path.to_string_lossy().into_owned()),
        inputs.clone(),
    )?;

    let luajit = rock_host::project::luajit_path();
    if !luajit.is_file() {
        anyhow::bail!(
            "package: the project LuaJIT is missing at {}; package uses that exact binary",
            luajit.display()
        );
    }
    let bake = Command::new(&luajit)
        .current_dir(&root_abs)
        .env("LUA_PATH", rock_host::project::lua_path_value())
        .args(["-b"])
        .arg(&lua_path)
        .arg(&bytecode_path)
        .output()
        .map_err(|e| anyhow::anyhow!("package: failed to run {}: {e}", luajit.display()))?;
    if !bake.status.success() {
        anyhow::bail!("package: LuaJIT bytecode baking failed ({})", bake.status);
    }
    if !bytecode_path.is_file() {
        anyhow::bail!("package: LuaJIT reported success but did not create {}", bytecode_path.display());
    }

    let manifest = package_manifest(
        &project_abs,
        &game_key,
        config.as_ref(),
        &inputs,
        &lua_path,
        &bytecode_path,
        &staged,
    )?;
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    rock_host::project::write_atomic(&manifest_path, &bytes)?;
    log::info!(
        target: "PACKAGE",
        "{} -> {} ({} release assets, manifest {})",
        game_key,
        output_dir.display(),
        staged.file_count,
        manifest_path.display()
    );
    Ok(())
}

fn package_manifest(
    project: &Path,
    game_key: &str,
    config: Option<&rock_host::stick::StickConfig>,
    inputs: &ComptimeInputs,
    lua_path: &Path,
    bytecode_path: &Path,
    staged: &rock_host::release::StagedAssets,
) -> Result<Value> {
    let mut vars = Map::new();
    let declarations = config
        .and_then(|config| config.build.as_ref())
        .map(|build| &build.vars);
    let mut names: Vec<_> = inputs.vars.keys().map(|name| name.as_str().to_string()).collect();
    names.sort();
    for name in names {
        let value = &inputs.vars[&ustr::Ustr::from(name.as_str())];
        let source = declarations
            .and_then(|decls| decls.get(&name))
            .map(source_kind)
            .unwrap_or("resolved");
        let mut metadata = Map::new();
        metadata.insert("source".to_string(), Value::String(source.to_string()));
        metadata.insert("secret".to_string(), Value::Bool(value.secret));
        if value.secret {
            metadata.insert(
                "fingerprint".to_string(),
                Value::String(fingerprint(&value.value)),
            );
        } else {
            metadata.insert("value".to_string(), Value::String(value.value.clone()));
        }
        vars.insert(name, Value::Object(metadata));
    }

    let package_name = config
        .map(|config| config.package.name.clone())
        .unwrap_or_else(|| project.file_name().unwrap_or_default().to_string_lossy().into_owned());
    let package_version = config.map(|config| config.package.version().to_string());
    let mut package = Map::new();
    package.insert("name".to_string(), Value::String(package_name));
    if let Some(version) = package_version {
        package.insert("version".to_string(), Value::String(version));
    }
    package.insert("game".to_string(), Value::String(game_key.to_string()));
    package.insert(
        "channel".to_string(),
        Value::String(if inputs.channel.is_empty() {
            "dev".to_string()
        } else {
            inputs.channel.clone()
        }),
    );

    Ok(json!({
        "schema": 1,
        "package": package,
        "build_vars": vars,
        "artifacts": {
            "lua": artifact_metadata(lua_path)?,
            "bytecode": artifact_metadata(bytecode_path)?,
            "assets": {
                "path": "assets",
                "file_count": staged.file_count,
                "keys": staged.keys,
            },
        },
    }))
}

fn source_kind(var: &rock_host::stick::BuildVar) -> &'static str {
    if var.cmd.is_some() {
        "cmd"
    } else if var.builtin.is_some() {
        "builtin"
    } else {
        "value"
    }
}

fn artifact_metadata(path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path)?;
    Ok(json!({
        "path": path.file_name().unwrap_or_default().to_string_lossy(),
        "bytes": bytes.len(),
        "sha256": fingerprint_bytes(&bytes),
    }))
}

fn fingerprint(value: &str) -> String {
    fingerprint_bytes(value.as_bytes())
}

fn fingerprint_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn secret_manifest_value_is_replaced_by_a_fingerprint() {
        let secret = "known-secret";
        assert_eq!(fingerprint(secret), format!("sha256:{:x}", Sha256::digest(secret.as_bytes())));
        let mut vars = BTreeMap::new();
        vars.insert("TOKEN", fingerprint(secret));
        let encoded = serde_json::to_string(&vars).unwrap();
        assert!(!encoded.contains(secret));
    }
}
