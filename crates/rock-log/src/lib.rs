//! The one logging backend.
//!
//! Single `ROCK_LOG` parser, single level, single format
//! (`[CATEGORY] msg`), single stream (stderr). Every Rust crate logs
//! through the `log` facade (`log::info!(target: "CAT", …)`) plus the
//! `log_once!` / `log_throttled!` filters; this is the only thing that
//! actually writes. `eprintln!` here is the *one* sanctioned raw write.

use once_cell::sync::Lazy;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use bone_core::prelude::Instant;

// 0=error 1=warn 2=info 3=debug 4=trace. Default info (2). Single
// source of truth for the whole process; external level bridges delegate here.
static LEVEL: AtomicU8 = AtomicU8::new(2);

/// Parse `error|warn|info|debug|trace` or `0..4`. Unknown / unset =>
/// `None` (caller keeps the default). `info` is the default elsewhere.
pub fn parse_level(s: &str) -> Option<u8> {
    match s.trim().to_lowercase().as_str() {
        "error" | "0" => Some(0),
        "warn" | "1" => Some(1),
        "info" | "2" => Some(2),
        "debug" | "3" => Some(3),
        "trace" | "4" => Some(4),
        _ => None,
    }
}

pub fn set_level(level: u8) {
    let l = level.min(4);
    LEVEL.store(l, Ordering::Relaxed);
    // Keep the `log` facade's static max-level gate in sync. Without
    // this, `log::info!` etc. short-circuit against the max-level set
    // once at init() and runtime level changes never open the gate even though our atomic
    // changed. Found by ROCK_LOG=error + --verbose showing nothing.
    log::set_max_level(facade_max());
}

/// The facade's static gate: the installed filter's widest level, else the
/// single level.
fn facade_max() -> log::LevelFilter {
    match FILTER.get() {
        Some(filter) => filter.0.filter(),
        None => level_to_log(level()),
    }
}

pub fn level() -> u8 {
    LEVEL.load(Ordering::Relaxed)
}

fn level_to_log(level: u8) -> log::LevelFilter {
    match level {
        0 => log::LevelFilter::Error,
        1 => log::LevelFilter::Warn,
        2 => log::LevelFilter::Info,
        3 => log::LevelFilter::Debug,
        _ => log::LevelFilter::Trace,
    }
}

fn log_to_level(l: log::Level) -> u8 {
    match l {
        log::Level::Error => 0,
        log::Level::Warn => 1,
        log::Level::Info => 2,
        log::Level::Debug => 3,
        log::Level::Trace => 4,
    }
}

/// Third-party crate target prefixes whose info/debug chatter is
/// noise for us (decode progress, etc.). Capped at warn so their
/// real problems still surface. This replaces the per-crate
/// filtering (`RUST_LOG=info,symphonia=warn`) that env_logger gave
/// and the single-global-level unification dropped. `symphonia`
/// (audio decode via rodio) floods `ignoring unknown chunk` /
/// `selected vorbis mapper` at info — that's the motivating case.
const NOISY_TARGET_PREFIXES: &[&str] = &["symphonia"];

fn is_noisy(target: &str) -> bool {
    NOISY_TARGET_PREFIXES
        .iter()
        .any(|p| target.starts_with(p))
}

/// Effective max level for a target: noisy third-party targets are
/// capped at warn (1) even when the global level is higher.
fn effective_max(target: &str) -> u8 {
    let l = level();
    if is_noisy(target) { l.min(1) } else { l }
}

/// Per-target filtering in `RUST_LOG` directive syntax, for a process that
/// installs it with [`init_filtered`] in place of the single level.
pub struct Filter(env_filter::Filter);

impl Filter {
    /// `default` for every target, refined by `directives` (`info`,
    /// `my_crate=debug,warn`, ...), then by `overrides`. Unparsable
    /// directives are an error rather than silently dropped.
    pub fn parse(
        default: log::LevelFilter,
        directives: Option<&str>,
        overrides: &[(&str, log::LevelFilter)],
    ) -> Result<Filter, String> {
        let mut builder = env_filter::Builder::new();
        builder.filter_level(default);
        if let Some(spec) = directives {
            builder.try_parse(spec).map_err(|e| e.to_string())?;
        }
        for (target, level) in overrides {
            builder.filter(Some(target), *level);
        }
        Ok(Filter(builder.build()))
    }

    pub fn enabled(&self, target: &str, level: log::Level) -> bool {
        self.0.enabled(&log::Metadata::builder().target(target).level(level).build())
    }
}

/// Where the log lines go.
pub enum Sink {
    Stderr,
    File(std::fs::File),
}

static FILTER: OnceLock<Filter> = OnceLock::new();
static FILE_SINK: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

fn target_enabled(level: log::Level, target: &str) -> bool {
    match FILTER.get() {
        Some(filter) => filter.enabled(target, level),
        None => log_to_level(level) <= effective_max(target),
    }
}

fn level_tag(l: log::Level) -> &'static str {
    match l {
        log::Level::Error => "ERROR",
        log::Level::Warn => "WARN ",
        log::Level::Info => "INFO ",
        log::Level::Debug => "DEBUG",
        log::Level::Trace => "TRACE",
    }
}

/// SGR sequence for a level, or `""` when colour is off. Only error and
/// warn get one: colouring every line is noise, and the point is that the
/// two that need action stand out of a stream the application and its driver both
/// write to.
fn level_color(l: log::Level) -> &'static str {
    match l {
        log::Level::Error => "\x1b[1;31m", // bold red
        log::Level::Warn => "\x1b[33m",    // yellow
        _ => "",
    }
}

const SGR_RESET: &str = "\x1b[0m";

/// Colour is on only when stderr is a terminal, so piped/captured output
/// (sim harnesses, `2>` redirects, CI) stays byte-clean. `ROCK_LOG_COLOR`
/// = `always` | `never` | `auto` overrides; `NO_COLOR` (any value) forces
/// off, per no-color.org.
static COLOR: Lazy<bool> = Lazy::new(|| {
    match std::env::var("ROCK_LOG_COLOR").as_deref().map(str::trim) {
        Ok("always") | Ok("1") => return true,
        Ok("never") | Ok("0") => return false,
        _ => {}
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::IsTerminal::is_terminal(&std::io::stderr())
});

/// Body (without the time/level prefix). `target` is the deliberate
/// `[CATEGORY]` only when it's one of our short tags; a module-path
/// target (contains "::") or empty is third-party / untargeted and
/// prints bare. Pure for testability.
fn fmt_body(target: &str, args: &std::fmt::Arguments) -> String {
    if target.is_empty() || target.contains("::") {
        format!("{}", args)
    } else {
        format!("[{}] {}", target, args)
    }
}

// Shared log epoch (UNIX millis). A parent and the child
// process it spawns are separate processes, each with its own logger
// install. With a per-process monotonic `Instant` origin their
// interleaved `[<elapsed>s]` timestamps jump backward every hot-reload
// / restart.txt respawn (the child's clock restarts at ~0 while the
// long-lived driver is at 100s+). Anchoring both to one wall-clock
// epoch keeps the combined stream monotonic: the first process to
// init() sets ROCK_LOG_EPOCH_MS; every child inherits it via the
// environment and reports elapsed against the same origin.
static EPOCH_MS: Lazy<u128> = Lazy::new(|| {
    if let Ok(v) = std::env::var("ROCK_LOG_EPOCH_MS") {
        if let Ok(ms) = v.parse::<u128>() {
            return ms;
        }
    }
    let now = unix_millis(SystemTime::now());
    // SAFETY: init() (the only caller, via Lazy::force) runs at process
    // entry before any thread that concurrently mutates the environment;
    // children only ever take the parse branch above (var already set).
    unsafe { std::env::set_var("ROCK_LOG_EPOCH_MS", now.to_string()) };
    now
});

/// Separator between the stamp and the body for a record DECLARED a
/// wall-clock measurement at its log site, in place of the ordinary space.
///
/// A record whose content IS a duration or a frame counter cannot be compared
/// across two runs of the same work, so differential consumers can drop it.
/// The declaration rides in the stamp
/// rather than in the message, so the drop never has to sniff for an `ms`
/// suffix. One character, so a declared record and a plain one align.
///
/// The exact stamp grammar is RESERVED PROTOCOL between this emitter and that
/// harness. A `print()` reproducing it byte for byte is indistinguishable from
/// a log record, and always was -- the harness already strips such a prefix
/// and loses the category with it; the declaration only adds one more
/// consequence to text that was already protocol. What the strict grammar
/// buys is that ordinary application output cannot match it by ACCIDENT.
pub const WALL_CLOCK_MARK: &str = "~";

/// The full emitted line, minus colour. Pure so both the emitter and its
/// tests -- and the harness that has to parse it back -- read one grammar.
fn format_line(
    elapsed: f64,
    level: log::Level,
    target: &str,
    args: &std::fmt::Arguments,
    wall_clock: bool,
) -> String {
    format!(
        "[{:>8.3}s {}]{}{}",
        elapsed,
        level_tag(level),
        if wall_clock { WALL_CLOCK_MARK } else { " " },
        fmt_body(target, args),
    )
}

fn elapsed_secs() -> f64 {
    let now = unix_millis(SystemTime::now());
    now.saturating_sub(*EPOCH_MS) as f64 / 1000.0
}

struct Logger;

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        target_enabled(m.level(), m.target())
    }
    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // The whole line is wrapped, not just the level tag -- the body is
        // the part you read while scrolling for what went wrong.
        emit(record.level(), record.target(), record.args(), false);
    }
    fn flush(&self) {}
}

fn emit(level: log::Level, target: &str, args: &std::fmt::Arguments, wall_clock: bool) {
    if let Some(table) = record_table(level) {
        table.lock().unwrap().record(target, &args.to_string(), elapsed_secs());
    }
    let line = format_line(elapsed_secs(), level, target, args, wall_clock);
    if let Some(file) = FILE_SINK.get() {
        let mut file = file.lock().unwrap();
        if let Err(e) = writeln!(file, "{line}") {
            // The log file is the only channel this process configured, so
            // its failure goes to stderr with the line it lost.
            eprintln!("rock-log: cannot write the log file ({e}): {line}");
        }
        return;
    }
    let color = if *COLOR { level_color(level) } else { "" };
    let reset = if color.is_empty() { "" } else { SGR_RESET };
    eprintln!("{}{}{}", color, line, reset);
}

/// Emit a record DECLARED a wall-clock measurement at its log site: its
/// content is a duration, a frame counter, or otherwise a property of when the
/// process ran. Same backend, same gate, same format as every other record --
/// only the separator differs (see [`WALL_CLOCK_MARK`]).
///
/// This bypasses the `log` facade because the facade has no channel for the
/// declaration; it does not bypass the logger.
pub fn emit_wall_clock(level: log::Level, target: &str, args: std::fmt::Arguments) {
    init();
    if !target_enabled(level, target) {
        return;
    }
    emit(level, target, &args, true);
}

static LOGGER: Logger = Logger;
static INIT: std::sync::Once = std::sync::Once::new();

/// Idempotent. Reads `ROCK_LOG`, installs the global logger. Safe to
/// call from every process entrypoint.
/// Panics if another logger is installed or the clock precedes the UNIX epoch.
pub fn init() {
    INIT.call_once(|| {
        // Establish (or inherit) the shared epoch and publish it to the
        // environment before any child is spawned.
        Lazy::force(&EPOCH_MS);
        log::set_logger(&LOGGER).expect("failed to install logger");
        if let Ok(v) = std::env::var("ROCK_LOG") {
            if let Some(l) = parse_level(&v) {
                set_level(l); // also syncs log::max_level
            }
        }
    });
    // set_logger is one-shot; set_level (here + at runtime via
    // --verbose / the FFI bridge) keeps the facade max-level == our
    // level. This final sync covers ROCK_LOG-unset (set_level above
    // not called) and idempotent re-init.
    log::set_max_level(facade_max());
}

/// [`init`] with per-target `filter` in place of the single level (`ROCK_LOG`
/// and [`set_level`] no longer gate anything), writing to `sink`.
/// Panics if a filter or sink was already installed.
pub fn init_filtered(filter: Filter, sink: Sink) {
    if FILTER.set(filter).is_err() {
        panic!("rock-log: a filter is already installed");
    }
    if let Sink::File(file) = sink {
        if FILE_SINK.set(Mutex::new(file)).is_err() {
            panic!("rock-log: a log file is already installed");
        }
    }
    init();
}

// ---- error and warning records: what an on-screen development badge reads ----

/// One distinct ERROR or WARN record: the target plus the message's first line,
/// so a traceback or a varying tail does not split one fault into many rows.
#[derive(Clone, Debug, PartialEq)]
pub struct LogRecord {
    pub target: String,
    pub message: String,
    pub count: u64,
    pub last_seen_secs: f64,
}

/// Distinct records kept per level; past it the least recently seen is evicted
/// and counted, so the table stays bounded without losing the fact of a record.
pub const RECORD_CAP: usize = 64;

#[derive(Default)]
pub struct RecordTable {
    rows: Vec<LogRecord>,
    total: u64,
    evicted: u64,
}

impl RecordTable {
    pub fn record(&mut self, target: &str, message: &str, now_secs: f64) {
        self.total += 1;
        let first_line = message.lines().next().unwrap_or("");
        if let Some(row) = self
            .rows
            .iter_mut()
            .find(|r| r.target == target && r.message == first_line)
        {
            row.count += 1;
            row.last_seen_secs = now_secs;
            return;
        }
        if self.rows.len() >= RECORD_CAP {
            let stalest = self
                .rows
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.last_seen_secs.total_cmp(&b.1.last_seen_secs))
                .map(|(i, _)| i)
                .expect("a full table has rows");
            self.rows.swap_remove(stalest);
            self.evicted += 1;
        }
        self.rows.push(LogRecord {
            target: target.to_string(),
            message: first_line.to_string(),
            count: 1,
            last_seen_secs: now_secs,
        });
    }

    /// Every record emitted, evicted rows included.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Distinct rows evicted to stay under `RECORD_CAP`.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }

    /// Rows, most recently seen first.
    pub fn rows(&self) -> Vec<LogRecord> {
        let mut rows = self.rows.clone();
        rows.sort_by(|a, b| b.last_seen_secs.total_cmp(&a.last_seen_secs));
        rows
    }
}

static ERRORS: Lazy<Mutex<RecordTable>> = Lazy::new(|| Mutex::new(RecordTable::default()));
static WARNINGS: Lazy<Mutex<RecordTable>> = Lazy::new(|| Mutex::new(RecordTable::default()));

fn record_table(level: log::Level) -> Option<&'static Mutex<RecordTable>> {
    match level {
        log::Level::Error => Some(&ERRORS),
        log::Level::Warn => Some(&WARNINGS),
        _ => None,
    }
}

/// Every record at `level` this process emitted, Rust and Rock alike. Only
/// ERROR and WARN are kept; other levels read as zero.
pub fn records_total(level: log::Level) -> u64 {
    record_table(level).map_or(0, |t| t.lock().unwrap().total())
}

pub fn records_evicted(level: log::Level) -> u64 {
    record_table(level).map_or(0, |t| t.lock().unwrap().evicted())
}

/// Distinct records at `level`, most recently seen first.
pub fn records(level: log::Level) -> Vec<LogRecord> {
    record_table(level).map_or_else(Vec::new, |t| t.lock().unwrap().rows())
}

// ---- log_once / log_throttled filter state (the one dedup/throttle) ----

static DEDUP: Lazy<Mutex<HashSet<&'static str>>> =
    Lazy::new(|| Mutex::new(HashSet::new()));
static THROTTLE: Lazy<Mutex<HashMap<&'static str, Instant>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Sites that dedup per SUBJECT rather than per site: an offending image
/// handle, an unknown enum int. Separate from `DEDUP` only because the key has
/// a second component; it is the same store in every other sense and
/// `reset_dedup`, the image and the count all cover both.
static DEDUP_SUBJECT: Lazy<Mutex<HashSet<(&'static str, u64)>>> =
    Lazy::new(|| Mutex::new(HashSet::new()));

/// True the first time `key` (a `concat!(file!(),":",line!())`) is
/// seen, false after. The emit itself goes through `log::` in the macro.
pub fn once_should_emit(key: &'static str) -> bool {
    DEDUP.lock().unwrap().insert(key)
}

/// True the first time this `(key, subject)` pair is seen.
///
/// The variant for a diagnostic whose interesting part is WHICH thing was
/// wrong. A site-only key reports the first offender and swallows every later
/// one, so the log shows one row where eight are wrong while the failure count
/// stays correct -- which is why the subject belongs IN the key rather than
/// only in the message. A shared set lets one reset reach every caller.
pub fn once_should_emit_for(key: &'static str, subject: u64) -> bool {
    DEDUP_SUBJECT.lock().unwrap().insert((key, subject))
}

/// True if `key` hasn't fired within `interval`. Records the time on
/// emit. The caller emits through `log::` after this suppression check.
pub fn throttle_should_emit(key: &'static str, interval: Duration) -> bool {
    let mut m = THROTTLE.lock().unwrap();
    let now = Instant::now();
    match m.get(key) {
        Some(&last) if now.duration_since(last) < interval => false,
        _ => {
            m.insert(key, now);
            true
        }
    }
}

/// Clear dedup + throttle state (tests; hot reload).
pub fn reset_dedup() {
    DEDUP.lock().unwrap().clear();
    DEDUP_SUBJECT.lock().unwrap().clear();
    THROTTLE.lock().unwrap().clear();
}

/// Every suppression record this logger holds, copied out.
///
/// The sim worker's entry boundary needs boot's IMAGE rather than an empty
/// store, for the reason the Lua half already documents: a fresh process
/// reaches its first entry with whatever boot itself logged already deduped, so
/// clearing would make a worker emit warnings no isolated run has.
pub struct DedupImage {
    once: HashSet<&'static str>,
    subject: HashSet<(&'static str, u64)>,
    throttle: HashMap<&'static str, Instant>,
}

impl DedupImage {
    /// Distinct suppression records this image holds, on the same scale
    /// `dedup_count` reports so a caller can compare the two.
    pub fn len(&self) -> usize {
        self.once.len() + self.subject.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub fn dedup_image() -> DedupImage {
    DedupImage {
        once: DEDUP.lock().unwrap().clone(),
        subject: DEDUP_SUBJECT.lock().unwrap().clone(),
        throttle: THROTTLE.lock().unwrap().clone(),
    }
}

/// Put a previously copied image back, replacing whatever has accumulated
/// since. Every store the image covers is replaced, never merged: a merge
/// would keep a successor's warnings suppressed by its predecessor, which is
/// the whole defect.
pub fn restore_dedup_image(image: &DedupImage) {
    *DEDUP.lock().unwrap() = image.once.clone();
    *DEDUP_SUBJECT.lock().unwrap() = image.subject.clone();
    *THROTTLE.lock().unwrap() = image.throttle.clone();
}

/// Distinct `log_once!` call sites that have fired, site-keyed and
/// subject-keyed together.
pub fn dedup_count() -> usize {
    DEDUP.lock().unwrap().len() + DEDUP_SUBJECT.lock().unwrap().len()
}

/// Distinct `log_throttled!` call sites currently tracked.
pub fn throttle_count() -> usize {
    THROTTLE.lock().unwrap().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression: set_level must keep the `log` facade's static
    // max-level in sync, else `log::info!` etc. short-circuit against
    // the level set once at init() and runtime level changes never open
    // the gate. Reproduced
    // as ROCK_LOG=error + --verbose showing nothing.
    // `log::max_level()` is the facade's static gate that `log::info!`
    // short-circuits against; asserting it directly pins the bug
    // without an installed logger (`log_enabled!` is always false
    // under the test NopLogger so it can't distinguish levels).
    #[test]
    fn set_level_syncs_facade_max_level() {
        set_level(0);
        assert_eq!(log::max_level(), log::LevelFilter::Error);

        set_level(2);
        assert_eq!(log::max_level(), log::LevelFilter::Info);

        // The exact --verbose-over-ROCK_LOG=error case: drop to
        // error, then raise via `level().max(3)`. Must open to debug.
        set_level(0);
        set_level(level().max(3));
        assert_eq!(log::max_level(), log::LevelFilter::Debug);
        assert_eq!(level(), 3);

        set_level(4);
        assert_eq!(log::max_level(), log::LevelFilter::Trace);
    }

    // Regression: noisy third-party targets (symphonia decode chatter,
    // surfaced once a process-wide logger was installed) are capped at
    // warn even when the global level is higher; our own UPPERCASE
    // category targets are not.
    #[test]
    fn noisy_third_party_targets_capped_at_warn() {
        set_level(3); // debug — generous global level
        // symphonia info/debug suppressed; warn/error still emit.
        assert!(!is_noisy("AUDIO"));
        assert!(is_noisy("symphonia_core::probe"));
        assert_eq!(effective_max("symphonia_core::probe"), 1); // warn
        assert_eq!(effective_max("AUDIO"), 3); // our cat: full level
        // log_to_level: info=2 > 1 ⇒ symphonia info gated; warn=1 ⇒ ok.
        assert!(log_to_level(log::Level::Info) > effective_max("symphonia_format_ogg"));
        assert!(log_to_level(log::Level::Warn) <= effective_max("symphonia_format_ogg"));
        // our own info still passes at debug global level.
        assert!(log_to_level(log::Level::Info) <= effective_max("AUDIO"));
    }

    /// A wall-clock record is DECLARED at its log site, and the declaration
    /// travels in the stamp the emitter owns, separately from the message an
    /// application writes. It occupies the separator column, so declared and
    /// plain records put their bodies at the same offset.
    #[test]
    fn a_declared_wall_clock_record_is_marked_in_the_stamp() {
        let plain = format_line(
            1.296,
            log::Level::Error,
            "SLOWFRAME",
            &format_args!("frame 60 took 69.7ms"),
            false,
        );
        assert_eq!(plain, "[   1.296s ERROR] [SLOWFRAME] frame 60 took 69.7ms");

        let declared = format_line(
            1.296,
            log::Level::Error,
            "SLOWFRAME",
            &format_args!("frame 60 took 69.7ms"),
            true,
        );
        assert_eq!(declared, "[   1.296s ERROR]~[SLOWFRAME] frame 60 took 69.7ms");
        assert_eq!(
            plain.len(),
            declared.len(),
            "the declaration must not shift the body column"
        );
    }

    #[test]
    fn record_table_keys_on_target_and_first_line() {
        let mut table = RecordTable::default();
        table.record("LEVEL", "rejected: floor\nstack traceback: a", 1.0);
        table.record("LEVEL", "rejected: floor\nstack traceback: b", 2.0);
        table.record("SAVE", "rejected: floor", 3.0);
        assert_eq!(table.total(), 3);
        let rows = table.rows();
        assert_eq!(rows.len(), 2, "one fault with different tails is one row: {rows:?}");
        assert_eq!(rows[0].target, "SAVE", "most recently seen first");
        assert_eq!(rows[1].message, "rejected: floor");
        assert_eq!(rows[1].count, 2);
        assert_eq!(rows[1].last_seen_secs, 2.0);
    }

    #[test]
    fn record_table_evicts_the_least_recently_seen_and_counts_it() {
        let mut table = RecordTable::default();
        for i in 0..RECORD_CAP {
            table.record("T", &format!("fault {i}"), i as f64);
        }
        table.record("T", "fault 0", 100.0);
        table.record("T", "fresh", 101.0);
        let rows = table.rows();
        assert_eq!(rows.len(), RECORD_CAP);
        assert_eq!(table.evicted(), 1);
        assert_eq!(table.total(), RECORD_CAP as u64 + 2);
        assert!(rows.iter().any(|r| r.message == "fault 0"), "a repeat refreshes its row");
        assert!(!rows.iter().any(|r| r.message == "fault 1"), "the stalest row is evicted");
        assert!(rows.iter().any(|r| r.message == "fresh"));
    }

    #[test]
    fn logged_errors_and_warnings_reach_their_own_records() {
        init();
        let warnings_before = records_total(log::Level::Warn);
        log::error!(target: "RLTEST_ERRORS", "error table witness {}", 7);
        log::warn!(target: "RLTEST_ERRORS", "warning table witness {}", 8);
        let witness = |level| -> Vec<LogRecord> {
            records(level).into_iter().filter(|r| r.target == "RLTEST_ERRORS").collect()
        };
        let errors = witness(log::Level::Error);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].message, "error table witness 7");
        let warnings = witness(log::Level::Warn);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].message, "warning table witness 8");
        assert!(records_total(log::Level::Warn) > warnings_before);
        assert!(records(log::Level::Info).is_empty(), "only errors and warnings are kept");
    }

    #[test]
    fn fmt_body_brackets_only_our_categories() {
        let a = format_args!("hi {}", 1).to_string();
        // module-path / empty target -> bare (third-party, untargeted)
        assert_eq!(fmt_body("symphonia_core::x", &format_args!("m")), "m");
        assert_eq!(fmt_body("", &format_args!("m")), "m");
        // our short category -> [CAT] prefix
        assert_eq!(fmt_body("AUDIO", &format_args!("m")), "[AUDIO] m");
        let _ = a;
    }
}

fn unix_millis(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH).expect("system clock is before UNIX epoch").as_millis()
}

#[cfg(test)]
mod release_tests {
    use super::*;
    #[test]
    #[should_panic(expected = "before UNIX epoch")]
    fn pre_epoch_clock_is_reported() {
        unix_millis(UNIX_EPOCH - Duration::from_secs(1));
    }

    #[test]
    fn filtered_logger_writes_enabled_targets_to_its_file() {
        if let Some(path) = std::env::var_os("FILTERED_TEST_CHILD") {
            let file = std::fs::File::create(path).unwrap();
            let filter = Filter::parse(log::LevelFilter::Warn, Some("chosen=debug"), &[("timing", log::LevelFilter::Info)]);
            init_filtered(filter.unwrap(), Sink::File(file));
            log::debug!(target: "chosen", "kept debug");
            log::info!(target: "timing", "kept info");
            log::info!(target: "other", "dropped info");
            log::warn!(target: "other", "kept warn");
            set_level(4);
            log::trace!(target: "chosen", "dropped trace");
            return;
        }
        let dir = std::env::temp_dir().join(format!("rock-log-filtered-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.log");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["release_tests::filtered_logger_writes_enabled_targets_to_its_file", "--exact", "--nocapture"])
            .env("FILTERED_TEST_CHILD", &path).output().unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let lines: Vec<&str> = written.lines().map(|l| l.split_once("] ").unwrap().1).collect();
        assert_eq!(lines, ["[chosen] kept debug", "[timing] kept info", "[other] kept warn"]);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("kept"));
    }

    #[test]
    fn logger_conflict_is_reported() {
        if std::env::var_os("LOGGER_TEST_CHILD").is_some() {
            log::set_logger(&LOGGER).unwrap();
            init();
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["release_tests::logger_conflict_is_reported", "--exact", "--nocapture"])
            .env("LOGGER_TEST_CHILD", "1").output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("install logger"));
    }
}
