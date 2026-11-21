use rock_core::clock::Instant;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static LEVEL: AtomicU8 = AtomicU8::new(2);

fn parse_level(s: &str) -> Option<u8> {
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
    LEVEL.store(level.min(4), Ordering::Relaxed);
    // The facade's static gate must track runtime level changes.
    log::set_max_level(facade_max());
}

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

pub struct Filter(env_filter::Filter);

impl Filter {
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
        self.0
            .enabled(&log::Metadata::builder().target(target).level(level).build())
    }
}

pub enum Sink {
    Stderr,
    File(std::fs::File),
}

static FILTER: OnceLock<Filter> = OnceLock::new();
static FILE_SINK: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

fn target_enabled(record_level: log::Level, target: &str) -> bool {
    if let Some(filter) = FILTER.get() {
        return filter.enabled(target, record_level);
    }
    // Third-party chatter (symphonia decode progress) is capped at warn so real problems still show.
    record_level <= level_to_log(level())
        && (!target.starts_with("symphonia") || record_level <= log::Level::Warn)
}

fn level_color(l: log::Level) -> &'static str {
    match l {
        log::Level::Error => "\x1b[1;31m", // bold red
        log::Level::Warn => "\x1b[33m",    // yellow
        _ => "",
    }
}

const SGR_RESET: &str = "\x1b[0m";

static COLOR: LazyLock<bool> = LazyLock::new(|| {
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

fn fmt_body(target: &str, args: &std::fmt::Arguments) -> String {
    if target.is_empty() || target.contains("::") {
        format!("{}", args)
    } else {
        format!("[{}] {}", target, args)
    }
}

// A shared epoch keeps parent and child elapsed timestamps aligned across restarts.
static EPOCH_MS: LazyLock<u128> = LazyLock::new(|| {
    if let Ok(v) = std::env::var("ROCK_LOG_EPOCH_MS")
        && let Ok(ms) = v.parse::<u128>()
    {
        return ms;
    }
    let now = unix_millis(SystemTime::now());
    // SAFETY: init() (the only caller, via LazyLock::force) runs at process
    // entry before any thread that concurrently mutates the environment;
    // children only ever take the parse branch above (var already set).
    unsafe { std::env::set_var("ROCK_LOG_EPOCH_MS", now.to_string()) };
    now
});

/// Reserved stamp separator marking wall-clock records for differential consumers.
pub const WALL_CLOCK_MARK: &str = "~";

fn format_line(
    elapsed: f64,
    level: log::Level,
    target: &str,
    args: &std::fmt::Arguments,
    wall_clock: bool,
) -> String {
    format!(
        "[{elapsed:>8.3}s {level:<5}]{}{}",
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
        emit(record.level(), record.target(), record.args(), false);
    }
    fn flush(&self) {}
}

fn emit(level: log::Level, target: &str, args: &std::fmt::Arguments, wall_clock: bool) {
    if let Some(table) = record_table(level) {
        table
            .lock()
            .unwrap()
            .record(target, &args.to_string(), elapsed_secs());
    }
    if level == log::Level::Error {
        collect_error(target, args);
    }
    let line = format_line(elapsed_secs(), level, target, args, wall_clock);
    if let Some(file) = FILE_SINK.get() {
        let mut file = file.lock().unwrap();
        if let Err(e) = writeln!(file, "{line}") {
            eprintln!("rock-log: cannot write the log file ({e}): {line}");
        }
        return;
    }
    let color = if *COLOR { level_color(level) } else { "" };
    let reset = if color.is_empty() { "" } else { SGR_RESET };
    eprintln!("{}{}{}", color, line, reset);
}

/// Marks timing-dependent output for differential consumers. The `log` facade
/// has no channel for this declaration, so this calls the logger directly.
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
        LazyLock::force(&EPOCH_MS);
        log::set_logger(&LOGGER).expect("failed to install logger");
        if let Ok(v) = std::env::var("ROCK_LOG")
            && let Some(l) = parse_level(&v)
        {
            set_level(l);
        }
    });
    // Sync the facade even when ROCK_LOG is unset or the logger is already installed.
    log::set_max_level(facade_max());
}

/// [`init`] with per-target `filter` in place of the single level (`ROCK_LOG`
/// and [`set_level`] no longer gate anything), writing to `sink`.
/// Panics if a filter or sink was already installed.
pub fn init_filtered(filter: Filter, sink: Sink) {
    if FILTER.set(filter).is_err() {
        panic!("rock-log: a filter is already installed");
    }
    if let Sink::File(file) = sink
        && FILE_SINK.set(Mutex::new(file)).is_err()
    {
        panic!("rock-log: a log file is already installed");
    }
    init();
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogRecord {
    pub target: String,
    pub message: String,
    pub count: u64,
    pub last_seen_secs: f64,
}

const RECORD_CAP: usize = 64;

#[derive(Default)]
struct RecordTable {
    rows: Vec<LogRecord>,
    total: u64,
    evicted: u64,
}

impl RecordTable {
    fn record(&mut self, target: &str, message: &str, now_secs: f64) {
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

    fn rows(&self) -> Vec<LogRecord> {
        let mut rows = self.rows.clone();
        rows.sort_by(|a, b| b.last_seen_secs.total_cmp(&a.last_seen_secs));
        rows
    }
}

static ERRORS: LazyLock<Mutex<RecordTable>> = LazyLock::new(Mutex::default);
static WARNINGS: LazyLock<Mutex<RecordTable>> = LazyLock::new(Mutex::default);

fn record_table(level: log::Level) -> Option<&'static Mutex<RecordTable>> {
    match level {
        log::Level::Error => Some(&ERRORS),
        log::Level::Warn => Some(&WARNINGS),
        _ => None,
    }
}

pub fn records_total(level: log::Level) -> u64 {
    record_table(level).map_or(0, |t| t.lock().unwrap().total)
}

pub fn records_evicted(level: log::Level) -> u64 {
    record_table(level).map_or(0, |t| t.lock().unwrap().evicted)
}

pub fn records(level: log::Level) -> Vec<LogRecord> {
    record_table(level).map_or_else(Vec::new, |t| t.lock().unwrap().rows())
}

pub const COLLECTED_ERROR_CAP: usize = 256;

struct CollectedError {
    message: String,
    thread: std::thread::ThreadId,
}

#[derive(Default)]
struct CollectedErrors {
    records: Vec<CollectedError>,
    overflow: u64,
}

static COLLECTED_ERRORS: Mutex<Option<CollectedErrors>> = Mutex::new(None);

fn collect_error(target: &str, args: &std::fmt::Arguments) {
    let mut collected = COLLECTED_ERRORS.lock().unwrap();
    let Some(collected) = collected.as_mut() else {
        return;
    };
    if collected.records.len() >= COLLECTED_ERROR_CAP {
        collected.overflow += 1;
    } else {
        collected.records.push(CollectedError {
            message: fmt_body(target, args),
            thread: std::thread::current().id(),
        });
    }
}

pub fn collect_errors() {
    *COLLECTED_ERRORS.lock().unwrap() = Some(CollectedErrors::default());
}

pub fn collected_errors() -> Vec<String> {
    collected_errors_for(None)
}

pub fn scoped_errors() -> Vec<String> {
    collected_errors_for(Some(std::thread::current().id()))
}

fn collected_errors_for(thread: Option<std::thread::ThreadId>) -> Vec<String> {
    COLLECTED_ERRORS
        .lock()
        .unwrap()
        .iter()
        .flat_map(|collected| &collected.records)
        .filter(|record| thread.is_none_or(|thread| record.thread == thread))
        .map(|record| record.message.clone())
        .collect()
}

pub fn collected_error_overflow() -> u64 {
    COLLECTED_ERRORS
        .lock()
        .unwrap()
        .as_ref()
        .map_or(0, |c| c.overflow)
}

pub fn take_collected_error(record: &str) -> bool {
    take_collected_error_for(record, None)
}

pub fn take_scoped_error(record: &str) -> bool {
    take_collected_error_for(record, Some(std::thread::current().id()))
}

fn take_collected_error_for(record: &str, thread: Option<std::thread::ThreadId>) -> bool {
    let mut collected = COLLECTED_ERRORS.lock().unwrap();
    let Some(records) = collected.as_mut().map(|c| &mut c.records) else {
        return false;
    };
    records
        .iter()
        .position(|r| r.message == record && thread.is_none_or(|thread| r.thread == thread))
        .map(|index| records.remove(index))
        .is_some()
}

static DEDUP: LazyLock<Mutex<HashSet<&'static str>>> = LazyLock::new(Mutex::default);
static DEDUP_SUBJECT: LazyLock<Mutex<HashSet<(&'static str, u64)>>> = LazyLock::new(Mutex::default);
static THROTTLE: LazyLock<Mutex<HashMap<&'static str, Instant>>> = LazyLock::new(Mutex::default);

pub fn once_should_emit(key: &'static str) -> bool {
    DEDUP.lock().unwrap().insert(key)
}

pub fn once_should_emit_for(key: &'static str, subject: u64) -> bool {
    DEDUP_SUBJECT.lock().unwrap().insert((key, subject))
}

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

pub fn reset_dedup() {
    DEDUP.lock().unwrap().clear();
    DEDUP_SUBJECT.lock().unwrap().clear();
    THROTTLE.lock().unwrap().clear();
}

pub fn dedup_count() -> usize {
    DEDUP.lock().unwrap().len() + DEDUP_SUBJECT.lock().unwrap().len()
}

pub fn throttle_count() -> usize {
    THROTTLE.lock().unwrap().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The level is process-wide and the harness runs tests in parallel.
    static LOG_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock_logger() -> std::sync::MutexGuard<'static, ()> {
        LOG_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn set_level_syncs_facade_max_level() {
        let _logger = lock_logger();
        set_level(0);
        assert_eq!(log::max_level(), log::LevelFilter::Error);

        set_level(2);
        assert_eq!(log::max_level(), log::LevelFilter::Info);

        set_level(0);
        set_level(level().max(3));
        assert_eq!(log::max_level(), log::LevelFilter::Debug);
        assert_eq!(level(), 3);

        set_level(4);
        assert_eq!(log::max_level(), log::LevelFilter::Trace);
    }

    #[test]
    fn noisy_third_party_targets_capped_at_warn() {
        let _logger = lock_logger();
        set_level(0);
        assert!(!target_enabled(log::Level::Warn, "symphonia_core::probe"));
        set_level(3);
        assert!(!target_enabled(log::Level::Info, "symphonia_core::probe"));
        assert!(!target_enabled(log::Level::Debug, "symphonia_format_ogg"));
        assert!(target_enabled(log::Level::Warn, "symphonia_format_ogg"));
        assert!(target_enabled(log::Level::Debug, "AUDIO"));
    }

    #[test]
    fn checked_runs_collect_error_records_up_to_the_cap() {
        let _logger = lock_logger();
        init();
        set_level(2);
        collect_errors();
        log::warn!(target: "COLLECT", "not an error");
        log::error!(target: "COLLECT", "first");
        log::error!(target: "COLLECT", "second");
        assert_eq!(collected_errors(), ["[COLLECT] first", "[COLLECT] second"]);
        assert!(take_collected_error("[COLLECT] second"));
        assert!(!take_collected_error("[COLLECT] second"));
        std::thread::spawn(|| log::error!(target: "COLLECT", "first"))
            .join()
            .unwrap();
        assert_eq!(scoped_errors(), ["[COLLECT] first"]);
        assert!(take_scoped_error("[COLLECT] first"));
        assert!(!take_scoped_error("[COLLECT] first"));
        assert_eq!(
            collected_errors(),
            ["[COLLECT] first"],
            "the equal worker record remains unverified"
        );
        for i in 0..COLLECTED_ERROR_CAP {
            log::error!(target: "COLLECT", "filler {i}");
        }
        assert_eq!(collected_errors().len(), COLLECTED_ERROR_CAP);
        assert_eq!(collected_error_overflow(), 1);
        collect_errors();
        assert!(collected_errors().is_empty());
        assert_eq!(collected_error_overflow(), 0);
    }

    #[test]
    fn fmt_body_brackets_only_our_categories() {
        assert_eq!(fmt_body("symphonia_core::x", &format_args!("m")), "m");
        assert_eq!(fmt_body("", &format_args!("m")), "m");
        assert_eq!(fmt_body("AUDIO", &format_args!("m")), "[AUDIO] m");
    }
}

fn unix_millis(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .expect("system clock is before UNIX epoch")
        .as_millis()
}

#[cfg(test)]
mod release_tests {
    use super::*;

    fn test_child(test: &str) -> std::process::Command {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child.args([test, "--exact", "--nocapture"]);
        child
    }

    #[test]
    #[should_panic(expected = "before UNIX epoch")]
    fn pre_epoch_clock_is_reported() {
        unix_millis(UNIX_EPOCH - Duration::from_secs(1));
    }

    #[test]
    fn filtered_logger_writes_enabled_targets_to_its_file() {
        if let Some(path) = std::env::var_os("FILTERED_TEST_CHILD") {
            let file = std::fs::File::create(path).unwrap();
            let filter = Filter::parse(
                log::LevelFilter::Warn,
                Some("chosen=debug"),
                &[("timing", log::LevelFilter::Info)],
            );
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
        let output =
            test_child("release_tests::filtered_logger_writes_enabled_targets_to_its_file")
                .env("FILTERED_TEST_CHILD", &path)
                .output()
                .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let lines: Vec<&str> = written
            .lines()
            .map(|l| l.split_once("] ").unwrap().1)
            .collect();
        assert_eq!(
            lines,
            [
                "[chosen] kept debug",
                "[timing] kept info",
                "[other] kept warn"
            ]
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("kept"));
    }
}
