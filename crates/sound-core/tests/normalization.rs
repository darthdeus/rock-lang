use sound_core::{AssetMapResolver, compile_project, parse_project, serialize_runtime_bank};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "rock-loudness-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn wav(&self, name: &str, amplitude: f32) -> PathBuf {
        let path = self.0.join(name);
        write_tone(&path, amplitude);
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn write_tone(path: &Path, amplitude: f32) {
    let samples: Vec<f32> = (0..48000)
        .map(|i| amplitude * (std::f32::consts::TAU * 1000.0 * i as f32 / 48000.0).sin())
        .collect();
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
fn compile(value: &serde_json::Value, paths: &[(String, PathBuf)]) -> serde_json::Value {
    let project = parse_project(&value.to_string()).unwrap();
    let resolver = AssetMapResolver::new(paths.iter().cloned().collect::<BTreeMap<_, _>>());
    let bank = compile_project(&project, &resolver).unwrap();
    serde_json::from_slice(&serialize_runtime_bank(&bank).unwrap()).unwrap()
}
fn gain(bank: &serde_json::Value, key: &str) -> f64 {
    bank["normalizations"][key]["gain"].as_f64().unwrap_or(1.0)
}
#[test]
fn different_sources_have_equal_played_loudness_at_zero_offset() {
    let f = Fixture::new();
    let bank = compile(
        &project(),
        &[
            ("assets/a.wav".into(), f.wav("a.wav", 0.9)),
            ("assets/b.wav".into(), f.wav("b.wav", 0.01)),
        ],
    );
    let ratio = 0.9 * gain(&bank, "assets/a.wav") / (0.01 * gain(&bank, "assets/b.wav"));
    assert!(gain(&bank, "assets/a.wav") < 1.0 && gain(&bank, "assets/b.wav") > 1.0);
    assert!(
        (20.0 * ratio.log10()).abs() < 0.1,
        "same waveform must play equally loud, ratio={ratio}"
    );
}
#[test]
fn an_event_without_a_group_fails_compile() {
    let f = Fixture::new();
    let resolver = AssetMapResolver::new(
        [
            ("assets/a.wav".into(), f.wav("a.wav", 0.1)),
            ("assets/b.wav".into(), f.wav("b.wav", 0.01)),
        ]
        .into(),
    );
    let mut ungrouped = project();
    ungrouped["events"][0]
        .as_object_mut()
        .unwrap()
        .remove("group");
    let result = parse_project(&ungrouped.to_string()).and_then(|p| compile_project(&p, &resolver));
    assert!(result.is_err(), "ungrouped event must be rejected");
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
        .push(serde_json::json!({"sources":["assets/missing.wav"]}));
    let partial = sound_core::compile_project_allowing_missing_assets(
        &parse_project(&partial.to_string()).unwrap(),
        &resolver,
    )
    .unwrap();
    assert_eq!(
        partial.played_peak_warnings().len(),
        1,
        "a missing layer must not hide the surviving layers' sum"
    );
    let bank: serde_json::Value =
        serde_json::from_slice(&serialize_runtime_bank(&compiled).unwrap()).unwrap();
    let cache = sound_core::MeasurementCache::new(f.0.join("cache"));
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
    let project_path = f.0.join("project.json");
    std::fs::create_dir(f.0.join("assets")).unwrap();
    for (key, path) in &paths {
        std::fs::copy(path, f.0.join(key)).unwrap();
    }
    std::fs::write(&project_path, value.to_string()).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sound-loudness"))
        .args([
            "report",
            project_path.to_str().unwrap(),
            f.0.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(
        report
            .lines()
            .any(|line| line.contains("STEP_SUM") && line.contains("ABOVE_CEILING")),
        "{report}"
    );
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sound-loudness"))
        .args([
            "compile",
            project_path.to_str().unwrap(),
            f.0.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
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
    let cache_root = f.0.join("cache");
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
#[test]
fn cache_reuses_content_and_invalidates_when_content_changes() {
    let f = Fixture::new();
    let path = f.wav("changing.wav", 0.1);
    let cache_root = f.0.join("cache");
    let cache = sound_core::MeasurementCache::new(cache_root.clone());
    let first = cache.measure(&path, 1.0).unwrap();
    let entries = || {
        std::fs::read_dir(&cache_root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>()
    };
    let original = entries()[0].clone();
    let modified = std::fs::metadata(&original).unwrap().modified().unwrap();
    assert_eq!(first, cache.measure(&path, 1.0).unwrap());
    assert_eq!(
        std::fs::metadata(original).unwrap().modified().unwrap(),
        modified
    );
    write_tone(&path, 0.01);
    let second = cache.measure(&path, 1.0).unwrap();
    assert!((first.momentary_lufs - second.momentary_lufs - 20.0).abs() < 0.01);
    assert_eq!(entries().len(), 2, "content edit gets a new cache entry");
}
#[test]
fn offsets_need_reasons_and_groups_route_to_existing_buses() {
    let f = Fixture::new();
    let paths = [
        ("assets/a.wav".into(), f.wav("a.wav", 0.1)),
        ("assets/b.wav".into(), f.wav("b.wav", 0.01)),
    ];
    let resolver = AssetMapResolver::new(paths.iter().cloned().collect());
    let mut value = project();
    value["events"][0]["offset_db"] = (-3.0).into();
    let error =
        compile_project(&parse_project(&value.to_string()).unwrap(), &resolver).unwrap_err();
    assert!(error.to_string().contains("reason"));
    value["events"][0]["reason"] = "quieter event role".into();
    value["groups"]["test"]["bus"] = "ui".into();
    let bank = compile(&value, &paths);
    assert_eq!(bank["events"][0]["bus"], "ui");
    assert!(
        (bank["events"][0]["steps"][0]["layers"][0]["volume"]
            .as_f64()
            .unwrap()
            - 10.0_f64.powf(-3.0 / 20.0))
        .abs()
            < 1e-6
    );
}
