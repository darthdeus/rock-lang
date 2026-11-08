use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
};

use ebur128::{EbuR128, Mode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use symphonia::core::{
    audio::SampleBuffer,
    codecs::DecoderOptions,
    errors::Error,
    formats::FormatOptions,
    io::{MediaSourceStream, MediaSourceStreamOptions},
    meta::MetadataOptions,
    probe::Hint,
};

pub const PLAYED_PEAK_WARNING_DBTP: f64 = -1.0;
// Bump whenever decoding, windowing or measurement semantics change.
const MEASUREMENT_VERSION: &str = "r128-v1-ebur128-0.1.7-symphonia-0.5.5";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceMeasurement {
    pub momentary_lufs: f64,
    pub integrated_lufs: Option<f64>,
    pub true_peak_dbtp: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceNormalization {
    pub gain: f64,
    pub gain_db: f64,
    pub loudness_lufs: f64,
    pub true_peak_dbtp: f64,
    pub looped: bool,
}

impl SourceMeasurement {
    pub fn loudness(&self, looped: bool) -> Result<f64, String> {
        if looped {
            self.integrated_lufs
                .ok_or_else(|| "loop has no gated integrated loudness".into())
        } else {
            Ok(self.momentary_lufs)
        }
    }

    pub fn normalize(&self, target: f64, looped: bool) -> Result<SourceNormalization, String> {
        let loudness_lufs = self.loudness(looped)?;
        let gain_db = target - loudness_lufs;
        let gain = super::db_gain(gain_db);
        if !gain.is_finite() || gain <= 0.0 {
            return Err("normalization gain is not representable".into());
        }
        Ok(SourceNormalization {
            gain,
            gain_db,
            loudness_lufs,
            true_peak_dbtp: self.true_peak_dbtp,
            looped,
        })
    }
}

/// Shared by development, release packaging and the report. File contents, not
/// paths or mtimes, identify measurements; a renamed source reuses its result.
pub struct MeasurementCache {
    root: PathBuf,
}
impl MeasurementCache {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn standard() -> Result<Self, String> {
        let root = if let Some(root) = std::env::var_os("XDG_CACHE_HOME") {
            PathBuf::from(root)
        } else {
            PathBuf::from(
                std::env::var_os("HOME").ok_or("HOME is unset; cannot locate loudness cache")?,
            )
            .join(".cache")
        };
        Ok(Self::new(root.join("rock/audio-loudness")))
    }
    pub fn measure(&self, path: &Path, speed: f64) -> Result<SourceMeasurement, String> {
        if !speed.is_finite() || speed <= 0.0 {
            return Err("measurement speed must be positive".into());
        }
        let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut hash = Sha256::new();
        hash.update(MEASUREMENT_VERSION.as_bytes());
        hash.update(speed.to_bits().to_le_bytes());
        hash.update(&bytes);
        let key = format!("{:x}", hash.finalize());
        self.cached(&key, || {
            measure_bytes(bytes, path.extension().and_then(|s| s.to_str()), speed)
        })
    }
    pub fn measure_mix(&self, layers: &[RenderLayer]) -> Result<SourceMeasurement, String> {
        let mut hash = Sha256::new();
        hash.update(MEASUREMENT_VERSION.as_bytes());
        hash.update(b"dry-mix-v1-48khz-linear");
        for layer in layers {
            let bytes =
                fs::read(&layer.path).map_err(|e| format!("{}: {e}", layer.path.display()))?;
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
            hash.update(layer.gain.to_bits().to_le_bytes());
            hash.update(layer.speed.to_bits().to_le_bytes());
            hash.update(layer.offset.to_bits().to_le_bytes());
        }
        self.cached(&format!("{:x}", hash.finalize()), || measure_mix(layers))
    }
    fn cached(
        &self,
        key: &str,
        measure: impl FnOnce() -> Result<SourceMeasurement, String>,
    ) -> Result<SourceMeasurement, String> {
        let cached = self.root.join(format!("{key}.json"));
        if cached.is_file() {
            let text = fs::read(&cached).map_err(|e| format!("{}: {e}", cached.display()))?;
            return serde_json::from_slice(&text)
                .map_err(|e| format!("invalid loudness cache {}: {e}", cached.display()));
        }
        let result = measure()?;
        fs::create_dir_all(&self.root).map_err(|e| format!("{}: {e}", self.root.display()))?;
        // Same-content concurrent writers publish identical results; unique temporary
        // files keep readers from seeing a partial JSON record.
        let temp = tempfile::Builder::new()
            .prefix("measurement-")
            .tempfile_in(&self.root)
            .map_err(|e| e.to_string())?;
        serde_json::to_writer(temp.as_file(), &result).map_err(|e| e.to_string())?;
        temp.persist(&cached).map_err(|e| e.to_string())?;
        Ok(result)
    }
}

struct DecodedAudio {
    rate: u32,
    channels: usize,
    samples: Vec<f32>,
}

fn decode(bytes: Vec<u8>, extension: Option<&str>) -> Result<DecodedAudio, String> {
    let source = MediaSourceStream::new(
        Box::new(Cursor::new(bytes)),
        MediaSourceStreamOptions::default(),
    );
    let mut hint = Hint::new();
    if let Some(extension) = extension {
        hint.with_extension(extension);
    }
    let mut format = symphonia::default::get_probe()
        .format(
            &hint,
            source,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|e| e.to_string())?
        .format;
    let track = format.default_track().ok_or("no default audio track")?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| e.to_string())?;
    let mut decoded = DecodedAudio {
        rate: 0,
        channels: 0,
        samples: Vec::new(),
    };
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.to_string()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let audio = decoder.decode(&packet).map_err(|e| e.to_string())?;
        let spec = *audio.spec();
        if decoded.rate == 0 {
            decoded.rate = spec.rate;
            decoded.channels = spec.channels.count();
        }
        if decoded.rate != spec.rate || decoded.channels != spec.channels.count() {
            return Err("audio format changes within source".into());
        }
        let mut samples = SampleBuffer::<f32>::new(audio.capacity() as u64, spec);
        samples.copy_interleaved_ref(audio);
        if samples.samples().iter().any(|s| !s.is_finite()) {
            return Err("nonfinite audio sample".into());
        }
        decoded.samples.extend_from_slice(samples.samples());
    }
    if decoded.samples.is_empty() {
        return Err("source contains no audio".into());
    }
    Ok(decoded)
}

fn measure_bytes(
    bytes: Vec<u8>,
    extension: Option<&str>,
    speed: f64,
) -> Result<SourceMeasurement, String> {
    measure_samples(&decode(bytes, extension)?, speed)
}

fn measure_samples(audio: &DecodedAudio, speed: f64) -> Result<SourceMeasurement, String> {
    let rate = (f64::from(audio.rate) * speed).round() as u32;
    let channels = audio.channels;
    let mut meter = EbuR128::new(channels as u32, rate, Mode::I | Mode::TRUE_PEAK)
        .map_err(|e| e.to_string())?;
    let hop = (rate as usize / 100).max(1) * channels;
    let mut maximum = f64::NEG_INFINITY;
    for chunk in audio.samples.chunks(hop) {
        meter.add_frames_f32(chunk).map_err(|e| e.to_string())?;
        maximum = maximum.max(meter.loudness_momentary().map_err(|e| e.to_string())?);
    }
    let integrated = meter.loudness_global().map_err(|e| e.to_string())?;
    // Scan the tail and complete short transient windows without adding padding
    // to the integrated measurement used by loops.
    let silence = vec![0.0f32; hop];
    for _ in 0..40 {
        meter.add_frames_f32(&silence).map_err(|e| e.to_string())?;
        maximum = maximum.max(meter.loudness_momentary().map_err(|e| e.to_string())?);
    }
    let mut peak = 0.0f64;
    for channel in 0..channels {
        peak = peak.max(meter.true_peak(channel as u32).map_err(|e| e.to_string())?);
    }
    if !maximum.is_finite() || peak <= 0.0 {
        return Err("source is silent; cannot normalize loudness".into());
    }
    Ok(SourceMeasurement {
        momentary_lufs: maximum,
        integrated_lufs: integrated.is_finite().then_some(integrated),
        true_peak_dbtp: 20.0 * peak.log10(),
    })
}

#[derive(Clone)]
pub struct RenderLayer {
    pub path: PathBuf,
    pub gain: f64,
    pub speed: f64,
    pub offset: f64,
}

/// Offline, dry, centered sum for bank diagnostics and the report, never playback.
/// Mono/stereo sources are linearly resampled to 48 kHz; mono
/// contributes equal-power channels when mixed alongside stereo.
pub fn measure_mix(layers: &[RenderLayer]) -> Result<SourceMeasurement, String> {
    let decoded = layers
        .iter()
        .map(|layer| {
            decode(
                fs::read(&layer.path).map_err(|e| e.to_string())?,
                layer.path.extension().and_then(|s| s.to_str()),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let channels = decoded
        .iter()
        .map(|audio| audio.channels)
        .max()
        .ok_or("empty mix")?;
    if channels > 2 {
        return Err("report mix rendering supports mono and stereo sources".into());
    }
    let mut frames = 0;
    for (layer, audio) in layers.iter().zip(&decoded) {
        if !layer.gain.is_finite()
            || !layer.speed.is_finite()
            || layer.speed <= 0.0
            || !layer.offset.is_finite()
            || layer.offset < 0.0
        {
            return Err("invalid render layer".into());
        }
        frames = frames.max(
            (layer.offset * 48000.0).round() as usize
                + ((audio.samples.len() / audio.channels) as f64 * 48000.0
                    / (audio.rate as f64 * layer.speed))
                    .ceil() as usize,
        );
    }
    let mut mixed = DecodedAudio {
        rate: 48000,
        channels,
        samples: vec![0.0; frames * channels],
    };
    for (layer, audio) in layers.iter().zip(&decoded) {
        let start = (layer.offset * 48000.0).round() as usize;
        let source_frames = audio.samples.len() / audio.channels;
        for frame in start..frames {
            let position = (frame - start) as f64 * audio.rate as f64 * layer.speed / 48000.0;
            let index = position.floor() as usize;
            if index >= source_frames {
                break;
            }
            let next = (index + 1).min(source_frames - 1);
            let fraction = position.fract();
            for channel in 0..channels {
                let source_channel = channel.min(audio.channels - 1);
                let a = audio.samples[index * audio.channels + source_channel] as f64;
                let b = audio.samples[next * audio.channels + source_channel] as f64;
                let mono_gain = if audio.channels == 1 && channels == 2 {
                    std::f64::consts::FRAC_1_SQRT_2
                } else {
                    1.0
                };
                mixed.samples[frame * channels + channel] +=
                    ((a + (b - a) * fraction) * layer.gain * mono_gain) as f32;
            }
        }
    }
    measure_samples(&mixed, 1.0)
}
