//! Prepare declared assets, bytecode and metadata in a caller-selected output directory.

use anyhow::Result;
use bone_semantic::inference::ComptimeInputs;
use rodio::Source;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sound_core::AssetResolver;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const PACKAGE_MANIFEST_NAME: &str = "package-manifest.json";

struct PackageContext {
    project: PathBuf,
    game_key: String,
    config: Option<rock_host::stick::StickConfig>,
}

fn project_and_config(project: &str) -> Result<(PathBuf, Option<rock_host::stick::StickConfig>)> {
    let project_path = PathBuf::from(project);
    let project_abs = project_path
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot resolve project {}: {e}", project_path.display()))?;
    let config =
        rock_host::stick::StickConfig::from_path_optional(&project_abs.join("stick.toml"))?;
    if let Some(config) = config.as_ref() {
        config.validate_build()?;
    }
    Ok((project_abs, config))
}

fn package_context(project: &str) -> Result<PackageContext> {
    let (project_abs, config) = project_and_config(project)?;
    let game_key = match &config {
        Some(config) => config.package.name.clone(),
        None => project_abs
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("package: project needs a name"))?
            .to_owned(),
    };
    Ok(PackageContext {
        project: project_abs,
        game_key,
        config,
    })
}

pub fn sound_project_path_for(project: &str) -> Result<Option<PathBuf>> {
    let (_, config) = project_and_config(project)?;
    if config
        .as_ref()
        .and_then(|config| config.sound.as_ref())
        .is_none()
    {
        return Ok(None);
    }
    let context = package_context(project)?;
    rock_engine_host::release::sound_project_path(&context.project, context.config.as_ref())?
        .map(|path| path.canonicalize().map_err(Into::into))
        .transpose()
}

#[derive(Default)]
pub(crate) struct SoundWatchInputs {
    pub directories: BTreeSet<PathBuf>,
    selections: Vec<(PathBuf, globset::GlobMatcher)>,
}

impl SoundWatchInputs {
    pub fn contains(&self, path: &Path) -> bool {
        self.directories.contains(path)
            || self.selections.iter().any(|(root, matcher)| {
                path.strip_prefix(root)
                    .is_ok_and(|key| matcher.is_match(key))
            })
    }
}

pub(crate) fn sound_watch_inputs(project: &str) -> Result<SoundWatchInputs> {
    let (project, config) = project_and_config(project)?;
    let Some(path) = rock_engine_host::release::sound_project_path(&project, config.as_ref())?
    else {
        return Ok(SoundWatchInputs::default());
    };
    let catalog = sound_core::parse_project(&std::fs::read_to_string(path)?)?;
    let mut roots = rock_engine_host::release::asset_overlay_roots(&project, config.as_ref())?;
    roots.push(project);
    let mut inputs = SoundWatchInputs::default();
    for event in &catalog.events {
        for step in &event.steps {
            for layer in &step.layers {
                for key in &layer.sources {
                    let directory =
                        sound_core::source_directory(key).map_err(anyhow::Error::msg)?;
                    for root in &roots {
                        let path = root.join(directory);
                        // Watch an existing ancestor so creating the selected directory is observable.
                        let existing =
                            path.ancestors()
                                .find(|parent| parent.is_dir())
                                .ok_or_else(|| {
                                    anyhow::anyhow!("no sound watch root for {}", path.display())
                                })?;
                        inputs.directories.insert(existing.canonicalize()?);
                        inputs.selections.push((
                            root.clone(),
                            sound_core::source_matcher(key).map_err(anyhow::Error::msg)?,
                        ));
                    }
                }
            }
        }
    }
    Ok(inputs)
}

pub fn check_project_assets(project: &Path) -> Result<()> {
    let (project, config) = project_and_config(
        project
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("project path is not UTF-8"))?,
    )?;
    let overlays = rock_engine_host::release::asset_overlay_roots(&project, config.as_ref())?;
    let prepared = crate::asset_selection::prepare(&project, "game", config.as_ref(), &overlays)?;
    for missing in prepared.assets.missing {
        log::warn!(target: "ASSETS", "release asset {} does not exist, skipping", missing.display());
    }
    Ok(())
}

pub fn prepare_sound_bank(project: &str) -> Result<Option<PathBuf>> {
    let (_, config) = project_and_config(project)?;
    if config
        .as_ref()
        .and_then(|config| config.sound.as_ref())
        .is_none()
    {
        return Ok(None);
    }
    let context = package_context(project)?;
    let overlays = rock_engine_host::release::configured_asset_overlays_from_with_vault(
        &context.project,
        context.config.as_ref(),
        std::env::var_os(rock_engine_host::release::ASSET_OVERLAYS_ENV).as_deref(),
        &rock_engine_host::art_vault::root,
    )?;
    let mut selection = rock_engine_host::release::AssetSelection::default();
    let Some(sound) = crate::asset_selection::prepare_sound(
        &context.project,
        &context.game_key,
        context.config.as_ref(),
        &overlays,
        &mut selection,
        false,
    )?
    else {
        return Ok(None);
    };
    let sound = compile_sound_project(&context.project, &context.game_key, sound)?;
    let bank_path = context.project.join("assets/sound_bank.json");
    match std::fs::read(&bank_path) {
        Ok(existing) if existing == sound.bank_bytes => return Ok(Some(bank_path)),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if let Some(parent) = bank_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    rock_host::project::write_atomic(&bank_path, &sound.bank_bytes)?;
    Ok(Some(bank_path))
}

fn shipped_native_libraries(project: &str) -> Result<serde_json::Value> {
    let libs: Vec<_> = rock_engine_host::engine_build::game_engine_libs(project)?
        .into_iter()
        .filter(|lib| lib.shipped)
        .collect();
    rock_engine_host::native_libraries::registry_json(&libs)
}

pub fn run_package(project: &str, output: &Path) -> Result<()> {
    // A package ships with the project's LuaJIT, which need not read line
    // directives; a release build never carries them.
    if skull::line_directives_mode() {
        anyhow::bail!(
            "package: Lua line directives are a development option; drop --lua-line-directives and ROCK_SKULL_LINE_DIRECTIVES"
        );
    }
    let context = package_context(project)?;
    let project_abs = context.project;
    let game_key = context.game_key;
    let config = context.config;
    let overlays = rock_engine_host::release::asset_overlay_roots(&project_abs, config.as_ref())?;
    let prepared =
        crate::asset_selection::prepare(&project_abs, &game_key, config.as_ref(), &overlays)?;
    if let Some(visual) = &prepared.visual {
        visual.verify(&prepared.assets)?;
    }
    let selection = prepared.assets;
    let visual = prepared.visual;
    let sound = prepared
        .sound
        .map(|sound| compile_sound_project(&project_abs, &game_key, sound))
        .transpose()?;
    for missing in &selection.missing {
        log::warn!(target: "PACKAGE", "release asset {} does not exist, skipping", missing.display());
    }

    let output_dir = output.to_path_buf();
    if output_dir.exists() && !output_dir.is_dir() {
        anyhow::bail!(
            "package: output {} is not a directory",
            output_dir.display()
        );
    }
    std::fs::create_dir_all(&output_dir)?;
    let output_dir = output_dir.canonicalize()?;
    let native_path = output_dir.join("native-libraries.json");
    remove_if_present(&native_path)?;
    let lua_path = output_dir.join("main.lua");
    let bytecode_path = output_dir.join("main.bc");
    let manifest_path = output_dir.join(PACKAGE_MANIFEST_NAME);
    remove_if_present(&lua_path)?;
    remove_if_present(&bytecode_path)?;
    remove_if_present(&manifest_path)?;
    let native_libs = shipped_native_libraries(project)?;
    rock_host::project::write_atomic(&native_path, &serde_json::to_vec_pretty(&native_libs)?)?;
    let assets_dir = output_dir.join("assets");
    let mut staged = rock_engine_host::release::stage_assets(&selection, &assets_dir)?;
    if let Some(sound) = sound.as_ref() {
        write_sound_bank(&assets_dir, &mut staged, sound)?;
    }

    // Resolve once for the package and let the compiler return its recorded
    // values for the manifest. In particular, a secret command is never run a
    // second time merely to create the manifest.
    let inputs = match config.as_ref() {
        Some(config) => {
            crate::comptime::inputs_for_project_with_config(&project_abs, config, None)?
        }
        None => crate::comptime::inputs_for_project_dir(&project_abs)?,
    };
    let inputs = crate::skull_backend::skull_compile_project_with_recorded_inputs(
        &project_abs.to_string_lossy(),
        Some(lua_path.to_string_lossy().into_owned()),
        inputs,
    )?;

    let luajit = rock_engine_host::runtime::luajit_path();
    if !luajit.is_file() {
        anyhow::bail!(
            "package: the project LuaJIT is missing at {}; package uses that exact binary",
            luajit.display()
        );
    }
    let bake = Command::new(&luajit)
        .current_dir(&project_abs)
        .env("LUA_PATH", rock_engine_host::runtime::lua_path_value())
        .args(["-b"])
        .arg(&lua_path)
        .arg(&bytecode_path)
        .output()
        .map_err(|e| anyhow::anyhow!("package: failed to run {}: {e}", luajit.display()))?;
    if !bake.status.success() {
        anyhow::bail!("package: LuaJIT bytecode baking failed ({})", bake.status);
    }
    if !bytecode_path.is_file() {
        anyhow::bail!(
            "package: LuaJIT reported success but did not create {}",
            bytecode_path.display()
        );
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

struct CompiledSoundPackage {
    project: String,
    bank_key: String,
    bank_bytes: Vec<u8>,
    dependencies: Vec<String>,
}

fn sound_metadata_cached(project_dir: &Path, source_path: &Path, bytes: &[u8]) -> Result<Value> {
    let digest = format!("{:x}", Sha256::digest(bytes));
    let cache_dir = project_dir.join("assets/.runtime-data/sounds");
    let cache_path = cache_dir.join(format!("{digest}.json"));
    if let Ok(cached) = std::fs::read(&cache_path) {
        if let Ok(value) = serde_json::from_slice::<Value>(&cached) {
            if sound_metadata_valid(&value) && value["digest"].as_str() == Some(digest.as_str()) {
                return Ok(value);
            }
        }
        log::warn!(target: "SOUND", "invalid sound metadata cache {}; decoding {}",
            cache_path.display(), source_path.display());
        match std::fs::remove_file(&cache_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    let mut metadata = decode_sound_metadata(bytes).map_err(|error| {
        anyhow::anyhow!("package: sound source {}: {error}", source_path.display())
    })?;
    metadata["digest"] = json!(digest);
    std::fs::create_dir_all(&cache_dir)?;
    let encoded = serde_json::to_vec(&metadata)?;
    let mut pending = tempfile::Builder::new()
        .prefix("sound-metadata-")
        .tempfile_in(&cache_dir)?;
    pending.write_all(&encoded)?;
    match pending.persist(&cache_path) {
        Ok(_) => {}
        Err(error) if cache_path.exists() => {
            let cached = std::fs::read(&cache_path)?;
            let value: Value = serde_json::from_slice(&cached)?;
            if !sound_metadata_valid(&value) || value["digest"].as_str() != Some(digest.as_str()) {
                return Err(anyhow::anyhow!(
                    "package: invalid sound metadata cache {}",
                    cache_path.display()
                ));
            }
            drop(error.file);
        }
        Err(error) => return Err(error.error.into()),
    }
    Ok(metadata)
}

fn sound_metadata_valid(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if object.len() != 4
        || object
            .keys()
            .any(|key| !matches!(key.as_str(), "frames" | "rate" | "channels" | "digest"))
    {
        return false;
    }
    let frames = value["frames"].as_u64().unwrap_or(0);
    let rate = value["rate"].as_u64().unwrap_or(0);
    let channels = value["channels"].as_u64().unwrap_or(0);
    let digest = value["digest"].as_str().unwrap_or("");
    frames > 0
        && rate > 0
        && channels > 0
        && digest.len() == 64
        && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn decode_sound_metadata(bytes: &[u8]) -> Result<Value> {
    let decoder = rodio::Decoder::new(std::io::Cursor::new(bytes.to_vec()))
        .map_err(|error| anyhow::anyhow!("decode failed: {error}"))?;
    let rate = decoder.sample_rate().get();
    let channels = decoder.channels().get();
    let sample_count = decoder.count();
    if sample_count == 0 || channels == 0 || rate == 0 {
        anyhow::bail!("decoded source has no PCM frames");
    }
    let frames = sample_count / channels as usize;
    if frames == 0 {
        anyhow::bail!("decoded source has no complete frame");
    }
    Ok(json!({ "frames": frames, "rate": rate, "channels": channels }))
}

fn compile_sound_project(
    project_dir: &Path,
    game_key: &str,
    sound: crate::asset_selection::SelectedSound,
) -> Result<CompiledSoundPackage> {
    let bank = sound_core::compile_prepared_project(sound.catalog, &sound.resolver)
        .map_err(|error| anyhow::anyhow!("sound project {}: {error}", sound.declaration))?;
    for warning in bank.played_peak_warnings() {
        log::warn!(target: "SOUND", "sound project {}: {}", sound.declaration, warning);
    }
    let bank_bytes = sound_core::serialize_runtime_bank(&bank).map_err(|error| {
        anyhow::anyhow!("package: sound project {}: {error}", sound.declaration)
    })?;
    let mut dependencies = Vec::with_capacity(bank.dependencies.len());
    let mut recipes = serde_json::Map::new();
    for dependency in &bank.dependencies {
        let Some(source) = sound.resolver.resolve(dependency) else {
            continue;
        };
        let source_bytes = std::fs::read(&source).map_err(|error| {
            anyhow::anyhow!(
                "sound dependency {} could not be read: {error}",
                source.display()
            )
        })?;
        recipes.insert(
            dependency.clone(),
            sound_metadata_cached(project_dir, &source, &source_bytes)?,
        );
        dependencies.push(format!("{game_key}/{dependency}"));
    }
    dependencies.sort();
    let mut bank_value: Value = serde_json::from_slice(&bank_bytes).map_err(|error| {
        anyhow::anyhow!("package: compiled sound bank could not be decoded: {error}")
    })?;
    bank_value["recipes"] = Value::Object(recipes);
    let bank_bytes = serde_json::to_vec(&bank_value).map_err(|error| {
        anyhow::anyhow!("package: compiled sound bank metadata could not be encoded: {error}")
    })?;
    Ok(CompiledSoundPackage {
        project: sound.declaration.clone(),
        bank_key: format!("{game_key}/assets/sound_bank.json"),
        bank_bytes,
        dependencies,
    })
}

fn write_sound_bank(
    assets_dir: &Path,
    staged: &mut rock_engine_host::release::StagedAssets,
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
    staged: &rock_engine_host::release::StagedAssets,
    sound: Option<&CompiledSoundPackage>,
    visual: Option<&crate::asset_selection::SelectedVisual>,
) -> Result<Value> {
    let mut vars = Map::new();
    let declarations = config
        .and_then(|config| config.build.as_ref())
        .map(|build| &build.vars);
    let mut names: Vec<_> = inputs
        .vars
        .keys()
        .map(|name| name.as_str().to_string())
        .collect();
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
        .unwrap_or_else(|| {
            project
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
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
    } else if var.source_fingerprint.is_some() {
        "source_fingerprint"
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
    #[test]
    fn native_release_selects_only_requested_shipped_libraries() {
        let project = tempfile::tempdir().unwrap();
        let manifest = project.path().join("stick.toml");
        let source = "[package]\nname = \"native-test\"\n[native]\nmanifest = \"Cargo.toml\"\nlibrary = \"probe\"\n";
        std::fs::write(&manifest, source).unwrap();
        std::fs::write(project.path().join("Cargo.toml"), "[package]\nname = \"source-name\"\nversion = \"0.1.0\"\n[lib]\npath = \"lib.rs\"\ncrate-type = [\"cdylib\"]\n[workspace]\n").unwrap();
        std::fs::write(project.path().join("lib.rs"), "").unwrap();
        let selected = super::shipped_native_libraries(project.path().to_str().unwrap()).unwrap();
        let selected = selected.as_array().unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0]["need"], "native-test");
        assert_eq!(selected[0]["src_stem"], "source_name");
        assert_eq!(selected[0]["dst_stem"], "probe");
        std::fs::write(&manifest, format!("{source}shipped = false\n")).unwrap();
        assert!(
            super::shipped_native_libraries(project.path().to_str().unwrap())
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    use super::*;
    use std::collections::BTreeMap;

    fn pcm_wav(samples: &[i16], sample_rate: u32) -> Vec<u8> {
        let data_bytes = (std::mem::size_of_val(samples)) as u32;
        let mut wav = Vec::with_capacity(44 + data_bytes as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_bytes.to_le_bytes());
        for sample in samples {
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }

    fn wav_header_oracle(bytes: &[u8]) -> (usize, u32, u16) {
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        assert_eq!(&bytes[36..40], b"data");
        let channels = u16::from_le_bytes(bytes[22..24].try_into().unwrap());
        let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let block_bytes = u16::from_le_bytes(bytes[32..34].try_into().unwrap());
        let data_bytes = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        (data_bytes as usize / block_bytes as usize, rate, channels)
    }

    #[test]
    fn sound_metadata_is_cached_by_digest_without_peak_thumbnails() {
        let project = tempfile::tempdir().expect("metadata cache fixture");
        let source = project.path().join("cue.wav");
        let samples: Vec<i16> = (0..128)
            .map(|index| if index % 2 == 0 { 16_384 } else { 0 })
            .collect();
        let bytes = pcm_wav(&samples, 8_000);
        std::fs::write(&source, &bytes).expect("source fixture");
        let (frames, rate, channels) = wav_header_oracle(&bytes);

        let first = sound_metadata_cached(project.path(), &source, &bytes)
            .expect("the PCM source should be decoded");
        assert_eq!(
            first["frames"].as_u64(),
            Some(frames as u64),
            "frame count agrees with the encoded WAV header"
        );
        assert_eq!(
            first["rate"].as_u64(),
            Some(rate as u64),
            "sample rate agrees with the encoded WAV header"
        );
        assert_eq!(
            first["channels"].as_u64(),
            Some(channels as u64),
            "channel count agrees with the encoded WAV header"
        );
        let digest = format!("{:x}", Sha256::digest(&bytes));
        assert_eq!(
            first["digest"].as_str(),
            Some(digest.as_str()),
            "the runtime recipe carries its source identity"
        );
        assert!(
            first.get("peaks").is_none(),
            "compiled sound recipes carry no peak thumbnails"
        );

        let cache_dir = project.path().join("assets/.runtime-data/sounds");
        let cache_path = cache_dir.join(format!("{digest}.json"));
        assert!(
            cache_path.is_file(),
            "the cache entry is named by the source digest"
        );
        let cached_bytes = std::fs::read(&cache_path).unwrap();
        let second = sound_metadata_cached(project.path(), &source, &bytes)
            .expect("the same digest should reuse its recipe");
        assert_eq!(second, first);
        assert_eq!(
            std::fs::read(&cache_path).unwrap(),
            cached_bytes,
            "a cache hit leaves its metadata untouched"
        );

        let mut thumbnail_cache = first.clone();
        thumbnail_cache["peaks"] = json!(vec![0.5; 64]);
        std::fs::write(&cache_path, serde_json::to_vec(&thumbnail_cache).unwrap()).unwrap();
        let repaired_thumbnail = sound_metadata_cached(project.path(), &source, &bytes)
            .expect("an old thumbnail-bearing cache should be replaced");
        assert_eq!(
            repaired_thumbnail, first,
            "the cached recipe is rebuilt without compile-time peaks"
        );
        let persisted: Value =
            serde_json::from_slice(&std::fs::read(&cache_path).unwrap()).unwrap();
        assert!(
            persisted.get("peaks").is_none(),
            "the digest cache does not retain editor peak thumbnails"
        );

        std::fs::write(&cache_path, b"corrupt cache entry").unwrap();
        let repaired = sound_metadata_cached(project.path(), &source, &bytes)
            .expect("a corrupt digest entry should be decoded and repaired");
        assert_eq!(repaired, first);
        assert!(
            sound_metadata_valid(
                &serde_json::from_slice::<Value>(&std::fs::read(&cache_path).unwrap()).unwrap()
            ),
            "the repaired digest entry is valid metadata"
        );

        let changed = pcm_wav(&[0; 128], 8_000);
        let _ = sound_metadata_cached(project.path(), &source, &changed)
            .expect("changed bytes have their own cache entry");
        let changed_digest = format!("{:x}", Sha256::digest(&changed));
        assert!(
            cache_dir.join(format!("{changed_digest}.json")).is_file(),
            "changed source bytes receive a separate metadata entry"
        );
    }

    #[test]
    fn development_bank_publishes_only_content_changes() {
        let workspace =
            rock_host::project::find_workspace_root(&std::env::current_dir().unwrap()).unwrap();
        let tmp = workspace.join("tmp");
        std::fs::create_dir_all(&tmp).expect("fixture scratch directory");
        let fixture = tempfile::tempdir_in(tmp).unwrap();
        std::fs::write(
            fixture.path().join("stick.toml"),
            "[package]\nname = \"stable-sound-dev\"\n[sound]\nproject = \"sound_project.json\"\n",
        )
        .unwrap();
        let project = fixture.path().join("sound_project.json");
        let source = r#"{"version":2,"normalization_lufs":-18.0,"groups":{"fixture":{"gain_db":0,"bus":"sfx"}},"events":[{"id":"fixture.first","group":"fixture","steps":[{"layers":[{"sources":["assets/variants/take_*.wav"]}]}]}]}"#;
        std::fs::write(&project, source).unwrap();
        let variants = fixture.path().join("assets/variants");
        std::fs::create_dir_all(&variants).unwrap();
        let samples = (0..8000)
            .map(|i| ((i as f64 * 0.1).sin() * 4000.0) as i16)
            .collect::<Vec<_>>();
        let wav = pcm_wav(&samples, 8000);
        std::fs::write(variants.join("take_02.wav"), &wav).unwrap();
        std::fs::write(variants.join("preview.wav"), &wav).unwrap();
        let path = fixture.path().to_str().unwrap();
        let bank = prepare_sound_bank(path).unwrap().unwrap();
        let original = std::fs::read(&bank).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&bank)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
            .unwrap();
        let timestamp = std::fs::metadata(&bank).unwrap().modified().unwrap();
        assert_eq!(prepare_sound_bank(path).unwrap(), Some(bank.clone()));
        assert_eq!(std::fs::read(&bank).unwrap(), original);
        assert_eq!(
            std::fs::metadata(&bank).unwrap().modified().unwrap(),
            timestamp,
            "identical compiler output must not trigger a running game's asset watcher"
        );
        std::fs::write(variants.join("take_01.wav"), &wav).unwrap();
        prepare_sound_bank(path).unwrap();
        let selected = std::fs::read(&bank).unwrap();
        let value: Value = serde_json::from_slice(&selected).unwrap();
        assert_eq!(
            value["dependencies"],
            json!(["assets/variants/take_01.wav", "assets/variants/take_02.wav"])
        );
        let config =
            rock_host::stick::StickConfig::from_path_optional(&fixture.path().join("stick.toml"))
                .unwrap();
        let mut selection = rock_engine_host::release::AssetSelection::default();
        let sound = crate::asset_selection::prepare_sound(
            fixture.path(),
            "game",
            config.as_ref(),
            &[],
            &mut selection,
            true,
        )
        .unwrap()
        .unwrap();
        let packaged = compile_sound_project(fixture.path(), "game", sound).unwrap();
        assert_eq!(
            packaged.bank_bytes, selected,
            "development and packaging must select the same bank"
        );
        assert_eq!(
            selection
                .assets
                .iter()
                .map(|asset| asset.key.as_str())
                .collect::<Vec<_>>(),
            [
                "game/assets/variants/take_01.wav",
                "game/assets/variants/take_02.wav"
            ]
        );
        std::fs::write(variants.join("take_03.wav"), b"corrupt audio").unwrap();
        assert!(prepare_sound_bank(path).is_err());
        assert_eq!(
            std::fs::read(&bank).unwrap(),
            selected,
            "failed discovery must preserve the previous bank"
        );
        std::fs::remove_file(variants.join("take_03.wav")).unwrap();
        std::fs::remove_file(variants.join("take_01.wav")).unwrap();
        prepare_sound_bank(path).unwrap();
        assert_eq!(std::fs::read(&bank).unwrap(), original);
        std::fs::write(&project, source.replace("fixture.first", "fixture.second")).unwrap();
        prepare_sound_bank(path).unwrap();
        let changed = std::fs::read(&bank).unwrap();
        assert_ne!(changed, original);
        let value: Value = serde_json::from_slice(&changed).unwrap();
        assert_eq!(value["events"][0]["id"], "fixture.second");
        std::fs::remove_file(variants.join("take_02.wav")).unwrap();
        let error = prepare_sound_bank(path).unwrap_err().to_string();
        assert!(error.contains("matches no files"), "{error}");
        assert_eq!(std::fs::read(&bank).unwrap(), changed);
    }

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
        let tmp = workspace.join("tmp");
        std::fs::create_dir_all(&tmp).expect("fixture scratch directory");
        let fixture = tempfile::tempdir_in(tmp).expect("sound fixture");
        std::fs::write(
            fixture.path().join("stick.toml"),
            "[package]\nname = \"missing-sound-dev\"\n\n[sound]\nproject = \"sound_project.json\"\n",
        )
        .unwrap();
        std::fs::write(
            fixture.path().join("sound_project.json"),
            r#"{
  "version": 2,
  "normalization_lufs": -18.0,
  "groups": {"fixture": {"gain_db": 0, "bus": "sfx"}},
  "events": [{"id": "fixture.event", "group": "fixture", "steps": [{"layers": [{"sources": ["assets/sfx/missing.ogg"]}]}]}]
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
    fn release_visual_bank_prefers_the_worktree_bank_over_a_stale_overlay_bank() {
        let workspace = rock_host::project::find_workspace_root(
            &std::env::current_dir().expect("current directory"),
        )
        .expect("workspace root");
        let tmp = workspace.join("tmp");
        std::fs::create_dir_all(&tmp).expect("fixture scratch directory");
        let fixture = tempfile::tempdir_in(tmp).expect("visual fixture");
        let overlay = fixture.path().join("overlay");
        let dir = "a".repeat(64);
        let project_object = fixture.path().join(format!("assets/.eat/{dir}"));
        std::fs::create_dir_all(&project_object).unwrap();
        std::fs::create_dir_all(overlay.join("assets")).unwrap();
        let bank_path = rock_visual_bank::BANK_PATH;
        std::fs::write(
            fixture.path().join(bank_path),
            format!(r#"{{"kind":"icon","id":"ui/ring","dir":"{dir}","frame_size":[8,8],"pivot":[0.5,0.5],"skin":false}}"#) + "\n",
        )
        .unwrap();
        std::fs::write(project_object.join("color.png"), b"project-object").unwrap();
        std::fs::write(overlay.join(bank_path), "not a bank line\n").unwrap();

        let selection =
            crate::asset_selection::prepare(fixture.path(), "fixture", None, &[overlay])
                .expect("the project-owned bank should be selected")
                .assets;
        let bank = selection
            .assets
            .iter()
            .find(|asset| asset.key == format!("fixture/{bank_path}"))
            .expect("selected bank");
        assert_eq!(bank.source, fixture.path().join(bank_path));
    }

    #[test]
    fn secret_manifest_value_is_replaced_by_a_fingerprint() {
        let secret = "known-secret";
        assert_eq!(
            fingerprint(secret),
            format!("sha256:{:x}", Sha256::digest(secret.as_bytes()))
        );
        let mut vars = BTreeMap::new();
        vars.insert("TOKEN", fingerprint(secret));
        let encoded = serde_json::to_string(&vars).unwrap();
        assert!(!encoded.contains(secret));
    }

    #[test]
    fn generated_sound_bank_does_not_duplicate_an_already_selected_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = "sample/assets/sound_bank.json".to_string();
        let mut staged = rock_engine_host::release::StagedAssets {
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

    #[test]
    fn package_manifest_records_resolved_source_fingerprint_value() {
        let project = tempfile::tempdir().unwrap();
        let lua_path = project.path().join("main.lua");
        let bytecode_path = project.path().join("main.bc");
        std::fs::write(&lua_path, b"return 1").unwrap();
        std::fs::write(&bytecode_path, b"bytecode").unwrap();
        let config = rock_host::stick::StickConfig::new_from_str(
            "[package]\nname = \"fingerprint-manifest\"\n[build.vars]\nREVISION = { source_fingerprint = { roots = [\"src\"] } }\n",
        )
        .unwrap();
        let mut inputs = ComptimeInputs::default();
        inputs.vars.insert(
            ustr::Ustr::from("REVISION"),
            bone_core::workspace::ComptimeVarValue {
                value: "source-v1:abc123".to_string(),
                secret: false,
            },
        );
        let staged = rock_engine_host::release::StagedAssets {
            file_count: 0,
            keys: Vec::new(),
        };

        let manifest = package_manifest(
            project.path(),
            "fixture",
            Some(&config),
            &inputs,
            &lua_path,
            &bytecode_path,
            &staged,
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            manifest["build_vars"]["REVISION"]["source"],
            "source_fingerprint"
        );
        assert_eq!(
            manifest["build_vars"]["REVISION"]["value"],
            "source-v1:abc123"
        );
    }
}
