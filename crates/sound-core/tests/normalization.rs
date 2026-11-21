use sound_core::{AssetMapResolver, compile_project, parse_project, serialize_runtime_bank};
use std::path::{Path, PathBuf};

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn wav(&self, name: &str, amplitude: f32) -> PathBuf {
        let path = self.path().join(name);
        write_tone(&path, amplitude);
        path
    }
}
fn write_tone(path: &Path, amplitude: f32) {
    let samples =
        (0..48000).map(|i| amplitude * (std::f32::consts::TAU * 1000.0 * i as f32 / 48000.0).sin());
    let bytes = (samples.len() * 4) as u32;
    let mut out = Vec::new();
    out.extend(b"RIFF");
    out.extend((36 + bytes).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(3u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(48000u32.to_le_bytes());
    out.extend(192000u32.to_le_bytes());
    out.extend(4u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    out.extend(b"data");
    out.extend(bytes.to_le_bytes());
    for sample in samples {
        out.extend(sample.to_le_bytes());
    }
    std::fs::write(path, out).unwrap();
}
fn project() -> serde_json::Value {
    serde_json::json!({"version":2,"normalization_lufs":-18.0,"groups":{"test":{"gain_db":0.0,"bus":"sfx"}},"events":[{"id":"tone","group":"test","steps":[{"layers":[{"sources":["assets/a.wav","assets/b.wav"]}]}]}]})
}
fn gain(bank: &serde_json::Value, key: &str) -> f64 {
    bank["normalizations"][key]["gain"].as_f64().unwrap()
}
#[test]
fn an_event_without_a_group_fails_to_parse() {
    let mut ungrouped = project();
    ungrouped["events"][0]
        .as_object_mut()
        .unwrap()
        .remove("group");
    let error = parse_project(&ungrouped.to_string()).unwrap_err();
    assert!(
        error.to_string().contains("missing field `group`"),
        "{error}"
    );
}

#[test]
fn played_peak_warning_does_not_reduce_source_or_layer_gains() {
    let f = Fixture::new();
    let mut value = project();
    value["normalization_lufs"] = 0.0.into();
    value["groups"]["test"]["gain_db"] = (-8.0).into();
    value["events"][0]["steps"][0]["layers"] = serde_json::json!([
        {"sources":["assets/a.wav"]}, {"sources":["assets/b.wav"]}
    ]);
    let paths = [
        ("assets/a.wav".into(), f.wav("a.wav", 0.9)),
        ("assets/b.wav".into(), f.wav("b.wav", 0.09)),
    ];
    let resolver = AssetMapResolver::new(paths.iter().cloned().collect());
    let compiled = compile_project(&parse_project(&value.to_string()).unwrap(), &resolver).unwrap();
    let warnings = compiled.played_peak_warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(
        warnings[0].layer, None,
        "the summed step, not either layer, exceeds the ceiling"
    );
    assert!(
        compiled.played["tone"][0].layers.iter().all(|layer| layer
            .level
            .as_ref()
            .unwrap()
            .true_peak_dbtp
            <= -1.0)
    );
    let mut partial = value.clone();
    partial["events"][0]["steps"][0]["layers"]
        .as_array_mut()
        .unwrap()
        .insert(1, serde_json::json!({"sources":["assets/missing.wav"]}));
    let partial = sound_core::prepare_project(
        &parse_project(&partial.to_string()).unwrap(),
        &resolver,
        false,
    )
    .and_then(|prepared| prepared.compile())
    .unwrap();
    assert_eq!(
        partial.played["tone"][0].sum, compiled.played["tone"][0].sum,
        "the sum contains both playable layers"
    );
    assert!(partial.played["tone"][0].layers[1].level.is_none());
    assert_eq!(partial.played_peak_warnings(), warnings);
    let bank: serde_json::Value =
        serde_json::from_slice(&serialize_runtime_bank(&compiled).unwrap()).unwrap();
    let cache = sound_core::MeasurementCache::new(f.path().join("cache"));
    for (key, amplitude) in [("assets/a.wav", 0.9), ("assets/b.wav", 0.09)] {
        let rendered = f.wav("rendered.wav", (amplitude * gain(&bank, key)) as f32);
        let measured = cache.measure(&rendered, 1.0).unwrap();
        assert!(
            measured.momentary_lufs.abs() < 0.001,
            "source must reach target, got {}",
            measured.momentary_lufs
        );
        assert!(
            measured.true_peak_dbtp > -1.0,
            "source peak must not limit its gain"
        );
    }
    for layer in bank["events"][0]["steps"][0]["layers"].as_array().unwrap() {
        assert!((layer["volume"].as_f64().unwrap() - sound_core::db_gain(-8.0)).abs() < 1e-6);
    }
    let project_path = f.path().join("project.json");
    std::fs::create_dir(f.path().join("assets")).unwrap();
    for (key, path) in &paths {
        std::fs::copy(path, f.path().join(key)).unwrap();
    }
    std::fs::write(&project_path, value.to_string()).unwrap();
    let loudness = |mode| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_sound-loudness"))
            .arg(mode)
            .arg(&project_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    let report = String::from_utf8(loudness("report").stdout).unwrap();
    assert!(
        report
            .lines()
            .any(|line| line.contains("STEP_SUM") && line.contains("ABOVE_CEILING")),
        "{report}"
    );
    let output = loudness("compile");
    assert!(String::from_utf8_lossy(&output.stderr).contains("WARNING tone"));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        bank
    );
}

#[test]
fn played_mix_cache_tracks_content_gain_speed_and_offset() {
    let f = Fixture::new();
    let path = f.wav("mix.wav", 0.1);
    let cache_root = f.path().join("cache");
    let cache = sound_core::MeasurementCache::new(cache_root.clone());
    let mut layers = [sound_core::RenderLayer {
        path: path.clone(),
        gain: 1.0,
        speed: 1.0,
        offset: 0.0,
    }];
    let first = cache.measure_mix(&layers).unwrap();
    let original = std::fs::read_dir(&cache_root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let modified = std::fs::metadata(&original).unwrap().modified().unwrap();
    assert_eq!(first, cache.measure_mix(&layers).unwrap());
    assert_eq!(
        std::fs::metadata(original).unwrap().modified().unwrap(),
        modified
    );
    layers[0].gain = 0.5;
    let quiet = cache.measure_mix(&layers).unwrap();
    assert!(quiet.momentary_lufs < first.momentary_lufs - 6.0);
    layers[0].speed = 0.5;
    cache.measure_mix(&layers).unwrap();
    layers[0].offset = 0.25;
    cache.measure_mix(&layers).unwrap();
    write_tone(&path, 0.01);
    let changed = cache.measure_mix(&layers).unwrap();
    assert!(changed.momentary_lufs < quiet.momentary_lufs - 19.0);
    assert_eq!(std::fs::read_dir(cache_root).unwrap().count(), 5);
}
