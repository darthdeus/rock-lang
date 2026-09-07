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
use sound_core::AssetResolver;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const PACKAGE_MANIFEST_NAME: &str = "package-manifest.json";

struct PackageContext {
    root: PathBuf,
    project: PathBuf,
    game_key: String,
    config: Option<rock_host::stick::StickConfig>,
}

pub(crate) struct SoundInspectionTarget {
    pub root: PathBuf,
    pub project: PathBuf,
    pub game_key: String,
    pub overlays: Vec<PathBuf>,
}

fn project_and_config(project: &str) -> Result<(PathBuf, Option<rock_host::stick::StickConfig>)> {
    let project_path = PathBuf::from(project);
    let project_abs = project_path
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot resolve project {}: {e}", project_path.display()))?;
    let config = rock_host::stick::StickConfig::from_path_optional(&project_abs.join("stick.toml"))?;
    if let Some(config) = config.as_ref() {
        config.validate_build()?;
    }
    Ok((project_abs, config))
}

fn package_context(project: &str) -> Result<PackageContext> {
    rock_host::project::ensure_workspace_root()?;
    let (project_abs, config) = project_and_config(project)?;
    let root = rock_host::project::find_source_root(&project_abs)
        .ok_or_else(|| anyhow::anyhow!("package: cannot find the source workspace root for {}", project_abs.display()))?;
    let root_abs = root.canonicalize()?;
    let game_key = project_abs
        .strip_prefix(&root_abs)
        .map_err(|_| anyhow::anyhow!("package: project must be inside the Rock workspace: {}", project_abs.display()))?
        .to_string_lossy()
        .replace('\\', "/");
    if game_key.is_empty() {
        anyhow::bail!("package: expected a game directory, got the workspace root");
    }
    Ok(PackageContext {
        root: root_abs,
        project: project_abs,
        game_key,
        config,
    })
}

pub fn sound_project_path_for(project: &str) -> Result<Option<PathBuf>> {
    let (_, config) = project_and_config(project)?;
    if config.as_ref().and_then(|config| config.sound.as_ref()).is_none() {
        return Ok(None);
    }
    let context = package_context(project)?;
    rock_host::release::sound_project_path(&context.project, context.config.as_ref())?
        .map(|path| path.canonicalize().map_err(Into::into))
        .transpose()
}

pub fn prepare_sound_bank(project: &str) -> Result<Option<PathBuf>> {
    let (_, config) = project_and_config(project)?;
    if config.as_ref().and_then(|config| config.sound.as_ref()).is_none() {
        return Ok(None);
    }
    let context = package_context(project)?;
    let overlays = rock_host::release::asset_overlay_roots(&context.project, context.config.as_ref())?;
    let mut selection = rock_host::release::AssetSelection::default();
    let Some(sound) = compile_sound_project(
        &context.project,
        &context.game_key,
        context.config.as_ref(),
        &overlays,
        &mut selection,
        MissingSoundAssets::Allow,
    )? else {
        return Ok(None);
    };
    let bank_path = context.project.join("assets/sound_bank.json");
    if let Some(parent) = bank_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    rock_host::project::write_atomic(&bank_path, &sound.bank_bytes)?;
    Ok(Some(bank_path))
}

pub(crate) fn prepare_sound_inspection_target(project: &str) -> Result<SoundInspectionTarget> {
    let context = package_context(project)?;
    if prepare_sound_bank(project)?.is_none() {
        anyhow::bail!(
            "sound-inspect: {} has no [sound].project declaration",
            context.game_key,
        );
    }
    let overlays = rock_host::release::asset_overlay_roots(
        &context.project,
        context.config.as_ref(),
    )?;
    Ok(SoundInspectionTarget {
        root: context.root,
        project: context.project,
        game_key: context.game_key,
        overlays,
    })
}

pub fn run_package(project: &str, output: Option<&str>) -> Result<()> {
    let context = package_context(project)?;
    let root_abs = context.root;
    let project_abs = context.project;
    let game_key = context.game_key;
    let config = context.config;
    let overlays = rock_host::release::asset_overlay_roots(&project_abs, config.as_ref())?;
    let selection = rock_host::release::select_assets_with_overlays(
        &project_abs,
        &game_key,
        config.as_ref(),
        &root_abs,
        false,
        &overlays,
    )?;
    let (selection, sound, visual) = compile_declared_asset_banks(
        &project_abs,
        &game_key,
        config.as_ref(),
        &overlays,
        selection,
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
    let mut staged = rock_host::release::stage_assets(&selection, &assets_dir)?;
    if let Some(sound) = sound.as_ref() {
        write_sound_bank(&assets_dir, &mut staged, sound)?;
    }

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
        sound.as_ref(),
        visual.as_ref(),
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

pub fn select_release_assets(
    project_dir: &Path,
    game_key: &str,
    config: Option<&rock_host::stick::StickConfig>,
    workspace_root: &Path,
    include_web_font: bool,
) -> Result<rock_host::release::AssetSelection> {
    let overlays = rock_host::release::asset_overlay_roots(project_dir, config)?;
    let selection = rock_host::release::select_assets_with_overlays(
        project_dir,
        game_key,
        config,
        workspace_root,
        include_web_font,
        &overlays,
    )?;
    let (selection, _, _) = compile_declared_asset_banks(
        project_dir,
        game_key,
        config,
        &overlays,
        selection,
    )?;
    Ok(selection)
}

struct CompiledSoundPackage {
    project: String,
    bank_key: String,
    bank_bytes: Vec<u8>,
    dependencies: Vec<String>,
}

struct CompiledVisualPackage {
    bank_key: String,
    schema: u64,
    plan_fingerprint: String,
    dependencies: Vec<String>,
    fixed_output_keys: BTreeSet<String>,
}

fn compile_declared_asset_banks(
    project_dir: &Path,
    game_key: &str,
    config: Option<&rock_host::stick::StickConfig>,
    overlays: &[PathBuf],
    mut selection: rock_host::release::AssetSelection,
) -> Result<(
    rock_host::release::AssetSelection,
    Option<CompiledSoundPackage>,
    Option<CompiledVisualPackage>,
)> {
    let sound = compile_sound_project(
        project_dir,
        game_key,
        config,
        overlays,
        &mut selection,
        MissingSoundAssets::Reject,
    )?;
    let visual = compile_visual_bank(project_dir, game_key, overlays, &mut selection)?;
    let loose_overlay = include_declared_overlay_assets(
        project_dir,
        game_key,
        config,
        overlays,
        &mut selection,
    )?;
    narrow_asset_overlay_selection(
        &mut selection,
        overlays,
        sound.as_ref(),
        visual.as_ref(),
        &loose_overlay,
    );
    Ok((selection, sound, visual))
}

fn include_declared_overlay_assets(
    project_dir: &Path,
    game_key: &str,
    config: Option<&rock_host::stick::StickConfig>,
    overlays: &[PathBuf],
    selection: &mut rock_host::release::AssetSelection,
) -> Result<Vec<String>> {
    let Some(config) = config else { return Ok(Vec::new()) };
    let logical_keys = config.release_overlay_assets()?;
    let mut dependencies = Vec::with_capacity(logical_keys.len());
    for logical_key in logical_keys {
        let asset = rock_host::release::resolve_asset_with_overlays(
            project_dir,
            game_key,
            overlays,
            &logical_key,
        )?
        .ok_or_else(|| anyhow::anyhow!(
            "package: declared overlay asset does not exist: {logical_key}"
        ))?;
        if !overlays.iter().any(|overlay| asset.source.starts_with(overlay)) {
            anyhow::bail!(
                "package: declared overlay asset resolves outside configured overlays: {logical_key}"
            );
        }
        dependencies.push(asset.key.clone());
        rock_host::release::include_release_asset(selection, asset);
    }
    dependencies.sort();
    dependencies.dedup();
    Ok(dependencies)
}

struct ReleaseSoundResolver<'a> {
    project_dir: &'a Path,
    game_key: &'a str,
    overlays: &'a [PathBuf],
}

impl AssetResolver for ReleaseSoundResolver<'_> {
    fn resolve(&self, key: &str) -> Option<PathBuf> {
        rock_host::release::resolve_asset_with_overlays(
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum MissingSoundAssets {
    Allow,
    Reject,
}

fn compile_sound_project(
    project_dir: &Path,
    game_key: &str,
    config: Option<&rock_host::stick::StickConfig>,
    overlays: &[PathBuf],
    selection: &mut rock_host::release::AssetSelection,
    missing_assets: MissingSoundAssets,
) -> Result<Option<CompiledSoundPackage>> {
    let Some(sound) = config.and_then(|config| config.sound.as_ref()) else {
        return Ok(None);
    };
    let project_path = rock_host::release::sound_project_path(project_dir, config)?
        .ok_or_else(|| anyhow::anyhow!("package: sound project declaration disappeared"))?;
    let project_text = std::fs::read_to_string(&project_path).map_err(|error| {
        anyhow::anyhow!(
            "package: sound project {} could not be read: {error}",
            project_path.display()
        )
    })?;
    let project = sound_core::parse_project(&project_text)
        .map_err(|error| anyhow::anyhow!("package: {}: {error}", sound.project))?;
    let resolver = ReleaseSoundResolver {
        project_dir,
        game_key,
        overlays,
    };
    let bank = match missing_assets {
        MissingSoundAssets::Allow => {
            sound_core::compile_project_allowing_missing_assets(&project, &resolver)
        }
        MissingSoundAssets::Reject => sound_core::compile_project(&project, &resolver),
    }
    .map_err(|error| {
        anyhow::anyhow!("package: sound project {}: {error}", sound.project)
    })?;
    let bank_bytes = sound_core::serialize_runtime_bank(&bank)
        .map_err(|error| anyhow::anyhow!("package: sound project {}: {error}", sound.project))?;
    let mut dependencies = Vec::with_capacity(bank.dependencies.len());
    for dependency in &bank.dependencies {
        let asset = rock_host::release::resolve_asset_with_overlays(
            project_dir,
            game_key,
            overlays,
            dependency,
        )?;
        let Some(asset) = asset else {
            if missing_assets == MissingSoundAssets::Allow {
                log::warn!(
                    target: "SOUND",
                    "sound project {} references missing asset {}; plays using it will be silent",
                    sound.project,
                    dependency,
                );
                continue;
            }
            return Err(anyhow::anyhow!(
                "package: sound dependency is not stageable: {dependency}"
            ));
        };
        rock_host::release::include_release_asset(selection, asset);
        dependencies.push(format!("{game_key}/{dependency}"));
    }
    dependencies.sort();
    Ok(Some(CompiledSoundPackage {
        project: sound.project.clone(),
        bank_key: format!("{game_key}/assets/sound_bank.json"),
        bank_bytes,
        dependencies,
    }))
}

fn compile_visual_bank(
    project_dir: &Path,
    game_key: &str,
    overlays: &[PathBuf],
    selection: &mut rock_host::release::AssetSelection,
) -> Result<Option<CompiledVisualPackage>> {
    let bank_logical_key = "assets/visual_bank.json";
    let Some(bank_asset) = rock_host::release::resolve_worktree_asset(
        project_dir,
        game_key,
        bank_logical_key,
    )? else {
        let bank_key = format!("{game_key}/{bank_logical_key}");
        selection.assets.retain(|asset| asset.key != bank_key);
        if rock_host::release::resolve_asset_with_overlays(
            project_dir,
            game_key,
            overlays,
            bank_logical_key,
        )?
        .is_some()
        {
            anyhow::bail!(
                "package: worktree visual bank is missing; refusing the overlay bank"
            );
        }
        return Ok(None);
    };
    let bytes = std::fs::read(&bank_asset.source)
        .map_err(|error| anyhow::anyhow!("package: visual bank could not be read: {error}"))?;
    let root: Value = serde_json::from_slice(&bytes)
        .map_err(|error| anyhow::anyhow!("package: visual bank is invalid JSON: {error}"))?;
    let schema = root.get("schema_version").and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("package: visual bank has no integer schema_version"))?;
    if schema != 1 {
        anyhow::bail!("package: unsupported visual bank schema {schema}");
    }
    let plan_fingerprint = root.get("plan_fingerprint").and_then(Value::as_str)
        .filter(|value| value.starts_with("sha256:") && value.len() == 71)
        .ok_or_else(|| anyhow::anyhow!("package: visual bank has no valid plan_fingerprint"))?
        .to_string();
    let values = root.get("dependencies").and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("package: visual bank has no dependency array"))?;
    let mut logical_dependencies = Vec::with_capacity(values.len());
    for value in values {
        let dependency = value.as_str()
            .ok_or_else(|| anyhow::anyhow!("package: visual bank dependency is not a string"))?;
        if !dependency.starts_with("assets/.eat/") {
            anyhow::bail!("package: visual dependency is not immutable: {dependency}");
        }
        logical_dependencies.push(dependency.to_string());
    }
    if !logical_dependencies.windows(2).all(|pair| pair[0] < pair[1]) {
        anyhow::bail!("package: visual bank dependencies must be sorted and unique");
    }

    rock_host::release::replace_release_asset(selection, bank_asset.clone());
    let mut dependencies = Vec::with_capacity(logical_dependencies.len());
    let mut fixed_output_keys = BTreeSet::new();
    for dependency in &logical_dependencies {
        let asset = rock_host::release::resolve_asset_with_overlays(
            project_dir,
            game_key,
            overlays,
            dependency,
        )?
        .ok_or_else(|| anyhow::anyhow!("package: visual dependency is not stageable: {dependency}"))?;
        rock_host::release::include_release_asset(selection, asset);
        dependencies.push(format!("{game_key}/{dependency}"));
        if let Some((_, original)) = dependency
            .strip_prefix("assets/.eat/")
            .and_then(|rest| rest.split_once('/'))
        {
            let fixed = format!("{game_key}/assets/{original}");
            fixed_output_keys.insert(fixed.clone());
            if let Some(stem) = fixed.strip_suffix(".png") {
                let base = stem.strip_suffix("-normal")
                    .or_else(|| stem.strip_suffix("-shadow"))
                    .unwrap_or(stem);
                fixed_output_keys.insert(format!("{base}.json"));
            }
        }
    }
    dependencies.sort();
    Ok(Some(CompiledVisualPackage {
        bank_key: format!("{game_key}/{bank_logical_key}"),
        schema,
        plan_fingerprint,
        dependencies,
        fixed_output_keys,
    }))
}

fn narrow_asset_overlay_selection(
    selection: &mut rock_host::release::AssetSelection,
    overlays: &[PathBuf],
    sound: Option<&CompiledSoundPackage>,
    visual: Option<&CompiledVisualPackage>,
    loose_overlay: &[String],
) {
    let Some(visual) = visual else { return };
    let mut admitted = BTreeSet::new();
    if let Some(sound) = sound {
        admitted.extend(sound.dependencies.iter().cloned());
    }
    admitted.extend(loose_overlay.iter().cloned());
    admitted.insert(visual.bank_key.clone());
    admitted.extend(visual.dependencies.iter().cloned());
    selection.assets.retain(|asset| {
        if visual.fixed_output_keys.contains(&asset.key) {
            return false;
        }
        let from_overlay = overlays.iter().any(|overlay| asset.source.starts_with(overlay));
        if from_overlay || asset.key.contains("/assets/.eat/") {
            return admitted.contains(&asset.key);
        }
        true
    });
}

fn write_sound_bank(
    assets_dir: &Path,
    staged: &mut rock_host::release::StagedAssets,
    sound: &CompiledSoundPackage,
) -> Result<()> {
    let destination = assets_dir.join(&sound.bank_key);
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    rock_host::project::write_atomic(&destination, &sound.bank_bytes)?;
    if !staged.keys.iter().any(|key| key == &sound.bank_key) {
        staged.keys.push(sound.bank_key.clone());
        staged.keys.sort();
    }
    staged.file_count = staged.keys.len();
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
    sound: Option<&CompiledSoundPackage>,
    visual: Option<&CompiledVisualPackage>,
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

    let mut artifacts = Map::new();
    artifacts.insert("lua".to_string(), artifact_metadata(lua_path)?);
    artifacts.insert("bytecode".to_string(), artifact_metadata(bytecode_path)?);
    artifacts.insert(
        "assets".to_string(),
        json!({
            "path": "assets",
            "file_count": staged.file_count,
            "keys": staged.keys,
        }),
    );
    if let Some(sound) = sound {
        artifacts.insert(
            "sound".to_string(),
            json!({
                "project": sound.project,
                "bank": sound.bank_key,
                "schema": sound_core::RUNTIME_SCHEMA_VERSION,
                "dependencies": sound.dependencies,
            }),
        );
    }
    if let Some(visual) = visual {
        artifacts.insert(
            "visual".to_string(),
            json!({
                "bank": visual.bank_key,
                "schema": visual.schema,
                "plan_fingerprint": visual.plan_fingerprint,
                "dependencies": visual.dependencies,
            }),
        );
    }

    Ok(json!({
        "schema": 1,
        "package": package,
        "build_vars": vars,
        "artifacts": artifacts,
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
    fn external_project_without_sound_does_not_need_workspace_context() {
        let fixture = tempfile::tempdir().expect("external fixture");
        std::fs::write(
            fixture.path().join("stick.toml"),
            "[package]\nname = \"external-no-sound\"\n",
        )
        .unwrap();

        assert_eq!(
            sound_project_path_for(fixture.path().to_str().unwrap()).unwrap(),
            None
        );
        assert_eq!(
            prepare_sound_bank(fixture.path().to_str().unwrap()).unwrap(),
            None
        );
    }

    #[test]
    fn development_bank_allows_a_missing_sound_dependency() {
        let workspace = rock_host::project::find_workspace_root(
            &std::env::current_dir().expect("current directory"),
        )
        .expect("workspace root");
        let fixture = tempfile::tempdir_in(workspace).expect("sound fixture");
        std::fs::write(
            fixture.path().join("stick.toml"),
            "[package]\nname = \"missing-sound-dev\"\n\n[sound]\nproject = \"sound_project.json\"\n",
        )
        .unwrap();
        std::fs::write(
            fixture.path().join("sound_project.json"),
            r#"{
  "version": 1,
  "events": [{"id": "fixture.event", "steps": [{"layers": [{"sources": ["assets/sfx/missing.ogg"]}]}]}]
}
"#,
        )
        .unwrap();

        let bank_path = prepare_sound_bank(fixture.path().to_str().unwrap())
            .expect("a missing sound must not prevent a development run")
            .expect("fixture declares a sound bank");
        let bank: Value = serde_json::from_slice(&std::fs::read(bank_path).unwrap()).unwrap();
        assert_eq!(
            bank["events"][0]["steps"][0]["layers"][0]["sources"],
            json!(["assets/sfx/missing.ogg"])
        );
    }

    #[test]
    fn release_visual_bank_rejects_a_missing_dependency() {
        let workspace = rock_host::project::find_workspace_root(
            &std::env::current_dir().expect("current directory"),
        )
        .expect("workspace root");
        let fixture = tempfile::tempdir_in(workspace).expect("visual fixture");
        let overlay = fixture.path().join("overlay");
        std::fs::create_dir_all(fixture.path().join("assets")).unwrap();
        std::fs::create_dir_all(overlay.join("assets")).unwrap();
        std::fs::write(
            fixture.path().join("assets/visual_bank.json"),
            format!(
                "{{\"schema_version\":1,\"plan_fingerprint\":\"sha256:{}\",\"dependencies\":[\"assets/.eat/missing/image.png\"]}}",
                "0".repeat(64),
            ),
        )
        .unwrap();
        std::fs::write(
            overlay.join("assets/visual_bank.json"),
            format!(
                "{{\"schema_version\":1,\"plan_fingerprint\":\"sha256:{}\",\"dependencies\":[\"assets/.eat/missing/image.png\"]}}",
                "0".repeat(64),
            ),
        )
        .unwrap();

        let mut selection = rock_host::release::AssetSelection::default();
        let result = compile_visual_bank(
            fixture.path(),
            "fixture",
            &[overlay],
            &mut selection,
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("release admission accepted a missing visual dependency"),
        };
        assert!(error.to_string().contains("visual dependency is not stageable"));
    }

    #[test]
    fn release_visual_bank_prefers_the_worktree_bank_over_a_stale_overlay_bank() {
        let workspace = rock_host::project::find_workspace_root(
            &std::env::current_dir().expect("current directory"),
        )
        .expect("workspace root");
        let fixture = tempfile::tempdir_in(workspace).expect("visual fixture");
        let overlay = fixture.path().join("overlay");
        let project_object = fixture
            .path()
            .join("assets/.eat/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        std::fs::create_dir_all(&project_object).unwrap();
        std::fs::create_dir_all(overlay.join("assets")).unwrap();
        std::fs::write(
            fixture.path().join("assets/visual_bank.json"),
            "{\"schema_version\":1,\"plan_fingerprint\":\"sha256:0000000000000000000000000000000000000000000000000000000000000000\",\"dependencies\":[\"assets/.eat/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/image.png\"]}\n",
        )
        .unwrap();
        std::fs::write(
            project_object.join("image.png"),
            b"project-object",
        )
        .unwrap();
        std::fs::write(
            overlay.join("assets/visual_bank.json"),
            "{\"schema_version\":1,\"plan_fingerprint\":\"sha256:0000000000000000000000000000000000000000000000000000000000000000\",\"dependencies\":[\"assets/.eat/missing/image.png\"]}\n",
        )
        .unwrap();

        let mut selection = rock_host::release::AssetSelection::default();
        compile_visual_bank(
            fixture.path(),
            "fixture",
            &[overlay],
            &mut selection,
        )
        .expect("the project-owned bank should be selected");
        let bank = selection
            .assets
            .iter()
            .find(|asset| asset.key == "fixture/assets/visual_bank.json")
            .expect("selected bank");
        assert_eq!(bank.source, fixture.path().join("assets/visual_bank.json"));
    }

    #[test]
    fn secret_manifest_value_is_replaced_by_a_fingerprint() {
        let secret = "known-secret";
        assert_eq!(fingerprint(secret), format!("sha256:{:x}", Sha256::digest(secret.as_bytes())));
        let mut vars = BTreeMap::new();
        vars.insert("TOKEN", fingerprint(secret));
        let encoded = serde_json::to_string(&vars).unwrap();
        assert!(!encoded.contains(secret));
    }

    #[test]
    fn exact_banks_exclude_fixed_stale_and_unrelated_overlay_assets() {
        let overlay = PathBuf::from("/asset-overlay");
        let game = "mag";
        let generated = format!("{game}/assets/.eat/current/characters/mage/mage-idle.png");
        let sound_key = format!("{game}/assets/sfx/cast.ogg");
        let loose_key = format!("{game}/assets/particles/blood.png");
        let bank_key = format!("{game}/assets/visual_bank.json");
        let mut selection = rock_host::release::AssetSelection {
            assets: vec![
                rock_host::release::ReleaseAsset { key: generated.clone(), source: overlay.join("assets/.eat/current/mage.png") },
                rock_host::release::ReleaseAsset { key: format!("{game}/assets/.eat/stale/mage.png"), source: overlay.join("assets/.eat/stale/mage.png") },
                rock_host::release::ReleaseAsset { key: bank_key.clone(), source: overlay.join("assets/visual_bank.json") },
                rock_host::release::ReleaseAsset { key: sound_key.clone(), source: overlay.join("assets/sfx/cast.ogg") },
                rock_host::release::ReleaseAsset { key: loose_key.clone(), source: overlay.join("assets/particles/blood.png") },
                rock_host::release::ReleaseAsset { key: format!("{game}/assets/unrelated-sentinel.txt"), source: overlay.join("assets/unrelated-sentinel.txt") },
                rock_host::release::ReleaseAsset { key: format!("{game}/assets/characters/mage/mage-idle.png"), source: PathBuf::from("/project/assets/characters/mage/mage-idle.png") },
                rock_host::release::ReleaseAsset { key: format!("{game}/assets/ui/frame.png"), source: PathBuf::from("/project/assets/ui/frame.png") },
            ],
            missing: Vec::new(),
        };
        let sound = CompiledSoundPackage {
            project: "sound_project.json".to_string(),
            bank_key: format!("{game}/assets/sound_bank.json"),
            bank_bytes: Vec::new(),
            dependencies: vec![sound_key.clone()],
        };
        let visual = CompiledVisualPackage {
            bank_key: bank_key.clone(),
            schema: 1,
            plan_fingerprint: format!("sha256:{}", "0".repeat(64)),
            dependencies: vec![generated.clone()],
            fixed_output_keys: [format!("{game}/assets/characters/mage/mage-idle.png")].into(),
        };

        narrow_asset_overlay_selection(
            &mut selection,
            &[overlay],
            Some(&sound),
            Some(&visual),
            std::slice::from_ref(&loose_key),
        );
        let keys: BTreeSet<_> = selection.assets.iter().map(|asset| asset.key.as_str()).collect();
        assert_eq!(keys.len(), 5, "admission and exclusion populations");
        assert!(keys.contains(generated.as_str()));
        assert!(keys.contains(bank_key.as_str()));
        assert!(keys.contains(sound_key.as_str()));
        assert!(keys.contains(loose_key.as_str()));
        assert!(keys.contains("mag/assets/ui/frame.png"));
        assert!(!keys.iter().any(|key| key.contains("stale") || key.contains("sentinel")));
        assert!(!keys.contains("mag/assets/characters/mage/mage-idle.png"));
    }

    #[test]
    fn generated_sound_bank_does_not_duplicate_an_already_selected_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = "mag/assets/sound_bank.json".to_string();
        let mut staged = rock_host::release::StagedAssets {
            file_count: 1,
            keys: vec![key.clone()],
        };
        let sound = CompiledSoundPackage {
            project: "sound_project.json".to_string(),
            bank_key: key.clone(),
            bank_bytes: b"bank".to_vec(),
            dependencies: Vec::new(),
        };
        write_sound_bank(dir.path(), &mut staged, &sound).unwrap();
        assert_eq!(staged.file_count, 1);
        assert_eq!(staged.keys, vec![key]);
    }
}
