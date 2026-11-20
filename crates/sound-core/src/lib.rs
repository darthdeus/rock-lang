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
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub(crate) const AUTHORING_SCHEMA_VERSION: u32 = 2;
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
    #[serde(default)]
    pub volume: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default = "default_speed")]
    pub speed: f32,
    #[serde(default)]
    pub offset: f32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream: bool,
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
    assets: BTreeMap<String, Option<PathBuf>>,
}

impl AssetMapResolver {
    pub fn new(assets: BTreeMap<String, PathBuf>) -> Self {
        Self {
            assets: assets
                .into_iter()
                .map(|(key, path)| (key, Some(path)))
                .collect(),
        }
    }
}

impl AssetResolver for AssetMapResolver {
    fn resolve(&self, key: &str) -> Option<PathBuf> {
        self.assets.get(key).cloned().flatten()
    }

    fn discover(&self, pattern: &str) -> Result<Vec<String>, String> {
        let matcher = sources::source_matcher(pattern)?;
        Ok(self
            .assets
            .iter()
            .filter(|(key, path)| path.is_some() && matcher.is_match(key))
            .map(|(key, _)| key.clone())
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
    prepare_project(project, resolver, true)?.compile()
}

pub struct PreparedSoundProject {
    pub project: SoundProject,
    looped_sources: BTreeSet<String>,
    sources: AssetMapResolver,
}

impl PreparedSoundProject {
    pub fn resolved_sources(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.sources
            .assets
            .iter()
            .filter_map(|(key, path)| Some((key.as_str(), path.as_deref()?)))
    }

    pub fn missing_sources(&self) -> impl Iterator<Item = &str> {
        self.sources
            .assets
            .iter()
            .filter(|(_, path)| path.is_none())
            .map(|(key, _)| key.as_str())
    }

    pub fn compile(&self) -> Result<RuntimeBank, SoundCoreError> {
        let project = &self.project;
        let cache = MeasurementCache::standard().map_err(measurement_error)?;
        let mut normalizations = BTreeMap::new();
        for (key, path) in self.resolved_sources() {
            let measurement = cache
                .measure(path, 1.0)
                .map_err(|error| measurement_error(format!("{key}: {error}")))?;
            let normalized = measurement
                .normalize(
                    project.normalization_lufs,
                    self.looped_sources.contains(key),
                )
                .map_err(|error| measurement_error(format!("{key}: {error}")))?;
            normalizations.insert(key.to_owned(), normalized);
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
        let played = played::analyze_project(project, &normalizations, &self.sources, &cache)
            .map_err(measurement_error)?;
        Ok(RuntimeBank {
            schema_version: RUNTIME_SCHEMA_VERSION,
            events,
            dependencies: self.sources.assets.keys().cloned().collect(),
            normalizations,
            played,
        })
    }
}

pub fn prepare_project<R: AssetResolver + ?Sized>(
    project: &SoundProject,
    resolver: &R,
    require_assets: bool,
) -> Result<PreparedSoundProject, SoundCoreError> {
    let mut project = project.clone();
    let mut looped_sources = BTreeSet::new();
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
                if layer.looped || layer.stream {
                    looped_sources.extend(layer.sources.iter().cloned());
                }
            }
        }
    }
    let mut sources = BTreeMap::new();
    let diagnostics = validate_project(&project, |key| {
        let source = sources
            .entry(key.to_owned())
            .or_insert_with(|| resolver.resolve(key));
        source.is_some() || !require_assets
    });
    if !diagnostics.is_empty() {
        return Err(SoundCoreError::Diagnostics(diagnostics));
    }
    Ok(PreparedSoundProject {
        project,
        looped_sources,
        sources: AssetMapResolver { assets: sources },
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

fn validate_project(
    project: &SoundProject,
    mut source_available: impl FnMut(&str) -> bool,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut check = |ok: bool, path: &dyn Display, message: &dyn Display| {
        if !ok {
            diagnostics.push(diagnostic(path.to_string(), message.to_string()));
        }
    };
    let finite_gain = |db: f64| db.is_finite() && db_gain(db).is_finite();
    let explained = |value: f64, reason: &Option<String>| {
        value == 0.0
            || reason
                .as_deref()
                .is_some_and(|reason| !reason.trim().is_empty())
    };
    check(
        project.version == AUTHORING_SCHEMA_VERSION,
        &"version",
        &format_args!(
            "unsupported schema version {}; expected {AUTHORING_SCHEMA_VERSION}",
            project.version
        ),
    );
    check(
        !project.events.is_empty(),
        &"events",
        &"project must contain at least one event",
    );
    check(
        project.normalization_lufs.is_finite(),
        &"normalization_lufs",
        &"target must be finite",
    );
    for (name, group) in &project.groups {
        check(
            !name.trim().is_empty() && group.gain_db.is_finite(),
            &format_args!("groups.{name}"),
            &"group name must be nonempty and gain_db finite",
        );
        check(
            ["sfx", "ui", "foley", "ambience", "music"].contains(&group.bus.as_str()),
            &format_args!("groups.{name}.bus"),
            &"unknown sound bus",
        );
    }
    let mut seen_ids = BTreeMap::new();
    let mut source_kinds: BTreeMap<&str, (bool, String)> = BTreeMap::new();
    for (event_index, event) in project.events.iter().enumerate() {
        let event_path = format!("events[{event_index}]");
        let id_path = format_args!("{event_path}.id");
        if event.id.trim().is_empty() {
            check(false, &id_path, &"event ID must not be empty");
        } else {
            let first = seen_ids.entry(event.id.as_str()).or_insert(event_index);
            check(
                *first == event_index,
                &id_path,
                &format_args!("duplicate event ID; first declared at events[{first}]"),
            );
        }
        let group = project.groups.get(&event.group);
        check(
            group.is_some(),
            &format_args!("{event_path}.group"),
            &format_args!("unknown mix group: {}", event.group),
        );
        check(
            finite_gain(event.offset_db),
            &format_args!("{event_path}.offset_db"),
            &"offset must be finite",
        );
        check(
            explained(event.offset_db, &event.reason),
            &format_args!("{event_path}.reason"),
            &"a nonzero event offset needs a reason",
        );
        check(
            !event.steps.is_empty(),
            &format_args!("{event_path}.steps"),
            &"event must contain at least one step",
        );

        for (step_index, step) in event.steps.iter().enumerate() {
            let step_path = format!("{event_path}.steps[{step_index}]");
            check(
                !step.layers.is_empty(),
                &format_args!("{step_path}.layers"),
                &"step must contain at least one layer",
            );
            for (layer_index, layer) in step.layers.iter().enumerate() {
                let layer_path = format!("{step_path}.layers[{layer_index}]");
                let volume = f64::from(layer.volume);
                check(
                    !layer.sources.is_empty(),
                    &format_args!("{layer_path}.sources"),
                    &"layer must contain at least one source variant",
                );
                check(
                    finite_gain(volume),
                    &format_args!("{layer_path}.volume"),
                    &"volume must be finite dB",
                );
                check(
                    group.is_none_or(|group| {
                        (db_gain(group.gain_db + event.offset_db + volume) as f32).is_finite()
                    }),
                    &format_args!("{layer_path}.volume"),
                    &"combined group, event and layer gain overflows runtime gain",
                );
                check(
                    explained(volume, &layer.reason),
                    &format_args!("{layer_path}.reason"),
                    &"a nonzero layer balance needs a reason",
                );
                check(
                    layer.speed.is_finite() && layer.speed > 0.0,
                    &format_args!("{layer_path}.speed"),
                    &"speed must be finite and strictly positive",
                );
                check(
                    layer.offset.is_finite() && layer.offset >= 0.0,
                    &format_args!("{layer_path}.offset"),
                    &"offset must be finite and non-negative",
                );
                for (source_index, source) in layer.sources.iter().enumerate() {
                    let source_path = format!("{layer_path}.sources[{source_index}]");
                    let kind = |stream: bool| if stream { "stream" } else { "decoded" };
                    match source_kinds.get(source.as_str()) {
                        Some((stream, first)) => check(
                            *stream == layer.stream,
                            &source_path,
                            &format_args!(
                                "{source} is a {} source here and a {} source at {first}",
                                kind(layer.stream),
                                kind(*stream),
                            ),
                        ),
                        None => {
                            source_kinds.insert(source, (layer.stream, source_path.clone()));
                        }
                    }
                    match validate_asset_key(source) {
                        Err(message) => check(false, &source_path, &message),
                        Ok(()) => check(
                            source_available(source),
                            &source_path,
                            &format_args!("referenced asset does not exist: {source}"),
                        ),
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
        SoundStep, compile_project, parse_project, prepare_project, serialize_runtime_bank,
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

    fn project_json(version: u32, events: &str) -> String {
        format!(
            r#"{{"version": {version}, "normalization_lufs": -18.0,
              "groups": {{"fixture": {{"gain_db": 0, "bus": "sfx"}}}},
              "events": [{events}]}}"#
        )
    }

    fn event_json(id: &str, layer: &str) -> String {
        format!(r#"{{"id": "{id}", "group": "fixture", "steps": [{{"layers": [{layer}]}}]}}"#)
    }

    fn project(events: &[String]) -> SoundProject {
        parse_project(&project_json(2, &events.join(","))).expect("project shape should parse")
    }

    fn event(steps: Vec<SoundStep>) -> SoundProject {
        SoundProject {
            version: 2,
            normalization_lufs: -18.0,
            groups: BTreeMap::from([(
                "fixture".to_owned(),
                MixGroup {
                    gain_db: 0.0,
                    bus: "sfx".to_owned(),
                },
            )]),
            events: vec![SoundEvent {
                id: "event".to_owned(),
                group: "fixture".to_owned(),
                offset_db: 0.0,
                reason: None,
                steps,
            }],
        }
    }

    fn valid_project() -> SoundProject {
        project(&[
            r#"{
                "id": "z.event",
                "group": "fixture",
                "steps": [{"layers": [
                    {
                        "sources": ["assets/sfx/shared.ogg", "assets/sfx/alt.ogg"],
                        "volume": 0.5,
                        "reason": "fixture balance",
                        "speed": 1.25
                    },
                    {"sources": ["assets/sfx/shared.ogg"], "volume": 0.0, "speed": 0.5, "offset": 0.25}
                ]}]
            }"#
            .into(),
            event_json("a.event", r#"{"sources": ["assets/sfx/solo.ogg"]}"#),
            event_json(
                "unconsumed.event",
                r#"{"sources": ["assets/sfx/shared.ogg"]}"#,
            ),
        ])
    }

    #[test]
    fn valid_project_compiles_layers_variants_and_sorted_manifest() {
        let project = valid_project();
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
    fn shared_sources_resolve_once_per_bank() {
        use super::AssetResolver;
        use std::cell::RefCell;

        struct CountedResolver(AssetMapResolver, RefCell<BTreeMap<String, usize>>);

        impl AssetResolver for CountedResolver {
            fn resolve(&self, key: &str) -> Option<PathBuf> {
                *self.1.borrow_mut().entry(key.to_owned()).or_default() += 1;
                self.0.resolve(key)
            }

            fn discover(&self, pattern: &str) -> Result<Vec<String>, String> {
                self.0.discover(pattern)
            }
        }

        let resolver = CountedResolver(resolver(&[SHARED, ALT, SOLO]), RefCell::default());
        compile_project(&valid_project(), &resolver).unwrap();
        assert_eq!(
            resolver.1.into_inner(),
            BTreeMap::from([(ALT.into(), 1), (SHARED.into(), 1), (SOLO.into(), 1)])
        );
    }

    #[test]
    fn discovered_variants_compile_to_the_same_bank_as_explicit_sources() {
        let mut explicit = valid_project();
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
    fn missing_dependency_is_a_diagnostic() {
        let project = valid_project();
        let error = compile_project(&project, &resolver(&[SHARED, SOLO]))
            .expect_err("missing dependency should reject the project");

        assert!(matches!(error, SoundCoreError::Diagnostics(_)));
        assert!(error.to_string().contains(ALT));
    }

    #[test]
    fn development_compile_retains_missing_dependencies_for_runtime_silence() {
        let project = valid_project();
        let resolver = resolver(&[SHARED, SOLO]);
        let bank = prepare_project(&project, &resolver, false)
            .and_then(|prepared| prepared.compile())
            .expect("a missing sound must not reject a development bank");

        assert_eq!(bank.events[2].steps[0].layers[0].sources, vec![SHARED, ALT]);
        assert_eq!(bank.dependencies, vec![ALT, SHARED, SOLO]);
    }

    #[test]
    fn unknown_event_is_allowed() {
        let project = project(&[event_json(
            "future.game.event",
            r#"{"sources": ["assets/sfx/solo.ogg"]}"#,
        )]);
        assert!(compile_project(&project, &resolver(&[SOLO])).is_ok());
    }

    #[test]
    fn invalid_project_json_is_rejected() {
        let error = parse_project("{ not json }").expect_err("malformed JSON should reject");

        assert!(matches!(error, SoundCoreError::Parse(_)));
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let solo = event_json("event", r#"{"sources": ["assets/sfx/solo.ogg"]}"#);
        let error = parse_project(&project_json(3, &solo))
            .and_then(|project| compile_project(&project, &resolver(&[SOLO])))
            .expect_err("unsupported schema should reject");

        assert!(error.to_string().contains("version"));
    }

    #[test]
    fn empty_sources_steps_and_layers_are_rejected() {
        let no_sources = project(&[event_json("event", r#"{"sources": []}"#)]);
        let no_layers = event(vec![SoundStep { layers: Vec::new() }]);
        for (project, missing) in [
            (no_sources, "sources"),
            (event(Vec::new()), "steps"),
            (no_layers, "layers"),
        ] {
            let error =
                compile_project(&project, &resolver(&[])).expect_err("an empty list should reject");
            assert!(error.to_string().contains(missing), "{missing}: {error}");
        }
    }

    #[test]
    fn duplicate_and_empty_event_ids_are_rejected() {
        let solo = r#"{"sources": ["assets/sfx/solo.ogg"]}"#;
        let duplicate = project(&[(); 3].map(|()| event_json("event", solo)));
        let empty = project(&[event_json("  ", solo)]);
        for (project, message, count) in [
            (
                duplicate,
                "duplicate event ID; first declared at events[0]",
                2,
            ),
            (empty, "event ID must not be empty", 1),
        ] {
            let error = compile_project(&project, &resolver(&[SOLO]))
                .expect_err("a bad event ID should reject");
            assert_eq!(error.to_string().matches(message).count(), count, "{error}");
        }
    }

    #[test]
    fn numeric_constraints_reject_invalid_and_nonfinite_values() {
        let base = SoundLayer {
            sources: vec![SOLO.to_owned()],
            volume: 0.0,
            reason: None,
            speed: 1.0,
            offset: 0.0,
            stream: false,
            looped: false,
        };
        let balanced = Some("fixture balance".to_owned());
        let cases = [
            (
                "volume",
                SoundLayer {
                    volume: f32::INFINITY,
                    reason: balanced.clone(),
                    ..base.clone()
                },
            ),
            (
                "volume",
                SoundLayer {
                    volume: f32::NAN,
                    reason: balanced,
                    ..base.clone()
                },
            ),
            (
                "speed",
                SoundLayer {
                    speed: 0.0,
                    ..base.clone()
                },
            ),
            (
                "speed",
                SoundLayer {
                    speed: f32::INFINITY,
                    ..base.clone()
                },
            ),
            (
                "offset",
                SoundLayer {
                    offset: -0.01,
                    ..base.clone()
                },
            ),
            (
                "offset",
                SoundLayer {
                    offset: f32::NEG_INFINITY,
                    ..base
                },
            ),
        ];

        for (field, layer) in cases {
            let project = event(vec![SoundStep {
                layers: vec![layer],
            }]);
            let error = compile_project(&project, &resolver(&[SOLO]))
                .expect_err("invalid numeric value should reject");
            assert!(error.to_string().contains(field), "diagnostic for {field}");
        }
    }

    #[test]
    fn stream_layers_compile_to_streamed_runtime_layers() {
        let project = project(&[
            event_json(
                "bed",
                r#"{"sources": ["assets/sfx/solo.ogg"], "stream": true}"#,
            ),
            event_json("shot", r#"{"sources": ["assets/sfx/alt.ogg"]}"#),
        ]);
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
        let project = project(&[
            event_json(
                "bed",
                r#"{"sources": ["assets/sfx/shared.ogg"], "stream": true}"#,
            ),
            event_json("shot", r#"{"sources": ["assets/sfx/shared.ogg"]}"#),
        ]);
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
        let text = project_json(2, "")
            .replace(r#""events""#, r#""legacy_path": "summon-1.wav", "events""#);
        let error = parse_project(&text).expect_err("unknown project metadata should not parse");

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
            let project = project(&[event_json("event", &format!(r#"{{"sources": ["{key}"]}}"#))]);
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
            let project = project(&[event_json("event", &format!(r#"{{"sources": ["{key}"]}}"#))]);
            let bank = compile_project(&project, &resolver(&[key]))
                .expect("directory names must not affect admission");
            assert_eq!(bank.dependencies, vec![key]);
        }
    }
}
