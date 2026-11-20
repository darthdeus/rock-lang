use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, c_float};
use std::num::NonZero;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::handles::Class;
use freeverb::Freeverb;
use rodio::{
    ChannelCount, Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, SampleRate, Source,
};

pub fn stub_audio() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("STONE_AUDIO_STUB").as_deref() == Ok("1"))
}

/// Whether this process opens no output device. Automated runs never open one,
/// including packaged games and direct LuaJIT probes that bypass the CLI, but
/// they still decode, so package tests keep real sound contracts. An explicit
/// `STONE_AUDIO_STUB` selection wins: "1" is the stub, "0" a live device.
/// Windowed interactive runs keep device audio without setting anything.
fn audio_device_disabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("STONE_AUDIO_STUB")
            .map(|value| value == "1")
            .unwrap_or_else(|| {
                std::env::var("STONE_HEADLESS").as_deref() == Ok("1")
                    || std::env::var("PERF_MODE").as_deref() == Ok("1")
                    || std::env::var_os("ROCK_RUN_SIM").is_some()
                    || std::env::var_os("ROCK_JOB_ID").is_some()
                    || std::env::var("FRAME_COUNT")
                        .ok()
                        .and_then(|value| value.parse::<u64>().ok())
                        .is_some_and(|frames| frames > 0)
            })
    })
}

#[no_mangle]
pub extern "C" fn stone_audio_stub_active() -> bool {
    stub_audio()
}

fn output_muted() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        output_muted_from(
            std::env::var("STONE_QUIET").ok().as_deref(),
            std::env::var("STONE_HEADLESS").ok().as_deref(),
            std::env::var("ROCK_RUN_SIM").ok().as_deref(),
        )
    })
}

// Separate from the environment lookup so tests need not mutate the process environment.
fn output_muted_from(quiet: Option<&str>, headless: Option<&str>, run_sim: Option<&str>) -> bool {
    quiet == Some("1") || (quiet.is_none() && headless == Some("1")) || run_sim.is_some()
}

/// Latched at process start, so a mid-run `os_setenv` cannot make a sim audible.
#[no_mangle]
pub extern "C" fn stone_audio_quiet_env() -> bool {
    output_muted()
}

/// The format a stubbed run's bus graph mixes in. Nothing drains it, so these
/// decide an internal buffer layout and nothing audible; they match the shape a
/// commodity sink negotiates so the stubbed graph is not a different one.
const STUB_CHANNELS: rodio::ChannelCount = rodio::nz!(2);
const STUB_SAMPLE_RATE: rodio::SampleRate = rodio::nz!(48_000);

fn audio_asset_exists(path_str: &str) -> bool {
    match crate::asset_registry::resolve_asset_prefix(path_str, 4) {
        Some(b) if !b.is_empty() => true,
        _ => {
            crate::log_once!("AUDIO", "sound load: {} does not resolve", path_str);
            false
        }
    }
}

/// A decoded source holds PCM; a stream source holds its encoded file and each
/// voice decodes it incrementally.
enum Payload {
    Pcm(Arc<Vec<f32>>),
    Encoded(Arc<[u8]>),
}

struct Loaded {
    payload: Payload,
    sample_rate: u32,
    channels: u16,
}

enum SoundState {
    Recipe,
    Admitting,
    /// A play needs no decode or file read on the game thread.
    Playable(Payload),
}

struct SoundData {
    path: String,
    frames: u64,
    sample_rate: u32,
    channels: u16,
    stream: bool,
    state: SoundState,
}

impl SoundData {
    fn recipe(path: String, frames: u64, sample_rate: u32, channels: u16, stream: bool) -> Self {
        Self {
            path,
            frames,
            sample_rate,
            channels,
            stream,
            state: SoundState::Recipe,
        }
    }

    fn playable(&self) -> bool {
        matches!(self.state, SoundState::Playable(_))
    }

    fn samples(&self) -> Option<&Arc<Vec<f32>>> {
        match &self.state {
            SoundState::Playable(Payload::Pcm(samples)) => Some(samples),
            _ => None,
        }
    }

    fn estimated_bytes(&self) -> u64 {
        self.frames
            .saturating_mul(u64::from(self.channels))
            .saturating_mul(std::mem::size_of::<f32>() as u64)
    }

    fn duration_seconds(&self) -> f32 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.frames as f32 / self.sample_rate as f32
        }
    }

    /// The first valid load wins; a later one cannot replace admitted data.
    fn publish(&mut self, loaded: Loaded) -> bool {
        if self.playable() {
            return false;
        }
        let Loaded {
            payload,
            sample_rate,
            channels,
        } = loaded;
        let valid = sample_rate > 0
            && channels > 0
            && match &payload {
                Payload::Pcm(samples) => samples.len() >= channels as usize,
                Payload::Encoded(bytes) => !bytes.is_empty(),
            };
        if !valid {
            self.state = SoundState::Recipe;
            return false;
        }
        if let Payload::Pcm(samples) = &payload {
            self.frames = (samples.len() / channels as usize) as u64;
        }
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.state = SoundState::Playable(payload);
        true
    }

    fn voice(
        &self,
        looped: bool,
        ratio: f32,
        decay: f32,
        pan: Option<Arc<AtomicU32>>,
    ) -> Option<BoxedSource> {
        match &self.state {
            SoundState::Playable(Payload::Pcm(samples)) => {
                let source = SoundSource {
                    samples: samples.clone(),
                    sample_rate: self.sample_rate,
                    channels: self.channels,
                    position: 0,
                    looped,
                };
                Some(voice_source(source, ratio, decay, pan))
            }
            SoundState::Playable(Payload::Encoded(bytes)) => {
                match stream_source(bytes.clone(), looped) {
                    Ok(source) => Some(voice_source(source, ratio, decay, pan)),
                    Err(e) => {
                        crate::log_once!(
                            "AUDIO",
                            "stone_play_sound: stream {} does not decode: {}",
                            self.path,
                            e
                        );
                        None
                    }
                }
            }
            _ => None,
        }
    }
}

struct SoundAdmissionResult {
    handle: u64,
    path: String,
    reserved_bytes: u64,
    loaded: Option<Loaded>,
}

static SOUND_ADMISSION_RESULTS: OnceLock<Mutex<VecDeque<SoundAdmissionResult>>> = OnceLock::new();

fn sound_admission_results() -> &'static Mutex<VecDeque<SoundAdmissionResult>> {
    SOUND_ADMISSION_RESULTS.get_or_init(|| Mutex::new(VecDeque::new()))
}

const SOUND_DECODE_IN_FLIGHT_BYTES: u64 = 128 * 1024 * 1024;

struct SoundSource {
    samples: Arc<Vec<f32>>,
    sample_rate: u32,
    channels: u16,
    position: usize,
    looped: bool,
}

impl Iterator for SoundSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.position >= self.samples.len() {
            if !self.looped || self.samples.is_empty() {
                return None;
            }
            self.position = 0;
        }

        let sample = self.samples[self.position];
        self.position += 1;
        Some(sample)
    }
}

impl Source for SoundSource {
    fn current_span_len(&self) -> Option<usize> {
        if self.looped {
            return None;
        }
        Some(self.samples.len() - self.position)
    }

    fn channels(&self) -> ChannelCount {
        NonZero::new(self.channels).unwrap_or(NonZero::new(1u16).unwrap())
    }

    fn sample_rate(&self) -> SampleRate {
        NonZero::new(self.sample_rate).unwrap_or(NonZero::new(44100u32).unwrap())
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        if self.looped {
            return None;
        }
        let total_samples = self.samples.len() / self.channels as usize;
        Some(std::time::Duration::from_secs_f64(
            total_samples as f64 / self.sample_rate as f64,
        ))
    }
}

type BoxedSource = Box<dyn Source<Item = f32> + Send>;

/// Decodes an encoded file incrementally as it plays.
fn stream_source(
    encoded: Arc<[u8]>,
    looped: bool,
) -> Result<BoxedSource, rodio::decoder::DecoderError> {
    let cursor = std::io::Cursor::new(encoded);
    Ok(if looped {
        Box::new(Decoder::new_looped(cursor)?)
    } else {
        Box::new(Decoder::new(cursor)?)
    })
}

const SLEW_TAU_SECS: f32 = 0.010;

const LOWPASS_MAX_FRAC: f32 = 0.45;

const METER_FALL_DB_PER_SEC: f32 = 20.0;

/// Interleaved frame scratch. Fixed so the audio callback never allocates;
/// devices beyond this many channels fold down to the first `MAX_CHANNELS`.
const MAX_CHANNELS: usize = 8;

fn f32_at(cell: &AtomicU32) -> f32 {
    f32::from_bits(cell.load(Ordering::Relaxed))
}

fn store_f32(cell: &AtomicU32, v: f32) {
    cell.store(v.to_bits(), Ordering::Relaxed);
}

// Written on the game thread, read on the audio thread: atomics only, never a Mutex.
struct BusParams {
    /// f32 bits. The *target*; BusSource slews the applied value toward it.
    gain: AtomicU32,
    samples_pulled: AtomicU64,

    meter: AtomicU32,

    /// Lowpass. Cutoff is slewed in log2(Hz) -- a linear Hz sweep sounds
    /// wrong, because pitch perception is logarithmic.
    cutoff: AtomicU32,
    q: AtomicU32,

    distortion_drive: AtomicU32,
    distortion_mix: AtomicU32,

    /// Bumped after the reverb fields are written, so the audio thread applies
    /// a consistent set rather than a torn one.
    reverb_gen: AtomicU64,
    reverb_enabled: AtomicBool,
    reverb_room: AtomicU32,
    reverb_damping: AtomicU32,
    reverb_wet: AtomicU32,
    reverb_dry: AtomicU32,
    reverb_width: AtomicU32,

    /// Same latch trick: params first, then the generation, so a duck is
    /// triggered exactly once with the values it was fired with.
    duck_gen: AtomicU64,
    duck_depth: AtomicU32,
    duck_attack: AtomicU32,
    duck_release: AtomicU32,
}

impl BusParams {
    fn new(sample_rate: SampleRate) -> Self {
        BusParams {
            gain: AtomicU32::new(1.0f32.to_bits()),
            samples_pulled: AtomicU64::new(0),
            meter: AtomicU32::new(0.0f32.to_bits()),
            cutoff: AtomicU32::new(max_cutoff(sample_rate).to_bits()),
            q: AtomicU32::new(0.707f32.to_bits()),
            distortion_drive: AtomicU32::new(1.0f32.to_bits()),
            distortion_mix: AtomicU32::new(0.0f32.to_bits()),
            reverb_gen: AtomicU64::new(0),
            reverb_enabled: AtomicBool::new(false),
            reverb_room: AtomicU32::new(0.7f32.to_bits()),
            reverb_damping: AtomicU32::new(0.5f32.to_bits()),
            reverb_wet: AtomicU32::new(0.3f32.to_bits()),
            reverb_dry: AtomicU32::new(0.7f32.to_bits()),
            reverb_width: AtomicU32::new(1.0f32.to_bits()),
            duck_gen: AtomicU64::new(0),
            duck_depth: AtomicU32::new(1.0f32.to_bits()),
            duck_attack: AtomicU32::new(0.01f32.to_bits()),
            duck_release: AtomicU32::new(0.1f32.to_bits()),
        }
    }
}

fn max_cutoff(sample_rate: SampleRate) -> f32 {
    sample_rate.get() as f32 * LOWPASS_MAX_FRAC
}

/// RBJ biquad lowpass. Coefficients are recomputed only when the smoothed
/// cutoff actually moves -- the per-sample path is 5 multiplies.
#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    state: [[f32; 2]; MAX_CHANNELS],
}

impl Biquad {
    fn new() -> Self {
        Biquad {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            state: [[0.0; 2]; MAX_CHANNELS],
        }
    }

    fn set_lowpass(&mut self, cutoff: f32, q: f32, sample_rate: f32) {
        let w0 = std::f32::consts::TAU * (cutoff / sample_rate);
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q.max(0.05));
        let a0 = 1.0 + alpha;
        self.b0 = ((1.0 - cos_w0) * 0.5) / a0;
        self.b1 = (1.0 - cos_w0) / a0;
        self.b2 = self.b0;
        self.a1 = (-2.0 * cos_w0) / a0;
        self.a2 = (1.0 - alpha) / a0;
    }

    /// Transposed direct form II -- better numerical behaviour at low cutoffs
    /// than DF-I, which matters because the damage muffle lives down there.
    #[inline]
    fn process(&mut self, ch: usize, x: f32) -> f32 {
        let s = &mut self.state[ch];
        let y = self.b0 * x + s[0];
        s[0] = self.b1 * x - self.a1 * y + s[1];
        s[1] = self.b2 * x - self.a2 * y;
        // Subnormal feedback can persist indefinitely and make silence costly.
        for state in s {
            if state.is_subnormal() {
                *state = 0.0;
            }
        }
        y
    }
}

#[derive(Clone, Copy, PartialEq)]
enum DuckPhase {
    Idle,
    Attack,
    Release,
}

#[derive(Clone)]
struct Bus {
    mixer: rodio::mixer::Mixer,
    params: Arc<BusParams>,
    sample_rate: SampleRate,
}

impl Bus {
    /// A bus and its one output, for exactly one parent.
    fn new(channels: ChannelCount, sample_rate: SampleRate) -> (Self, BusSource) {
        let (mixer, inner) = rodio::mixer::mixer(channels, sample_rate);
        let params = Arc::new(BusParams::new(sample_rate));
        let rate = sample_rate.get() as f32;

        let mut lowpass = Biquad::new();
        lowpass.set_lowpass(max_cutoff(sample_rate), 0.707, rate);

        let source = BusSource {
            inner,
            params: params.clone(),
            channels: channels.get() as usize,
            ch: 0,
            frame: [0.0; MAX_CHANNELS],
            applied_gain: 1.0,
            meter_env: 0.0,
            meter_decay: (10.0f32).powf(-METER_FALL_DB_PER_SEC / 20.0 / rate),
            applied_cutoff_log2: max_cutoff(sample_rate).log2(),
            applied_drive: 1.0,
            applied_mix: 0.0,
            lowpass,
            coeff_cutoff_log2: max_cutoff(sample_rate).log2(),
            reverb: Freeverb::new(sample_rate.get() as usize),
            reverb_gen: 0,
            reverb_enabled: false,
            duck_phase: DuckPhase::Idle,
            duck_env: 1.0,
            duck_depth: 1.0,
            duck_attack_step: 0.0,
            duck_release_step: 0.0,
            duck_gen: 0,
            slew: slew_coeff(sample_rate),
            sample_rate: rate,
        };
        let bus = Bus {
            mixer,
            params,
            sample_rate,
        };
        (bus, source)
    }

    fn set_gain(&self, gain: f32) {
        store_f32(&self.params.gain, gain.max(0.0));
    }

    /// Current meter envelope. A plain load -- reading it changes nothing.
    fn meter(&self) -> f32 {
        f32_at(&self.params.meter)
    }

    fn samples_pulled(&self) -> u64 {
        self.params.samples_pulled.load(Ordering::Relaxed)
    }

    /// Cutoff is clamped into (20Hz, 0.45*rate]; the top of that range is the
    /// transparent "off" position.
    fn set_lowpass(&self, cutoff_hz: f32, q: f32) {
        let max = max_cutoff(self.sample_rate);
        store_f32(&self.params.cutoff, cutoff_hz.clamp(20.0, max));
        store_f32(&self.params.q, q.clamp(0.05, 8.0));
    }

    /// `drive` >= 1 is pre-gain into the soft clipper; `mix` 0..1 blends the
    /// clipped signal against dry, so mix 0 is bit-transparent.
    fn set_distortion(&self, drive: f32, mix: f32) {
        store_f32(&self.params.distortion_drive, drive.max(1.0));
        store_f32(&self.params.distortion_mix, mix.clamp(0.0, 1.0));
    }

    fn set_reverb(&self, enabled: bool, room: f32, damping: f32, wet: f32, dry: f32, width: f32) {
        let p = &self.params;
        p.reverb_enabled.store(enabled, Ordering::Relaxed);
        store_f32(&p.reverb_room, room.clamp(0.0, 1.0));
        store_f32(&p.reverb_damping, damping.clamp(0.0, 1.0));
        store_f32(&p.reverb_wet, wet.clamp(0.0, 1.0));
        store_f32(&p.reverb_dry, dry.clamp(0.0, 1.0));
        store_f32(&p.reverb_width, width.clamp(0.0, 1.0));
        // Release: every store above must be visible before the generation is.
        p.reverb_gen.fetch_add(1, Ordering::Release);
    }

    fn duck(&self, depth: f32, attack: f32, release: f32) {
        let p = &self.params;
        store_f32(&p.duck_depth, depth.clamp(0.0, 1.0));
        store_f32(&p.duck_attack, attack.max(0.0));
        store_f32(&p.duck_release, release.max(0.0));
        p.duck_gen.fetch_add(1, Ordering::Release);
    }
}

/// One-pole coefficient for `SLEW_TAU_SECS` at this rate.
fn slew_coeff(sample_rate: SampleRate) -> f32 {
    1.0 - (-1.0 / (SLEW_TAU_SECS * sample_rate.get() as f32)).exp()
}

// Infinite by construction: a parent mixer permanently drops a child that returns None,
// so a dry bus yields 0.0. Runs on the audio thread: no allocation, no locks.
struct BusSource {
    inner: rodio::mixer::MixerSource,
    params: Arc<BusParams>,
    channels: usize,
    ch: usize,
    frame: [f32; MAX_CHANNELS],

    applied_gain: f32,
    meter_env: f32,
    meter_decay: f32,
    applied_cutoff_log2: f32,
    applied_drive: f32,
    applied_mix: f32,

    lowpass: Biquad,
    coeff_cutoff_log2: f32,

    reverb: Freeverb<f64>,
    reverb_gen: u64,
    reverb_enabled: bool,

    duck_phase: DuckPhase,
    duck_env: f32,
    duck_depth: f32,
    duck_attack_step: f32,
    duck_release_step: f32,
    duck_gen: u64,

    slew: f32,
    sample_rate: f32,
}

impl BusSource {
    fn latch_reverb(&mut self) {
        let gen = self.params.reverb_gen.load(Ordering::Acquire);
        if gen == self.reverb_gen {
            return;
        }
        self.reverb_gen = gen;
        let p = &self.params;
        self.reverb_enabled = p.reverb_enabled.load(Ordering::Relaxed);
        self.reverb.set_room_size(f32_at(&p.reverb_room) as f64);
        self.reverb.set_dampening(f32_at(&p.reverb_damping) as f64);
        self.reverb.set_wet(f32_at(&p.reverb_wet) as f64);
        self.reverb.set_dry(f32_at(&p.reverb_dry) as f64);
        self.reverb.set_width(f32_at(&p.reverb_width) as f64);
    }

    fn latch_duck(&mut self) {
        let gen = self.params.duck_gen.load(Ordering::Acquire);
        if gen == self.duck_gen {
            return;
        }
        self.duck_gen = gen;
        let p = &self.params;
        self.duck_depth = f32_at(&p.duck_depth);
        let attack = f32_at(&p.duck_attack) * self.sample_rate;
        let release = f32_at(&p.duck_release) * self.sample_rate;
        self.duck_attack_step = if attack < 1.0 { 1.0 } else { 1.0 / attack };
        self.duck_release_step = if release < 1.0 { 1.0 } else { 1.0 / release };
        self.duck_phase = DuckPhase::Attack;
    }

    fn advance_duck(&mut self) {
        match self.duck_phase {
            DuckPhase::Idle => {}
            DuckPhase::Attack => {
                let span = 1.0 - self.duck_depth;
                self.duck_env -= span * self.duck_attack_step;
                if self.duck_env <= self.duck_depth {
                    self.duck_env = self.duck_depth;
                    self.duck_phase = DuckPhase::Release;
                }
            }
            DuckPhase::Release => {
                let span = 1.0 - self.duck_depth;
                self.duck_env += span * self.duck_release_step;
                if self.duck_env >= 1.0 {
                    self.duck_env = 1.0;
                    self.duck_phase = DuckPhase::Idle;
                }
            }
        }
    }

    fn fill_frame(&mut self) {
        self.latch_reverb();
        self.latch_duck();

        for i in 0..self.channels.min(MAX_CHANNELS) {
            self.frame[i] = self.inner.next().unwrap_or(0.0);
        }
        // Channels past the scratch must still be consumed to keep frame alignment.
        for _ in MAX_CHANNELS..self.channels {
            let _ = self.inner.next();
        }

        let p = &self.params;
        self.applied_gain += (f32_at(&p.gain) - self.applied_gain) * self.slew;
        self.applied_drive += (f32_at(&p.distortion_drive) - self.applied_drive) * self.slew;
        self.applied_mix += (f32_at(&p.distortion_mix) - self.applied_mix) * self.slew;

        let target_log2 = f32_at(&p.cutoff).max(1.0).log2();
        self.applied_cutoff_log2 += (target_log2 - self.applied_cutoff_log2) * self.slew;
        if (self.applied_cutoff_log2 - self.coeff_cutoff_log2).abs() > 0.0005 {
            self.coeff_cutoff_log2 = self.applied_cutoff_log2;
            let q = f32_at(&p.q);
            self.lowpass
                .set_lowpass(self.applied_cutoff_log2.exp2(), q, self.sample_rate);
        }

        let active = self.channels.min(MAX_CHANNELS);
        for i in 0..active {
            let dry = self.frame[i];
            let wet = (dry * self.applied_drive).tanh();
            self.frame[i] = self
                .lowpass
                .process(i, dry + (wet - dry) * self.applied_mix);
        }

        if self.reverb_enabled && active >= 2 {
            let (l, r) = self
                .reverb
                .tick((self.frame[0] as f64, self.frame[1] as f64));
            self.frame[0] = l as f32;
            self.frame[1] = r as f32;
        }

        self.advance_duck();
        let fader = self.applied_gain * self.duck_env;
        let mut loudest = 0.0f32;
        for sample in &mut self.frame[..active] {
            *sample *= fader;
            loudest = loudest.max(sample.abs());
        }

        self.meter_env = (self.meter_env * self.meter_decay).max(loudest);
        store_f32(&self.params.meter, self.meter_env);
    }
}

impl Iterator for BusSource {
    type Item = f32;

    #[inline]
    fn next(&mut self) -> Option<f32> {
        if self.ch == 0 {
            self.fill_frame();
        }
        let sample = self.frame[self.ch.min(MAX_CHANNELS - 1)];
        self.ch = (self.ch + 1) % self.channels;
        self.params.samples_pulled.fetch_add(1, Ordering::Relaxed);
        Some(sample)
    }
}

impl Source for BusSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }

    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

/// One playing instance of a sound. `sound` is the asset handle it came from,
/// so asset-level operations (stop/unload/reload) can find every instance.
struct Voice {
    sound: u64,
    player: Player,
    pan: Option<Arc<AtomicU32>>,
}

struct MusicPlayback {
    handle: u64,
    player: Player,
}

/// Bus ids, mirrored by `STONE_BUS_*` in `stone/engine/lib.rock`: master, then
/// sfx, music, ambience, ui and foley, which all feed master. Fixed set -- buses
/// are a mixing topology, not content, so there is no dynamic creation.
const BUS_MASTER: u32 = 0;
const BUS_MUSIC: u32 = 2;
const BUS_COUNT: usize = 6;

/// Master is the single child of the device sink's mixer.
struct Buses([Bus; BUS_COUNT]);

impl Buses {
    fn new(sink: &MixerDeviceSink, muted: bool) -> Self {
        Self::attached(
            sink.mixer(),
            sink.config().channel_count(),
            sink.config().sample_rate(),
            muted,
        )
    }

    fn attached(
        mixer: &rodio::mixer::Mixer,
        channels: rodio::ChannelCount,
        rate: rodio::SampleRate,
        muted: bool,
    ) -> Self {
        let (buses, master) = Buses::detached(channels, rate);
        if muted {
            mixer.add(master.amplify(0.0));
        } else {
            mixer.add(master);
        }
        buses
    }

    /// The buses with every leaf wired into master, and master's output.
    fn detached(channels: rodio::ChannelCount, rate: rodio::SampleRate) -> (Self, BusSource) {
        let (master, master_out) = Bus::new(channels, rate);
        let buses = Buses(std::array::from_fn(|id| {
            if id == BUS_MASTER as usize {
                return master.clone();
            }
            let (leaf, out) = Bus::new(channels, rate);
            master.mixer.add(out);
            leaf
        }));
        (buses, master_out)
    }

    fn get(&self, id: u32) -> Option<&Bus> {
        self.0.get(id as usize)
    }
}

struct AudioState {
    _stream: Option<MixerDeviceSink>,
    buses: Buses,
    sounds: HashMap<u64, SoundData>,
    pending_sound_admissions: VecDeque<u64>,
    inflight_sound_bytes: u64,
    music: HashMap<u64, Arc<[u8]>>,
    music_playback: Option<MusicPlayback>,
    // Membership is the liveness answer `stone_voice_active` gives.
    voices: HashMap<u64, Voice>,
}

impl AudioState {
    fn new(stream: Option<MixerDeviceSink>, buses: Buses) -> Self {
        Self {
            _stream: stream,
            buses,
            sounds: HashMap::new(),
            pending_sound_admissions: VecDeque::new(),
            inflight_sound_bytes: 0,
            music: HashMap::new(),
            music_playback: None,
            voices: HashMap::new(),
        }
    }
}

thread_local! {
    static AUDIO_STATE: RefCell<Option<AudioState>> = const { RefCell::new(None) };
}

#[derive(Clone)]
struct AudioStreamErrorCount {
    message: String,
    occurrences: u64,
    reported: u64,
}

#[derive(Clone)]
struct AudioStreamErrors {
    device: String,
    interval: std::time::Duration,
    kinds: HashMap<rodio::cpal::ErrorKind, AudioStreamErrorCount>,
}

impl AudioStreamErrors {
    fn new(device: String, interval: std::time::Duration) -> Self {
        Self {
            device,
            interval,
            kinds: HashMap::new(),
        }
    }

    fn report(&mut self, error: rodio::cpal::Error) {
        let row = self
            .kinds
            .entry(error.kind())
            .or_insert_with(|| AudioStreamErrorCount {
                message: error.to_string(),
                occurrences: 0,
                reported: 0,
            });
        row.occurrences = row.occurrences.saturating_add(1);
        let periodic = rock_log::throttle_should_emit("stone::audio::stream_error", self.interval);
        if row.occurrences == 1 || periodic {
            row.message = error.to_string();
            ::log::error!(target: "AUDIO", "output '{}': {} ({} occurrences; stream remains open)",
                self.device, row.message, row.occurrences);
            row.reported = row.occurrences;
        }
    }
}

impl Drop for AudioStreamErrors {
    fn drop(&mut self) {
        for row in self.kinds.values() {
            if row.occurrences > row.reported {
                ::log::error!(target: "AUDIO", "output '{}': {} ({} occurrences; stream closed)",
                    self.device, row.message, row.occurrences);
            }
        }
    }
}

fn audio_output_builder(
    device: rodio::Device,
) -> Result<DeviceSinkBuilder<impl FnMut(rodio::cpal::Error) + Send + Clone>, rodio::DeviceSinkError>
{
    use rodio::DeviceTrait;
    let name = device
        .description()
        .map(|description| description.name().to_owned())
        .unwrap_or_else(|error| format!("unidentified device ({error})"));
    let mut errors = AudioStreamErrors::new(name, std::time::Duration::from_secs(5));
    Ok(DeviceSinkBuilder::from_device(device)?
        .with_error_callback(move |error| errors.report(error)))
}

fn open_audio_output() -> Result<MixerDeviceSink, rodio::DeviceSinkError> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    let host = rodio::cpal::default_host();
    host.default_output_device()
        .ok_or(rodio::DeviceSinkError::NoDevice)
        .and_then(|device| audio_output_builder(device)?.open_stream())
        .or_else(|original_error| {
            let devices = match host.output_devices() {
                Ok(devices) => devices,
                Err(error) => {
                    ::log::error!(target: "AUDIO", "cannot enumerate output devices: {error}");
                    return Err(original_error);
                }
            };
            for device in devices {
                if !device.description().is_ok_and(|description| {
                    description.driver().is_some_and(|driver| driver != "null")
                }) {
                    continue;
                }
                if let Ok(stream) =
                    audio_output_builder(device).and_then(|builder| builder.open_sink_or_fallback())
                {
                    return Ok(stream);
                }
            }
            Err(original_error)
        })
}

#[no_mangle]
pub extern "C" fn stone_audio_init() -> bool {
    AUDIO_STATE.with(|state| {
        if state.borrow().is_some() {
            return true;
        }

        if audio_device_disabled() {
            *state.borrow_mut() = Some(AudioState::new(
                None,
                Buses::detached(STUB_CHANNELS, STUB_SAMPLE_RATE).0,
            ));
            return true;
        }

        let t_open = std::time::Instant::now();
        let opened = open_audio_output();
        ::log::debug!(target: "AUDIO", "open_audio_output {:.0}ms",
            t_open.elapsed().as_secs_f64() * 1000.0);
        match opened {
            Ok(mut stream) => {
                stream.log_on_drop(false);
                let buses = Buses::new(&stream, output_muted());
                *state.borrow_mut() = Some(AudioState::new(Some(stream), buses));
                true
            }
            Err(e) => {
                ::log::error!(target: "AUDIO", "stone_audio_init: failed to initialize audio: {}", e);
                false
            }
        }
    })
}

#[no_mangle]
pub extern "C" fn stone_audio_quit() {
    AUDIO_STATE.with(|state| {
        if let Some(audio) = state.borrow().as_ref() {
            for handle in audio.sounds.keys() {
                crate::handles::retire(Class::Sound, *handle);
            }
            for handle in audio.music.keys() {
                crate::handles::retire(Class::Music, *handle);
            }
            for handle in audio.voices.keys() {
                crate::handles::retire(Class::Voice, *handle);
            }
        }
        *state.borrow_mut() = None;
    });
    sound_admission_results()
        .lock()
        .expect("sound admissions poisoned")
        .clear();
}

fn read_audio_asset(path: &str) -> Option<Vec<u8>> {
    let bytes = crate::asset_registry::resolve_asset(path);
    if bytes.is_none() {
        crate::log_once!("AUDIO", "sound load: Failed to read {}", path);
    }
    bytes
}

fn open_decoder<R>(reader: R, path: &str) -> Option<Decoder<R>>
where
    R: std::io::Read + std::io::Seek + Send + Sync + 'static,
{
    Decoder::new(reader)
        .map_err(|e| crate::log_once!("AUDIO", "sound load: Failed to decode {}: {}", path, e))
        .ok()
}

/// Decoded PCM, or a stream source's encoded file. The stream's decoder probe
/// checks the format once here so a voice never starts on bytes that cannot decode.
fn load_sound(path: &str, stream: bool) -> Option<Loaded> {
    let bytes = read_audio_asset(path)?;
    if !stream {
        let (samples, sample_rate, channels) = decode_sound_bytes(bytes, path)?;
        return Some(Loaded {
            payload: Payload::Pcm(Arc::new(samples)),
            sample_rate,
            channels,
        });
    }
    let bytes: Arc<[u8]> = bytes.into();
    let decoder = open_decoder(std::io::Cursor::new(bytes.clone()), path)?;
    Some(Loaded {
        sample_rate: decoder.sample_rate().get(),
        channels: decoder.channels().get(),
        payload: Payload::Encoded(bytes),
    })
}

fn decode_sound_bytes(audio_bytes: Vec<u8>, path: &str) -> Option<(Vec<f32>, u32, u16)> {
    let decoder = open_decoder(std::io::Cursor::new(audio_bytes), path)?;
    let sample_rate = decoder.sample_rate().get();
    let channels = decoder.channels().get();
    let mut samples: Vec<f32> = decoder.collect();
    samples.shrink_to_fit();
    if samples.is_empty() {
        crate::log_once!("AUDIO", "sound load: No samples in {}", path);
        return None;
    }
    Some((samples, sample_rate, channels))
}

fn probe_sound_recipe(path: &str) -> Option<(u64, u32, u16)> {
    let decoder = open_decoder(std::io::Cursor::new(read_audio_asset(path)?), path)?;
    let sample_rate = decoder.sample_rate().get();
    let channels = decoder.channels().get();
    let frames = decoder.total_duration().map_or(0, |duration| {
        (duration.as_secs_f64() * sample_rate as f64).round() as u64
    });
    Some((frames, sample_rate, channels))
}

fn load_sound_recipe(
    path: *const c_char,
    frames: i64,
    sample_rate: i64,
    channels: i64,
    stream: bool,
) -> u64 {
    let Some(path_str) = (unsafe { crate::filesystem::cstr(path) }) else {
        return 0;
    };
    if !audio_asset_exists(path_str) {
        return 0;
    }
    let (frames, sample_rate, channels) = if frames > 0 && sample_rate > 0 && channels > 0 {
        (frames as u64, sample_rate as u32, channels as u16)
    } else if frames == 0 && sample_rate == 0 && channels == 0 {
        probe_sound_recipe(path_str).unwrap_or((0, 0, 0))
    } else {
        crate::log_once!("AUDIO", "sound load: invalid recipe for {}", path_str);
        return 0;
    };

    with_audio_mut(|audio| {
        let id = crate::handles::mint(Class::Sound);
        let sound = SoundData::recipe(path_str.to_owned(), frames, sample_rate, channels, stream);
        audio.sounds.insert(id, sound);
        id
    })
    .unwrap_or_else(|| {
        crate::log_once!(
            "AUDIO",
            "sound load: no audio state, so `{}` gets no handle",
            path_str
        );
        0
    })
}

#[no_mangle]
pub extern "C" fn stone_load_sound_deferred(
    path: *const c_char,
    frames: i64,
    sample_rate: i64,
    channels: i64,
) -> u64 {
    load_sound_recipe(path, frames, sample_rate, channels, false)
}

#[no_mangle]
pub extern "C" fn stone_load_sound_stream(
    path: *const c_char,
    frames: i64,
    sample_rate: i64,
    channels: i64,
) -> u64 {
    load_sound_recipe(path, frames, sample_rate, channels, true)
}

#[no_mangle]
pub extern "C" fn stone_sound_admit(handle: u64) -> bool {
    if stub_audio() {
        return with_sound(handle, |_| ()).is_some();
    }
    with_audio_mut(|audio| {
        let Some(sound) = audio.sounds.get_mut(&handle) else {
            return false;
        };
        if matches!(sound.state, SoundState::Recipe) {
            sound.state = SoundState::Admitting;
            audio.pending_sound_admissions.push_back(handle);
        }
        true
    })
    .unwrap_or(false)
}

#[no_mangle]
pub extern "C" fn stone_service_sound_admissions() -> i64 {
    let completed: Vec<SoundAdmissionResult> = {
        let mut results = sound_admission_results()
            .lock()
            .expect("sound admissions poisoned");
        results.drain(..).collect()
    };
    let (published, scheduled) = with_audio_mut(|audio| {
        let mut published = 0;
        for result in completed {
            audio.inflight_sound_bytes = audio
                .inflight_sound_bytes
                .saturating_sub(result.reserved_bytes);
            let Some(sound) = audio.sounds.get_mut(&result.handle) else {
                continue;
            };
            if sound.path != result.path || !matches!(sound.state, SoundState::Admitting) {
                continue;
            }
            match result.loaded {
                Some(loaded) => published += i64::from(sound.publish(loaded)),
                None => {
                    sound.state = SoundState::Recipe;
                    crate::log_once!("AUDIO", "sound admission failed for {}", sound.path);
                }
            }
        }

        let mut scheduled = Vec::new();
        while let Some(handle) = audio.pending_sound_admissions.front().copied() {
            let Some(sound) = audio
                .sounds
                .get(&handle)
                .filter(|sound| matches!(sound.state, SoundState::Admitting))
            else {
                audio.pending_sound_admissions.pop_front();
                continue;
            };
            let limit = SOUND_DECODE_IN_FLIGHT_BYTES;
            let bytes = match (sound.stream, sound.estimated_bytes()) {
                (true, _) => 0,
                (false, 0) => limit,
                (false, estimated) => estimated,
            };
            if bytes > 0
                && audio.inflight_sound_bytes.saturating_add(bytes) > limit
                && audio.inflight_sound_bytes > 0
            {
                break;
            }
            scheduled.push((handle, sound.path.clone(), bytes, sound.stream));
            audio.pending_sound_admissions.pop_front();
            audio.inflight_sound_bytes = audio.inflight_sound_bytes.saturating_add(bytes);
        }
        (published, scheduled)
    })
    .unwrap_or_default();

    for (handle, path, reserved_bytes, stream) in scheduled {
        rayon::spawn(move || {
            let loaded = load_sound(&path, stream);
            let result = SoundAdmissionResult {
                handle,
                path,
                reserved_bytes,
                loaded,
            };
            sound_admission_results()
                .lock()
                .expect("sound admissions poisoned")
                .push_back(result);
        });
    }
    published
}

#[no_mangle]
pub extern "C" fn stone_sound_decode_sync(handle: u64) -> i64 {
    let path = with_sound(handle, |sound| {
        if sound.stream {
            crate::log_once!(
                "AUDIO",
                "stone_sound_decode_sync: {} is a stream source",
                sound.path
            );
            return None;
        }
        Some((!sound.playable()).then(|| sound.path.clone()))
    })
    .flatten();
    let Some(path) = path else { return -1 };
    let Some(path) = path else { return 0 };
    let started = std::time::Instant::now();
    let Some(loaded) = load_sound(&path, false) else {
        return -1;
    };
    let elapsed_us = started.elapsed().as_micros().min(i64::MAX as u128) as i64;
    let published = with_audio_mut(|audio| {
        audio
            .sounds
            .get_mut(&handle)
            .is_some_and(|sound| sound.publish(loaded))
    })
    .unwrap_or(false);
    if published {
        elapsed_us
    } else {
        -1
    }
}

#[no_mangle]
pub extern "C" fn stone_sound_resident(handle: u64) -> bool {
    with_sound(handle, |sound| sound.samples().is_some()).unwrap_or(false)
}

/// Admitted for playback: resident PCM, or a stream source's encoded file.
#[no_mangle]
pub extern "C" fn stone_sound_playable(handle: u64) -> bool {
    with_sound(handle, SoundData::playable).unwrap_or(false)
}

/// Encoded bytes a stream source holds (0 before admission), or -1 for a
/// handle that is not a live stream source.
#[no_mangle]
pub extern "C" fn stone_sound_stream_bytes(handle: u64) -> i64 {
    with_sound(handle, |sound| match &sound.state {
        _ if !sound.stream => -1,
        SoundState::Playable(Payload::Encoded(bytes)) => bytes.len() as i64,
        _ => 0,
    })
    .unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn stone_reload_sound(handle: u64, path: *const c_char) -> bool {
    if handle == 0 {
        return false;
    }
    let Some(path_str) = (unsafe { crate::filesystem::cstr(path) }) else {
        return false;
    };
    let Some(stream) = with_sound(handle, |sound| sound.stream) else {
        return false;
    };
    let Some(loaded) = load_sound(path_str, stream) else {
        return false;
    };
    with_audio_mut(|audio| {
        let Some(sound) = audio.sounds.get_mut(&handle) else {
            return false;
        };
        sound.path = path_str.to_owned();
        sound.state = SoundState::Recipe;
        let published = sound.publish(loaded);
        stop_voices_of(&mut audio.voices, handle);
        published
    })
    .unwrap_or(false)
}

/// How many decoded samples one sound holds NOW. Zero means the recipe exists
/// without PCM; -1 means the handle is not live.
#[no_mangle]
pub extern "C" fn stone_sound_sample_count(handle: u64) -> i64 {
    with_sound(handle, |sound| {
        sound.samples().map_or(0, |s| s.len() as i64)
    })
    .unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn stone_music_position_ms() -> i64 {
    with_audio(|audio| {
        let playback = audio.music_playback.as_ref()?;
        Some(playback.player.get_pos().as_millis() as i64)
    })
    .unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn stone_load_music(path: *const c_char) -> u64 {
    let Some(path_str) = (unsafe { crate::filesystem::cstr(path) }) else {
        return 0;
    };
    let bytes: Arc<[u8]> = if stub_audio() {
        if !audio_asset_exists(path_str) {
            return 0;
        }
        Arc::new([])
    } else {
        let Some(bytes) = read_audio_asset(path_str) else {
            return 0;
        };
        let bytes: Arc<[u8]> = bytes.into();
        if open_decoder(std::io::Cursor::new(bytes.clone()), path_str).is_none() {
            return 0;
        }
        bytes
    };

    with_audio_mut(|audio| {
        let id = crate::handles::mint(Class::Music);
        audio.music.insert(id, bytes);
        id
    })
    .unwrap_or_else(|| {
        crate::log_once!(
            "AUDIO",
            "stone_load_music: no audio state, so `{}` gets no handle",
            path_str
        );
        0
    })
}

/// Live loaded-sound handles (`native_ledger`).
pub(crate) fn live_sound_ids() -> Vec<u64> {
    with_audio(|audio| Some(audio.sounds.keys().copied().collect())).unwrap_or_default()
}

/// Live decoded-music handles (`native_ledger`).
pub(crate) fn live_music_ids() -> Vec<u64> {
    with_audio(|audio| Some(audio.music.keys().copied().collect())).unwrap_or_default()
}

/// Encoded bytes one music track holds, for the asset report's CPU column.
/// -1 for a handle that is not live.
#[no_mangle]
pub extern "C" fn stone_music_byte_len(handle: u64) -> i64 {
    with_audio(|audio| audio.music.get(&handle).map(|bytes| bytes.len() as i64)).unwrap_or(-1)
}

/// Live PLAYING voices (`native_ledger`).
///
/// `STONE_QUIET` makes the ordinary `play` path return the never-started
/// `Voice(0)` sentinel, but that is a property of that path and not of the map:
/// a sim reaching the raw FFI still mints a real voice, so the class is
/// observable and is inventoried rather than assumed empty.
pub(crate) fn live_voice_ids() -> Vec<u64> {
    with_audio(|audio| Some(audio.voices.keys().copied().collect())).unwrap_or_default()
}

#[no_mangle]
pub extern "C" fn stone_music_playing_handle() -> u64 {
    with_audio(|audio| audio.music_playback.as_ref().map(|p| p.handle)).unwrap_or(0)
}

fn stop_music_playback(playback: &mut Option<MusicPlayback>) {
    if let Some(p) = playback.take() {
        p.player.stop();
    }
}

#[no_mangle]
pub extern "C" fn stone_unload_music(handle: u64) -> bool {
    with_audio_mut(|audio| {
        if audio.music.remove(&handle).is_none() {
            return false;
        }
        crate::handles::retire(Class::Music, handle);
        if audio
            .music_playback
            .as_ref()
            .is_some_and(|p| p.handle == handle)
        {
            stop_music_playback(&mut audio.music_playback);
        }
        true
    })
    .unwrap_or(false)
}

/// Free a loaded sound, stopping its playing voices first so a hot-reloaded
/// file does not keep playing the old samples. False if the handle was absent.
#[no_mangle]
pub extern "C" fn stone_unload_sound(handle: u64) -> bool {
    with_audio_mut(|audio| {
        stop_voices_of(&mut audio.voices, handle);
        audio
            .pending_sound_admissions
            .retain(|pending| *pending != handle);
        let removed = audio.sounds.remove(&handle).is_some();
        if removed {
            crate::handles::retire(Class::Sound, handle);
        }
        removed
    })
    .unwrap_or(false)
}

// The speed floor matters: a zero or negative ratio yields a player that never empties
// and would stay in the voice map forever.
fn sound_play_params(volume: f32, speed: f32) -> (f32, f32) {
    (volume.max(0.0), speed.clamp(0.05, 8.0))
}

// `decay` is in seconds of output, so it is applied after the speed change.
fn shaped_source<S>(source: S, ratio: f32, decay: f32) -> BoxedSource
where
    S: Source<Item = f32> + Send + 'static,
{
    let shaped = source.speed(ratio);

    if decay > 0.0 {
        let d = std::time::Duration::from_secs_f32(decay);
        Box::new(shaped.take_duration(d).fade_out(d))
    } else {
        Box::new(shaped)
    }
}

fn constant_power_pan(pan: f32) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    if pan <= -1.0 {
        return (1.0, 0.0);
    }
    if pan >= 1.0 {
        return (0.0, 1.0);
    }
    let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
    (angle.cos(), angle.sin())
}

struct SpatialVoiceSource {
    input: BoxedSource,
    pan: Arc<AtomicU32>,
    pan_bits: u32,
    left: f32,
    right: f32,
    output_channel: usize,
    mono: f32,
}

impl SpatialVoiceSource {
    fn new(input: BoxedSource, pan: Arc<AtomicU32>) -> Self {
        let pan_bits = pan.load(Ordering::Relaxed);
        let (left, right) = constant_power_pan(f32::from_bits(pan_bits));
        Self {
            input,
            pan,
            pan_bits,
            left,
            right,
            output_channel: 2,
            mono: 0.0,
        }
    }
}

impl Iterator for SpatialVoiceSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.output_channel >= 2 {
            let bits = self.pan.load(Ordering::Relaxed);
            if bits != self.pan_bits {
                (self.left, self.right) = constant_power_pan(f32::from_bits(bits));
                self.pan_bits = bits;
            }
            let channels = self.input.channels().get() as usize;
            let mut sum = 0.0;
            let mut read = 0;
            for _ in 0..channels {
                let Some(sample) = self.input.next() else {
                    break;
                };
                sum += sample;
                read += 1;
            }
            if read == 0 {
                return None;
            }
            self.mono = sum / read as f32;
            self.output_channel = 0;
        }

        let sample = self.mono
            * if self.output_channel == 0 {
                self.left
            } else {
                self.right
            };
        self.output_channel += 1;
        Some(sample)
    }
}

impl Source for SpatialVoiceSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        NonZero::new(2u16).unwrap()
    }

    fn sample_rate(&self) -> SampleRate {
        self.input.sample_rate()
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        self.input.total_duration()
    }

    fn try_seek(&mut self, pos: std::time::Duration) -> Result<(), rodio::source::SeekError> {
        self.output_channel = 2;
        self.input.try_seek(pos)
    }
}

fn voice_source<S>(source: S, ratio: f32, decay: f32, pan: Option<Arc<AtomicU32>>) -> BoxedSource
where
    S: Source<Item = f32> + Send + 'static,
{
    let shaped = shaped_source(source, ratio, decay);
    match pan {
        Some(control) => Box::new(SpatialVoiceSource::new(shaped, control)),
        None => shaped,
    }
}

fn stop_voices_of(voices: &mut HashMap<u64, Voice>, sound: u64) {
    voices.retain(|id, v| {
        if v.sound == sound {
            v.player.stop();
            crate::handles::retire(Class::Voice, *id);
            false
        } else {
            true
        }
    });
}

/// Returns the voice id, or 0. Nonzero ids are never reused, so a stale id cannot address
/// a later voice.
#[no_mangle]
pub extern "C" fn stone_play_sound(
    handle: u64,
    bus: u32,
    volume: c_float,
    speed: c_float,
    decay: c_float,
    looped: bool,
    spatial: bool,
    pan: c_float,
) -> u64 {
    with_audio_mut(|audio| {
        // Nothing drains a stubbed bus graph, so an appended player would never empty.
        if audio._stream.is_none() {
            return 0;
        }
        let Some(bus) = audio.buses.get(bus) else {
            crate::log_once!("AUDIO", "stone_play_sound: unknown bus {}", bus);
            return 0;
        };
        let Some(sound_data) = audio.sounds.get(&handle) else {
            return 0;
        };
        let (gain, ratio) = sound_play_params(volume, speed);
        let pan_control = spatial.then(|| Arc::new(AtomicU32::new(pan.to_bits())));
        let Some(input) = sound_data.voice(looped, ratio, decay, pan_control.clone()) else {
            return 0;
        };
        let player = Player::connect_new(&bus.mixer);
        player.set_volume(gain);
        player.append(input);

        // Safe to reap here: `append` bumps sound_count synchronously, so a voice started
        // this call never looks empty.
        audio.voices.retain(|id, v| {
            let live = !v.player.empty();
            if !live {
                crate::handles::retire(Class::Voice, *id);
            }
            live
        });

        let voice_id = crate::handles::mint(Class::Voice);
        audio.voices.insert(
            voice_id,
            Voice {
                sound: handle,
                player,
                pan: pan_control,
            },
        );
        voice_id
    })
    .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn stone_get_sound_duration(handle: u64) -> c_float {
    with_sound(handle, SoundData::duration_seconds).unwrap_or(0.0)
}

/// Peak-amplitude envelope of resident PCM: `buckets` evenly-spaced spans of
/// sample data, each reduced to the largest absolute sample in it. The Rock
/// adapter requests admission and returns empty while an unresident sound loads.
///
/// PEAK, not mean: a mean flattens a percussive attack into the same bar as a
/// pad, which is exactly the distinction the shape is being drawn to show.
/// Channels are folded together -- this is a shape, not a stereo image.
///
/// Writes through an out-pointer rather than returning a buffer: an FFI struct
/// return is a JIT-abort trap, and the caller
/// already owns a NativeBuffer to hand over.
///
/// # Safety
/// `out` must point to at least `buckets` writable `f32`s.
#[no_mangle]
pub unsafe extern "C" fn stone_get_sound_peaks(
    handle: u64,
    out: *mut c_float,
    buckets: i32,
) -> i32 {
    if out.is_null() || buckets <= 0 {
        return 0;
    }
    with_sound(handle, |sound| {
        let Some(samples) = sound.samples() else {
            return 0;
        };
        let n = samples.len();
        if n == 0 {
            return 0;
        }
        let buckets = buckets as usize;
        let slice = std::slice::from_raw_parts_mut(out, buckets);
        for (b, slot) in slice.iter_mut().enumerate() {
            let lo = n * b / buckets;
            let hi = (n * (b + 1) / buckets).max(lo + 1).min(n);
            let mut peak = 0.0f32;
            for s in &samples[lo..hi] {
                let a = s.abs();
                if a > peak {
                    peak = a;
                }
            }
            *slot = peak;
        }
        buckets as i32
    })
    .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn stone_stop_sound(handle: u64) {
    with_audio_mut(|audio| stop_voices_of(&mut audio.voices, handle));
}

/// Map membership, not `Player::empty()`: rodio's `stop()` drains on the audio thread
/// later, so an empty()-based answer would race our own stops.
#[no_mangle]
pub extern "C" fn stone_voice_active(voice: u64) -> bool {
    with_voice(voice, |_| ()).is_some()
}

/// Stop one voice, leaving every sibling voice of the same asset playing.
/// False if the voice was already gone (idempotent).
#[no_mangle]
pub extern "C" fn stone_voice_stop(voice: u64) -> bool {
    with_audio_mut(|audio| {
        let Some(v) = audio.voices.remove(&voice) else {
            return false;
        };
        crate::handles::retire(Class::Voice, voice);
        v.player.stop();
        true
    })
    .unwrap_or(false)
}

/// Per-voice gain, composed with the global SFX volume. Takes effect within
/// rodio's 5ms control tick. False for an unknown/dead voice.
#[no_mangle]
pub extern "C" fn stone_voice_set_gain(voice: u64, gain: c_float) -> bool {
    with_voice(voice, |v| v.player.set_volume(gain.max(0.0))).is_some()
}

/// Change a spatial voice at the next stereo frame without restarting it.
#[no_mangle]
pub extern "C" fn stone_voice_set_pan(voice: u64, pan: c_float) -> bool {
    if !pan.is_finite() {
        return false;
    }
    with_voice(voice, |v| {
        v.pan
            .as_ref()
            .map(|control| control.store(pan.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed))
    })
    .flatten()
    .is_some()
}

/// Playback rate: resamples, so pitch and duration move together.
/// False for an unknown/dead voice.
#[no_mangle]
pub extern "C" fn stone_voice_set_speed(voice: u64, speed: c_float) -> bool {
    with_voice(voice, |v| v.player.set_speed(speed.max(0.01))).is_some()
}

#[no_mangle]
pub extern "C" fn stone_play_music(handle: u64) {
    with_audio_mut(|audio| {
        let Some(bytes) = audio.music.get(&handle).cloned() else {
            crate::log_once!("AUDIO", "stone_play_music: unknown music handle {}", handle);
            return;
        };
        let source = match stream_source(bytes, true) {
            Ok(source) => source,
            Err(e) => {
                crate::log_once!(
                    "AUDIO",
                    "stone_play_music: Failed to decode music {}: {}",
                    handle,
                    e
                );
                return;
            }
        };

        stop_music_playback(&mut audio.music_playback);
        if audio._stream.is_none() {
            return;
        }
        let player = Player::connect_new(&audio.buses.0[BUS_MUSIC as usize].mixer);
        player.append(source);
        audio.music_playback = Some(MusicPlayback { handle, player });
    });
}

/// Target gain for a bus. Slewed sample-side over ~10ms, so a per-frame
/// stepped value from Rock does not zipper. False for an unknown bus.
#[no_mangle]
pub extern "C" fn stone_bus_set_gain(bus: u32, gain: c_float) -> bool {
    with_bus(bus, |b| b.set_gain(gain))
}

#[no_mangle]
pub extern "C" fn stone_bus_samples_pulled(bus: u32) -> u64 {
    read_bus(bus, 0, Bus::samples_pulled)
}

/// Reading clears the hold, so this is single-consumer.
#[no_mangle]
pub extern "C" fn stone_bus_peak(bus: u32) -> c_float {
    read_bus(bus, 0.0, Bus::meter)
}

#[no_mangle]
pub extern "C" fn stone_bus_set_lowpass(bus: u32, cutoff_hz: c_float, q: c_float) -> bool {
    with_bus(bus, |b| b.set_lowpass(cutoff_hz, q))
}

/// Distortion insert. `drive` >= 1 into a soft clipper, `mix` 0..1 against
/// dry -- mix 0 is transparent.
#[no_mangle]
pub extern "C" fn stone_bus_set_distortion(bus: u32, drive: c_float, mix: c_float) -> bool {
    with_bus(bus, |b| b.set_distortion(drive, mix))
}

#[no_mangle]
pub extern "C" fn stone_bus_set_reverb(
    bus: u32,
    enabled: bool,
    room_size: c_float,
    damping: c_float,
    wet: c_float,
    dry: c_float,
    width: c_float,
) -> bool {
    with_bus(bus, |b| {
        b.set_reverb(enabled, room_size, damping, wet, dry, width)
    })
}

#[no_mangle]
pub extern "C" fn stone_bus_duck(
    bus: u32,
    depth: c_float,
    attack_secs: c_float,
    release_secs: c_float,
) -> bool {
    with_bus(bus, |b| b.duck(depth, attack_secs, release_secs))
}

fn with_bus(bus: u32, f: impl FnOnce(&Bus)) -> bool {
    read_bus(bus, false, |b| {
        f(b);
        true
    })
}

fn read_bus<T>(bus: u32, unknown: T, f: impl FnOnce(&Bus) -> T) -> T {
    with_audio(|audio| audio.buses.get(bus).map(f)).unwrap_or(unknown)
}

fn with_sound<T>(handle: u64, f: impl FnOnce(&SoundData) -> T) -> Option<T> {
    with_audio(|audio| audio.sounds.get(&handle).map(f))
}

fn with_voice<T>(voice: u64, f: impl FnOnce(&Voice) -> T) -> Option<T> {
    with_audio(|audio| audio.voices.get(&voice).map(f))
}

fn with_audio<T>(f: impl FnOnce(&AudioState) -> Option<T>) -> Option<T> {
    AUDIO_STATE.with(|state| state.borrow().as_ref().and_then(f))
}

fn with_audio_mut<T>(f: impl FnOnce(&mut AudioState) -> T) -> Option<T> {
    AUDIO_STATE.with(|state| state.borrow_mut().as_mut().map(f))
}

#[no_mangle]
pub extern "C" fn stone_set_reverb(
    enabled: bool,
    room_size: c_float,
    damping: c_float,
    wet: c_float,
    dry: c_float,
    width: c_float,
) {
    stone_bus_set_reverb(BUS_MASTER, enabled, room_size, damping, wet, dry, width);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::buffer::SamplesBuffer;

    #[test]
    fn lowpass_silence_does_not_retain_subnormal_state() {
        let mut filter = Biquad::new();
        filter.set_lowpass(21_600.0, 0.707, 48_000.0);
        assert!(filter.process(0, 1.0).abs() > 0.1);
        for _ in 0..48_000 {
            filter.process(0, 0.0);
        }
        assert_eq!(filter.state[0], [0.0, 0.0]);
        assert_eq!(filter.process(0, 0.0), 0.0);
    }

    #[test]
    fn stream_errors_are_counted_without_flooding_the_log() {
        if delegate_decoded_audio_fixture(
            "audio::tests::stream_errors_are_counted_without_flooding_the_log",
        ) {
            return;
        }
        rock_log::init();
        let before = rock_log::records_total(::log::Level::Error);
        let mut errors = AudioStreamErrors::new("fixture output".into(), std::time::Duration::MAX);
        for _ in 0..10_000 {
            errors.report(rodio::cpal::Error::new(
                rodio::cpal::ErrorKind::DeviceNotAvailable,
            ));
        }
        assert_eq!(
            rock_log::records_total(::log::Level::Error) - before,
            1,
            "a persistent device error emits once before its reporting interval"
        );
        errors.report(rodio::cpal::Error::new(rodio::cpal::ErrorKind::Xrun));
        assert_eq!(
            rock_log::records_total(::log::Level::Error) - before,
            2,
            "a different error kind is visible immediately"
        );
        drop(errors);
        let records = rock_log::records(::log::Level::Error);
        assert!(
            records.iter().any(|record| record.target == "AUDIO"
                && record.message.contains("fixture output")
                && record.message.contains("10000 occurrences; stream closed")),
            "closing the stream reports the final suppressed count"
        );
        assert_eq!(rock_log::records_total(::log::Level::Error) - before, 3);
    }

    #[test]
    fn automated_runs_default_to_device_free_audio() {
        const CHILD: &str = "STONE_AUDIO_DEFAULT_WITNESS";
        if let Ok(expected) = std::env::var(CHILD) {
            let (no_device, stub) = (expected.starts_with('1'), expected.ends_with('1'));
            assert_eq!(audio_device_disabled(), no_device, "device selection");
            assert_eq!(
                stone_audio_stub_active(),
                stub,
                "decoding stays real unless explicitly stubbed"
            );
            if no_device {
                assert!(stone_audio_init());
                AUDIO_STATE
                    .with(|state| assert!(state.borrow().as_ref().unwrap()._stream.is_none()));
                stone_audio_quit();
            }
            return;
        }
        let cases = &[
            (&[] as &[(&str, &str)], false, false),
            (&[("STONE_HEADLESS", "1")], true, false),
            (&[("PERF_MODE", "1")], true, false),
            (&[("ROCK_RUN_SIM", "fixture")], true, false),
            (&[("ROCK_JOB_ID", "fixture")], true, false),
            (&[("FRAME_COUNT", "1")], true, false),
            (&[("FRAME_COUNT", "0")], false, false),
            (&[("FRAME_COUNT", "-1")], false, false),
            (&[("STONE_HEADLESS", "0"), ("PERF_MODE", "0")], false, false),
            (&[("STONE_AUDIO_STUB", "1")], true, true),
            (
                &[("STONE_HEADLESS", "1"), ("STONE_QUIET", "0")],
                true,
                false,
            ),
            (
                &[("STONE_HEADLESS", "1"), ("STONE_AUDIO_STUB", "0")],
                false,
                false,
            ),
            (
                &[("PERF_MODE", "1"), ("STONE_AUDIO_STUB", "0")],
                false,
                false,
            ),
            (
                &[("ROCK_RUN_SIM", "fixture"), ("STONE_AUDIO_STUB", "0")],
                false,
                false,
            ),
            (
                &[("ROCK_JOB_ID", "fixture"), ("STONE_AUDIO_STUB", "0")],
                false,
                false,
            ),
            (
                &[("FRAME_COUNT", "1"), ("STONE_AUDIO_STUB", "0")],
                false,
                false,
            ),
        ];
        for (env, no_device, stub) in cases {
            let mut cmd = crate::test_support::child_test(
                "audio::tests::automated_runs_default_to_device_free_audio",
                CHILD,
                format!("{}{}", *no_device as u8, *stub as u8),
            );
            for key in [
                "STONE_AUDIO_STUB",
                "STONE_HEADLESS",
                "STONE_QUIET",
                "PERF_MODE",
                "ROCK_RUN_SIM",
                "ROCK_JOB_ID",
                "FRAME_COUNT",
            ] {
                cmd.env_remove(key);
            }
            crate::test_support::assert_child_passes(
                cmd.envs(env.iter().copied()),
                &format!("{env:?}"),
            );
        }
    }
    struct AudioStateRestore(Option<AudioState>);

    impl Drop for AudioStateRestore {
        fn drop(&mut self) {
            AUDIO_STATE.with(|state| *state.borrow_mut() = self.0.take());
        }
    }

    fn install_audio_state(audio: AudioState) -> AudioStateRestore {
        let previous = AUDIO_STATE.with(|state| state.replace(Some(audio)));
        AudioStateRestore(previous)
    }

    // Runs these fixtures in a fresh process: no environment mutation under parallel tests
    // and no output device.
    fn delegate_decoded_audio_fixture(test: &str) -> bool {
        const CHILD: &str = "STONE_DECODE_FIXTURE";
        if std::env::var(CHILD).as_deref() == Ok(test) {
            return false;
        }
        crate::test_support::assert_child_passes(
            crate::test_support::child_test(test, CHILD, test).env("STONE_AUDIO_STUB", "0"),
            test,
        );
        true
    }

    fn mono_source(samples: Vec<f32>, looped: bool) -> SoundSource {
        SoundSource {
            samples: Arc::new(samples),
            sample_rate: 1000,
            channels: 1,
            position: 0,
            looped,
        }
    }

    fn pcm(samples: Vec<f32>, sample_rate: u32, channels: u16) -> Loaded {
        Loaded {
            payload: Payload::Pcm(Arc::new(samples)),
            sample_rate,
            channels,
        }
    }

    fn empty_audio_state() -> AudioState {
        AudioState::new(None, Buses::detached(ch2(), sr()).0)
    }

    fn ch2() -> ChannelCount {
        NonZero::new(2u16).unwrap()
    }
    fn sr() -> SampleRate {
        NonZero::new(44100u32).unwrap()
    }
    fn wired() -> (Bus, impl Iterator<Item = f32>) {
        let (parent, out) = rodio::mixer::mixer(ch2(), sr());
        let (bus, source) = Bus::new(ch2(), sr());
        parent.add(source);
        (bus, out)
    }
    fn blip() -> SamplesBuffer {
        SamplesBuffer::new(ch2(), sr(), vec![1.0f32, 1.0])
    }
    /// Always pulls exactly `n` samples -- must NOT short-circuit, or the
    /// source under test never actually runs dry and the drain is a no-op.
    fn heard(out: &mut impl Iterator<Item = f32>, n: usize) -> bool {
        let mut any = false;
        for _ in 0..n {
            if let Some(s) = out.next() {
                if s != 0.0 {
                    any = true;
                }
            }
        }
        any
    }

    #[test]
    fn deferred_sound_recipe_has_duration_without_pcm() {
        let samples = vec![0.0, 0.2, -0.4, 0.8];
        let oracle = rodio::buffer::SamplesBuffer::new(
            ch2(),
            SampleRate::new(22_050).unwrap(),
            samples.clone(),
        );
        let expected_duration = oracle.total_duration().unwrap().as_secs_f32();
        let sound = SoundData::recipe("fixture.wav".to_owned(), 2, 22_050, 2, false);
        assert!(sound.samples().is_none(), "a recipe must not allocate PCM");
        assert!(
            (sound.duration_seconds() - expected_duration).abs() < f32::EPSILON,
            "duration comes from decoded frame metadata"
        );
    }

    #[test]
    fn decoded_sound_admission_publishes_one_arc() {
        let mut sound = SoundData::recipe("fixture.ogg".to_owned(), 2, 44_100, 1, false);
        assert!(sound.publish(pcm(vec![0.25, -0.5], 44_100, 1)));
        let published = sound.samples().unwrap().clone();
        assert!(
            !sound.publish(pcm(vec![1.0, 1.0], 22_050, 1)),
            "a second completion cannot replace an admitted buffer"
        );
        assert!(
            Arc::ptr_eq(&published, sound.samples().unwrap()),
            "the resident handle keeps the one Arc published by the winning decode"
        );
    }

    struct FixtureSoundRestore(u64);

    impl Drop for FixtureSoundRestore {
        fn drop(&mut self) {
            assert!(
                stone_unload_sound(self.0),
                "fixture cleanup retires its native source"
            );
        }
    }

    #[test]
    fn resident_sound_peaks_partition_pcm_into_known_buckets() {
        let mut audio = empty_audio_state();
        let mut sound = SoundData::recipe("fixture.ogg".to_owned(), 7, 48_000, 1, false);
        assert!(sound.publish(pcm(
            vec![0.2, -0.91, 0.1, -0.75, 0.3, -0.4, 0.67],
            48_000,
            1
        )));
        audio.sounds.insert(0x57, sound);
        let _restore = install_audio_state(audio);

        let mut peaks = [0.0f32; 3];
        let written =
            unsafe { stone_get_sound_peaks(0x57, peaks.as_mut_ptr(), peaks.len() as i32) };
        assert_eq!(
            written, 3,
            "the resident catalog sound fills each requested bucket"
        );
        assert_eq!(
            peaks,
            [0.91, 0.75, 0.67],
            "the known attack, middle and final sample spans set the independent envelope"
        );
    }

    #[test]
    fn bus_survives_going_silent_where_a_bare_submixer_goes_deaf() {
        let (parent, mut out) = rodio::mixer::mixer(ch2(), sr());
        let (child, child_src) = rodio::mixer::mixer(ch2(), sr());
        parent.add(child_src);
        child.add(blip());
        assert!(heard(&mut out, 64), "control: child is audible at first");
        child.add(blip());
        assert!(
            !heard(&mut out, 64),
            "control: a bare sub-mixer that survives running dry tests nothing below"
        );

        let (bus, mut out) = wired();
        assert_eq!(bus.samples_pulled(), 0);
        bus.mixer.add(blip());
        assert!(heard(&mut out, 64), "bus should be audible at first");
        bus.mixer.add(blip());
        assert!(heard(&mut out, 64), "bus went deaf after falling silent");
        assert_eq!(bus.samples_pulled(), 128, "every parent pull is a bus pull");
    }

    /// Stereo Nyquist: each channel alternates +1/-1, the worst case a
    /// lowpass should crush.
    fn nyquist(frames: usize) -> SamplesBuffer {
        let mut v = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let s = if i % 2 == 0 { 1.0f32 } else { -1.0 };
            v.push(s);
            v.push(s);
        }
        SamplesBuffer::new(ch2(), sr(), v)
    }

    /// Loaded sounds live for the process, so the decode must not keep the
    /// collect's growth slack; the samples must equal a plain collect.
    #[test]
    fn decoded_sound_samples_are_exact_length() {
        let bytes = include_bytes!("../../packages/stone/tests/fixtures/tone.ogg").to_vec();
        let reference: Vec<f32> = Decoder::new(std::io::Cursor::new(bytes.clone()))
            .expect("fixture decodes")
            .collect();
        let (samples, _, _) = decode_sound_bytes(bytes, "tone.ogg").expect("fixture decodes");
        assert!(!samples.is_empty());
        assert_eq!(
            samples.capacity(),
            samples.len(),
            "retained PCM keeps spare capacity"
        );
        assert_eq!(
            samples.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
            reference.iter().map(|s| s.to_bits()).collect::<Vec<_>>()
        );
    }

    fn dc(frames: usize, level: f32) -> SamplesBuffer {
        SamplesBuffer::new(ch2(), sr(), vec![level; frames * 2])
    }

    /// Stereo square wave; `period` frames per cycle, so 8 is ~5.5kHz at 44.1k
    /// -- high, but inside the band a wide-open lowpass should pass.
    fn square(frames: usize, period: usize) -> SamplesBuffer {
        let mut v = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let s = if (i % period) < period / 2 {
                1.0f32
            } else {
                -1.0
            };
            v.push(s);
            v.push(s);
        }
        SamplesBuffer::new(ch2(), sr(), v)
    }

    /// Mean |sample| over `n` pulls, after discarding `settle` pulls so slewed
    /// params have arrived.
    fn mean_abs(out: &mut impl Iterator<Item = f32>, settle: usize, n: usize) -> f32 {
        for _ in 0..settle {
            let _ = out.next();
        }
        let mut sum = 0.0;
        for _ in 0..n {
            sum += out.next().unwrap_or(0.0).abs();
        }
        sum / n as f32
    }

    /// Steady-state level of `source` through a bus configured by `configure`.
    fn bus_level(source: SamplesBuffer, configure: impl FnOnce(&Bus)) -> f32 {
        let (bus, mut out) = wired();
        bus.mixer.add(source);
        configure(&bus);
        mean_abs(&mut out, 8192, 8192)
    }

    #[test]
    fn lowpass_passes_highs_when_open_and_crushes_them_when_closed() {
        let open = bus_level(square(40_000, 8), |_| {});
        assert!(
            open > 0.7,
            "wide-open lowpass should pass 5.5kHz, got {open}"
        );
        let closed = bus_level(square(40_000, 8), |bus| bus.set_lowpass(400.0, 0.707));
        assert!(
            closed < 0.05,
            "400Hz lowpass should crush 5.5kHz, got {closed}"
        );
        let nyquist = bus_level(nyquist(40_000), |_| {});
        assert!(
            nyquist < 0.01,
            "Nyquist sits above max cutoff, got {nyquist}"
        );
    }

    /// Distortion must actually change the signal, and soft-clipping must not
    /// let it run away above unity.
    #[test]
    fn distortion_clips_without_exploding() {
        let level = bus_level(dc(40_000, 0.25), |bus| bus.set_distortion(8.0, 1.0));
        assert!(level > 0.3, "drive should lift a quiet signal, got {level}");
        assert!(level <= 1.0, "soft clip must not exceed unity, got {level}");
    }

    /// Linear ramps, so the timing is exact and assertable -- which is the
    /// point of an event-driven ducker over a signal-domain compressor.
    #[test]
    fn duck_attacks_then_releases() {
        let (bus, mut out) = wired();
        bus.mixer.add(dc(400_000, 1.0));

        let full = mean_abs(&mut out, 2048, 256);
        assert!(full > 0.9, "undicked bus should be near unity, got {full}");

        bus.duck(0.2, 0.010, 0.100);
        let attack_samples = (0.010 * 44100.0) as usize * 2;
        let ducked = mean_abs(&mut out, attack_samples, 256);
        assert!(
            (ducked - 0.2).abs() < 0.05,
            "should have reached duck depth 0.2, got {ducked}"
        );

        let release_samples = (0.100 * 44100.0) as usize * 2;
        let recovered = mean_abs(&mut out, release_samples, 256);
        assert!(
            recovered > 0.9,
            "should have recovered to unity after release, got {recovered}"
        );
    }

    /// Reverb lives on the bus, not per voice, so it must ring on after its
    /// input has stopped; without it the bus decays to silence.
    #[test]
    fn bus_reverb_rings_after_input_stops() {
        let tail = |reverb: bool| {
            let (bus, mut out) = wired();
            if reverb {
                bus.set_reverb(true, 0.9, 0.2, 0.9, 0.1, 1.0);
            }
            bus.mixer.add(dc(64, 1.0));
            mean_abs(&mut out, 512, 4096)
        };
        let ringing = tail(true);
        assert!(
            ringing > 0.0001,
            "reverb tail should outlast its input, got {ringing}"
        );
        let dry = tail(false);
        assert!(
            dry < 1e-9,
            "no reverb: bus should decay to silence, got {dry}"
        );
    }

    #[test]
    fn bus_peak_is_post_fader() {
        let (bus, mut out) = wired();
        bus.mixer.add(dc(400_000, 1.0));

        let steady_meter = |out: &mut dyn Iterator<Item = f32>| {
            for _ in 0..(1.5 * 44100.0) as usize * 2 {
                let _ = out.next();
            }
            bus.meter()
        };

        let open = steady_meter(&mut out);
        assert!(
            (open - 1.0).abs() < 0.05,
            "unity bus should meter at 1.0, got {open}"
        );

        bus.set_gain(0.25);
        let faded = steady_meter(&mut out);
        assert!(
            (faded - 0.25).abs() < 0.05,
            "meter should follow the fader, got {faded}"
        );
    }

    #[test]
    fn bus_meter_falls_after_signal_stops() {
        let (bus, mut out) = wired();
        bus.mixer.add(dc(64, 1.0));
        let _ = mean_abs(&mut out, 512, 1);

        let lit = bus.meter();
        assert!(lit > 0.9, "meter should catch the blip, got {lit}");
        assert_eq!(
            bus.meter(),
            lit,
            "reading the meter between callbacks must not consume it"
        );

        let _ = mean_abs(&mut out, (0.25 * 44100.0) as usize * 2, 1);
        let falling = bus.meter();
        assert!(
            falling < lit * 0.8,
            "meter should be falling, got {falling} from {lit}"
        );
        assert!(
            falling > 0.05,
            "and not have dropped straight out, got {falling}"
        );

        let _ = mean_abs(&mut out, (3.0 * 44100.0) as usize * 2, 1);
        let dark = bus.meter();
        assert!(dark < 0.001, "meter should decay to dark, got {dark}");
    }

    /// Gain is slewed, so it must converge on the target rather than step to
    /// it -- a stepped gain zippers.
    #[test]
    fn bus_gain_slews_to_target() {
        let (bus, mut out) = wired();
        bus.mixer.add(dc(20_000, 1.0));

        for _ in 0..2048 {
            let _ = out.next();
        }

        bus.set_gain(0.0);
        let first = out.next().expect("bus is infinite");
        assert!(first > 0.9, "gain should start open, got {first}");

        for _ in 0..64 {
            let _ = out.next();
        }
        let mid = out.next().expect("bus is infinite");
        assert!(
            mid < first && mid > 0.0,
            "expected mid-slew value, got {mid}"
        );

        for _ in 0..4096 {
            let _ = out.next();
        }
        let settled = out.next().expect("bus is infinite");
        assert!(
            settled < 0.01,
            "gain should have settled near 0, got {settled}"
        );
    }

    #[test]
    fn play_params_clamp_gain_and_bound_speed() {
        assert!((sound_play_params(0.4, 1.0).0 - 0.4).abs() < 1e-6);
        assert_eq!(sound_play_params(-0.3, 1.0).0, 0.0);
        for speed in [0.0, -1.0, f32::MIN] {
            let (_, ratio) = sound_play_params(1.0, speed);
            assert!(ratio >= 0.05, "speed {speed} -> ratio {ratio}");
        }
        assert_eq!(sound_play_params(1.0, 500.0).1, 8.0);
    }

    fn panned(source: SamplesBuffer, pan: Option<f32>) -> Vec<f32> {
        let pan = pan.map(|pan| Arc::new(AtomicU32::new(pan.to_bits())));
        voice_source(source, 1.0, 0.0, pan).collect()
    }

    #[test]
    fn voice_panning_keeps_centered_stereo_and_downmixes_positional_voices() {
        let stereo = SamplesBuffer::new(ch2(), sr(), vec![1.0f32, 0.25, 0.5, 0.75]);
        assert_eq!(panned(stereo, None), vec![1.0, 0.25, 0.5, 0.75]);

        let stereo = SamplesBuffer::new(ch2(), sr(), vec![1.0f32, 0.0, 0.0, 1.0]);
        assert_eq!(panned(stereo.clone(), Some(-1.0)), vec![0.5, 0.0, 0.5, 0.0]);
        assert_eq!(panned(stereo, Some(1.0)), vec![0.0, 0.5, 0.0, 0.5]);

        let mono = SamplesBuffer::new(NonZero::new(1u16).unwrap(), sr(), vec![1.0f32]);
        let center = panned(mono, Some(0.0));
        assert_eq!(center.len(), 2);
        assert!(
            (center[0] - center[1]).abs() < 1e-6,
            "center pan must be balanced: {center:?}"
        );
        assert!(
            (center[0] * center[0] + center[1] * center[1] - 1.0).abs() < 1e-6,
            "center pan must preserve power: {center:?}"
        );
    }

    #[test]
    fn positional_voice_changes_pan_without_restarting() {
        let pan = Arc::new(AtomicU32::new((-1.0f32).to_bits()));
        let source = SamplesBuffer::new(NonZero::new(1u16).unwrap(), sr(), vec![1.0f32, 0.5]);
        let mut out = SpatialVoiceSource::new(Box::new(source), pan.clone());
        assert_eq!(out.next(), Some(1.0));
        pan.store(1.0f32.to_bits(), Ordering::Relaxed);
        assert_eq!(out.next(), Some(0.0), "one stereo frame uses one pan");
        assert_eq!(out.next(), Some(0.0));
        assert_eq!(
            out.next(),
            Some(0.5),
            "the next frame uses the new pan and next sample"
        );
    }

    #[test]
    fn decay_fades_to_silence_over_output_time_not_source_time() {
        use rodio::Source;

        let shaped = shaped_source(mono_source(vec![1.0; 1000], false), 0.25, 0.5);
        let out_rate = shaped.sample_rate().get() as f32;
        let secs = shaped.count() as f32 / out_rate;
        assert!(
            (secs - 0.5).abs() < 0.02,
            "expected ~0.5s of playback, got {secs}s at {out_rate} Hz"
        );

        let out: Vec<f32> = shaped_source(mono_source(vec![1.0; 1000], false), 1.0, 0.5).collect();
        assert!(out[0] > 0.9, "starts at full amplitude, got {}", out[0]);
        let last = out[out.len() - 1];
        assert!(last < 0.05, "ends silent, got {last}");
    }

    #[test]
    fn looped_playback_repeats_and_one_shot_ends() {
        let looped: Vec<f32> = shaped_source(mono_source(vec![0.25, 0.75], true), 1.0, 0.0)
            .take(6)
            .collect();
        assert_eq!(looped, vec![0.25, 0.75, 0.25, 0.75, 0.25, 0.75]);

        let once: Vec<f32> =
            shaped_source(mono_source(vec![0.25, 0.75], false), 1.0, 0.0).collect();
        assert_eq!(once, vec![0.25, 0.75]);
    }

    /// 16-bit stereo WAV of a whole number of periods: a sine on the left and
    /// a cosine on the right, so the loop point sits at a full-scale sample.
    fn synthetic_loop_wav(frames: usize, period: usize, amplitude: f32) -> Vec<u8> {
        let rate = 44_100u32;
        let data_len = (frames * 2 * 2) as u32;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for frame in 0..frames {
            let phase = std::f32::consts::TAU * frame as f32 / period as f32;
            for value in [phase.sin(), phase.cos()] {
                wav.extend_from_slice(
                    &((value * amplitude * 32767.0).round() as i16).to_le_bytes(),
                );
            }
        }
        wav
    }

    fn looped_stream(bytes: Vec<u8>, decoded: &[f32], passes: usize) -> Vec<f32> {
        let stream = stream_source(Arc::from(bytes), true).expect("the fixture streams");
        let streamed: Vec<f32> = stream.take(decoded.len() * passes + 6).collect();
        assert_eq!(
            streamed.len(),
            decoded.len() * passes + 6,
            "a looped stream never ends"
        );
        for (index, sample) in streamed.iter().enumerate() {
            assert_eq!(
                *sample,
                decoded[index % decoded.len()],
                "streamed sample {index} matches the decoded loop it replaces"
            );
        }
        streamed
    }

    #[test]
    fn stream_loop_seam_repeats_the_decoded_loop_without_a_click() {
        const FRAMES: usize = 1000;
        const PERIOD: usize = 100;
        const AMPLITUDE: f32 = 0.5;
        let wav = synthetic_loop_wav(FRAMES, PERIOD, AMPLITUDE);
        let (decoded, _, channels) = decode_sound_bytes(wav.clone(), "synthetic-loop.wav")
            .expect("the synthetic loop decodes");
        assert_eq!(
            (decoded.len(), channels),
            (FRAMES * 2, 2),
            "the full decode holds exactly the authored loop"
        );
        let passes = 3;
        let streamed = looped_stream(wav, &decoded, passes);
        let max_step = AMPLITUDE * std::f32::consts::TAU / PERIOD as f32 + 2.0 / 32768.0;
        for pass in 1..passes {
            let seam = pass * decoded.len();
            for channel in 0..2 {
                let step = (streamed[seam + channel] - streamed[seam - 2 + channel]).abs();
                assert!(step <= max_step,
                    "loop seam {pass} channel {channel} steps {step}, over the waveform's {max_step}");
            }
        }
    }

    #[test]
    fn stream_loop_of_a_vorbis_source_repeats_its_full_decode() {
        let bytes = include_bytes!("../../packages/stone/tests/fixtures/tone.ogg").to_vec();
        let (decoded, _, _) = decode_sound_bytes(bytes.clone(), "tone.ogg")
            .expect("the shipped Vorbis fixture decodes");
        looped_stream(bytes, &decoded, 2);
    }

    #[test]
    fn stream_admission_holds_the_encoded_file_without_pcm() {
        if delegate_decoded_audio_fixture(
            "audio::tests::stream_admission_holds_the_encoded_file_without_pcm",
        ) {
            return;
        }
        let _audio_restore = install_audio_state(empty_audio_state());
        let path = format!(
            "{}/../packages/stone/tests/fixtures/tone.ogg",
            env!("CARGO_MANIFEST_DIR")
        );
        let file_bytes = std::fs::metadata(&path)
            .expect("the shipped fixture exists")
            .len() as i64;
        let c_path = std::ffi::CString::new(path.as_str()).unwrap();
        let handle = stone_load_sound_stream(c_path.as_ptr(), 0, 0, 0);
        assert_ne!(handle, 0, "the stream recipe gets a native handle");
        let _sound_restore = FixtureSoundRestore(handle);
        assert!(
            !stone_sound_playable(handle),
            "a stream recipe holds nothing before admission"
        );
        assert_eq!(stone_sound_stream_bytes(handle), 0);

        assert!(stone_sound_admit(handle));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !stone_sound_playable(handle) && std::time::Instant::now() < deadline {
            stone_service_sound_admissions();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            stone_sound_playable(handle),
            "admission publishes the encoded file"
        );
        assert!(
            !stone_sound_resident(handle),
            "an admitted stream holds no PCM"
        );
        assert_eq!(
            stone_sound_sample_count(handle),
            0,
            "an admitted stream holds no samples"
        );
        assert_eq!(
            stone_sound_stream_bytes(handle),
            file_bytes,
            "the stream holds exactly its encoded file"
        );
        assert_eq!(
            stone_sound_decode_sync(handle),
            -1,
            "a stream never decodes to PCM"
        );
        assert!(!stone_sound_resident(handle));
    }

    /// The env conditions `stone_quiet_init` applies, so the native latch and
    /// the Rock gate agree about which processes have no listener.
    #[test]
    fn output_mute_latches_on_the_same_env_the_rock_gate_reads() {
        let cases = [
            (None, None, None, false),
            (Some("1"), None, None, true),
            (None, Some("1"), None, true),
            (Some("0"), Some("1"), None, false),
            (Some("0"), None, Some("some_sim"), true),
            (None, None, Some("some_sim"), true),
        ];
        for (quiet, headless, run_sim, muted) in cases {
            assert_eq!(
                output_muted_from(quiet, headless, run_sim),
                muted,
                "quiet={quiet:?} headless={headless:?} sim={run_sim:?}"
            );
        }
    }

    #[test]
    fn a_muted_output_carries_no_signal_but_still_drains_its_bus() {
        let heard_through = |muted: bool| {
            let (mixer, mut out) = rodio::mixer::mixer(ch2(), sr());
            let buses = Buses::attached(&mixer, ch2(), sr(), muted);
            let (master, sfx) = (&buses.0[0], &buses.0[1]);
            sfx.mixer
                .add(SamplesBuffer::new(ch2(), sr(), vec![1.0f32; 256]));
            let any = heard(&mut out, 256);
            (
                any,
                master.samples_pulled(),
                sfx.samples_pulled(),
                sfx.meter(),
            )
        };

        let (live_audible, live_master_pulled, live_sfx_pulled, _) = heard_through(false);
        assert!(
            live_audible,
            "control: an unmuted output carries the signal"
        );

        let (muted_audible, muted_master_pulled, muted_sfx_pulled, muted_meter) =
            heard_through(true);
        assert!(!muted_audible, "a muted output carries no signal");
        assert_eq!(
            muted_master_pulled, live_master_pulled,
            "the muted output still pulls master"
        );
        assert_eq!(
            muted_sfx_pulled, live_sfx_pulled,
            "the muted output still drains its child bus"
        );
        assert!(muted_master_pulled > 0 && muted_sfx_pulled > 0);
        assert!(
            muted_meter > 0.0,
            "and the meter still reads the real mix, which sims assert on"
        );
    }
}
