use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, c_float, CStr};
use std::num::NonZero;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::handles::Class;
use freeverb::Freeverb;
use rodio::{
    ChannelCount, Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, SampleRate, Source,
};

/// `STONE_AUDIO_STUB=1` registers sounds WITHOUT decoding them: the handle is
/// real and everything downstream keeps working, but the buffer is empty, so a
/// play is silence that ends immediately.
///
/// Decoding is the whole cost here: mag's `mag_sfx_init` is ~100ms of Vorbis.
/// The sim runner selects it explicitly for every sim child.
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

/// Whether this process must not be heard, latched once from the environment.
///
/// `STONE_QUIET` and `audio_gate_muted()` are Rock-side and only advisory: they
/// are checked at the compat call site, while `stone_play_sound`,
/// `stone_play_music` and `stone_voice_set_gain` each drive a real device
/// whenever one is open. A fixture that needs live voices for its assertions
/// opens that gate -- mag's `ambience_region_switch` sets `STONE_QUIET = false`
/// so `Voice.active()` has something to answer about -- and with
/// `STONE_AUDIO_STUB=0` the ambience it starts was really audible during
/// `rock sim mag`. So the suppression a sim cannot defeat sits on the OUTPUT,
/// below every gate Rock can reach, and is read once rather than per play.
///
/// It mutes the output and nothing else. Every value a sim asserts on -- bus
/// gain, lowpass, reverb, duck, the meter envelope, `samples_pulled`, voice
/// liveness -- is computed upstream of the muted stage and is unchanged, which
/// is the whole point: silencing by not attaching master would answer "no
/// sound" and leave master at 0 pulls forever.
///
/// Nothing is set in a shipped run, so the stage is never installed there and
/// the mixer graph is the one it has always been.
///
/// This is the ONE place the condition is computed. `stone_quiet_init` in
/// `audio/playback.rock` seeds the Rock gate from it through
/// `stone_audio_quiet_env` rather than re-reading the environment, so a fourth
/// condition added here reaches both answers and they cannot drift apart. The
/// two are still used differently -- Rock's `STONE_QUIET` is a mutable global
/// that games also drive from their own settings, while this latch is what no
/// Rock code can reopen -- but they start from one reading.
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

/// The decision itself, separated from the lookup so it is testable without
/// mutating the process environment out from under a parallel test.
///
/// An explicit `STONE_QUIET=0` opts a deliberate headless audio test back in,
/// and a sim is quiet whatever it asks for, because `ROCK_RUN_SIM` is the
/// runner's variable and not a fixture's.
///
/// The opt-in is honoured ONCE and reaches both consequences from that one
/// reading: false here makes `stone_audio_quiet_env` answer false, which leaves
/// Rock's `STONE_QUIET` false so the compat gate passes plays through to the
/// native calls, AND makes `Buses::new` attach master without the zero-gain
/// stage so those plays are really audible. That is what unifying bought. While
/// the condition was spelled in both places the opt-in had to be honoured
/// twice, and nothing made the two honour it the same way.
fn output_muted_from(quiet: Option<&str>, headless: Option<&str>, run_sim: Option<&str>) -> bool {
    quiet == Some("1") || (quiet.is_none() && headless == Some("1")) || run_sim.is_some()
}

/// The latch Rock's `STONE_QUIET` is seeded from, so the gate and the output
/// mute are one reading of one condition rather than two spellings of it.
///
/// Latched, so it answers the environment the process STARTED with. That is
/// what a backstop has to do: a fixture is free to call `os_setenv` mid-run,
/// and that must not be able to buy an audible output.
#[no_mangle]
pub extern "C" fn stone_audio_quiet_env() -> bool {
    output_muted()
}

/// The format a stubbed run's bus graph mixes in. Nothing drains it, so these
/// decide an internal buffer layout and nothing audible; they match the shape a
/// commodity sink negotiates so the stubbed graph is not a different one.
const STUB_CHANNELS: rodio::ChannelCount = rodio::nz!(2);
const STUB_SAMPLE_RATE: rodio::SampleRate = rodio::nz!(48_000);

/// Proves the asset is there without decoding it. A stub that skipped the
/// lookup entirely would make a deleted or renamed sound invisible to every
/// sim -- the load would succeed on a path that resolves to nothing.
fn audio_asset_exists(path_str: &str) -> bool {
    match crate::asset_registry::resolve_asset_prefix(path_str, 4) {
        Some(b) if !b.is_empty() => true,
        _ => {
            crate::log_once!("AUDIO", "sound load: {} does not resolve", path_str);
            false
        }
    }
}

#[derive(Clone)]
struct SoundData {
    path: String,
    frames: u64,
    sample_rate: u32,
    channels: u16,
    samples: Option<Arc<Vec<f32>>>,
    /// A stream source admits its encoded file instead of PCM; each voice
    /// decodes it incrementally (`StreamSource`).
    stream: bool,
    encoded: Option<Arc<[u8]>>,
    admitting: bool,
}

impl SoundData {
    fn recipe(path: String, frames: u64, sample_rate: u32, channels: u16) -> Self {
        Self {
            path,
            frames,
            sample_rate,
            channels,
            samples: None,
            stream: false,
            encoded: None,
            admitting: false,
        }
    }

    fn stream_recipe(path: String, frames: u64, sample_rate: u32, channels: u16) -> Self {
        Self {
            stream: true,
            ..Self::recipe(path, frames, sample_rate, channels)
        }
    }

    /// Admitted: a play needs no decode or file read on the game thread.
    fn playable(&self) -> bool {
        self.samples.is_some() || self.encoded.is_some()
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

    fn publish_decoded(&mut self, samples: Vec<f32>, sample_rate: u32, channels: u16) -> bool {
        if self.stream
            || self.samples.is_some()
            || samples.len() < channels as usize
            || sample_rate == 0
            || channels == 0
        {
            self.admitting = false;
            return false;
        }
        self.frames = (samples.len() / channels as usize) as u64;
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.samples = Some(Arc::new(samples));
        self.admitting = false;
        true
    }

    fn publish_encoded(&mut self, encoded: Arc<[u8]>, sample_rate: u32, channels: u16) -> bool {
        if !self.stream
            || self.encoded.is_some()
            || encoded.is_empty()
            || sample_rate == 0
            || channels == 0
        {
            self.admitting = false;
            return false;
        }
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.encoded = Some(encoded);
        self.admitting = false;
        true
    }

    fn as_source(&self, looped: bool) -> Option<SoundSource> {
        Some(SoundSource {
            samples: self.samples.as_ref()?.clone(),
            sample_rate: self.sample_rate,
            channels: self.channels,
            position: 0,
            looped,
        })
    }
}

enum AdmissionPayload {
    Pcm(Vec<f32>, u32, u16),
    Encoded(Arc<[u8]>, u32, u16),
}

struct SoundAdmissionResult {
    handle: u64,
    path: String,
    reserved_bytes: u64,
    payload: Option<AdmissionPayload>,
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

type EncodedCursor = std::io::Cursor<Arc<[u8]>>;

enum StreamDecoder {
    Once(Decoder<EncodedCursor>),
    Looped(rodio::decoder::LoopedDecoder<EncodedCursor>),
}

/// Incremental decode of a stream source's encoded bytes. A looped decoder
/// rebuilds from the first byte at end of data, so every pass yields exactly
/// the samples a full decode yields and the seam is the authored loop point.
/// `position` counts samples taken, for `stone_voice_stream_frames`.
struct StreamSource {
    decoder: StreamDecoder,
    taken: u64,
    position: Arc<AtomicU64>,
}

impl StreamSource {
    fn new(
        encoded: Arc<[u8]>,
        looped: bool,
        position: Arc<AtomicU64>,
    ) -> Result<Self, rodio::decoder::DecoderError> {
        let cursor = std::io::Cursor::new(encoded);
        let decoder = if looped {
            StreamDecoder::Looped(Decoder::new_looped(cursor)?)
        } else {
            StreamDecoder::Once(Decoder::new(cursor)?)
        };
        Ok(Self {
            decoder,
            taken: 0,
            position,
        })
    }
}

impl Iterator for StreamSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let sample = match &mut self.decoder {
            StreamDecoder::Once(decoder) => decoder.next(),
            StreamDecoder::Looped(decoder) => decoder.next(),
        }?;
        self.taken += 1;
        self.position.store(self.taken, Ordering::Relaxed);
        Some(sample)
    }
}

impl Source for StreamSource {
    fn current_span_len(&self) -> Option<usize> {
        match &self.decoder {
            StreamDecoder::Once(decoder) => decoder.current_span_len(),
            StreamDecoder::Looped(decoder) => decoder.current_span_len(),
        }
    }

    fn channels(&self) -> ChannelCount {
        match &self.decoder {
            StreamDecoder::Once(decoder) => decoder.channels(),
            StreamDecoder::Looped(decoder) => decoder.channels(),
        }
    }

    fn sample_rate(&self) -> SampleRate {
        match &self.decoder {
            StreamDecoder::Once(decoder) => decoder.sample_rate(),
            StreamDecoder::Looped(decoder) => decoder.sample_rate(),
        }
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        match &self.decoder {
            StreamDecoder::Once(decoder) => decoder.total_duration(),
            StreamDecoder::Looped(_) => None,
        }
    }
}

/// Time for a slewed bus parameter to cover ~63% of its distance to target.
/// Long enough to kill zipper noise on a 60Hz-stepped target, short enough
/// that a duck still reads as a duck.
const SLEW_TAU_SECS: f32 = 0.010;

/// Highest cutoff we let the lowpass reach, as a fraction of sample rate. A
/// biquad goes unstable approaching Nyquist, so "off" is cutoff pinned here
/// rather than a bypass branch -- no discontinuity when automation engages it.
/// Not literally transparent: at 44.1kHz this sits at ~19.8kHz, so the top of
/// the audible band still rolls off and Nyquist itself is rejected outright
/// (`lowpass_open_still_rejects_nyquist`). Inaudible for real content.
const LOWPASS_MAX_FRAC: f32 = 0.45;

/// How fast a meter envelope falls once the signal has gone. Slow enough that
/// a transient is still readable on a screen refreshing at 60Hz, fast enough
/// that the meter follows a fade rather than trailing it.
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

fn load_f32(cell: &AtomicU32) -> f32 {
    f32::from_bits(cell.load(Ordering::Relaxed))
}

/// Shared, lock-free bus parameters. Written from the game thread, read on the
/// audio thread -- so atomics, never a Mutex (which is what rodio's own Player
/// controls use, and why we don't ride those).
struct BusParams {
    /// f32 bits. The *target*; BusSource slews the applied value toward it.
    gain: AtomicU32,
    samples_pulled: AtomicU64,

    /// f32 bits. Meter envelope: rises instantly to a new output peak, falls at
    /// METER_FALL_DB_PER_SEC. Maintained sample-side rather than by the reader,
    /// because the device callback and the video frame run on unrelated clocks
    /// -- a reader that did its own ballistics would be doing them on a signal
    /// that reads zero whenever a poll lands between two callbacks. Reading is
    /// a plain load: non-destructive, any number of consumers.
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
        y
    }
}

/// Where a duck currently is. Linear ramps, so the timing is exact and
/// assertable -- the reason to prefer an event-driven ducker over a
/// signal-domain compressor for gameplay.
#[derive(Clone, Copy, PartialEq)]
enum DuckPhase {
    Idle,
    Attack,
    Release,
}

/// A mix group: its own sub-mixer, plus a wrapper that applies slewed gain to
/// the summed result. Voices are added to `mixer()`; the wrapper is handed to
/// a parent bus (or the device mixer) exactly once via `take_source`.
///
/// Bus tree today: every bus feeds `master`, and `master` is the single child
/// of the device sink's mixer. Nesting means no rodio fork patch is needed to
/// get post-mix master effects -- `master` IS the post-mix stage.
/// One bus's game-thread-written state, as one value.
#[derive(Clone, Copy)]
struct BusAmbient {
    gain: f32,
    cutoff: f32,
    q: f32,
    drive: f32,
    mix: f32,
    reverb_enabled: bool,
    room: f32,
    damping: f32,
    wet: f32,
    dry: f32,
    width: f32,
    duck_depth: f32,
    duck_attack: f32,
    duck_release: f32,
}

struct Bus {
    mixer: rodio::mixer::Mixer,
    source: RefCell<Option<BusSource>>,
    params: Arc<BusParams>,
    sample_rate: SampleRate,
}

impl Bus {
    fn new(channels: ChannelCount, sample_rate: SampleRate) -> Self {
        let (mixer, inner) = rodio::mixer::mixer(channels, sample_rate);
        let params = Arc::new(BusParams::new(sample_rate));
        let rate = sample_rate.get() as f32;

        let mut lowpass = Biquad::new();
        lowpass.set_lowpass(max_cutoff(sample_rate), 0.707, rate);

        Bus {
            mixer,
            source: RefCell::new(Some(BusSource {
                inner,
                params: params.clone(),
                channels: channels.get() as usize,
                ch: 0,
                frame: [0.0; MAX_CHANNELS],
                applied_gain: 1.0,
                meter_env: 0.0,
                // Per-frame amplitude factor giving METER_FALL_DB_PER_SEC.
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
            })),
            params,
            sample_rate,
        }
    }

    fn mixer(&self) -> &rodio::mixer::Mixer {
        &self.mixer
    }

    /// Hand the wrapper to a parent. Panics if called twice -- a bus has one
    /// output, and a second call would mean a bus wired into two parents.
    fn take_source(&self) -> BusSource {
        self.source
            .borrow_mut()
            .take()
            .expect("bus source already taken: a bus has exactly one output")
    }

    fn set_gain(&self, gain: f32) {
        store_f32(&self.params.gain, gain.max(0.0));
    }

    /// Current meter envelope. A plain load -- reading it changes nothing.
    fn meter(&self) -> f32 {
        load_f32(&self.params.meter)
    }

    fn samples_pulled(&self) -> u64 {
        self.params.samples_pulled.load(Ordering::Relaxed)
    }

    fn duck_count(&self) -> u64 {
        self.params.duck_gen.load(Ordering::Relaxed)
    }

    fn gain(&self) -> f32 {
        load_f32(&self.params.gain)
    }

    fn lowpass_cutoff(&self) -> f32 {
        load_f32(&self.params.cutoff)
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
        // Release: everything above must be visible before the generation is.
        p.reverb_gen.fetch_add(1, Ordering::Release);
    }

    /// Every value the game thread has written into this bus.
    fn ambient(&self) -> BusAmbient {
        let p = &self.params;
        BusAmbient {
            gain: f32_at(&p.gain),
            cutoff: f32_at(&p.cutoff),
            q: f32_at(&p.q),
            drive: f32_at(&p.distortion_drive),
            mix: f32_at(&p.distortion_mix),
            reverb_enabled: p.reverb_enabled.load(Ordering::Relaxed),
            room: f32_at(&p.reverb_room),
            damping: f32_at(&p.reverb_damping),
            wet: f32_at(&p.reverb_wet),
            dry: f32_at(&p.reverb_dry),
            width: f32_at(&p.reverb_width),
            duck_depth: f32_at(&p.duck_depth),
            duck_attack: f32_at(&p.duck_attack),
            duck_release: f32_at(&p.duck_release),
        }
    }

    /// Duck to `depth` over `attack` seconds, then back to unity over
    /// `release`. Re-triggering restarts the attack, which is what you want
    /// for repeated events (each new hit re-ducks).
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

/// A bus's output. **Infinite by construction**: a rodio `MixerSource` returns
/// `None` once it runs dry, and a parent mixer permanently drops any child
/// that returns `None` -- so an unwrapped sub-mixer would go silently deaf the
/// first moment its last voice ended. Yielding 0.0 instead keeps the bus alive
/// for the next voice. Pinned by `bus_survives_going_silent`.
///
/// Processes a whole interleaved frame at a time: the biquad needs per-channel
/// state and the reverb is inherently stereo, neither of which a
/// sample-at-a-time loop can express. Chain order is
/// **distortion -> lowpass -> reverb -> gain*duck** -- drive, then tone, then
/// space, with the fader last so a duck attenuates the reverb tail too.
///
/// Allocation-free: fixed scratch, no per-call heap, no locks.
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
        // A zero-length stage means "jump", so guard the divide.
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
        // Channels past the scratch still have to be consumed, or the stream
        // would drift out of frame alignment.
        for _ in MAX_CHANNELS..self.channels {
            let _ = self.inner.next();
        }

        let p = &self.params;
        self.applied_gain += (f32_at(&p.gain) - self.applied_gain) * self.slew;
        self.applied_drive += (f32_at(&p.distortion_drive) - self.applied_drive) * self.slew;
        self.applied_mix += (f32_at(&p.distortion_mix) - self.applied_mix) * self.slew;

        // Cutoff slews in log2(Hz): a linear Hz ramp sweeps unevenly to the
        // ear, because pitch perception is logarithmic.
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
        for i in 0..active {
            self.frame[i] *= fader;
            loudest = loudest.max(self.frame[i].abs());
        }

        // Rise instantly, fall at a fixed rate: a meter that fell as slowly as
        // it rose would never show a transient, and one that fell as fast as it
        // rose would show nothing readable. The envelope lives here rather than
        // in the reader because only here is the clock the signal's own.
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
    /// Samples a stream voice has decoded, and its channel count.
    stream_position: Option<(Arc<AtomicU64>, u16)>,
}

struct MusicPlayback {
    handle: u64,
    player: Player,
}

/// Bus ids, mirrored by `STONE_BUS_*` in `stone/engine/lib.rock`. Fixed set --
/// buses are a mixing topology, not content, so there is no dynamic creation.
pub const BUS_MASTER: u32 = 0;
pub const BUS_SFX: u32 = 1;
pub const BUS_MUSIC: u32 = 2;
pub const BUS_AMBIENCE: u32 = 3;
pub const BUS_UI: u32 = 4;
pub const BUS_FOLEY: u32 = 5;

/// The whole tree: sfx/music/ambience/ui/foley feed master, master is the single
/// child of the device sink's mixer.
struct Buses {
    master: Bus,
    sfx: Bus,
    music: Bus,
    ambience: Bus,
    ui: Bus,
    foley: Bus,
}

impl Buses {
    /// `muted` pins the output silent WITHOUT touching the graph: master is
    /// still attached and still pulled, so every bus counter, meter and voice
    /// retires exactly as it does with audio on, and only the samples leaving
    /// for the device are zeroed.
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
        let buses = Buses::detached(channels, rate);
        let master = buses.master.take_source();
        if muted {
            mixer.add(master.amplify(0.0));
        } else {
            mixer.add(master);
        }
        buses
    }

    /// The same bus graph with nothing draining it.
    ///
    /// Every bus parameter -- gain, lowpass, distortion, reverb, duck -- is
    /// CPU-side state a sim entry can write and a successor can read, and none
    /// of it needs a device. Under `STONE_AUDIO_STUB` the graph is built and
    /// simply never attached to a sink, so that state exists and CROSSES
    /// exactly as it does with audio on. The alternative -- no buses at all --
    /// is what made the audio ambient row answer `Absent` for the default sim
    /// configuration, which is a laundering hole wearing the word "absent".
    fn detached(channels: rodio::ChannelCount, rate: rodio::SampleRate) -> Self {
        let master = Bus::new(channels, rate);
        let buses = Buses {
            sfx: Bus::new(channels, rate),
            music: Bus::new(channels, rate),
            ambience: Bus::new(channels, rate),
            ui: Bus::new(channels, rate),
            foley: Bus::new(channels, rate),
            master,
        };

        for leaf in [
            &buses.sfx,
            &buses.music,
            &buses.ambience,
            &buses.ui,
            &buses.foley,
        ] {
            buses.master.mixer().add(leaf.take_source());
        }
        buses
    }

    fn get(&self, id: u32) -> Option<&Bus> {
        match id {
            BUS_MASTER => Some(&self.master),
            BUS_SFX => Some(&self.sfx),
            BUS_MUSIC => Some(&self.music),
            BUS_AMBIENCE => Some(&self.ambience),
            BUS_UI => Some(&self.ui),
            BUS_FOLEY => Some(&self.foley),
            _ => None,
        }
    }
}

struct AudioState {
    /// The output device, or `None` under `STONE_AUDIO_STUB`.
    ///
    /// A stubbed run has a real `AudioState` with no device rather than no
    /// state at all. The difference is not cosmetic: with no state,
    /// `stone_load_sound` minted an id and handed it to Rock while nothing was
    /// inserted, so the `sound` class published an EMPTY live set -- measured
    /// on bitgun's boot as `sound issued=23 live=0 unaccounted=23`. The
    /// identity diff is structurally blind to that, since empty compares equal
    /// to empty; only id accounting could name it.
    ///
    /// What a device gates is PLAYBACK, and only playback. Everything else --
    /// the decoded buffers, the loaded tracks, the volumes, every bus
    /// parameter -- is CPU-side state a sim entry can write and its successor
    /// can read, so it exists either way.
    _stream: Option<MixerDeviceSink>,
    buses: Buses,
    sounds: HashMap<u64, SoundData>,
    pending_sound_admissions: VecDeque<u64>,
    inflight_sound_bytes: u64,
    // Encoded bytes (ogg/etc.), decoded incrementally by each play
    // (`StreamSource`). Held as bytes rather than a path so embedded-asset
    // builds (fire) play music from the registry -- there is no loose file on
    // disk in a shipped install.
    music: HashMap<u64, Arc<[u8]>>,
    music_playback: Option<MusicPlayback>,
    // Every currently-playing voice, keyed by voice id. Voices that ended on
    // their own are pruned on each play_sound call; membership here IS the
    // liveness answer `stone_voice_active` gives (see `voice_active`).
    voices: HashMap<u64, Voice>,
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
    // Preserve rodio's default-device, then non-null fallback-device search,
    // installing the engine callback before opening every attempted stream.
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

        // Device-free runs retain decoding and bus state without negotiating
        // an output stream with the system audio server.
        if audio_device_disabled() {
            // A real state, with no device. The rate and channel count decide
            // only the bus mixers' internal format, and under a stub no sample
            // ever leaves them.
            *state.borrow_mut() = Some(AudioState {
                _stream: None,
                buses: Buses::detached(STUB_CHANNELS, STUB_SAMPLE_RATE),
                sounds: HashMap::new(),
                pending_sound_admissions: VecDeque::new(),
                inflight_sound_bytes: 0,
                music: HashMap::new(),
                music_playback: None,
                voices: HashMap::new(),
            });
            return true;
        }

        let t_open = std::time::Instant::now();
        let opened = open_audio_output();
        ::log::debug!(target: "AUDIO", "open_audio_output {:.0}ms",
            t_open.elapsed().as_secs_f64() * 1000.0);
        match opened {
            Ok(mut stream) => {
                // Silence rodio's "Dropping DeviceSink, audio playing through
                // this sink will stop" warning on program exit. We intentionally
                // drop the sink at shutdown — that's the expected path, not a bug.
                stream.log_on_drop(false);
                let buses = Buses::new(&stream, output_muted());
                *state.borrow_mut() = Some(AudioState {
                    _stream: Some(stream),
                    buses,
                    sounds: HashMap::new(),
                    pending_sound_admissions: VecDeque::new(),
                    inflight_sound_bytes: 0,
                    music: HashMap::new(),
                    music_playback: None,
                    voices: HashMap::new(),
                });
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
        // Dropping the whole state takes three classes' live sets with it, so
        // it retires all three -- a bulk drop leaves the same accounting hole a
        // skipped `remove` would.
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

/// Decode an audio file (asset registry first, then disk) to PCM samples. Shared
/// by load + reload; `None` on failure (no embedded fallback -- callers decide).
fn decode_sound_samples(path_str: &str) -> Option<(Vec<f32>, u32, u16)> {
    let audio_bytes = match crate::asset_registry::resolve_asset(path_str) {
        Some(data) => data,
        None => {
            crate::log_once!("AUDIO", "decode_sound_samples: Failed to read {}", path_str);
            return None;
        }
    };
    decode_sound_bytes(audio_bytes, path_str)
}

fn decode_sound_bytes(audio_bytes: Vec<u8>, path_str: &str) -> Option<(Vec<f32>, u32, u16)> {
    let decoder = match Decoder::new(std::io::Cursor::new(audio_bytes)) {
        Ok(d) => d,
        Err(e) => {
            crate::log_once!(
                "AUDIO",
                "decode_sound_samples: Failed to decode {}: {}",
                path_str,
                e
            );
            return None;
        }
    };

    let sample_rate = decoder.sample_rate().get();
    let channels = decoder.channels().get();
    let mut samples: Vec<f32> = decoder.collect();
    // The buffer is retained for the process lifetime; drop the growth slack.
    samples.shrink_to_fit();

    if samples.is_empty() {
        crate::log_once!("AUDIO", "decode_sound_samples: No samples in {}", path_str);
        return None;
    }

    Some((samples, sample_rate, channels))
}

fn probe_sound_recipe(path_str: &str) -> Option<(u64, u32, u16)> {
    let bytes = crate::asset_registry::resolve_asset(path_str).or_else(|| {
        crate::log_once!(
            "AUDIO",
            "stone_load_sound_deferred: Failed to read {}",
            path_str
        );
        None
    })?;
    let decoder = Decoder::new(std::io::Cursor::new(bytes)).ok().or_else(|| {
        crate::log_once!(
            "AUDIO",
            "stone_load_sound_deferred: Failed to probe {}",
            path_str
        );
        None
    })?;
    let sample_rate = decoder.sample_rate().get();
    let channels = decoder.channels().get();
    let frames = decoder.total_duration().map_or(0, |duration| {
        (duration.as_secs_f64() * sample_rate as f64).round() as u64
    });
    Some((frames, sample_rate, channels))
}

/// Read and validate a stream source's encoded file. The decoder probe checks
/// the format once here so a voice never starts on bytes that cannot decode.
fn load_stream_bytes(path_str: &str) -> Option<(Arc<[u8]>, u32, u16)> {
    let bytes: Arc<[u8]> = match crate::asset_registry::resolve_asset(path_str) {
        Some(data) => data.into(),
        None => {
            crate::log_once!("AUDIO", "stream source: Failed to read {}", path_str);
            return None;
        }
    };
    let decoder = match Decoder::new(std::io::Cursor::new(bytes.clone())) {
        Ok(decoder) => decoder,
        Err(e) => {
            crate::log_once!(
                "AUDIO",
                "stream source: Failed to decode {}: {}",
                path_str,
                e
            );
            return None;
        }
    };
    Some((bytes, decoder.sample_rate().get(), decoder.channels().get()))
}

fn load_sound_recipe(
    path: *const c_char,
    frames: i64,
    sample_rate: i64,
    channels: i64,
    stream: bool,
) -> u64 {
    if path.is_null() {
        return 0;
    }
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
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

    AUDIO_STATE.with(|state| {
        let Some(ref mut audio) = *state.borrow_mut() else {
            crate::log_once!(
                "AUDIO",
                "sound load: no audio state, so `{}` gets no handle",
                path_str
            );
            return 0;
        };
        let id = crate::handles::mint(Class::Sound);
        let path = path_str.to_owned();
        let sound = if stream {
            SoundData::stream_recipe(path, frames, sample_rate, channels)
        } else {
            SoundData::recipe(path, frames, sample_rate, channels)
        };
        audio.sounds.insert(id, sound);
        id
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

/// A looped source that holds its encoded file once admitted and decodes it
/// incrementally per voice; it never holds PCM (`stone_sound_resident` stays
/// false). The recipe is the same metadata a deferred sound carries.
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
        return AUDIO_STATE.with(|state| {
            state
                .borrow()
                .as_ref()
                .is_some_and(|audio| audio.sounds.contains_key(&handle))
        });
    }
    AUDIO_STATE.with(|state| {
        let Some(ref mut audio) = *state.borrow_mut() else {
            return false;
        };
        let Some(sound) = audio.sounds.get_mut(&handle) else {
            return false;
        };
        if sound.playable() || sound.admitting {
            return true;
        }
        sound.admitting = true;
        audio.pending_sound_admissions.push_back(handle);
        true
    })
}

#[no_mangle]
pub extern "C" fn stone_service_sound_admissions() -> i64 {
    let completed: Vec<SoundAdmissionResult> = {
        let mut results = sound_admission_results()
            .lock()
            .expect("sound admissions poisoned");
        results.drain(..).collect()
    };
    let (published, scheduled) = AUDIO_STATE.with(|state| {
        let Some(ref mut audio) = *state.borrow_mut() else {
            return (0, Vec::new());
        };
        let mut published = 0;
        for result in completed {
            audio.inflight_sound_bytes = audio
                .inflight_sound_bytes
                .saturating_sub(result.reserved_bytes);
            let Some(sound) = audio.sounds.get_mut(&result.handle) else {
                continue;
            };
            if sound.path != result.path || !sound.admitting {
                continue;
            }
            match result.payload {
                Some(AdmissionPayload::Pcm(samples, sample_rate, channels)) => {
                    if sound.publish_decoded(samples, sample_rate, channels) {
                        published += 1
                    }
                }
                Some(AdmissionPayload::Encoded(encoded, sample_rate, channels)) => {
                    if sound.publish_encoded(encoded, sample_rate, channels) {
                        published += 1
                    }
                }
                None => {
                    sound.admitting = false;
                    crate::log_once!("AUDIO", "sound admission failed for {}", sound.path);
                }
            }
        }

        let mut scheduled = Vec::new();
        while let Some(handle) = audio.pending_sound_admissions.front().copied() {
            let Some(sound) = audio.sounds.get(&handle) else {
                audio.pending_sound_admissions.pop_front();
                continue;
            };
            if !sound.admitting || sound.playable() {
                audio.pending_sound_admissions.pop_front();
                continue;
            }
            let limit = SOUND_DECODE_IN_FLIGHT_BYTES;
            let stream = sound.stream;
            // The budget bounds decoded PCM; a stream reads only its encoded file.
            let bytes = if stream {
                0
            } else {
                let estimated = sound.estimated_bytes();
                if estimated == 0 {
                    limit
                } else {
                    estimated
                }
            };
            if bytes > 0
                && audio.inflight_sound_bytes.saturating_add(bytes) > limit
                && audio.inflight_sound_bytes > 0
            {
                break;
            }
            let path = sound.path.clone();
            audio.pending_sound_admissions.pop_front();
            audio.inflight_sound_bytes = audio.inflight_sound_bytes.saturating_add(bytes);
            scheduled.push((handle, path, bytes, stream));
        }
        (published, scheduled)
    });

    for (handle, path, reserved_bytes, stream) in scheduled {
        rayon::spawn(move || {
            let payload = if stream {
                load_stream_bytes(&path).map(|(encoded, rate, channels)| {
                    AdmissionPayload::Encoded(encoded, rate, channels)
                })
            } else {
                decode_sound_samples(&path)
                    .map(|(samples, rate, channels)| AdmissionPayload::Pcm(samples, rate, channels))
            };
            let result = SoundAdmissionResult {
                handle,
                path: path.clone(),
                reserved_bytes,
                payload,
            };
            let mut results = sound_admission_results()
                .lock()
                .expect("sound admissions poisoned");
            results.push_back(result);
        });
    }
    published
}

#[no_mangle]
pub extern "C" fn stone_sound_decode_sync(handle: u64) -> i64 {
    let path = AUDIO_STATE.with(|state| {
        let state = state.borrow();
        let sound = state.as_ref()?.sounds.get(&handle)?;
        if sound.stream {
            crate::log_once!(
                "AUDIO",
                "stone_sound_decode_sync: {} is a stream source",
                sound.path
            );
            return None;
        }
        if sound.samples.is_some() {
            return Some(None);
        }
        Some(Some(sound.path.clone()))
    });
    let Some(path) = path else { return -1 };
    let Some(path) = path else { return 0 };
    let started = std::time::Instant::now();
    let Some((samples, sample_rate, channels)) = decode_sound_samples(&path) else {
        return -1;
    };
    let elapsed_us = started.elapsed().as_micros().min(i64::MAX as u128) as i64;
    let published = AUDIO_STATE.with(|state| {
        let Some(ref mut audio) = *state.borrow_mut() else {
            return false;
        };
        let Some(sound) = audio.sounds.get_mut(&handle) else {
            return false;
        };
        sound.publish_decoded(samples, sample_rate, channels)
    });
    if published {
        elapsed_us
    } else {
        -1
    }
}

#[no_mangle]
pub extern "C" fn stone_sound_resident(handle: u64) -> bool {
    AUDIO_STATE.with(|state| {
        state
            .borrow()
            .as_ref()
            .and_then(|audio| audio.sounds.get(&handle))
            .is_some_and(|sound| sound.samples.is_some())
    })
}

/// Admitted for playback: resident PCM, or a stream source's encoded file.
#[no_mangle]
pub extern "C" fn stone_sound_playable(handle: u64) -> bool {
    AUDIO_STATE.with(|state| {
        state
            .borrow()
            .as_ref()
            .and_then(|audio| audio.sounds.get(&handle))
            .is_some_and(SoundData::playable)
    })
}

/// Encoded bytes a stream source holds (0 before admission), or -1 for a
/// handle that is not a live stream source.
#[no_mangle]
pub extern "C" fn stone_sound_stream_bytes(handle: u64) -> i64 {
    AUDIO_STATE.with(|state| {
        state
            .borrow()
            .as_ref()
            .and_then(|audio| audio.sounds.get(&handle))
            .filter(|sound| sound.stream)
            .map_or(-1, |sound| {
                sound.encoded.as_ref().map_or(0, |bytes| bytes.len() as i64)
            })
    })
}

/// Decoded PCM size: resident samples, else the recipe's estimate. A stream
/// source never holds PCM, so it always reports the estimate.
#[no_mangle]
pub extern "C" fn stone_sound_pcm_bytes(handle: u64) -> i64 {
    AUDIO_STATE.with(|state| {
        state
            .borrow()
            .as_ref()
            .and_then(|audio| audio.sounds.get(&handle))
            .map_or(-1, |sound| {
                let bytes = sound.samples.as_ref().map_or_else(
                    || sound.estimated_bytes(),
                    |samples| (samples.len() * std::mem::size_of::<f32>()) as u64,
                );
                bytes.min(i64::MAX as u64) as i64
            })
    })
}

/// Re-decode `path` into the same sound `handle` in place. Stops in-flight
/// players first; keeps the old recipe and samples on unknown handle or decode
/// failure, so a corrupt/missing file never wipes a working sound.
#[no_mangle]
pub extern "C" fn stone_reload_sound(handle: u64, path: *const c_char) -> bool {
    if handle == 0 || path.is_null() {
        return false;
    }
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return false,
    };

    let stream = AUDIO_STATE.with(|state| {
        state
            .borrow()
            .as_ref()
            .and_then(|audio| audio.sounds.get(&handle))
            .map(|sound| sound.stream)
    });
    let Some(stream) = stream else { return false };
    let payload = if stream {
        let Some((encoded, rate, channels)) = load_stream_bytes(path_str) else {
            return false;
        };
        AdmissionPayload::Encoded(encoded, rate, channels)
    } else {
        let Some((samples, rate, channels)) = decode_sound_samples(path_str) else {
            return false;
        };
        AdmissionPayload::Pcm(samples, rate, channels)
    };

    AUDIO_STATE.with(|state| {
        if let Some(ref mut audio) = *state.borrow_mut() {
            if !audio.sounds.contains_key(&handle) {
                return false;
            }
            stop_voices_of(&mut audio.voices, handle);
            let Some(sound) = audio.sounds.get_mut(&handle) else {
                return false;
            };
            sound.path = path_str.to_owned();
            sound.admitting = false;
            sound.samples = None;
            sound.encoded = None;
            match payload {
                AdmissionPayload::Pcm(samples, rate, channels) => {
                    sound.publish_decoded(samples, rate, channels)
                }
                AdmissionPayload::Encoded(encoded, rate, channels) => {
                    sound.publish_encoded(encoded, rate, channels)
                }
            }
        } else {
            false
        }
    })
}

/// How many decoded samples one sound holds NOW. Zero means the recipe exists
/// without PCM; -1 means the handle is not live.
#[no_mangle]
pub extern "C" fn stone_sound_sample_count(handle: u64) -> i64 {
    AUDIO_STATE.with(|state| {
        let Some(ref audio) = *state.borrow() else {
            return -1;
        };
        audio.sounds.get(&handle).map_or(-1, |sd| {
            sd.samples
                .as_ref()
                .map_or(0, |samples| samples.len() as i64)
        })
    })
}

/// How far into the track the single sink is now, in milliseconds, or -1 when
/// nothing is playing. -1 rather than 0 because a sink that just started IS at
/// 0, and "silent" is a different state from "at the beginning".
#[no_mangle]
pub extern "C" fn stone_music_position_ms() -> i64 {
    AUDIO_STATE.with(|state| {
        let Some(ref audio) = *state.borrow() else {
            return -1;
        };
        audio
            .music_playback
            .as_ref()
            .map_or(-1, |p| p.player.get_pos().as_millis() as i64)
    })
}

#[no_mangle]
pub extern "C" fn stone_load_music(path: *const c_char) -> u64 {
    if path.is_null() {
        return 0;
    }
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };

    // Music is held compressed and decoded on play, so the stub skips the read
    // and the decoder probe; an empty buffer plays as silence.
    let bytes = if stub_audio() {
        if !audio_asset_exists(path_str) {
            return 0;
        }
        Vec::new()
    } else {
        let bytes = match crate::asset_registry::resolve_asset(path_str) {
            Some(b) => b,
            None => {
                crate::log_once!("AUDIO", "stone_load_music: Failed to read {}", path_str);
                return 0;
            }
        };
        if Decoder::new(std::io::Cursor::new(bytes.clone())).is_err() {
            crate::log_once!("AUDIO", "stone_load_music: Failed to decode {}", path_str);
            return 0;
        }
        bytes
    };

    // Same order as `stone_load_sound`, for the same reason: an id the class
    // cannot show is `unaccounted`, and that is a property of there being no
    // audio state rather than of the stub.
    AUDIO_STATE.with(|state| {
        let Some(ref mut audio) = *state.borrow_mut() else {
            crate::log_once!(
                "AUDIO",
                "stone_load_music: no audio state, so `{}` gets no handle",
                path_str
            );
            return 0;
        };
        let id = crate::handles::mint(Class::Music);
        audio.music.insert(id, bytes.into());
        id
    })
}

// Free a previously-loaded sound. Stops any currently-playing instances
// before dropping the slot — hot-reload-from-disk replaces the .wav, the
// in-flight samples should not keep playing. Returns false if the handle
// was already absent (idempotent, no panic).
/// Live loaded-sound handles (`native_ledger`).
pub(crate) fn live_sound_ids() -> Vec<u64> {
    AUDIO_STATE.with(|state| match *state.borrow() {
        Some(ref audio) => audio.sounds.keys().copied().collect(),
        None => Vec::new(),
    })
}

/// Live decoded-music handles (`native_ledger`).
pub(crate) fn live_music_ids() -> Vec<u64> {
    AUDIO_STATE.with(|state| match *state.borrow() {
        Some(ref audio) => audio.music.keys().copied().collect(),
        None => Vec::new(),
    })
}

/// Encoded bytes one music track holds, for the asset report's CPU column.
/// -1 for a handle that is not live.
#[no_mangle]
pub extern "C" fn stone_music_byte_len(handle: u64) -> i64 {
    AUDIO_STATE.with(|state| {
        let Some(ref audio) = *state.borrow() else {
            return -1;
        };
        audio
            .music
            .get(&handle)
            .map_or(-1, |bytes| bytes.len() as i64)
    })
}

/// Live PLAYING voices (`native_ledger`).
///
/// `STONE_QUIET` makes the ordinary `play` path return the never-started
/// `Voice(0)` sentinel, but that is a property of that path and not of the map:
/// a sim reaching the raw FFI still mints a real voice, so the class is
/// observable and is inventoried rather than assumed empty.
pub(crate) fn live_voice_ids() -> Vec<u64> {
    AUDIO_STATE.with(|state| match *state.borrow() {
        Some(ref audio) => audio.voices.keys().copied().collect(),
        None => Vec::new(),
    })
}

/// Whether the single music sink is playing something, and if so which handle
/// (0 when nothing is). One sink and any number of loaded tracks, so "is music
/// loaded" and "is music playing" are different questions.
#[no_mangle]
pub extern "C" fn stone_music_playing_handle() -> u64 {
    AUDIO_STATE.with(|state| match *state.borrow() {
        Some(ref audio) => audio.music_playback.as_ref().map_or(0, |p| p.handle),
        None => 0,
    })
}

fn stop_music_playback(playback: &mut Option<MusicPlayback>) {
    if let Some(p) = playback.take() {
        p.player.stop();
    }
}

/// Drop a decoded music buffer, stopping playback only when the sink is playing
/// THIS handle. There is one sink and any number of loaded tracks, so stopping
/// unconditionally would silence whatever else was playing -- an entry's
/// teardown of its own track would stop the boot process's music.
#[no_mangle]
pub extern "C" fn stone_unload_music(handle: u64) -> bool {
    AUDIO_STATE.with(|state| {
        if let Some(ref mut audio) = *state.borrow_mut() {
            let removed = audio.music.remove(&handle).is_some();
            if removed {
                crate::handles::retire(Class::Music, handle);
            }
            if removed
                && audio
                    .music_playback
                    .as_ref()
                    .is_some_and(|p| p.handle == handle)
            {
                stop_music_playback(&mut audio.music_playback);
            }
            return removed;
        }
        false
    })
}

#[no_mangle]
pub extern "C" fn stone_unload_sound(handle: u64) -> bool {
    AUDIO_STATE.with(|state| {
        if let Some(ref mut audio) = *state.borrow_mut() {
            stop_voices_of(&mut audio.voices, handle);
            audio
                .pending_sound_admissions
                .retain(|pending| *pending != handle);
            let removed = audio.sounds.remove(&handle).is_some();
            if removed {
                crate::handles::retire(Class::Sound, handle);
            }
            removed
        } else {
            false
        }
    })
}

/// Per-play gain and playback ratio. The gain is the caller's alone -- the
/// bus applies its own downstream, and there is no third global on top of
/// either. There used to be: `sound_volume` was folded in here regardless of
/// which bus the voice was headed for, so the "sfx" level silently attenuated
/// ambience and UI as well.
///
/// The speed floor is load-bearing: rodio resamples by the ratio, so 0 (or a
/// negative, which a caller computing a pitch from a lifetime can produce)
/// yields a player that consumes no samples and never reports empty -- it would
/// sit in the voice map forever rather than fail.
fn sound_play_params(volume: f32, speed: f32) -> (f32, f32) {
    (volume.max(0.0), speed.clamp(0.05, 8.0))
}

/// `source` at `ratio` playback speed, under a `decay` envelope when one is
/// asked for.
///
/// `decay` is in seconds of OUTPUT, which is why it is applied after the speed:
/// a caller asking for a two-second rumble means two seconds of what it hears,
/// not two seconds of source it then stretched to six.
///
/// The ramp runs the whole length rather than only the tail because that is the
/// shape the callers want (a struck thing ringing down); a sound that should
/// hold and then release wants an envelope, not this.
///
/// Boxed so the two arms have one type and the shaping is reachable from a
/// test. That costs nothing: `Player::append` hands the source to its queue,
/// which boxes it anyway (rodio `queue.rs`, `append_with_signal`).
fn shaped_source<S>(source: S, ratio: f32, decay: f32) -> Box<dyn Source<Item = f32> + Send>
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

struct SpatialVoiceSource<I>
where
    I: Source<Item = f32>,
{
    input: I,
    pan: Arc<AtomicU32>,
    pan_bits: u32,
    left: f32,
    right: f32,
    output_channel: usize,
    mono: f32,
}

impl<I> SpatialVoiceSource<I>
where
    I: Source<Item = f32>,
{
    fn new(input: I, pan: Arc<AtomicU32>) -> Self {
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

impl<I> Iterator for SpatialVoiceSource<I>
where
    I: Source<Item = f32>,
{
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

impl<I> Source for SpatialVoiceSource<I>
where
    I: Source<Item = f32>,
{
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

fn voice_source<S>(
    source: S,
    ratio: f32,
    decay: f32,
    pan: Option<Arc<AtomicU32>>,
) -> Box<dyn Source<Item = f32> + Send>
where
    S: Source<Item = f32> + Send + 'static,
{
    let shaped = shaped_source(source, ratio, decay);
    match pan {
        Some(control) => Box::new(SpatialVoiceSource::new(shaped, control)),
        None => shaped,
    }
}

/// Stop and forget every voice playing `sound`. Used by asset-level ops
/// (stop/unload/reload) -- a voice outliving its sound asset would keep
/// playing samples the caller believes are gone.
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

/// Start `handle` as a new voice on `bus`; returns its id, or 0 if the sound
/// isn't loaded, the bus is unknown, or audio never initialized. Nonzero ids
/// are unique for the process lifetime -- never reused, so a stale id can't
/// address a later voice. A looped voice repeats the decoded sample buffer
/// until explicitly stopped.
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
    AUDIO_STATE.with(|state| {
        if let Some(ref mut audio) = *state.borrow_mut() {
            // PLAYBACK is what a device gates, and it is gated explicitly here
            // rather than falling out of there being no state at all. Nothing
            // drains a stubbed bus graph, so a player appended to one would
            // never empty -- `stone_voice_active` would answer true forever and
            // `voices` would grow for the process lifetime. Returning 0 is the
            // same answer a stubbed run has always given; what changed is that
            // the sound the caller loaded is now really in the registry.
            if audio._stream.is_none() {
                return 0;
            }
            let Some(bus) = audio.buses.get(bus) else {
                crate::log_once!("AUDIO", "stone_play_sound: unknown bus {}", bus);
                return 0;
            };
            if let Some(sound_data) = audio.sounds.get(&handle) {
                let (gain, ratio) = sound_play_params(volume, speed);

                // Per-play gain rides the player; the bus applies its own
                // slewed gain downstream, so the two compose rather than
                // one having to know about the other.
                let player = Player::connect_new(bus.mixer());
                player.set_volume(gain);

                // Reverb is a BUS insert now, not a per-voice wrapper. Two
                // consequences worth knowing: one tank costs the same whether
                // one voice is playing or forty, and the tail no longer pitches
                // with a slowed sample -- the reverb sits downstream of the
                // speed, so the room keeps its own character.
                let pan_control = spatial.then(|| Arc::new(AtomicU32::new(pan.to_bits())));
                let mut stream_position = None;
                let input = if sound_data.stream {
                    let Some(encoded) = sound_data.encoded.clone() else {
                        return 0;
                    };
                    let position = Arc::new(AtomicU64::new(0));
                    let source = match StreamSource::new(encoded, looped, position.clone()) {
                        Ok(source) => source,
                        Err(e) => {
                            crate::log_once!(
                                "AUDIO",
                                "stone_play_sound: stream {} does not decode: {}",
                                sound_data.path,
                                e
                            );
                            return 0;
                        }
                    };
                    stream_position = Some((position, source.channels().get()));
                    voice_source(source, ratio, decay, pan_control.clone())
                } else {
                    let Some(source) = sound_data.as_source(looped) else {
                        return 0;
                    };
                    voice_source(source, ratio, decay, pan_control.clone())
                };
                player.append(input);

                // Reap voices that ended on their own. Safe here because
                // `append` bumps the player's sound_count synchronously, so a
                // voice started this call never looks empty to a later one.
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
                        stream_position,
                    },
                );
                return voice_id;
            }
        }
        0
    })
}

/// Length of a sound recipe in seconds, at its own rate (before any playback
/// speed). 0 for an unknown handle -- callers treat that as "no duration to
/// draw" rather than as an error, since a stale handle is already visible as
/// such elsewhere.
#[no_mangle]
pub extern "C" fn stone_get_sound_duration(handle: u64) -> c_float {
    AUDIO_STATE.with(|state| {
        if let Some(ref audio) = *state.borrow() {
            if let Some(sd) = audio.sounds.get(&handle) {
                return sd.duration_seconds();
            }
        }
        0.0
    })
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
    AUDIO_STATE.with(|state| {
        let Some(ref audio) = *state.borrow() else {
            return 0;
        };
        let Some(sd) = audio.sounds.get(&handle) else {
            return 0;
        };
        let Some(samples) = sd.samples.as_ref() else {
            return 0;
        };
        let n = samples.len();
        if n == 0 {
            return 0;
        }
        let buckets = buckets as usize;
        let slice = std::slice::from_raw_parts_mut(out, buckets);
        for (b, slot) in slice.iter_mut().enumerate() {
            // Span the samples by bucket index rather than stepping a cursor, so
            // the last bucket always lands on the end however n divides.
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
}

#[no_mangle]
pub extern "C" fn stone_stop_sound(handle: u64) {
    AUDIO_STATE.with(|state| {
        if let Some(ref mut audio) = *state.borrow_mut() {
            stop_voices_of(&mut audio.voices, handle);
        }
    });
}

/// True while `voice` is addressable. This is MAP MEMBERSHIP, not
/// `Player::empty()`: rodio's `stop()` only sets an atomic, and the player's
/// sound_count doesn't drop until the audio thread next drains the source, so
/// an empty()-based answer would race with our own stop calls. A voice that
/// ends on its own stays "active" until the next `stone_play_sound` reaps it.
/// Source frames a stream voice has decoded since it started, counting every
/// loop pass; -1 for an unknown voice or one playing resident PCM.
#[no_mangle]
pub extern "C" fn stone_voice_stream_frames(voice: u64) -> i64 {
    AUDIO_STATE.with(|state| {
        let state = state.borrow();
        let Some(voice) = state.as_ref().and_then(|audio| audio.voices.get(&voice)) else {
            return -1;
        };
        voice
            .stream_position
            .as_ref()
            .map_or(-1, |(position, channels)| {
                (position.load(Ordering::Relaxed) / u64::from((*channels).max(1))) as i64
            })
    })
}

#[no_mangle]
pub extern "C" fn stone_voice_active(voice: u64) -> bool {
    AUDIO_STATE.with(|state| {
        state
            .borrow()
            .as_ref()
            .is_some_and(|audio| audio.voices.contains_key(&voice))
    })
}

/// Stop one voice, leaving every sibling voice of the same asset playing.
/// False if the voice was already gone (idempotent).
#[no_mangle]
pub extern "C" fn stone_voice_stop(voice: u64) -> bool {
    AUDIO_STATE.with(|state| {
        if let Some(ref mut audio) = *state.borrow_mut() {
            if let Some(v) = audio.voices.remove(&voice) {
                crate::handles::retire(Class::Voice, voice);
                v.player.stop();
                return true;
            }
        }
        false
    })
}

/// Per-voice gain, composed with the global SFX volume. Takes effect within
/// rodio's 5ms control tick. False for an unknown/dead voice.
#[no_mangle]
pub extern "C" fn stone_voice_set_gain(voice: u64, gain: c_float) -> bool {
    AUDIO_STATE.with(|state| {
        if let Some(ref mut audio) = *state.borrow_mut() {
            let scaled = gain.max(0.0);
            if let Some(v) = audio.voices.get(&voice) {
                v.player.set_volume(scaled);
                return true;
            }
        }
        false
    })
}

/// Change a spatial voice at the next stereo frame without restarting it.
#[no_mangle]
pub extern "C" fn stone_voice_set_pan(voice: u64, pan: c_float) -> bool {
    if !pan.is_finite() {
        return false;
    }
    AUDIO_STATE.with(|state| {
        let state = state.borrow();
        let Some(audio) = state.as_ref() else {
            return false;
        };
        let Some(control) = audio.voices.get(&voice).and_then(|v| v.pan.as_ref()) else {
            return false;
        };
        control.store(pan.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed);
        true
    })
}

/// Playback rate: resamples, so pitch and duration move together.
/// False for an unknown/dead voice.
#[no_mangle]
pub extern "C" fn stone_voice_set_speed(voice: u64, speed: c_float) -> bool {
    AUDIO_STATE.with(|state| {
        if let Some(ref mut audio) = *state.borrow_mut() {
            if let Some(v) = audio.voices.get(&voice) {
                v.player.set_speed(speed.max(0.01));
                return true;
            }
        }
        false
    })
}

#[no_mangle]
pub extern "C" fn stone_play_music(handle: u64) {
    AUDIO_STATE.with(|state| {
        if let Some(ref mut audio) = *state.borrow_mut() {
            let Some(bytes) = audio.music.get(&handle).cloned() else {
                crate::log_once!("AUDIO", "stone_play_music: unknown music handle {}", handle);
                return;
            };
            let source = match StreamSource::new(bytes, true, Arc::new(AtomicU64::new(0))) {
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
            // Same gate as `stone_play_sound`, and it comes after the decode on
            // purpose: a stubbed run still proves the track DECODES, so a
            // corrupt file is a failure there as much as here.
            if audio._stream.is_none() {
                return;
            }
            let player = Player::connect_new(audio.buses.music.mixer());
            player.append(source);
            audio.music_playback = Some(MusicPlayback { handle, player });
        }
    });
}

#[no_mangle]
pub extern "C" fn stone_stop_music() {
    AUDIO_STATE.with(|state| {
        if let Some(ref mut audio) = *state.borrow_mut() {
            stop_music_playback(&mut audio.music_playback);
        }
    });
}

const BUS_IDS: [u32; 6] = [
    BUS_MASTER,
    BUS_SFX,
    BUS_MUSIC,
    BUS_AMBIENCE,
    BUS_UI,
    BUS_FOLEY,
];

thread_local! {
    static AUDIO_AMBIENT_REPLY: RefCell<Option<std::ffi::CString>> = const { RefCell::new(None) };
}

/// Every bus's parameters: all the audio state that belongs to no handle.
/// There is no master/music volume alongside the buses -- `mixer.rock` is the
/// only caller of `bus_set_gain`, so a bus's own gain already is the level.
#[derive(Clone, Copy)]
struct AudioAmbient {
    buses: [BusAmbient; BUS_IDS.len()],
}

fn audio_ambient_now(audio: &AudioState) -> Option<AudioAmbient> {
    let mut buses = [audio.buses.get(BUS_MASTER)?.ambient(); BUS_IDS.len()];
    for (slot, id) in buses.iter_mut().zip(BUS_IDS) {
        *slot = audio.buses.get(id)?.ambient();
    }
    Some(AudioAmbient { buses })
}

/// The audio ambient as one line, or `absent` when this process has no audio
/// state. A diagnostic reader: none of these values has a getter of its own.
///
/// The trailing `output=` field is not bus state: it is whether anything this
/// process mixes can be heard, which is a property of the process rather than
/// of a bus. It rides here because this is already the one place Rock reads
/// native audio state that has no getter of its own.
#[no_mangle]
pub extern "C" fn stone_audio_ambient_report() -> *const c_char {
    let s = AUDIO_STATE.with(|state| {
        let borrowed = state.borrow();
        let Some(a) = borrowed.as_ref().and_then(audio_ambient_now) else {
            return "absent".to_string();
        };
        // A stubbed run has no device at all, so its output is silent for a
        // different reason than the latch; the field answers the one question.
        let no_device = borrowed
            .as_ref()
            .is_some_and(|audio| audio._stream.is_none());
        let muted = output_muted() || no_device;
        let mut out = String::new();
        for (b, id) in a.buses.iter().zip(BUS_IDS) {
            if !out.is_empty() {
                out.push_str("; ");
            }
            out.push_str(&format!(
                "bus{id}:gain={:.4},cutoff={:.2},q={:.4},drive={:.4},mix={:.4},\
                 reverb={},{:.4},{:.4},{:.4},{:.4},{:.4},duck={:.4},{:.4},{:.4}",
                b.gain,
                b.cutoff,
                b.q,
                b.drive,
                b.mix,
                u8::from(b.reverb_enabled),
                b.room,
                b.damping,
                b.wet,
                b.dry,
                b.width,
                b.duck_depth,
                b.duck_attack,
                b.duck_release,
            ));
        }
        out.push_str(if muted {
            "; output=muted"
        } else {
            "; output=live"
        });
        out
    });
    AUDIO_AMBIENT_REPLY.with(|r| {
        let Ok(c) = std::ffi::CString::new(s) else {
            return std::ptr::null();
        };
        let ptr = c.as_ptr();
        *r.borrow_mut() = Some(c);
        ptr
    })
}

/// Target gain for a bus. Slewed sample-side over ~10ms, so a per-frame
/// stepped value from Rock does not zipper. False for an unknown bus.
#[no_mangle]
pub extern "C" fn stone_bus_set_gain(bus: u32, gain: c_float) -> bool {
    AUDIO_STATE.with(|state| {
        if let Some(ref audio) = *state.borrow() {
            if let Some(b) = audio.buses.get(bus) {
                b.set_gain(gain);
                return true;
            }
        }
        false
    })
}

/// Samples this bus has produced. Monotonic; a bus that is alive and being
/// pulled by the device advances it. Lets a headless test observe that a bus
/// is live without capturing audio, and is the basis for level metering.
#[no_mangle]
pub extern "C" fn stone_bus_samples_pulled(bus: u32) -> u64 {
    AUDIO_STATE.with(|state| {
        state
            .borrow()
            .as_ref()
            .and_then(|audio| audio.buses.get(bus))
            .map_or(0, |b| b.samples_pulled())
    })
}

/// This bus's TARGET gain -- what was last asked for, not the slewed value the
/// audio thread is currently applying (which lives sample-side). Enough to
/// assert that a settings apply reached the right bus.
#[no_mangle]
pub extern "C" fn stone_bus_gain(bus: u32) -> c_float {
    AUDIO_STATE.with(|state| {
        state
            .borrow()
            .as_ref()
            .and_then(|audio| audio.buses.get(bus))
            .map_or(0.0, |b| b.gain())
    })
}

/// Loudest sample this bus has OUTPUT since the last call, post-fader -- so it
/// reads what the bus is contributing to the mix, not what is playing into it,
/// and a ducked or turned-down bus meters low. Can exceed 1.0: nothing limits
/// the sum, so that is the clip indication.
///
/// Reading CLEARS the hold, which is what lets a per-video-frame poll catch a
/// transient that peaked and decayed between two polls -- but also makes this
/// single-consumer. Two callers polling the same bus each see whichever peaks
/// landed since the other's read.
#[no_mangle]
pub extern "C" fn stone_bus_peak(bus: u32) -> c_float {
    AUDIO_STATE.with(|state| {
        state
            .borrow()
            .as_ref()
            .and_then(|audio| audio.buses.get(bus))
            .map_or(0.0, |b| b.meter())
    })
}

/// How many ducks have been FIRED on this bus. The latch generation the audio
/// thread already reads to notice a retrigger, exposed as-is -- so a caller can
/// assert that an event ducked the right bus without capturing audio. It says
/// nothing about the envelope's shape, which runs sample-side; that is pinned
/// by `duck_attacks_then_releases`.
#[no_mangle]
pub extern "C" fn stone_bus_duck_count(bus: u32) -> u64 {
    AUDIO_STATE.with(|state| {
        state
            .borrow()
            .as_ref()
            .and_then(|audio| audio.buses.get(bus))
            .map_or(0, |b| b.duck_count())
    })
}

/// Lowpass insert. `cutoff_hz` is the automation target -- slewed sample-side
/// in log2(Hz), so a per-frame value from Rock ramps smoothly. Cutoff at max
/// (~0.45 * rate) is the transparent "off" position.
#[no_mangle]
pub extern "C" fn stone_bus_set_lowpass(bus: u32, cutoff_hz: c_float, q: c_float) -> bool {
    with_bus(bus, |b| b.set_lowpass(cutoff_hz, q))
}

#[no_mangle]
pub extern "C" fn stone_bus_lowpass_cutoff(bus: u32) -> c_float {
    AUDIO_STATE.with(|state| {
        state
            .borrow()
            .as_ref()
            .and_then(|audio| audio.buses.get(bus))
            .map_or(0.0, |b| b.lowpass_cutoff())
    })
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

/// Duck this bus to `depth` over `attack_secs`, then recover over
/// `release_secs`. Event-driven on purpose: the envelope is exact and
/// assertable, unlike a signal-domain sidechain. Re-triggering restarts the
/// attack, so repeated events re-duck.
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
    AUDIO_STATE.with(|state| {
        if let Some(ref audio) = *state.borrow() {
            if let Some(b) = audio.buses.get(bus) {
                f(b);
                return true;
            }
        }
        false
    })
}
/// Set Freeverb reverb effect for sounds
/// - enabled: whether to apply reverb to sounds
/// - room_size: 0.0-1.0 (larger = more reverb)
/// - damping: 0.0-1.0 (higher = darker sound)
/// - wet: 0.0-1.0 (reverb signal level)
/// - dry: 0.0-1.0 (original signal level)
/// - width: 0.0-1.0 (stereo width)
///
/// Reverb on the MASTER bus. Was a per-voice wrapper applied at play time,
/// which meant it only ever reached sounds started after it was switched on
/// and never reached music at all; as a master insert it reaches everything,
/// including already-playing voices. Signature unchanged, so games need no
/// edit. Use `stone_bus_set_reverb` to put reverb on a specific bus.
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
        // (environment, opens no device, skips decoding)
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
            let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
            cmd.args([
                "--exact",
                "audio::tests::automated_runs_default_to_device_free_audio",
            ])
            .env(CHILD, format!("{}{}", *no_device as u8, *stub as u8));
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
            let output = cmd.envs(env.iter().copied()).output().unwrap();
            assert!(
                output.status.success(),
                "{env:?}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
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

    // These fixtures install detached audio state and exercise real decoding.
    // Select it in a fresh process without mutating parallel tests' environment
    // or acquiring an output device.
    fn delegate_decoded_audio_fixture(test: &str) -> bool {
        const CHILD: &str = "STONE_DECODE_FIXTURE";
        if std::env::var(CHILD).as_deref() == Ok(test) {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test])
            .env(CHILD, test)
            .env("STONE_AUDIO_STUB", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    fn ch2() -> ChannelCount {
        NonZero::new(2u16).unwrap()
    }
    fn sr() -> SampleRate {
        NonZero::new(44100u32).unwrap()
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
        let sound = SoundData::recipe("fixture.wav".to_owned(), 2, 22_050, 2);
        assert!(sound.samples.is_none(), "a recipe must not allocate PCM");
        assert!(
            (sound.duration_seconds() - expected_duration).abs() < f32::EPSILON,
            "duration comes from decoded frame metadata"
        );
    }

    #[test]
    fn decoded_sound_admission_publishes_one_arc() {
        let mut sound = SoundData::recipe("fixture.ogg".to_owned(), 2, 44_100, 1);
        assert!(sound.publish_decoded(vec![0.25, -0.5], 44_100, 1));
        let published = sound.samples.as_ref().unwrap().clone();
        assert!(
            !sound.publish_decoded(vec![1.0, 1.0], 22_050, 1),
            "a second completion cannot replace an admitted buffer"
        );
        assert!(
            Arc::ptr_eq(&published, sound.samples.as_ref().unwrap()),
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
        let mut audio = AudioState {
            _stream: None,
            buses: Buses::detached(ch2(), sr()),
            sounds: HashMap::new(),
            pending_sound_admissions: VecDeque::new(),
            inflight_sound_bytes: 0,
            music: HashMap::new(),
            music_playback: None,
            voices: HashMap::new(),
        };
        let mut sound = SoundData::recipe("fixture.ogg".to_owned(), 7, 48_000, 1);
        assert!(sound.publish_decoded(vec![0.2, -0.91, 0.1, -0.75, 0.3, -0.4, 0.67], 48_000, 1));
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

    /// Control: proves the trap Bus exists to avoid is real. A bare sub-mixer
    /// returns None once it runs dry, and the parent drops any child that
    /// returns None -- so it goes permanently deaf, silently.
    #[test]
    fn bare_submixer_goes_deaf_after_running_dry() {
        let (parent, mut out) = rodio::mixer::mixer(ch2(), sr());
        let (child, child_src) = rodio::mixer::mixer(ch2(), sr());
        parent.add(child_src);
        child.add(blip());
        assert!(
            heard(&mut out, 64),
            "control: child should be audible at first"
        );

        child.add(blip());
        assert!(
            !heard(&mut out, 64),
            "control assumption broken: a bare sub-mixer survived running dry, \
             so bus_survives_going_silent is not testing anything"
        );
    }

    #[test]
    fn bus_survives_going_silent() {
        let (parent, mut out) = rodio::mixer::mixer(ch2(), sr());
        let bus = Bus::new(ch2(), sr());
        parent.add(bus.take_source());

        bus.mixer().add(blip());
        assert!(heard(&mut out, 64), "bus should be audible at first");

        bus.mixer().add(blip());
        assert!(heard(&mut out, 64), "bus went deaf after falling silent");
    }

    /// The pull counter is what lets a sim observe that a bus is live without
    /// capturing audio.
    #[test]
    fn bus_counts_pulled_samples() {
        let (parent, mut out) = rodio::mixer::mixer(ch2(), sr());
        let bus = Bus::new(ch2(), sr());
        parent.add(bus.take_source());

        assert_eq!(bus.samples_pulled(), 0);
        for _ in 0..64 {
            let _ = out.next();
        }
        assert_eq!(bus.samples_pulled(), 64, "every parent pull is a bus pull");
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

    fn wired() -> (Bus, impl Iterator<Item = f32>) {
        let (parent, out) = rodio::mixer::mixer(ch2(), sr());
        let bus = Bus::new(ch2(), sr());
        parent.add(bus.take_source());
        (bus, out)
    }

    #[test]
    fn lowpass_open_passes_audible_highs() {
        let (bus, mut out) = wired();
        bus.mixer().add(square(40_000, 8));
        let level = mean_abs(&mut out, 8192, 8192);
        assert!(
            level > 0.7,
            "wide-open lowpass should pass 5.5kHz, got {level}"
        );
    }

    #[test]
    fn lowpass_attenuates_highs() {
        let (bus, mut out) = wired();
        bus.mixer().add(square(40_000, 8));
        bus.set_lowpass(400.0, 0.707);
        let level = mean_abs(&mut out, 8192, 8192);
        assert!(
            level < 0.05,
            "400Hz lowpass should crush 5.5kHz, got {level}"
        );
    }

    /// Pins what "wide open" actually means: cutoff maxes at 0.45*rate, so
    /// Nyquist is above it and gets rejected. Documents the limit rather than
    /// pretending the open position is bit-transparent.
    #[test]
    fn lowpass_open_still_rejects_nyquist() {
        let (bus, mut out) = wired();
        bus.mixer().add(nyquist(40_000));
        let level = mean_abs(&mut out, 8192, 8192);
        assert!(level < 0.01, "Nyquist sits above max cutoff, got {level}");
    }

    /// Distortion must actually change the signal, and soft-clipping must not
    /// let it run away above unity.
    #[test]
    fn distortion_clips_without_exploding() {
        let (bus, mut out) = wired();
        bus.mixer().add(dc(40_000, 0.25));
        bus.set_distortion(8.0, 1.0);
        let level = mean_abs(&mut out, 8192, 8192);
        assert!(level > 0.3, "drive should lift a quiet signal, got {level}");
        assert!(level <= 1.0, "soft clip must not exceed unity, got {level}");
    }

    /// Linear ramps, so the timing is exact and assertable -- which is the
    /// point of an event-driven ducker over a signal-domain compressor.
    #[test]
    fn duck_attacks_then_releases() {
        let (bus, mut out) = wired();
        bus.mixer().add(dc(400_000, 1.0));

        let full = mean_abs(&mut out, 2048, 256);
        assert!(full > 0.9, "undicked bus should be near unity, got {full}");

        // 10ms attack to 0.2, 100ms release.
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

    /// Reverb lives on the bus now, not per voice -- so it must ring on after
    /// its input has stopped.
    #[test]
    fn bus_reverb_rings_after_input_stops() {
        let (bus, mut out) = wired();
        bus.set_reverb(true, 0.9, 0.2, 0.9, 0.1, 1.0);
        bus.mixer().add(dc(64, 1.0));

        // Past the end of the 64-frame input.
        let tail = mean_abs(&mut out, 512, 4096);
        assert!(
            tail > 0.0001,
            "reverb tail should outlast its input, got {tail}"
        );
    }

    /// Without reverb the bus decays to silence. Not exactly 0.0: the lowpass
    /// always runs (there is no bypass branch, by design), and biquad state
    /// decays asymptotically -- so this asserts inaudible, not bit-zero.
    #[test]
    fn bus_without_reverb_decays_to_silence() {
        let (bus, mut out) = wired();
        bus.mixer().add(dc(64, 1.0));
        let tail = mean_abs(&mut out, 512, 4096);
        assert!(
            tail < 1e-9,
            "no reverb: bus should decay to silence, got {tail}"
        );
    }

    /// The meter reads the bus OUTPUT, so it sits after the fader: a bus
    /// turned down is a bus that is not making noise, whatever is playing
    /// into it.
    #[test]
    fn bus_peak_is_post_fader() {
        let (bus, mut out) = wired();
        bus.mixer().add(dc(400_000, 1.0));

        // Settle first. The slowest thing here is not the fader's 10ms slew but
        // the meter's own 20dB/sec fall: dropping to a quarter is -12dB, which
        // takes 0.6s to walk down. Read before that and the assertion is about
        // the falloff, not about where the meter ends up.
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

    /// Reading must not disturb the meter. This is the flicker regression: the
    /// device callback and the video frame run on unrelated clocks (~23ms of
    /// buffer against 16.7ms of vsync), so polls land between callbacks. A
    /// reset-on-read counter hands those polls a true zero and the meter
    /// strobes; an envelope maintained sample-side does not care when it is
    /// looked at.
    #[test]
    fn bus_meter_is_stable_between_reads() {
        let (bus, mut out) = wired();
        bus.mixer().add(dc(400_000, 1.0));
        let _ = mean_abs(&mut out, 8192, 1);

        let first = bus.meter();
        let second = bus.meter();
        assert!(first > 0.9, "meter should be lit, got {first}");
        assert_eq!(first, second, "reading the meter must not consume it");

        // A poll landing with no samples pulled since the last one reads the
        // same value, not zero.
        let idle = bus.meter();
        assert_eq!(first, idle, "a poll between callbacks must not read zero");
    }

    /// It still has to fall, or every meter pins at the loudest thing that has
    /// ever happened. Falls in the audio domain, so the rate is in dB/sec and
    /// does not depend on how often anyone looks.
    #[test]
    fn bus_meter_falls_after_signal_stops() {
        let (bus, mut out) = wired();
        bus.mixer().add(dc(64, 1.0));
        let _ = mean_abs(&mut out, 512, 1);

        let lit = bus.meter();
        assert!(lit > 0.9, "meter should catch the blip, got {lit}");

        // ~0.25s of silence: 20dB/sec puts it near -5dB, down but not out.
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

        // ~3s: well below the -60dB floor the UI draws.
        let _ = mean_abs(&mut out, (3.0 * 44100.0) as usize * 2, 1);
        let dark = bus.meter();
        assert!(dark < 0.001, "meter should decay to dark, got {dark}");
    }

    /// Gain is slewed, so it must converge on the target rather than step to
    /// it -- a stepped gain zippers.
    #[test]
    fn bus_gain_slews_to_target() {
        let (parent, mut out) = rodio::mixer::mixer(ch2(), sr());
        let bus = Bus::new(ch2(), sr());
        parent.add(bus.take_source());
        bus.mixer()
            .add(SamplesBuffer::new(ch2(), sr(), vec![1.0f32; 40_000]));

        // Settle the lowpass first -- its first output is b0 (~0.8 at the open
        // cutoff), which has nothing to do with gain.
        for _ in 0..2048 {
            let _ = out.next();
        }

        bus.set_gain(0.0);
        let first = out.next().expect("bus is infinite");
        assert!(first > 0.9, "gain should start open, got {first}");

        // Mid-slew: on its way down, but not there yet.
        for _ in 0..64 {
            let _ = out.next();
        }
        let mid = out.next().expect("bus is infinite");
        assert!(
            mid < first && mid > 0.0,
            "expected mid-slew value, got {mid}"
        );

        // Settled.
        for _ in 0..4096 {
            let _ = out.next();
        }
        let settled = out.next().expect("bus is infinite");
        assert!(
            settled < 0.01,
            "gain should have settled near 0, got {settled}"
        );
    }

    // Per-play shaping: the parameters a caller sets when the voice
    // starts, as against the bus gain that shapes everything on it.

    #[test]
    fn per_play_gain_is_the_callers_alone() {
        let (gain, _) = sound_play_params(0.4, 1.0);
        assert!((gain - 0.4).abs() < 1e-6, "gain {gain}");
    }

    #[test]
    fn per_play_gain_is_never_negative() {
        // A caller fading a sound out past zero must go silent, not invert the
        // waveform's phase against the other players on the mixer.
        let (gain, _) = sound_play_params(-0.3, 1.0);
        assert_eq!(gain, 0.0);
    }

    #[test]
    fn centered_voice_source_preserves_authored_stereo() {
        let source = SamplesBuffer::new(ch2(), sr(), vec![1.0f32, 0.25, 0.5, 0.75]);
        let out: Vec<f32> = voice_source(source, 1.0, 0.0, None).collect();
        assert_eq!(out, vec![1.0, 0.25, 0.5, 0.75]);
    }

    #[test]
    fn positional_voice_source_downmixes_before_panning() {
        let source = SamplesBuffer::new(ch2(), sr(), vec![1.0f32, 0.0, 0.0, 1.0]);
        let left: Vec<f32> = voice_source(
            source.clone(),
            1.0,
            0.0,
            Some(Arc::new(AtomicU32::new((-1.0f32).to_bits()))),
        )
        .collect();
        let right: Vec<f32> = voice_source(
            source,
            1.0,
            0.0,
            Some(Arc::new(AtomicU32::new(1.0f32.to_bits()))),
        )
        .collect();

        assert_eq!(left, vec![0.5, 0.0, 0.5, 0.0]);
        assert_eq!(right, vec![0.0, 0.5, 0.0, 0.5]);
    }

    #[test]
    fn positional_voice_changes_pan_without_restarting() {
        let pan = Arc::new(AtomicU32::new((-1.0f32).to_bits()));
        let source = SamplesBuffer::new(NonZero::new(1u16).unwrap(), sr(), vec![1.0f32, 0.5]);
        let mut out = SpatialVoiceSource::new(source, pan.clone());
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
    fn positional_voice_source_uses_constant_power_center() {
        let source = SamplesBuffer::new(NonZero::new(1u16).unwrap(), sr(), vec![1.0f32]);
        let out: Vec<f32> = voice_source(
            source,
            1.0,
            0.0,
            Some(Arc::new(AtomicU32::new(0.0f32.to_bits()))),
        )
        .collect();
        assert_eq!(out.len(), 2);
        assert!(
            (out[0] - out[1]).abs() < 1e-6,
            "center pan must be balanced: {out:?}"
        );
        assert!(
            (out[0] * out[0] + out[1] * out[1] - 1.0).abs() < 1e-6,
            "center pan must preserve power: {out:?}"
        );
    }

    #[test]
    fn speed_never_reaches_zero() {
        // See sound_play_params: a zero ratio is a player that never finishes.
        for speed in [0.0, -1.0, f32::MIN] {
            let (_, ratio) = sound_play_params(1.0, speed);
            assert!(ratio >= 0.05, "speed {speed} -> ratio {ratio}");
        }
    }

    #[test]
    fn speed_is_bounded_above() {
        let (_, ratio) = sound_play_params(1.0, 500.0);
        assert_eq!(ratio, 8.0);
    }

    /// The decay envelope is measured in seconds of OUTPUT, which is why it is
    /// applied after the speed: a caller asking for a half-second rumble means
    /// half a second of what it hears, not half a second of source it then
    /// stretched to two.
    ///
    /// Note what that does NOT mean. `speed` does not resample; it lowers the
    /// rate the source REPORTS (0.25 on a 1000 Hz source reports 250 Hz), and
    /// `take_duration` counts against that reported rate. So the sample COUNT
    /// falls to a quarter while the playing time is the half-second asked for.
    /// Asserting on the count alone reads as an 8x error and is not one.
    #[test]
    fn decay_caps_output_length_not_source_length() {
        use rodio::Source;

        let rate = 1000u32;
        let src = SoundSource {
            samples: std::sync::Arc::new(vec![1.0; rate as usize]), // 1.0s mono
            sample_rate: rate,
            channels: 1,
            position: 0,
            looped: false,
        };

        let shaped = super::shaped_source(src, 0.25, 0.5);
        let out_rate = shaped.sample_rate().get() as f32;
        let secs = shaped.count() as f32 / out_rate;

        // Without the cap this source plays for 4.0s (1.0s slowed to a quarter).
        assert!(
            (secs - 0.5).abs() < 0.02,
            "expected ~0.5s of playback, got {secs}s at {out_rate} Hz"
        );
    }

    /// ... and the envelope really reaches silence, rather than cutting at
    /// full amplitude (which is a click, and the reason this is a fade at all).
    #[test]
    fn decay_ramps_to_silence() {
        let rate = 1000u32;
        let src = SoundSource {
            samples: std::sync::Arc::new(vec![1.0; rate as usize]),
            sample_rate: rate,
            channels: 1,
            position: 0,
            looped: false,
        };

        let out: Vec<f32> = super::shaped_source(src, 1.0, 0.5).collect();

        assert!(out[0] > 0.9, "starts at full amplitude, got {}", out[0]);
        let last = out[out.len() - 1];
        assert!(last < 0.05, "ends silent, got {last}");
    }

    #[test]
    fn looped_playback_repeats_past_the_source_end() {
        let src = SoundSource {
            samples: std::sync::Arc::new(vec![0.25, 0.75]),
            sample_rate: 1000,
            channels: 1,
            position: 0,
            looped: true,
        };

        let out: Vec<f32> = super::shaped_source(src, 1.0, 0.0).take(6).collect();
        assert_eq!(out, vec![0.25, 0.75, 0.25, 0.75, 0.25, 0.75]);
    }

    #[test]
    fn one_shot_playback_still_ends() {
        let src = SoundSource {
            samples: std::sync::Arc::new(vec![0.25, 0.75]),
            sample_rate: 1000,
            channels: 1,
            position: 0,
            looped: false,
        };

        let out: Vec<f32> = super::shaped_source(src, 1.0, 0.0).collect();
        assert_eq!(out, vec![0.25, 0.75]);
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
        let position = Arc::new(AtomicU64::new(0));
        let stream = StreamSource::new(Arc::from(wav), true, position.clone())
            .expect("the synthetic loop streams");
        let passes = 3;
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
        // Largest step of the authored waveform, plus one quantum per side.
        let max_step = AMPLITUDE * std::f32::consts::TAU / PERIOD as f32 + 2.0 / 32768.0;
        for pass in 1..passes {
            let seam = pass * decoded.len();
            for channel in 0..2 {
                let step = (streamed[seam + channel] - streamed[seam - 2 + channel]).abs();
                assert!(step <= max_step,
                    "loop seam {pass} channel {channel} steps {step}, over the waveform's {max_step}");
            }
        }
        assert_eq!(
            position.load(Ordering::Relaxed),
            streamed.len() as u64,
            "the stream position counts every sample taken across passes"
        );
    }

    #[test]
    fn stream_loop_of_a_vorbis_source_repeats_its_full_decode() {
        let bytes = include_bytes!("../../packages/stone/tests/fixtures/tone.ogg").to_vec();
        let (decoded, _, _) = decode_sound_bytes(bytes.clone(), "tone.ogg")
            .expect("the shipped Vorbis fixture decodes");
        let stream = StreamSource::new(Arc::from(bytes), true, Arc::new(AtomicU64::new(0)))
            .expect("the shipped Vorbis fixture streams");
        let streamed: Vec<f32> = stream.take(decoded.len() * 2 + 64).collect();
        assert_eq!(
            streamed.len(),
            decoded.len() * 2 + 64,
            "a looped Vorbis stream never ends"
        );
        for (index, sample) in streamed.iter().enumerate() {
            assert_eq!(
                *sample,
                decoded[index % decoded.len()],
                "streamed Vorbis sample {index} matches the decoded loop"
            );
        }
    }

    #[test]
    fn stream_admission_holds_the_encoded_file_without_pcm() {
        if delegate_decoded_audio_fixture(
            "audio::tests::stream_admission_holds_the_encoded_file_without_pcm",
        ) {
            return;
        }
        let _audio_restore = install_audio_state(AudioState {
            _stream: None,
            buses: Buses::detached(ch2(), sr()),
            sounds: HashMap::new(),
            pending_sound_admissions: VecDeque::new(),
            inflight_sound_bytes: 0,
            music: HashMap::new(),
            music_playback: None,
            voices: HashMap::new(),
        });
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
        assert!(
            !output_muted_from(None, None, None),
            "a shipped run is heard"
        );
        assert!(output_muted_from(Some("1"), None, None), "explicit quiet");
        assert!(
            output_muted_from(None, Some("1"), None),
            "headless defaults quiet"
        );
        assert!(
            !output_muted_from(Some("0"), Some("1"), None),
            "an explicit STONE_QUIET=0 opts a headless audio test back in"
        );
    }

    /// A sim cannot buy itself an audible output. `ROCK_RUN_SIM` is the
    /// runner's variable, so the fixture that flips the Rock gate to get live
    /// voices -- mag's `ambience_region_switch` -- stays silent regardless.
    #[test]
    fn a_sim_is_muted_even_asking_for_the_gate_open() {
        assert!(output_muted_from(Some("0"), None, Some("some_sim")));
        assert!(output_muted_from(None, None, Some("some_sim")));
    }

    /// The mute takes the OUTPUT and leaves the accounting: nothing audible
    /// reaches the device while the bus is still pulled and still meters.
    ///
    /// The control arm is the point. Without it this asserts only that a
    /// silent thing is silent, which a graph that was never fed also satisfies.
    #[test]
    fn a_muted_output_carries_no_signal_but_still_drains_its_bus() {
        let heard_through = |muted: bool| {
            let (mixer, mut out) = rodio::mixer::mixer(ch2(), sr());
            let buses = Buses::attached(&mixer, ch2(), sr(), muted);
            buses
                .sfx
                .mixer()
                .add(SamplesBuffer::new(ch2(), sr(), vec![1.0f32; 256]));
            let any = heard(&mut out, 256);
            (
                any,
                buses.master.samples_pulled(),
                buses.sfx.samples_pulled(),
                buses.sfx.meter(),
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
