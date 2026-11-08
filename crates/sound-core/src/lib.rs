mod sources;
pub use sources::{expand_sources, source_directory, source_matcher};
mod loudness;
mod played;
pub use loudness::{
    MeasurementCache, PLAYED_PEAK_WARNING_DBTP, RenderLayer, SourceMeasurement,
    SourceNormalization, measure_mix,
};
pub use played::{
    PlayedLayer, PlayedLevel, PlayedPeakWarning, PlayedSource, PlayedStep, measure_step,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::{self, Display, Formatter},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

pub const AUTHORING_SCHEMA_VERSION: u32 = 2;
pub const RUNTIME_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoundProject {
    pub version: u32,
    pub normalization_lufs: f64,
    pub groups: BTreeMap<String, MixGroup>,
    pub events: Vec<SoundEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixGroup {
    pub gain_db: f64,
    pub bus: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoundEvent {
    pub id: String,
    pub group: String,
    #[serde(default)]
    pub offset_db: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub steps: Vec<SoundStep>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoundStep {
    pub layers: Vec<SoundLayer>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoundLayer {
    pub sources: Vec<String>,
    /// dB relative to the event and group; zero is unity.
    #[serde(default)]
    pub volume: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default = "default_speed")]
    pub speed: f32,
    #[serde(default)]
    pub offset: f32,
    /// Loop by incremental decode of held encoded bytes instead of resident PCM.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream: bool,
    /// Retained PCM loops need integrated loudness too, even without streaming.
    #[serde(default, skip_serializing_if = "is_false")]
    pub looped: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuntimeBank {
    #[serde(rename = "version")]
    pub schema_version: u32,
    pub events: Vec<RuntimeEvent>,
    pub dependencies: Vec<String>,
    pub normalizations: BTreeMap<String, SourceNormalization>,
    #[serde(skip)]
    pub played: BTreeMap<String, Vec<PlayedStep>>,
}

impl RuntimeBank {
    pub fn played_peak_warnings(&self) -> Vec<PlayedPeakWarning> {
        played::warnings(&self.played)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuntimeEvent {
    pub id: String,
    pub group: String,
    pub bus: String,
    pub steps: Vec<RuntimeStep>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuntimeStep {
    pub layers: Vec<RuntimeLayer>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuntimeLayer {
    pub sources: Vec<String>,
    pub volume: f32,
    pub speed: f32,
    pub offset: f32,
    #[serde(skip_serializing_if = "is_false")]
    pub stream: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: String,
    pub message: String,
}

impl Display for Diagnostic {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path, self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoundCoreError {
    Parse(String),
    Diagnostics(Vec<Diagnostic>),
    Serialize(String),
}

impl Display for SoundCoreError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) => write!(formatter, "sound project parse error: {message}"),
            Self::Diagnostics(diagnostics) => {
                for (index, diagnostic) in diagnostics.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str("; ")?;
                    }
                    diagnostic.fmt(formatter)?;
                }
                Ok(())
            }
            Self::Serialize(message) => {
                write!(formatter, "sound bank serialization error: {message}")
            }
        }
    }
}

impl Error for SoundCoreError {}

pub trait AssetResolver {
    fn resolve(&self, key: &str) -> Option<PathBuf>;
    fn discover(&self, pattern: &str) -> Result<Vec<String>, String>;
}

#[derive(Debug, Clone, Default)]
pub struct AssetMapResolver {
    assets: BTreeMap<String, PathBuf>,
}

impl AssetMapResolver {
    pub fn new(assets: BTreeMap<String, PathBuf>) -> Self {
        Self { assets }
    }
}

impl AssetResolver for AssetMapResolver {
    fn resolve(&self, key: &str) -> Option<PathBuf> {
        self.assets.get(key).cloned()
    }

    fn discover(&self, pattern: &str) -> Result<Vec<String>, String> {
        let matcher = sources::source_matcher(pattern)?;
        Ok(self
            .assets
            .keys()
            .filter(|key| matcher.is_match(key))
            .cloned()
            .collect())
    }
}

#[derive(Debug, Clone)]
pub struct FsAssetResolver {
    roots: Vec<PathBuf>,
}

impl FsAssetResolver {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }
}

impl AssetResolver for FsAssetResolver {
    fn resolve(&self, key: &str) -> Option<PathBuf> {
        self.roots
            .iter()
            .map(|root| root.join(key))
            .find(|candidate| candidate.is_file())
    }
    fn discover(&self, pattern: &str) -> Result<Vec<String>, String> {
        sources::discover_files(&self.roots, pattern)
    }
}

pub fn parse_project(text: &str) -> Result<SoundProject, SoundCoreError> {
    serde_json::from_str(text).map_err(|error| SoundCoreError::Parse(error.to_string()))
}

pub fn compile_project<R: AssetResolver + ?Sized>(
    project: &SoundProject,
    resolver: &R,
) -> Result<RuntimeBank, SoundCoreError> {
    compile_project_with_asset_requirement(project, resolver, true)
}

pub fn compile_project_allowing_missing_assets<R: AssetResolver + ?Sized>(
    project: &SoundProject,
    resolver: &R,
) -> Result<RuntimeBank, SoundCoreError> {
    compile_project_with_asset_requirement(project, resolver, false)
}

fn compile_project_with_asset_requirement<R: AssetResolver + ?Sized>(
    project: &SoundProject,
    resolver: &R,
    require_assets: bool,
) -> Result<RuntimeBank, SoundCoreError> {
    compile_prepared_project(
        prepare_project(project, resolver, require_assets)?,
        resolver,
    )
}

pub fn compile_prepared_project<R: AssetResolver + ?Sized>(
    prepared: PreparedSoundProject,
    resolver: &R,
) -> Result<RuntimeBank, SoundCoreError> {
    let PreparedSoundProject {
        project,
        dependencies,
        looped_sources,
    } = prepared;
    let project = &project;
    let cache = MeasurementCache::standard().map_err(measurement_error)?;
    let mut normalizations = BTreeMap::new();
    for key in &dependencies {
        let Some(path) = resolver.resolve(key) else {
            continue;
        };
        let measurement = cache
            .measure(&path, 1.0)
            .map_err(|error| measurement_error(format!("{key}: {error}")))?;
        let normalized = measurement
            .normalize(project.normalization_lufs, looped_sources.contains(key))
            .map_err(|error| measurement_error(format!("{key}: {error}")))?;
        normalizations.insert(key.clone(), normalized);
    }
    let mut events = project
        .events
        .iter()
        .map(|event| {
            let group = &project.groups[&event.group];
            RuntimeEvent {
                id: event.id.clone(),
                group: event.group.clone(),
                bus: group.bus.clone(),
                steps: event
                    .steps
                    .iter()
                    .map(|step| RuntimeStep {
                        layers: step
                            .layers
                            .iter()
                            .map(|layer| RuntimeLayer {
                                sources: layer.sources.clone(),
                                volume: db_gain(layer_gain_db(project, event, layer)) as f32,
                                speed: layer.speed,
                                offset: layer.offset,
                                stream: layer.stream,
                            })
                            .collect(),
                    })
                    .collect(),
            }
        })
        .collect::<Vec<_>>();
    events.sort_by(|left, right| left.id.cmp(&right.id));
    let played = played::analyze_project(project, &normalizations, resolver, &cache)
        .map_err(measurement_error)?;
    Ok(RuntimeBank {
        schema_version: RUNTIME_SCHEMA_VERSION,
        events,
        dependencies: dependencies.into_iter().collect(),
        normalizations,
        played,
    })
}

pub struct PreparedSoundProject {
    pub project: SoundProject,
    pub dependencies: BTreeSet<String>,
    looped_sources: BTreeSet<String>,
}

/// Validate catalog references without decoding audio or measuring loudness.
pub fn prepare_project<R: AssetResolver + ?Sized>(
    project: &SoundProject,
    resolver: &R,
    require_assets: bool,
) -> Result<PreparedSoundProject, SoundCoreError> {
    let mut project = project.clone();
    for event in &mut project.events {
        for (step_index, step) in event.steps.iter_mut().enumerate() {
            for (layer_index, layer) in step.layers.iter_mut().enumerate() {
                layer.sources = expand_sources(&layer.sources, resolver).map_err(|error| {
                    SoundCoreError::Diagnostics(vec![diagnostic(
                        format!(
                            "event {} steps[{step_index}].layers[{layer_index}].sources",
                            event.id
                        ),
                        error,
                    )])
                })?;
            }
        }
    }
    let diagnostics = validate_project(&project, resolver, require_assets);
    if !diagnostics.is_empty() {
        return Err(SoundCoreError::Diagnostics(diagnostics));
    }

    let mut dependencies = BTreeSet::new();
    let mut looped_sources = BTreeSet::new();
    for event in &project.events {
        for step in &event.steps {
            for layer in &step.layers {
                for source in &layer.sources {
                    dependencies.insert(source.clone());
                    if layer.looped || layer.stream {
                        looped_sources.insert(source.clone());
                    }
                }
            }
        }
    }
    Ok(PreparedSoundProject {
        project,
        dependencies,
        looped_sources,
    })
}

fn measurement_error(message: impl Into<String>) -> SoundCoreError {
    SoundCoreError::Diagnostics(vec![diagnostic("normalization", message)])
}

pub fn db_gain(db: f64) -> f64 {
    10.0_f64.powf(db / 20.0)
}

pub(crate) fn layer_gain_db(project: &SoundProject, event: &SoundEvent, layer: &SoundLayer) -> f64 {
    project.groups[&event.group].gain_db + event.offset_db + f64::from(layer.volume)
}

pub fn serialize_runtime_bank(bank: &RuntimeBank) -> Result<Vec<u8>, SoundCoreError> {
    serde_json::to_vec(bank).map_err(|error| SoundCoreError::Serialize(error.to_string()))
}

pub fn validate_project<R: AssetResolver + ?Sized>(
    project: &SoundProject,
    resolver: &R,
    require_assets: bool,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    if project.version != AUTHORING_SCHEMA_VERSION {
        diagnostics.push(diagnostic(
            "version",
            format!(
                "unsupported schema version {}; expected {}",
                project.version, AUTHORING_SCHEMA_VERSION
            ),
        ));
    }
    if project.events.is_empty() {
        diagnostics.push(diagnostic(
            "events",
            "project must contain at least one event",
        ));
    }

    if !project.normalization_lufs.is_finite() {
        diagnostics.push(diagnostic("normalization_lufs", "target must be finite"));
    }
    for (name, group) in &project.groups {
        if name.trim().is_empty() || !group.gain_db.is_finite() {
            diagnostics.push(diagnostic(
                format!("groups.{name}"),
                "group name must be nonempty and gain_db finite",
            ));
        }
        if !["sfx", "ui", "foley", "ambience", "music"].contains(&group.bus.as_str()) {
            diagnostics.push(diagnostic(
                format!("groups.{name}.bus"),
                "unknown sound bus",
            ));
        }
    }
    let mut seen_ids = BTreeMap::new();
    // One source loads as one native kind; the first layer naming it decides.
    let mut source_kinds: BTreeMap<&str, (bool, String)> = BTreeMap::new();
    for (event_index, event) in project.events.iter().enumerate() {
        let event_path = format!("events[{event_index}]");
        if event.id.trim().is_empty() {
            diagnostics.push(diagnostic(
                format!("{event_path}.id"),
                "event ID must not be empty",
            ));
        } else if let Some(previous_index) = seen_ids.insert(event.id.clone(), event_index) {
            diagnostics.push(diagnostic(
                format!("{event_path}.id"),
                format!("duplicate event ID; first declared at events[{previous_index}]"),
            ));
        }
        if !project.groups.contains_key(&event.group) {
            diagnostics.push(diagnostic(
                format!("{event_path}.group"),
                format!("unknown mix group: {}", event.group),
            ));
        }
        if !event.offset_db.is_finite() || !db_gain(event.offset_db).is_finite() {
            diagnostics.push(diagnostic(
                format!("{event_path}.offset_db"),
                "offset must be finite",
            ));
        }
        if event.offset_db != 0.0
            && event
                .reason
                .as_deref()
                .is_none_or(|reason| reason.trim().is_empty())
        {
            diagnostics.push(diagnostic(
                format!("{event_path}.reason"),
                "a nonzero event offset needs a reason",
            ));
        }
        if event.steps.is_empty() {
            diagnostics.push(diagnostic(
                format!("{event_path}.steps"),
                "event must contain at least one step",
            ));
        }

        for (step_index, step) in event.steps.iter().enumerate() {
            let step_path = format!("{event_path}.steps[{step_index}]");
            if step.layers.is_empty() {
                diagnostics.push(diagnostic(
                    format!("{step_path}.layers"),
                    "step must contain at least one layer",
                ));
            }
            for (layer_index, layer) in step.layers.iter().enumerate() {
                let layer_path = format!("{step_path}.layers[{layer_index}]");
                if layer.sources.is_empty() {
                    diagnostics.push(diagnostic(
                        format!("{layer_path}.sources"),
                        "layer must contain at least one source variant",
                    ));
                }
                if !layer.volume.is_finite() || !db_gain(f64::from(layer.volume)).is_finite() {
                    diagnostics.push(diagnostic(
                        format!("{layer_path}.volume"),
                        "volume must be finite dB",
                    ));
                }
                if let Some(group) = project.groups.get(&event.group) {
                    let gain =
                        db_gain(group.gain_db + event.offset_db + f64::from(layer.volume)) as f32;
                    if !gain.is_finite() {
                        diagnostics.push(diagnostic(
                            format!("{layer_path}.volume"),
                            "combined group, event and layer gain overflows runtime gain",
                        ));
                    }
                }
                if layer.volume != 0.0
                    && layer
                        .reason
                        .as_deref()
                        .is_none_or(|reason| reason.trim().is_empty())
                {
                    diagnostics.push(diagnostic(
                        format!("{layer_path}.reason"),
                        "a nonzero layer balance needs a reason",
                    ));
                }
                if !layer.speed.is_finite() || layer.speed <= 0.0 {
                    diagnostics.push(diagnostic(
                        format!("{layer_path}.speed"),
                        "speed must be finite and strictly positive",
                    ));
                }
                if !layer.offset.is_finite() || layer.offset < 0.0 {
                    diagnostics.push(diagnostic(
                        format!("{layer_path}.offset"),
                        "offset must be finite and non-negative",
                    ));
                }
                for (source_index, source) in layer.sources.iter().enumerate() {
                    let source_path = format!("{layer_path}.sources[{source_index}]");
                    match source_kinds.get(source.as_str()) {
                        Some((stream, first)) if *stream != layer.stream => {
                            diagnostics.push(diagnostic(
                                source_path.clone(),
                                format!(
                                    "{source} is a {} source here and a {} source at {first}",
                                    if layer.stream { "stream" } else { "decoded" },
                                    if *stream { "stream" } else { "decoded" },
                                ),
                            ));
                        }
                        Some(_) => {}
                        None => {
                            source_kinds.insert(source, (layer.stream, source_path.clone()));
                        }
                    }
                    if let Err(message) = validate_asset_key(source) {
                        diagnostics.push(diagnostic(source_path.clone(), message));
                    } else if require_assets && resolver.resolve(source).is_none() {
                        diagnostics.push(diagnostic(
                            source_path,
                            format!("referenced asset does not exist: {source}"),
                        ));
                    }
                }
            }
        }
    }
    diagnostics
}

fn validate_asset_key(key: &str) -> Result<(), String> {
    if key.is_empty()
        || key.starts_with('/')
        || key.starts_with('\\')
        || key.starts_with('~')
        || key.as_bytes().get(1) == Some(&b':')
        || key.contains('\\')
    {
        return Err(format!("asset key is not a canonical relative key: {key}"));
    }
    if !key.starts_with("assets/") {
        return Err(format!("asset key must begin with assets/: {key}"));
    }
    for component in key.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(format!(
                "asset key contains an invalid path component: {key}"
            ));
        }
    }
    Ok(())
}

fn diagnostic(path: impl Into<String>, message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        path: path.into(),
        message: message.into(),
    }
}

fn default_speed() -> f32 {
    1.0
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        f64::consts::TAU,
        path::{Path, PathBuf},
        sync::OnceLock,
    };

    use super::{
        AssetMapResolver, MixGroup, SoundCoreError, SoundEvent, SoundLayer, SoundProject,
        SoundStep, compile_project, compile_project_allowing_missing_assets, parse_project,
        serialize_runtime_bank,
    };

    const SHARED: &str = "assets/sfx/shared.ogg";
    const ALT: &str = "assets/sfx/alt.ogg";
    const SOLO: &str = "assets/sfx/solo.ogg";

    fn fixture_wav() -> PathBuf {
        static FIXTURE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
        FIXTURE
            .get_or_init(|| {
                let directory = tempfile::tempdir().expect("shared sound fixture directory");
                let path = directory.path().join("sine.wav");
                write_fixture_wav(&path);
                (directory, path)
            })
            .1
            .clone()
    }

    fn write_fixture_wav(path: &Path) {
        const SAMPLE_RATE: u32 = 48_000;
        const FRAME_COUNT: u32 = SAMPLE_RATE;
        let data_len = FRAME_COUNT * 2;
        let mut wav = Vec::with_capacity((44 + data_len) as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for frame in 0..FRAME_COUNT {
            let phase = f64::from(frame) * 440.0 * TAU / f64::from(SAMPLE_RATE);
            let sample = (phase.sin() * f64::from(i16::MAX) * 0.25).round() as i16;
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        std::fs::write(path, wav).expect("write sound fixture WAV");
    }

    fn resolver(keys: &[&str]) -> AssetMapResolver {
        let path = fixture_wav();
        AssetMapResolver::new(
            keys.iter()
                .map(|key| ((*key).to_owned(), path.clone()))
                .collect(),
        )
    }

    fn fixture_groups() -> BTreeMap<String, MixGroup> {
        BTreeMap::from([(
            "fixture".to_owned(),
            MixGroup {
                gain_db: 0.0,
                bus: "sfx".to_owned(),
            },
        )])
    }

    fn fixture_project(events: Vec<SoundEvent>) -> SoundProject {
        SoundProject {
            version: 2,
            normalization_lufs: -18.0,
            groups: fixture_groups(),
            events,
        }
    }

    fn valid_project() -> &'static str {
        r#"
        {
          "version": 2,
          "normalization_lufs": -18.0,
          "groups": {"fixture": {"gain_db": 0, "bus": "sfx"}},
          "events": [
            {
              "id": "z.event",
              "group": "fixture",
              "steps": [{
                "layers": [
                  {
                    "sources": ["assets/sfx/shared.ogg", "assets/sfx/alt.ogg"],
                    "volume": 0.5,
                    "reason": "fixture balance",
                    "speed": 1.25,
                    "offset": 0.0
                  },
                  {
                    "sources": ["assets/sfx/shared.ogg"],
                    "volume": 0.0,
                    "speed": 0.5,
                    "offset": 0.25
                  }
                ]
              }]
            },
            {
              "id": "a.event",
              "group": "fixture",
              "steps": [{
                "layers": [{"sources": ["assets/sfx/solo.ogg"]}]
              }]
            },
            {
              "id": "unconsumed.event",
              "group": "fixture",
              "steps": [{
                "layers": [{"sources": ["assets/sfx/shared.ogg"]}]
              }]
            }
          ]
        }
        "#
    }

    #[test]
    fn valid_project_compiles_layers_variants_and_sorted_manifest() {
        let project = parse_project(valid_project()).expect("valid project should parse");
        let bank = compile_project(&project, &resolver(&[SHARED, ALT, SOLO]))
            .expect("valid project should compile");

        assert_eq!(bank.events.len(), 3);
        assert_eq!(bank.events[0].id, "a.event");
        assert_eq!(bank.events[2].id, "z.event");
        assert_eq!(bank.events[2].steps[0].layers.len(), 2);
        assert_eq!(bank.events[2].steps[0].layers[0].sources.len(), 2);
        assert_eq!(bank.dependencies, vec![ALT, SHARED, SOLO]);
        assert_eq!(bank.schema_version, 2);
    }

    #[test]
    fn discovered_variants_compile_to_the_same_bank_as_explicit_sources() {
        let mut explicit = parse_project(valid_project()).unwrap();
        explicit.events[0].steps[0].layers[0].sources =
            vec![ALT.into(), SHARED.into(), SOLO.into()];
        let mut discovered = explicit.clone();
        discovered.events[0].steps[0].layers[0].sources = vec!["assets/sfx/*.ogg".into()];
        let resolver = resolver(&[SHARED, SOLO, ALT]);
        let expected = compile_project(&explicit, &resolver).unwrap();
        let actual = compile_project(&discovered, &resolver).unwrap();
        assert_eq!(
            serialize_runtime_bank(&actual).unwrap(),
            serialize_runtime_bank(&expected).unwrap()
        );
    }

    #[test]
    fn compiled_bank_serialization_is_deterministic() {
        let project = parse_project(valid_project()).expect("valid project should parse");
        let resolver = resolver(&[SHARED, ALT, SOLO]);
        let first = compile_project(&project, &resolver).expect("first compile should pass");
        let second = compile_project(&project, &resolver).expect("second compile should pass");

        assert_eq!(
            serialize_runtime_bank(&first).expect("first serialization should pass"),
            serialize_runtime_bank(&second).expect("second serialization should pass")
        );
    }

    #[test]
    fn missing_dependency_is_a_diagnostic() {
        let project = parse_project(valid_project()).expect("valid project should parse");
        let error = compile_project(&project, &resolver(&[SHARED, SOLO]))
            .expect_err("missing dependency should reject the project");

        assert!(matches!(error, SoundCoreError::Diagnostics(_)));
        assert!(error.to_string().contains(ALT));
    }

    #[test]
    fn development_compile_retains_missing_dependencies_for_runtime_silence() {
        let project = parse_project(valid_project()).expect("valid project should parse");
        let bank = compile_project_allowing_missing_assets(&project, &resolver(&[SHARED, SOLO]))
            .expect("a missing sound must not reject a development bank");

        assert_eq!(bank.events[2].steps[0].layers[0].sources, vec![SHARED, ALT]);
        assert_eq!(bank.dependencies, vec![ALT, SHARED, SOLO]);
    }

    #[test]
    fn unknown_event_is_allowed() {
        let project = parse_project(
            r#"{
              "version": 2,
              "normalization_lufs": -18.0,
              "groups": {"fixture": {"gain_db": 0, "bus": "sfx"}},
              "events": [{
                "id": "future.game.event",
                "group": "fixture",
                "steps": [{"layers": [{"sources": ["assets/sfx/solo.ogg"]}]}]
              }]
            }"#,
        )
        .expect("unknown event project should parse");

        assert!(compile_project(&project, &resolver(&[SOLO])).is_ok());
    }

    #[test]
    fn invalid_project_json_is_rejected() {
        let error = parse_project("{ not json }").expect_err("malformed JSON should reject");

        assert!(matches!(error, SoundCoreError::Parse(_)));
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let error = parse_project(
            r#"{
              "version": 3,
              "normalization_lufs": -18.0,
              "groups": {"fixture": {"gain_db": 0, "bus": "sfx"}},
              "events": [{
                "id": "event",
                "group": "fixture",
                "steps": [{"layers": [{"sources": ["assets/sfx/solo.ogg"]}]}]
              }]
            }"#,
        )
        .and_then(|project| compile_project(&project, &resolver(&[SOLO])))
        .expect_err("unsupported schema should reject");

        assert!(error.to_string().contains("version"));
    }

    #[test]
    fn empty_sources_are_rejected() {
        let error = parse_project(
            r#"{
              "version": 2,
              "normalization_lufs": -18.0,
              "groups": {"fixture": {"gain_db": 0, "bus": "sfx"}},
              "events": [{
                "id": "event",
                "group": "fixture",
                "steps": [{"layers": [{"sources": []}]}]
              }]
            }"#,
        )
        .and_then(|project| compile_project(&project, &resolver(&[])))
        .expect_err("empty source list should reject");

        assert!(error.to_string().contains("sources"));
    }

    #[test]
    fn empty_steps_and_layers_are_rejected() {
        let no_steps = SoundProject {
            version: 2,
            normalization_lufs: -18.0,
            groups: fixture_groups(),
            events: vec![SoundEvent {
                id: "event".to_owned(),
                group: "fixture".to_owned(),
                offset_db: 0.0,
                reason: None,
                steps: Vec::new(),
            }],
        };
        let no_layers = SoundProject {
            version: 2,
            normalization_lufs: -18.0,
            groups: fixture_groups(),
            events: vec![SoundEvent {
                id: "event".to_owned(),
                group: "fixture".to_owned(),
                offset_db: 0.0,
                reason: None,
                steps: vec![SoundStep { layers: Vec::new() }],
            }],
        };

        let no_steps_error = compile_project(&no_steps, &resolver(&[]))
            .expect_err("an event without steps should reject");
        let no_layers_error = compile_project(&no_layers, &resolver(&[]))
            .expect_err("a step without layers should reject");

        assert!(no_steps_error.to_string().contains("steps"));
        assert!(no_layers_error.to_string().contains("layers"));
    }

    #[test]
    fn duplicate_and_empty_event_ids_are_rejected() {
        let duplicate = parse_project(
            r#"{
              "version": 2,
              "normalization_lufs": -18.0,
              "groups": {"fixture": {"gain_db": 0, "bus": "sfx"}},
              "events": [
                {"id": "event", "group": "fixture", "steps": [{"layers": [{"sources": ["assets/sfx/solo.ogg"]}]}]},
                {"id": "event", "group": "fixture", "steps": [{"layers": [{"sources": ["assets/sfx/solo.ogg"]}]}]}
              ]
            }"#,
        )
        .expect("duplicate project shape should parse");
        let empty = parse_project(
            r#"{
              "version": 2,
              "normalization_lufs": -18.0,
              "groups": {"fixture": {"gain_db": 0, "bus": "sfx"}},
              "events": [{"id": "  ", "group": "fixture", "steps": [{"layers": [{"sources": ["assets/sfx/solo.ogg"]}]}]}]
            }"#,
        )
        .expect("empty ID project shape should parse");

        let duplicate_error = compile_project(&duplicate, &resolver(&[SOLO]))
            .expect_err("duplicate event IDs should reject");
        let empty_error =
            compile_project(&empty, &resolver(&[SOLO])).expect_err("empty event IDs should reject");

        assert!(duplicate_error.to_string().contains("duplicate event ID"));
        assert!(
            empty_error
                .to_string()
                .contains("event ID must not be empty")
        );
    }

    #[test]
    fn numeric_constraints_reject_invalid_and_nonfinite_values() {
        let cases = [
            (
                "volume",
                SoundLayer {
                    sources: vec![SOLO.to_owned()],
                    volume: f32::INFINITY,
                    reason: Some("fixture balance".to_owned()),
                    speed: 1.0,
                    offset: 0.0,
                    stream: false,
                    looped: false,
                },
            ),
            (
                "speed",
                SoundLayer {
                    sources: vec![SOLO.to_owned()],
                    volume: 0.0,
                    reason: None,
                    speed: 0.0,
                    offset: 0.0,
                    stream: false,
                    looped: false,
                },
            ),
            (
                "offset",
                SoundLayer {
                    sources: vec![SOLO.to_owned()],
                    volume: 0.0,
                    reason: None,
                    speed: 1.0,
                    offset: -0.01,
                    stream: false,
                    looped: false,
                },
            ),
            (
                "volume",
                SoundLayer {
                    sources: vec![SOLO.to_owned()],
                    volume: f32::NAN,
                    reason: Some("fixture balance".to_owned()),
                    speed: 1.0,
                    offset: 0.0,
                    stream: false,
                    looped: false,
                },
            ),
            (
                "speed",
                SoundLayer {
                    sources: vec![SOLO.to_owned()],
                    volume: 0.0,
                    reason: None,
                    speed: f32::INFINITY,
                    offset: 0.0,
                    stream: false,
                    looped: false,
                },
            ),
            (
                "offset",
                SoundLayer {
                    sources: vec![SOLO.to_owned()],
                    volume: 0.0,
                    reason: None,
                    speed: 1.0,
                    offset: f32::NEG_INFINITY,
                    stream: false,
                    looped: false,
                },
            ),
        ];

        for (field, layer) in cases {
            let project = fixture_project(vec![SoundEvent {
                id: "event".to_owned(),
                group: "fixture".to_owned(),
                offset_db: 0.0,
                reason: None,
                steps: vec![SoundStep {
                    layers: vec![layer],
                }],
            }]);
            let error = compile_project(&project, &resolver(&[SOLO]))
                .expect_err("invalid numeric value should reject");
            assert!(error.to_string().contains(field), "diagnostic for {field}");
        }
    }

    #[test]
    fn stream_layers_compile_to_streamed_runtime_layers() {
        let project = parse_project(
            r#"{
              "version": 2,
              "normalization_lufs": -18.0,
              "groups": {"fixture": {"gain_db": 0, "bus": "sfx"}},
              "events": [
                {"id": "bed", "group": "fixture", "steps": [{"layers": [{"sources": ["assets/sfx/solo.ogg"], "stream": true}]}]},
                {"id": "shot", "group": "fixture", "steps": [{"layers": [{"sources": ["assets/sfx/alt.ogg"]}]}]}
              ]
            }"#,
        )
        .expect("a stream layer should parse");
        let bank = compile_project(&project, &resolver(&[SOLO, ALT]))
            .expect("a stream layer should compile");
        let runtime: serde_json::Value = serde_json::from_slice(
            &serialize_runtime_bank(&bank).expect("the stream bank should serialize"),
        )
        .expect("the runtime bank is JSON");
        let layer = |id: &str| {
            runtime["events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|event| event["id"] == id)
                .map(|event| event["steps"][0]["layers"][0].clone())
                .unwrap()
        };
        assert_eq!(
            layer("bed")["stream"],
            serde_json::Value::Bool(true),
            "the authored stream layer reaches the runtime bank"
        );
        assert!(
            layer("shot").get("stream").is_none(),
            "a decoded layer keeps the runtime bank shape it had before streams"
        );
    }

    #[test]
    fn a_source_cannot_be_both_streamed_and_decoded() {
        let project = parse_project(
            r#"{
              "version": 2,
              "normalization_lufs": -18.0,
              "groups": {"fixture": {"gain_db": 0, "bus": "sfx"}},
              "events": [
                {"id": "bed", "group": "fixture", "steps": [{"layers": [{"sources": ["assets/sfx/shared.ogg"], "stream": true}]}]},
                {"id": "shot", "group": "fixture", "steps": [{"layers": [{"sources": ["assets/sfx/shared.ogg"]}]}]}
              ]
            }"#,
        )
        .expect("the mixed project shape should parse");
        let error = compile_project(&project, &resolver(&[SHARED]))
            .expect_err("one source must load as one kind");
        assert!(
            error.to_string().contains(SHARED),
            "the diagnostic names the source"
        );
        assert!(
            error.to_string().contains("stream"),
            "the diagnostic names the conflict"
        );
    }

    #[test]
    fn unknown_project_fields_are_rejected() {
        let error = parse_project(
            r#"{
              "version": 2,
              "normalization_lufs": -18.0,
              "groups": {"fixture": {"gain_db": 0, "bus": "sfx"}},
              "events": [],
              "legacy_path": "summon-1.wav"
            }"#,
        )
        .expect_err("unknown project metadata should not parse");

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn noncanonical_asset_keys_are_rejected() {
        for key in [
            "/absolute.ogg",
            "~/personal.ogg",
            "C:/machine.ogg",
            "outside-assets/sound.ogg",
            "assets/../escape.ogg",
            "assets/./same.ogg",
            "assets//empty.ogg",
        ] {
            let text = format!(
                r#"{{
                  "version": 2,
                  "normalization_lufs": -18.0,
                  "groups": {{"fixture": {{"gain_db": 0, "bus": "sfx"}}}},
                  "events": [{{
                    "id": "event",
                    "group": "fixture",
                    "steps": [{{"layers": [{{"sources": ["{key}"]}}]}}]
                  }}]
                }}"#
            );
            let project = parse_project(&text).expect("shape should parse");
            let error = compile_project(&project, &resolver(&[]))
                .expect_err("noncanonical key should reject");
            assert!(error.to_string().contains(key), "diagnostic for {key}");
        }
    }

    #[test]
    fn asset_directory_names_are_not_policy() {
        for key in [
            "assets/sfx/_inbox/new.ogg",
            "assets/sfx/_sources/raw.ogg",
            "assets/sfx/scratch/test.ogg",
            "assets/sfx/tmp/test.ogg",
            "assets/sfx/temp/test.ogg",
        ] {
            let text = format!(
                r#"{{
                  "version": 2,
                  "normalization_lufs": -18.0,
                  "groups": {{"fixture": {{"gain_db": 0, "bus": "sfx"}}}},
                  "events": [{{
                    "id": "event",
                    "group": "fixture",
                    "steps": [{{"layers": [{{"sources": ["{key}"]}}]}}]
                  }}]
                }}"#
            );
            let project = parse_project(&text).expect("shape should parse");
            let bank = compile_project(&project, &resolver(&[key]))
                .expect("directory names must not affect admission");
            assert_eq!(bank.dependencies, vec![key]);
        }
    }
}
