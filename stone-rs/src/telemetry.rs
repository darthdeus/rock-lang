use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::ffi::{c_char, CStr};
use std::fs::{self, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

static TELEMETRY: Mutex<Option<TelemetryClient>> = Mutex::new(None);

const MAX_FIELD_CHARS: usize = 128;
const MAX_GAME_CHARS: usize = 64;
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_EVENT_BYTES: usize = 8 * 1024;
const MAX_DATA_KEYS: usize = 32;
const MAX_DATA_KEY_CHARS: usize = 64;
const MAX_DATA_STRING_CHARS: usize = 256;
const MAX_DATA_ARRAY_ITEMS: usize = 32;
const MAX_APPEND_BATCH_EVENTS: usize = 64;
const MAX_TIMESTAMP: i64 = 4_102_444_800;
const MAX_SPOOL_BYTES: u64 = 8 * 1024 * 1024;
const ENVELOPE_VERSION: i64 = 2;
const REVOCATION_FILE: &str = ".telemetry-revoked";
const CONSENT_FILE: &str = ".telemetry-consent";
const LAST_UPLOAD_FILE: &str = ".telemetry-last-upload";
const WORKER_RUNNING: u8 = 0;
const WORKER_DRAINING: u8 = 1;
const WORKER_DISCARDING: u8 = 2;

struct TelemetryIdentity {
    game: String,
    build: String,
    platform: String,
    channel: String,
}

struct TelemetrySpool {
    root: PathBuf,
    identity: TelemetryIdentity,
    session: Uuid,
    consent_generation: Uuid,
    next_seq: u64,
    events_path: PathBuf,
    io_lock: Arc<Mutex<()>>,
    writable: bool,
}

struct TelemetryDrain {
    events_path: PathBuf,
    cursor_path: PathBuf,
    quarantine_path: PathBuf,
    last_upload_path: PathBuf,
    consent_generation: Uuid,
    io_lock: Arc<Mutex<()>>,
}

struct TelemetryTransport {
    endpoint: String,
    token: String,
    game: String,
    channel: String,
}

enum UploadDisposition {
    Accepted,
    Retry(io::Error),
    Reject(u16),
    Blocked(io::Error),
    Cancelled,
}

enum HttpFailureClass {
    Retry,
    Reject,
    Blocked,
}

enum DrainOutcome {
    Complete,
    Retry(io::Error),
    Blocked(io::Error),
    Cancelled,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StopMode {
    Drain,
    Discard,
}

enum UploadCommand {
    Wake(Instant),
    Finish { mode: StopMode, deadline: Instant },
}

struct UploadWorker {
    tx: SyncSender<UploadCommand>,
    control: Arc<AtomicU8>,
    shutdown_budget: Duration,
    handle: JoinHandle<()>,
}

#[derive(Clone)]
struct UploadWaker {
    tx: SyncSender<UploadCommand>,
    control: Arc<AtomicU8>,
}

struct QueuedTelemetryEvent {
    name: String,
    timestamp: i64,
    data: Option<String>,
}

enum AppendCommand {
    Record(QueuedTelemetryEvent),
    Flush(Sender<bool>),
    Finish,
}

struct AppendWorker {
    tx: Sender<AppendCommand>,
    handle: JoinHandle<()>,
}

#[derive(Clone, Copy)]
struct UploadTiming {
    flush_window: Duration,
    retry_initial: Duration,
    retry_max: Duration,
    revocation_poll: Duration,
    request_timeout: Duration,
    shutdown_budget: Duration,
    shutdown_request_timeout: Duration,
}

impl Default for UploadTiming {
    fn default() -> Self {
        Self {
            flush_window: Duration::from_secs(60),
            retry_initial: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
            revocation_poll: Duration::from_millis(100),
            request_timeout: Duration::from_millis(500),
            shutdown_budget: Duration::from_secs(3),
            shutdown_request_timeout: Duration::from_secs(2),
        }
    }
}

struct TelemetryClient {
    append_worker: AppendWorker,
    upload_worker: UploadWorker,
    #[cfg(test)]
    events_path: PathBuf,
    #[cfg(test)]
    io_lock: Arc<Mutex<()>>,
    _root_lock: TelemetryRootLock,
}

struct TelemetryRootLock {
    _file: fs::File,
}

struct ConsentJournalLock {
    _file: fs::File,
}

#[derive(Clone, Copy)]
struct ConsentState {
    enabled: bool,
    generation: Uuid,
}

struct AppendFailure {
    error: io::Error,
    rollback_failed: bool,
}

#[derive(Debug)]
struct SpoolAtCapacity;

impl std::fmt::Display for SpoolAtCapacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("telemetry spool reached its 8 MiB cap")
    }
}

impl std::error::Error for SpoolAtCapacity {}

impl TelemetrySpool {
    #[cfg(test)]
    fn open(root: &Path, identity: TelemetryIdentity) -> io::Result<Self> {
        let consent_generation = load_or_create_consent_generation(root)?;
        Self::open_with_io_lock(root, identity, consent_generation, Arc::new(Mutex::new(())))
    }

    fn open_with_io_lock(
        root: &Path,
        identity: TelemetryIdentity,
        consent_generation: Uuid,
        io_lock: Arc<Mutex<()>>,
    ) -> io::Result<Self> {
        identity.validate()?;
        let stats_dir = prepare_channel_stats_dir(root, &identity.channel)?;

        let events_path = stats_dir.join("events.jsonl");
        truncate_torn_tail(&events_path)?;
        create_empty_queue(&events_path)?;
        Ok(Self {
            root: root.into(),
            identity,
            session: Uuid::new_v4(),
            consent_generation,
            next_seq: 1,
            events_path,
            io_lock,
            writable: true,
        })
    }

    #[cfg(test)]
    fn record_stat(&mut self, name: &str, timestamp: i64) -> io::Result<()> {
        self.record_stat_data(name, timestamp, None)
    }

    #[cfg(test)]
    fn record_stat_data(
        &mut self,
        name: &str,
        timestamp: i64,
        data: Option<&str>,
    ) -> io::Result<()> {
        let events_path = self.events_path.clone();
        self.record_stat_with_persist(name, timestamp, data, move |file| {
            file.sync_all()?;
            sync_parent(&events_path)
        })
    }

    #[cfg(test)]
    fn record_stat_with_persist(
        &mut self,
        name: &str,
        timestamp: i64,
        data: Option<&str>,
        persist: impl FnOnce(&fs::File) -> io::Result<()>,
    ) -> io::Result<()> {
        let event = QueuedTelemetryEvent::new(name, timestamp, data)?;
        self.record_batch_with_persist(&[event], persist)
    }

    fn record_batch(&mut self, events: &[QueuedTelemetryEvent]) -> io::Result<()> {
        let events_path = self.events_path.clone();
        self.record_batch_with_persist(events, move |file| {
            file.sync_all()?;
            sync_parent(&events_path)
        })
    }

    fn record_batch_with_persist(
        &mut self,
        events: &[QueuedTelemetryEvent],
        persist: impl FnOnce(&fs::File) -> io::Result<()>,
    ) -> io::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let io_lock = Arc::clone(&self.io_lock);
        let _guard = io_lock
            .lock()
            .map_err(|_| io::Error::other("telemetry I/O lock was poisoned"))?;
        if telemetry_revoked(&self.root) {
            let _ = discard_stats(&self.root);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "telemetry consent was withdrawn",
            ));
        }
        if !self.writable {
            return Err(io::Error::other("telemetry spool requires reopening"));
        }
        let current_len = fs::metadata(&self.events_path)
            .map(|metadata| metadata.len())
            .or_else(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    Ok(0)
                } else {
                    Err(error)
                }
            })?;
        if current_len >= MAX_SPOOL_BYTES {
            return Err(io::Error::new(io::ErrorKind::StorageFull, SpoolAtCapacity));
        }
        let session = self.session.to_string();
        let consent = self.consent_generation.to_string();
        let mut lines = Vec::new();
        for (index, event) in events.iter().enumerate() {
            let mut record = serde_json::json!({
                "v": ENVELOPE_VERSION,
                "kind": "stat",
                "t": event.timestamp,
                "session": session,
                "consent": consent,
                "seq": self.next_seq + index as u64,
                "build": self.identity.build,
                "platform": self.identity.platform,
                "game": self.identity.game,
                "channel": self.identity.channel,
                "n": event.name,
            });
            if let Some(data) = &event.data {
                record["data"] = validate_event_data(data)?;
            }
            let start = lines.len();
            serde_json::to_writer(&mut lines, &record)?;
            if lines.len() - start > MAX_EVENT_BYTES {
                return Err(invalid_input("telemetry event exceeds 8 KiB"));
            }
            lines.push(b'\n');
        }
        if current_len.saturating_add(lines.len() as u64) > MAX_SPOOL_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "telemetry spool reached its 8 MiB cap",
            ));
        }
        if let Err(failure) = append_transaction(&self.events_path, &lines, persist) {
            if failure.rollback_failed {
                self.writable = false;
            }
            return Err(failure.error);
        }
        if telemetry_revoked(&self.root) {
            discard_stats(&self.root)?;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "telemetry consent was withdrawn",
            ));
        }
        self.next_seq += events.len() as u64;
        Ok(())
    }

    fn discard_legacy_events(&self) -> io::Result<()> {
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| io::Error::other("telemetry I/O lock was poisoned"))?;
        let raw = match fs::read_to_string(&self.events_path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let has_legacy = raw.lines().any(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|event| event.get("v").and_then(Value::as_i64))
                != Some(ENVELOPE_VERSION)
        });
        if !has_legacy {
            return Ok(());
        }
        match fs::remove_file(&self.events_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match fs::remove_file(self.events_path.with_file_name("cursor.log")) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        sync_parent(&self.events_path)
    }
}

impl TelemetryRootLock {
    // `Ok(None)` is contention. It cannot be an `io::ErrorKind`: Windows reports it
    // as ERROR_LOCK_VIOLATION, which decodes to `Uncategorized`, not `WouldBlock`.
    fn acquire(root: &Path) -> io::Result<Option<Self>> {
        let reports = root.join("reports");
        fs::create_dir_all(&reports)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(reports.join(".telemetry.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }
}

impl ConsentJournalLock {
    fn acquire(root: &Path) -> io::Result<Self> {
        let reports = root.join("reports");
        fs::create_dir_all(&reports)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(reports.join(".telemetry-consent.lock"))?;
        file.lock()?;
        Ok(Self { _file: file })
    }
}

fn revocation_path(root: &Path) -> PathBuf {
    root.join("reports").join(REVOCATION_FILE)
}

fn consent_path(root: &Path) -> PathBuf {
    root.join("reports").join(CONSENT_FILE)
}

fn telemetry_revoked(root: &Path) -> bool {
    revocation_path(root).try_exists().unwrap_or(true)
}

fn publish_revocation(root: &Path) -> io::Result<()> {
    let reports = root.join("reports");
    fs::create_dir_all(&reports)?;
    sync_parent(&reports)?;
    let path = revocation_path(root);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    file.sync_all()?;
    sync_parent(&path)
}

fn discard_stats(root: &Path) -> io::Result<()> {
    let path = root.join("reports/stats");
    match fs::remove_dir_all(&path) {
        Ok(()) => sync_parent(&path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn discard_stats_with_io_lock(root: &Path, io_lock: &Mutex<()>) -> io::Result<()> {
    let _guard = io_lock
        .lock()
        .map_err(|_| io::Error::other("telemetry I/O lock was poisoned"))?;
    discard_stats(root)
}

fn load_consent_state_unlocked(root: &Path) -> io::Result<Option<ConsentState>> {
    let path = consent_path(root);
    truncate_torn_tail(&path)?;
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut generations = Vec::new();
    let mut latest = None;
    for line in raw.lines() {
        let mut fields = line.split(' ');
        let (Some(state), Some(generation), None) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid consent journal",
            ));
        };
        let enabled = match state {
            "on" => true,
            "off" => false,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid consent journal",
                ));
            }
        };
        let generation = Uuid::parse_str(generation)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid consent journal"))?;
        if generations.contains(&generation) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "reused consent generation",
            ));
        }
        generations.push(generation);
        latest = Some(ConsentState {
            enabled,
            generation,
        });
    }
    Ok(latest)
}

#[cfg(test)]
fn begin_consent_generation(root: &Path) -> io::Result<Uuid> {
    let path = consent_path(root);
    begin_consent_generation_with_persist(root, |file| {
        file.sync_all()?;
        sync_parent(&path)
    })
}

#[cfg(test)]
fn begin_consent_generation_with_persist(
    root: &Path,
    persist: impl FnOnce(&fs::File) -> io::Result<()>,
) -> io::Result<Uuid> {
    append_consent_state_with_persist(root, true, persist)
}

fn publish_withdrawal(root: &Path) -> io::Result<Uuid> {
    let _lock = ConsentJournalLock::acquire(root)?;
    publish_revocation(root)?;
    let path = consent_path(root);
    append_consent_state_unlocked(root, false, |file| {
        file.sync_all()?;
        sync_parent(&path)
    })
}

#[cfg(test)]
fn append_consent_state_with_persist(
    root: &Path,
    enabled: bool,
    persist: impl FnOnce(&fs::File) -> io::Result<()>,
) -> io::Result<Uuid> {
    let _lock = ConsentJournalLock::acquire(root)?;
    append_consent_state_unlocked(root, enabled, persist)
}

fn append_consent_state_unlocked(
    root: &Path,
    enabled: bool,
    persist: impl FnOnce(&fs::File) -> io::Result<()>,
) -> io::Result<Uuid> {
    let reports = root.join("reports");
    sync_parent(&reports)?;
    if !enabled {
        if let Some(state) = load_consent_state_unlocked(root)? {
            if !state.enabled {
                return Ok(state.generation);
            }
        }
    } else {
        let _ = load_consent_state_unlocked(root)?;
    }
    let path = consent_path(root);
    let generation = Uuid::new_v4();
    let state = if enabled { "on" } else { "off" };
    let line = format!("{state} {generation}\n");
    append_transaction(&path, line.as_bytes(), persist).map_err(|failure| failure.error)?;
    Ok(generation)
}

fn load_or_create_consent_generation(root: &Path) -> io::Result<Uuid> {
    let _lock = ConsentJournalLock::acquire(root)?;
    if let Some(state) = load_consent_state_unlocked(root)? {
        if state.enabled {
            return Ok(state.generation);
        }
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "telemetry consent was withdrawn",
        ));
    }
    if root.join("reports/stats").try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "telemetry queue has no consent generation",
        ));
    }
    let path = consent_path(root);
    append_consent_state_unlocked(root, true, |file| {
        file.sync_all()?;
        sync_parent(&path)
    })
}

fn prepare_opt_in(root: &Path) -> io::Result<Uuid> {
    let _lock = ConsentJournalLock::acquire(root)?;
    let path = consent_path(root);
    let generation = append_consent_state_unlocked(root, true, |file| {
        file.sync_all()?;
        sync_parent(&path)
    })?;
    discard_stats(root)?;
    let path = revocation_path(root);
    match fs::remove_file(&path) {
        Ok(()) => sync_parent(&path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(generation)
}

fn append_transaction(
    path: &Path,
    bytes: &[u8],
    persist: impl FnOnce(&fs::File) -> io::Result<()>,
) -> Result<(), AppendFailure> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| AppendFailure {
            error,
            rollback_failed: false,
        })?;
    let original_len = file
        .metadata()
        .map_err(|error| AppendFailure {
            error,
            rollback_failed: false,
        })?
        .len();
    let result = file.write_all(bytes).and_then(|_| persist(&file));
    if let Err(error) = result {
        let rollback = file
            .set_len(original_len)
            .and_then(|_| file.sync_all())
            .and_then(|_| sync_parent(path));
        return match rollback {
            Ok(()) => Err(AppendFailure {
                error,
                rollback_failed: false,
            }),
            Err(rollback) => Err(AppendFailure {
                error: io::Error::other(format!(
                    "telemetry append failed ({error}); rollback failed ({rollback})"
                )),
                rollback_failed: true,
            }),
        };
    }
    Ok(())
}

impl TelemetryIdentity {
    fn validate(&self) -> io::Result<()> {
        validate_game(&self.game)?;
        validate_field("build", &self.build)?;
        validate_field("platform", &self.platform)?;
        validate_channel(&self.channel)
    }
}

fn validate_game(game: &str) -> io::Result<()> {
    let len = game.chars().count();
    let first = game.as_bytes().first().copied();
    if len == 0
        || len > MAX_GAME_CHARS
        || game == "."
        || game == ".."
        || !first.is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !game.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
    {
        return Err(invalid_input("invalid telemetry game"));
    }
    Ok(())
}

fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

// The same bounds `validate_data` applies in tools/telemetry/ingest.py. They are
// restated here rather than derived because the two processes ship separately: a
// row the client accepts and the server refuses is a quarantined batch, so the
// client has to refuse it first.
fn valid_data_scalar(value: &Value) -> bool {
    match value {
        Value::Bool(_) => true,
        Value::Number(number) => {
            number.is_i64() || number.as_f64().is_some_and(f64::is_finite) && !number.is_u64()
        }
        Value::String(text) => text.chars().count() <= MAX_DATA_STRING_CHARS,
        _ => false,
    }
}

fn validate_event_data(raw: &str) -> io::Result<Value> {
    let parsed: Value =
        serde_json::from_str(raw).map_err(|_| invalid_input("telemetry data is not JSON"))?;
    let Value::Object(fields) = &parsed else {
        return Err(invalid_input("telemetry data must be an object"));
    };
    if fields.len() > MAX_DATA_KEYS {
        return Err(invalid_input("telemetry data has too many keys"));
    }
    for (key, value) in fields {
        let key_len = key.chars().count();
        if key_len == 0 || key_len > MAX_DATA_KEY_CHARS {
            return Err(invalid_input("invalid telemetry data key"));
        }
        if valid_data_scalar(value) {
            continue;
        }
        let Value::Array(items) = value else {
            return Err(invalid_input("invalid telemetry data value"));
        };
        if items.len() > MAX_DATA_ARRAY_ITEMS || !items.iter().all(valid_data_scalar) {
            return Err(invalid_input("invalid telemetry data array"));
        }
    }
    Ok(parsed)
}

fn validate_field(field: &str, value: &str) -> io::Result<()> {
    let len = value.chars().count();
    if len == 0 || len > MAX_FIELD_CHARS {
        return Err(invalid_input(&format!("invalid telemetry {field}")));
    }
    Ok(())
}

fn validate_channel(channel: &str) -> io::Result<()> {
    validate_field("channel", channel)?;
    if channel == "."
        || channel == ".."
        || !channel
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(invalid_input("invalid telemetry channel"));
    }
    Ok(())
}

fn channel_stats_dir(root: &Path, channel: &str) -> io::Result<PathBuf> {
    validate_channel(channel)?;
    Ok(root.join("reports/stats").join(channel))
}

fn prepare_channel_stats_dir(root: &Path, channel: &str) -> io::Result<PathBuf> {
    let directory = channel_stats_dir(root, channel)?;
    fs::create_dir_all(&directory)?;
    sync_parent(&directory)?;
    if let Some(stats) = directory.parent() {
        sync_parent(stats)?;
        if let Some(reports) = stats.parent() {
            sync_parent(reports)?;
        }
    }
    Ok(directory)
}

// Materialize the queue when it is opened, not on the first flush. Opening
// already reports failure as "could not open the gameplay-event queue", which
// is only true if the open actually proves the file is writable; deferring the
// create to the first append turned a permission or full-disk error into a
// silent success followed by a much later append failure. The file is synced
// like an append so an opened-and-empty queue survives a crash the same way a
// flushed one does.
fn create_empty_queue(path: &Path) -> io::Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    file.sync_all()?;
    sync_parent(path)
}

fn truncate_torn_tail(path: &Path) -> io::Result<()> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0_u8; 1];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }

    let mut end = len;
    let mut chunk = [0_u8; 8192];
    let complete_len = loop {
        let start = end.saturating_sub(chunk.len() as u64);
        let read_len = (end - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk[..read_len])?;
        if let Some(index) = chunk[..read_len].iter().rposition(|byte| *byte == b'\n') {
            break start + index as u64 + 1;
        }
        if start == 0 {
            break 0;
        }
        end = start;
    };
    file.set_len(complete_len)?;
    file.sync_all()?;
    sync_parent(path)
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("telemetry path has no parent"))?;
    fs::File::open(parent)?.sync_all()
}

// Windows exposes no directory fsync. Consent eligibility does not depend on this
// barrier: the synced, non-reusable generation in each row is the upload gate.
#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

impl TelemetryDrain {
    #[cfg(test)]
    fn open(root: &Path, channel: &str) -> io::Result<Self> {
        let consent_generation = load_or_create_consent_generation(root)?;
        Self::open_with_io_lock(root, channel, consent_generation, Arc::new(Mutex::new(())))
    }

    fn open_with_io_lock(
        root: &Path,
        channel: &str,
        consent_generation: Uuid,
        io_lock: Arc<Mutex<()>>,
    ) -> io::Result<Self> {
        let stats_dir = prepare_channel_stats_dir(root, channel)?;
        let drain = Self {
            events_path: stats_dir.join("events.jsonl"),
            cursor_path: stats_dir.join("cursor.log"),
            quarantine_path: stats_dir.join("quarantine.jsonl"),
            last_upload_path: root.join("reports").join(LAST_UPLOAD_FILE),
            consent_generation,
            io_lock,
        };
        truncate_torn_tail(&drain.events_path)?;
        Ok(drain)
    }

    fn cursor_for_len(&self, event_len: u64) -> io::Result<u64> {
        let raw = match fs::read(&self.cursor_path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let mut safe = 0;
        for line in raw.split_inclusive(|byte| *byte == b'\n') {
            if !line.ends_with(b"\n") {
                break;
            }
            let Ok(value) = std::str::from_utf8(&line[..line.len() - 1]) else {
                break;
            };
            let mut fields = value.trim_end_matches('\r').split(' ');
            let (Some(generation), Some(value), None) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let (Ok(generation), Ok(value)) = (Uuid::parse_str(generation), value.parse::<u64>())
            else {
                break;
            };
            if generation != self.consent_generation {
                continue;
            }
            if value < safe || value > event_len {
                break;
            }
            safe = value;
        }
        Ok(safe)
    }

    fn acknowledge(&self, cursor: u64) -> io::Result<()> {
        truncate_torn_tail(&self.cursor_path)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.cursor_path)?;
        writeln!(file, "{} {cursor}", self.consent_generation)?;
        file.sync_all()?;
        sync_parent(&self.cursor_path)
    }

    fn pending_bytes(&self) -> io::Result<u64> {
        let len = match fs::metadata(&self.events_path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        Ok(len - self.cursor_for_len(len)?)
    }

    fn quarantine(&self, status: u16, body: &str) -> io::Result<()> {
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| io::Error::other("telemetry I/O lock was poisoned"))?;
        truncate_torn_tail(&self.quarantine_path)?;
        let mut record = serde_json::to_vec(&serde_json::json!({
            "status": status,
            "body": body,
        }))?;
        record.push(b'\n');
        let current_len = fs::metadata(&self.quarantine_path)
            .map(|metadata| metadata.len())
            .or_else(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    Ok(0)
                } else {
                    Err(error)
                }
            })?;
        if current_len.saturating_add(record.len() as u64) > MAX_SPOOL_BYTES {
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.quarantine_path)?;
            file.sync_all()?;
            sync_parent(&self.quarantine_path)?;
        }
        append_transaction(&self.quarantine_path, &record, |file| {
            file.sync_all()?;
            sync_parent(&self.quarantine_path)
        })
        .map_err(|failure| failure.error)
    }

    fn mark_uploaded(&self) -> io::Result<()> {
        let uploaded_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| io::Error::other(format!("system clock before Unix epoch: {error}")))?
            .as_secs();
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| io::Error::other("telemetry I/O lock was poisoned"))?;
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.last_upload_path)?;
        writeln!(file, "{uploaded_at}")?;
        file.sync_all()?;
        sync_parent(&self.last_upload_path)
    }

    fn settle_batch(
        &self,
        disposition: UploadDisposition,
        body: &str,
        pending: u64,
        cursor: &mut u64,
    ) -> io::Result<Option<DrainOutcome>> {
        match disposition {
            UploadDisposition::Accepted => {
                if let Err(error) = self.mark_uploaded() {
                    log::warn!(target: "TELEMETRY", "could not record last upload time: {error}");
                }
            }
            UploadDisposition::Reject(status) => {
                self.quarantine(status, body)?;
                log::warn!(
                    target: "TELEMETRY",
                    "quarantined a batch rejected with HTTP {status}"
                );
            }
            UploadDisposition::Retry(error) => return Ok(Some(DrainOutcome::Retry(error))),
            UploadDisposition::Blocked(error) => return Ok(Some(DrainOutcome::Blocked(error))),
            UploadDisposition::Cancelled => return Ok(Some(DrainOutcome::Cancelled)),
        }
        *cursor += pending;
        self.acknowledge(*cursor)?;
        Ok(None)
    }

    fn reclaim_if_fully_acknowledged(&self) -> io::Result<()> {
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| io::Error::other("telemetry I/O lock was poisoned"))?;
        let len = match fs::metadata(&self.events_path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::remove_file(&self.cursor_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let cursor = self.cursor_for_len(len)?;
        if cursor != len || len == 0 {
            return Ok(());
        }
        fs::remove_file(&self.cursor_path)?;
        fs::remove_file(&self.events_path)?;
        sync_parent(&self.events_path)
    }

    #[cfg(test)]
    fn run(&self, mut send: impl FnMut(&str) -> io::Result<()>) -> io::Result<()> {
        match self.run_while(
            || true,
            |body| match send(body) {
                Ok(()) => UploadDisposition::Accepted,
                Err(error) => UploadDisposition::Retry(error),
            },
        )? {
            DrainOutcome::Complete => Ok(()),
            DrainOutcome::Retry(error) | DrainOutcome::Blocked(error) => Err(error),
            DrainOutcome::Cancelled => Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "telemetry drain cancelled",
            )),
        }
    }

    #[cfg(test)]
    fn run_reclaiming(&self, mut send: impl FnMut(&str) -> io::Result<()>) -> io::Result<()> {
        match self.run_while_reclaiming(
            || true,
            |body| match send(body) {
                Ok(()) => UploadDisposition::Accepted,
                Err(error) => UploadDisposition::Retry(error),
            },
        )? {
            DrainOutcome::Complete => Ok(()),
            DrainOutcome::Retry(error) | DrainOutcome::Blocked(error) => Err(error),
            DrainOutcome::Cancelled => Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "telemetry drain cancelled",
            )),
        }
    }

    fn run_while_reclaiming(
        &self,
        should_continue: impl FnMut() -> bool,
        send: impl FnMut(&str) -> UploadDisposition,
    ) -> io::Result<DrainOutcome> {
        let outcome = self.run_while(should_continue, send)?;
        if matches!(outcome, DrainOutcome::Complete) {
            self.reclaim_if_fully_acknowledged()?;
        }
        Ok(outcome)
    }

    fn run_while(
        &self,
        mut should_continue: impl FnMut() -> bool,
        mut send: impl FnMut(&str) -> UploadDisposition,
    ) -> io::Result<DrainOutcome> {
        let file = match fs::File::open(&self.events_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(DrainOutcome::Complete)
            }
            Err(error) => return Err(error),
        };
        let len = file.metadata()?.len();
        let mut cursor = self.cursor_for_len(len)?;

        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(cursor))?;
        // Rows accumulate into one JSONL body. `pending` counts every byte read
        // since the last acknowledgement, including rows dropped for a stale
        // generation, so the cursor advances past them with the batch that
        // followed them rather than on its own.
        let mut batch = String::new();
        let mut pending = 0_u64;
        let mut line = String::new();
        loop {
            if !should_continue() {
                return Ok(DrainOutcome::Cancelled);
            }
            line.clear();
            let read = reader.read_line(&mut line)?;
            if read == 0 {
                if !batch.is_empty() {
                    if let Some(outcome) =
                        self.settle_batch(send(&batch), &batch, pending, &mut cursor)?
                    {
                        return Ok(outcome);
                    }
                } else if pending > 0 {
                    cursor += pending;
                    self.acknowledge(cursor)?;
                }
                return Ok(DrainOutcome::Complete);
            }
            let body = line.trim_end_matches(['\r', '\n']);
            let keep = event_consent_generation(body) == Some(self.consent_generation);
            if keep && !batch.is_empty() && batch.len() + 1 + body.len() > MAX_BODY_BYTES {
                if let Some(outcome) =
                    self.settle_batch(send(&batch), &batch, pending, &mut cursor)?
                {
                    return Ok(outcome);
                }
                batch.clear();
                pending = 0;
            }
            if keep {
                if !batch.is_empty() {
                    batch.push('\n');
                }
                batch.push_str(body);
            }
            pending += read as u64;
        }
    }
}

fn event_consent_generation(body: &str) -> Option<Uuid> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|event| {
            event
                .get("consent")
                .and_then(|value| value.as_str())
                .and_then(|value| Uuid::parse_str(value).ok())
        })
}

fn schedule_flush(deadline: &mut Option<Instant>, now: Instant, window: Duration, flush_now: bool) {
    if flush_now {
        *deadline = Some(now);
    } else if deadline.is_none() {
        *deadline = Some(now + window);
    }
}

fn worker_wait(upload_at: Option<Instant>, now: Instant, revocation_poll: Duration) -> Duration {
    upload_at
        .map(|deadline| deadline.saturating_duration_since(now))
        .unwrap_or(revocation_poll)
        .min(revocation_poll)
}

fn hmac_sha256_hex(token: &str, body: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC accepts keys of any size");
    mac.update(body.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn classify_http_status(status: u16) -> HttpFailureClass {
    match status {
        408 | 429 | 500..=599 => HttpFailureClass::Retry,
        400 | 409 | 413 | 422 => HttpFailureClass::Reject,
        _ => HttpFailureClass::Blocked,
    }
}

impl TelemetryTransport {
    fn send(&self, agent: &ureq::Agent, body: &str) -> UploadDisposition {
        let signature = hmac_sha256_hex(&self.token, body);
        match agent
            .post(&self.endpoint)
            .header("Content-Type", "application/x-ndjson")
            .header("X-Rock-Game", &self.game)
            .header("X-Rock-Channel", &self.channel)
            .header("X-Rock-Signature", &format!("sha256={signature}"))
            .send(body)
        {
            Ok(_) => UploadDisposition::Accepted,
            Err(ureq::Error::StatusCode(status)) => {
                let error = || io::Error::other(format!("telemetry ingest returned HTTP {status}"));
                match classify_http_status(status) {
                    HttpFailureClass::Retry => UploadDisposition::Retry(error()),
                    HttpFailureClass::Reject => UploadDisposition::Reject(status),
                    HttpFailureClass::Blocked => UploadDisposition::Blocked(error()),
                }
            }
            Err(error) => UploadDisposition::Retry(io::Error::other(error)),
        }
    }
}

fn send_if_consented(
    root: &Path,
    transport: &TelemetryTransport,
    agent: &ureq::Agent,
    body: &str,
) -> UploadDisposition {
    if telemetry_revoked(root) {
        return UploadDisposition::Cancelled;
    }
    transport.send(agent, body)
}

fn final_drain(
    root: &Path,
    drain: &TelemetryDrain,
    transport: &TelemetryTransport,
    deadline: Instant,
    request_timeout: Duration,
) {
    let result = drain.run_while_reclaiming(
        || Instant::now() < deadline && !telemetry_revoked(root),
        |body| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return UploadDisposition::Cancelled;
            }
            let config = ureq::Agent::config_builder()
                .timeout_global(Some(remaining.min(request_timeout)))
                .build();
            let agent: ureq::Agent = config.into();
            send_if_consented(root, transport, &agent, body)
        },
    );
    match result {
        Ok(DrainOutcome::Complete | DrainOutcome::Cancelled) => {}
        Ok(DrainOutcome::Retry(error) | DrainOutcome::Blocked(error)) | Err(error) => {
            log::warn!(target: "TELEMETRY", "shutdown drain stopped: {error}");
        }
    }
}

fn finish_upload(
    root: &Path,
    mode: StopMode,
    deadline: Instant,
    drain: &TelemetryDrain,
    transport: &TelemetryTransport,
    request_timeout: Duration,
    upload_blocked: bool,
) {
    if mode == StopMode::Drain && !upload_blocked && !telemetry_revoked(root) {
        final_drain(root, drain, transport, deadline, request_timeout);
    }
}

impl UploadWorker {
    #[cfg(test)]
    fn start_with_timing(
        root: &Path,
        transport: TelemetryTransport,
        timing: UploadTiming,
    ) -> io::Result<Self> {
        let consent_generation = load_or_create_consent_generation(root)?;
        Self::start_with_io_lock(
            root,
            transport,
            timing,
            consent_generation,
            Arc::new(Mutex::new(())),
        )
    }

    fn start_with_io_lock(
        root: &Path,
        transport: TelemetryTransport,
        timing: UploadTiming,
        consent_generation: Uuid,
        io_lock: Arc<Mutex<()>>,
    ) -> io::Result<Self> {
        let drain = TelemetryDrain::open_with_io_lock(
            root,
            &transport.channel,
            consent_generation,
            Arc::clone(&io_lock),
        )?;
        let startup_pending = drain.pending_bytes()? > 0;
        let (tx, rx) = mpsc::sync_channel(1);
        let control = Arc::new(AtomicU8::new(WORKER_RUNNING));
        let worker_control = Arc::clone(&control);
        let worker_root = root.to_path_buf();
        let handle = std::thread::spawn(move || {
            let config = ureq::Agent::config_builder()
                .timeout_global(Some(timing.request_timeout))
                .build();
            let agent: ureq::Agent = config.into();
            let mut retry_at: Option<Instant> = None;
            let mut flush_at = startup_pending.then(Instant::now);
            let mut retry_delay = timing.retry_initial;
            let mut upload_blocked = false;
            loop {
                let upload_at = retry_at.or(flush_at);
                let command = match rx.recv_timeout(worker_wait(
                    upload_at,
                    Instant::now(),
                    timing.revocation_poll,
                )) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                if telemetry_revoked(&worker_root) {
                    let _ = discard_stats_with_io_lock(&worker_root, &io_lock);
                    return;
                }
                match command {
                    Some(UploadCommand::Finish { mode, deadline }) => {
                        finish_upload(
                            &worker_root,
                            mode,
                            deadline,
                            &drain,
                            &transport,
                            timing.shutdown_request_timeout,
                            upload_blocked,
                        );
                        return;
                    }
                    Some(UploadCommand::Wake(_)) if retry_at.is_some() || upload_blocked => {
                        continue
                    }
                    Some(UploadCommand::Wake(first_pending_at)) => {
                        let pending = match drain.pending_bytes() {
                            Ok(pending) => pending,
                            Err(error) => {
                                log::warn!(target: "TELEMETRY", "upload paused: {error}");
                                flush_at = None;
                                retry_at = Some(Instant::now() + retry_delay);
                                retry_delay = retry_delay.saturating_mul(2).min(timing.retry_max);
                                continue;
                            }
                        };
                        if pending == 0 {
                            continue;
                        }
                        schedule_flush(
                            &mut flush_at,
                            first_pending_at,
                            timing.flush_window,
                            pending >= MAX_BODY_BYTES as u64,
                        );
                        if flush_at.is_some_and(|deadline| Instant::now() < deadline) {
                            continue;
                        }
                    }
                    None if upload_at.is_none_or(|deadline| Instant::now() < deadline) => continue,
                    None => {}
                }

                match drain.run_while_reclaiming(
                    || {
                        worker_control.load(Ordering::SeqCst) == WORKER_RUNNING
                            && !telemetry_revoked(&worker_root)
                    },
                    |body| send_if_consented(&worker_root, &transport, &agent, body),
                ) {
                    Ok(DrainOutcome::Complete) => {
                        retry_at = None;
                        flush_at = None;
                        retry_delay = timing.retry_initial;
                    }
                    Ok(DrainOutcome::Blocked(error)) => {
                        if telemetry_revoked(&worker_root) {
                            let _ = discard_stats_with_io_lock(&worker_root, &io_lock);
                            return;
                        }
                        log::warn!(target: "TELEMETRY", "upload blocked: {error}");
                        retry_at = None;
                        flush_at = None;
                        upload_blocked = true;
                    }
                    Ok(DrainOutcome::Cancelled) => {}
                    Ok(DrainOutcome::Retry(error)) | Err(error) => {
                        if telemetry_revoked(&worker_root) {
                            let _ = discard_stats_with_io_lock(&worker_root, &io_lock);
                            return;
                        }
                        if worker_control.load(Ordering::SeqCst) != WORKER_RUNNING {
                            continue;
                        }
                        log::warn!(target: "TELEMETRY", "upload paused: {error}");
                        flush_at = None;
                        retry_at = Some(Instant::now() + retry_delay);
                        retry_delay = retry_delay.saturating_mul(2).min(timing.retry_max);
                    }
                }
            }
        });
        let worker = Self {
            tx,
            control,
            shutdown_budget: timing.shutdown_budget,
            handle,
        };
        Ok(worker)
    }

    fn start_offline(root: &Path, io_lock: Arc<Mutex<()>>) -> Self {
        let timing = UploadTiming::default();
        let (tx, rx) = mpsc::sync_channel(1);
        let control = Arc::new(AtomicU8::new(WORKER_RUNNING));
        let worker_root = root.to_path_buf();
        let handle = std::thread::spawn(move || loop {
            let command = match rx.recv_timeout(timing.revocation_poll) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            if telemetry_revoked(&worker_root) {
                let _ = discard_stats_with_io_lock(&worker_root, &io_lock);
                return;
            }
            if matches!(command, Some(UploadCommand::Finish { .. })) {
                return;
            }
        });
        Self {
            tx,
            control,
            shutdown_budget: timing.shutdown_budget,
            handle,
        }
    }

    fn waker(&self) -> UploadWaker {
        UploadWaker {
            tx: self.tx.clone(),
            control: Arc::clone(&self.control),
        }
    }

    fn finish(self, mode: StopMode) {
        let state = if mode == StopMode::Drain {
            WORKER_DRAINING
        } else {
            WORKER_DISCARDING
        };
        let deadline = Instant::now() + self.shutdown_budget;
        self.control.store(state, Ordering::SeqCst);
        let _ = self.tx.send(UploadCommand::Finish { mode, deadline });
        let _ = self.handle.join();
    }
}

impl UploadWaker {
    fn wake(&self) {
        if self.control.load(Ordering::SeqCst) == WORKER_RUNNING {
            let _ = self.tx.try_send(UploadCommand::Wake(Instant::now()));
        }
    }
}

impl QueuedTelemetryEvent {
    fn new(name: &str, timestamp: i64, data: Option<&str>) -> io::Result<Self> {
        validate_field("n", name)?;
        if !(0..=MAX_TIMESTAMP).contains(&timestamp) {
            return Err(invalid_input("invalid telemetry timestamp"));
        }
        if let Some(data) = data {
            validate_event_data(data)?;
        }
        Ok(Self {
            name: name.into(),
            timestamp,
            data: data.map(str::to_owned),
        })
    }
}

impl AppendWorker {
    fn report_failure(error: &io::Error) {
        if error.kind() == io::ErrorKind::StorageFull {
            crate::log_throttled!(
                "TELEMETRY",
                Duration::from_secs(10),
                "telemetry append failed: {error}"
            );
        } else {
            log::warn!(target: "TELEMETRY", "telemetry append failed: {error}");
        }
    }

    fn start(mut spool: TelemetrySpool, upload_waker: UploadWaker) -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut failed_since_flush = false;
            while let Ok(command) = rx.recv() {
                match command {
                    AppendCommand::Record(event) => {
                        let mut events = vec![event];
                        let mut flush = None;
                        let mut finished = false;
                        while events.len() < MAX_APPEND_BATCH_EVENTS {
                            match rx.try_recv() {
                                Ok(AppendCommand::Record(event)) => events.push(event),
                                Ok(AppendCommand::Flush(done)) => {
                                    flush = Some(done);
                                    break;
                                }
                                Ok(AppendCommand::Finish) | Err(TryRecvError::Disconnected) => {
                                    finished = true;
                                    break;
                                }
                                Err(TryRecvError::Empty) => break,
                            }
                        }
                        let mut appended = false;
                        if let Err(batch_error) = spool.record_batch(&events) {
                            // No row can fit an already-full spool. A merely oversized
                            // batch still retries individually to admit the rows that fit.
                            if events.len() == 1
                                || batch_error
                                    .get_ref()
                                    .is_some_and(|error| error.is::<SpoolAtCapacity>())
                            {
                                failed_since_flush = true;
                                Self::report_failure(&batch_error);
                            } else {
                                for event in &events {
                                    match spool.record_batch(std::slice::from_ref(event)) {
                                        Ok(()) => appended = true,
                                        Err(error) => {
                                            failed_since_flush = true;
                                            Self::report_failure(&error);
                                        }
                                    }
                                }
                            }
                        } else {
                            appended = true;
                        }
                        if appended {
                            upload_waker.wake();
                        }
                        if let Some(done) = flush {
                            let _ = done.send(!failed_since_flush);
                            failed_since_flush = false;
                        }
                        if finished {
                            return;
                        }
                    }
                    AppendCommand::Flush(done) => {
                        let _ = done.send(!failed_since_flush);
                        failed_since_flush = false;
                    }
                    AppendCommand::Finish => return,
                }
            }
        });
        Self { tx, handle }
    }

    fn record(&self, event: QueuedTelemetryEvent) -> io::Result<()> {
        self.tx.send(AppendCommand::Record(event)).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "telemetry append worker stopped")
        })
    }

    fn finish(self) {
        let _ = self.tx.send(AppendCommand::Finish);
        let _ = self.handle.join();
    }

    fn flush(&self) -> io::Result<()> {
        let (done_tx, done_rx) = mpsc::channel();
        self.tx.send(AppendCommand::Flush(done_tx)).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "telemetry append worker stopped")
        })?;
        let persisted = done_rx.recv().map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "telemetry append worker stopped")
        })?;
        if persisted {
            Ok(())
        } else {
            Err(io::Error::other(
                "telemetry persistence failed before flush",
            ))
        }
    }
}

impl TelemetryClient {
    fn open(
        root: &Path,
        identity: TelemetryIdentity,
        endpoint: &str,
        token: &str,
        explicit_reconsent: bool,
    ) -> io::Result<Self> {
        Self::open_with_timing(
            root,
            identity,
            endpoint,
            token,
            explicit_reconsent,
            UploadTiming::default(),
        )
    }

    fn open_with_timing(
        root: &Path,
        identity: TelemetryIdentity,
        endpoint: &str,
        token: &str,
        explicit_reconsent: bool,
        timing: UploadTiming,
    ) -> io::Result<Self> {
        let Some(root_lock) = TelemetryRootLock::acquire(root)? else {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another process owns the telemetry root",
            ));
        };
        let consent_generation = if explicit_reconsent {
            prepare_opt_in(root)?
        } else if telemetry_revoked(root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "telemetry consent was withdrawn",
            ));
        } else {
            load_or_create_consent_generation(root)?
        };
        let channel = identity.channel.clone();
        let game = identity.game.clone();
        let io_lock = Arc::new(Mutex::new(()));
        TelemetryDrain::open_with_io_lock(
            root,
            &channel,
            consent_generation,
            Arc::clone(&io_lock),
        )?
        .reclaim_if_fully_acknowledged()?;
        let spool = TelemetrySpool::open_with_io_lock(
            root,
            identity,
            consent_generation,
            Arc::clone(&io_lock),
        )?;
        spool.discard_legacy_events()?;
        #[cfg(test)]
        let events_path = spool.events_path.clone();
        #[cfg(test)]
        let client_io_lock = Arc::clone(&io_lock);
        let upload_worker = if endpoint.is_empty() || token.is_empty() {
            UploadWorker::start_offline(root, Arc::clone(&io_lock))
        } else {
            UploadWorker::start_with_io_lock(
                root,
                TelemetryTransport {
                    endpoint: endpoint.into(),
                    token: token.into(),
                    game,
                    channel,
                },
                timing,
                consent_generation,
                Arc::clone(&io_lock),
            )?
        };
        let append_worker = AppendWorker::start(spool, upload_worker.waker());
        Ok(Self {
            append_worker,
            upload_worker,
            #[cfg(test)]
            events_path,
            #[cfg(test)]
            io_lock: client_io_lock,
            _root_lock: root_lock,
        })
    }

    #[cfg(test)]
    fn record_stat(&mut self, name: &str, timestamp: i64) -> io::Result<()> {
        self.record_stat_data(name, timestamp, None)
    }

    fn record_stat_data(
        &mut self,
        name: &str,
        timestamp: i64,
        data: Option<&str>,
    ) -> io::Result<()> {
        self.append_worker
            .record(QueuedTelemetryEvent::new(name, timestamp, data)?)
    }

    fn finish(self, mode: StopMode) {
        self.append_worker.finish();
        self.upload_worker.finish(mode);
    }

    fn flush(&self) -> io::Result<()> {
        self.append_worker.flush()
    }

    #[cfg(test)]
    fn io_lock_for_test(&self) -> Arc<Mutex<()>> {
        Arc::clone(&self.io_lock)
    }
}

fn ffi_str<'a>(value: *const c_char) -> Option<&'a str> {
    if value.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(value) }.to_str().ok()
}

#[no_mangle]
pub extern "C" fn stone_telemetry_configure(
    root: *const c_char,
    game: *const c_char,
    build: *const c_char,
    platform: *const c_char,
    channel: *const c_char,
    endpoint: *const c_char,
    token: *const c_char,
    explicit_reconsent: bool,
) -> bool {
    let (
        Some(root),
        Some(game),
        Some(build),
        Some(platform),
        Some(channel),
        Some(endpoint),
        Some(token),
    ) = (
        ffi_str(root),
        ffi_str(game),
        ffi_str(build),
        ffi_str(platform),
        ffi_str(channel),
        ffi_str(endpoint),
        ffi_str(token),
    )
    else {
        log::warn!(target: "TELEMETRY", "configure received a null or non-UTF-8 argument");
        return false;
    };
    let previous = match TELEMETRY.lock() {
        Ok(mut active) => active.take(),
        Err(_) => {
            log::warn!(target: "TELEMETRY", "configure found the client lock poisoned");
            return false;
        }
    };
    if let Some(previous) = previous {
        previous.finish(StopMode::Drain);
    }
    let client = match TelemetryClient::open(
        Path::new(root),
        TelemetryIdentity {
            game: game.into(),
            build: build.into(),
            platform: platform.into(),
            channel: channel.into(),
        },
        endpoint,
        token,
        explicit_reconsent,
    ) {
        Ok(client) => client,
        Err(error) => {
            log::warn!(target: "TELEMETRY", "could not open the queue under {root}: {error}");
            return false;
        }
    };
    let Ok(mut active) = TELEMETRY.lock() else {
        log::warn!(target: "TELEMETRY", "configure found the client lock poisoned");
        return false;
    };
    *active = Some(client);
    true
}

#[no_mangle]
pub extern "C" fn stone_telemetry_record(
    name: *const c_char,
    timestamp: i64,
    data: *const c_char,
) -> bool {
    let Some(name) = ffi_str(name) else {
        return false;
    };
    // A null pointer and an empty string are both "this event has no data": the
    // caller that never built one and the caller whose builder stayed empty must
    // not produce different envelopes.
    let data = ffi_str(data).filter(|data| !data.is_empty());
    let Ok(mut active) = TELEMETRY.lock() else {
        return false;
    };
    let Some(client) = active.as_mut() else {
        return false;
    };
    client.record_stat_data(name, timestamp, data).is_ok()
}

/// The client's admission check without a client: whether `stone_telemetry_record`
/// would accept this event's name, timestamp and data. A sim's in-memory capture
/// (`stone/engine/systems/telemetry.rock`) admits through this so a payload the
/// queue refuses cannot pass a sim that never opened a queue.
#[no_mangle]
pub extern "C" fn stone_telemetry_validate(
    name: *const c_char,
    timestamp: i64,
    data: *const c_char,
) -> bool {
    let Some(name) = ffi_str(name) else {
        return false;
    };
    let data = ffi_str(data).filter(|data| !data.is_empty());
    QueuedTelemetryEvent::new(name, timestamp, data).is_ok()
}

#[no_mangle]
pub extern "C" fn stone_telemetry_close() {
    let client = match TELEMETRY.lock() {
        Ok(mut active) => active.take(),
        Err(_) => None,
    };
    if let Some(client) = client {
        client.finish(StopMode::Drain);
    }
}

#[no_mangle]
pub extern "C" fn stone_telemetry_flush() -> bool {
    let Ok(active) = TELEMETRY.lock() else {
        return false;
    };
    let Some(client) = active.as_ref() else {
        return false;
    };
    client.flush().is_ok()
}

/// Whether `root` holds a withdrawal that only an explicit opt-in lifts. An
/// unreadable marker counts as withdrawn, matching `TelemetryClient::open`.
#[no_mangle]
pub extern "C" fn stone_telemetry_withdrawn(root: *const c_char) -> bool {
    let Some(root) = ffi_str(root) else {
        log::warn!(target: "TELEMETRY", "withdrawal query received a null or non-UTF-8 root");
        return true;
    };
    telemetry_revoked(Path::new(root))
}

#[no_mangle]
pub extern "C" fn stone_telemetry_clear(root: *const c_char) -> bool {
    let Some(root) = ffi_str(root) else {
        return false;
    };
    let root = Path::new(root);
    let revoked = publish_withdrawal(root).is_ok();
    let client = match TELEMETRY.lock() {
        Ok(mut active) => active.take(),
        Err(_) => return false,
    };
    if let Some(client) = client {
        client.finish(StopMode::Discard);
    }
    if !revoked {
        return false;
    }
    match TelemetryRootLock::acquire(root) {
        Ok(Some(_root_lock)) => discard_stats(root).is_ok(),
        // The owner drops its own spool when it observes the tombstone.
        Ok(None) => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU32, Ordering};

    static UNIQ: AtomicU32 = AtomicU32::new(0);

    // `AppendWorker::report_failure` warns through a process-global 10 s
    // `log_throttled!` site, so every test that can emit through it serializes
    // here; otherwise a sibling's warning suppresses this test's.
    static TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    // Rows of this test's throttled "spool is full" line in the process-global
    // warn table. The caller holds TEST_LOCK with a freshly reset throttle, so
    // the before/after delta is this test's own rows, not a sibling's warns
    // through another site.
    fn capped_spool_warnings() -> u64 {
        crate::rock_log_reexport::records(log::Level::Warn)
            .iter()
            .filter(|row| {
                row.target == "TELEMETRY"
                    && row.message
                        == "telemetry append failed: telemetry spool reached its 8 MiB cap"
            })
            .map(|row| row.count)
            .sum()
    }

    fn fresh_root() -> std::path::PathBuf {
        let n = UNIQ.fetch_add(1, Ordering::SeqCst);
        // Pid plus a per-process counter collide when two suites share /tmp
        // without sharing a pid namespace; then both flock the same root
        // lock. The random tail keeps every root unique across processes.
        let tag = Uuid::new_v4().simple().to_string();
        let root = std::env::temp_dir().join(format!(
            "stone_telemetry_test_{}_{}_{}",
            std::process::id(),
            n,
            &tag[..16]
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn identity() -> TelemetryIdentity {
        TelemetryIdentity {
            game: "mag".into(),
            build: "abc123".into(),
            platform: "linux".into(),
            channel: "steam".into(),
        }
    }

    // The whole request: headers and body. An empty return is a peer that closed
    // without sending one: a stray port probe from another test process, never a
    // real upload. Test servers skip those connections instead of counting or
    // answering them.
    fn read_full_http_request(stream: &mut TcpStream) -> String {
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut request = Vec::new();
        let mut buf = [0_u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut buf).unwrap();
            if read == 0 {
                return String::new();
            }
            request.extend_from_slice(&buf[..read]);
            if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap();
        while request.len() < header_end + content_length {
            let read = stream.read(&mut buf).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buf[..read]);
        }
        String::from_utf8(request).unwrap()
    }

    /// The body of a request `read_full_http_request` returned.
    fn http_body(request: &str) -> &str {
        request
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or("")
    }

    /// The body alone, which is all a server that counts or replays rows
    /// inspects. Empty means the peer never sent a request.
    fn read_http_request(stream: &mut TcpStream) -> String {
        http_body(&read_full_http_request(stream)).to_string()
    }

    fn test_upload_timing() -> UploadTiming {
        UploadTiming {
            flush_window: Duration::from_millis(500),
            retry_initial: Duration::from_millis(20),
            retry_max: Duration::from_millis(40),
            revocation_poll: Duration::from_millis(10),
            request_timeout: Duration::from_millis(200),
            shutdown_budget: Duration::from_millis(200),
            shutdown_request_timeout: Duration::from_millis(100),
        }
    }

    #[test]
    fn normal_exit_allows_two_seconds_per_request_inside_three_seconds_total() {
        let timing = UploadTiming::default();
        assert_eq!(timing.shutdown_request_timeout, Duration::from_secs(2));
        assert_eq!(timing.shutdown_budget, Duration::from_secs(3));
    }

    // Clone the spool's single row until the log spans more than one request body,
    // so a drain is forced across a batch boundary. Cloning keeps the consent
    // generation the drain filters on. Returns the row names in log order.
    fn grow_log_past_one_batch(events_path: &Path) -> Vec<String> {
        let template = fs::read_to_string(events_path)
            .unwrap()
            .trim_end()
            .to_string();
        let mut log = String::from(&template);
        let mut names = vec!["row-0".to_string()];
        let mut index = 1;
        while log.len() <= MAX_BODY_BYTES {
            let name = format!("row-{index}");
            log.push('\n');
            log.push_str(&template.replace("row-0", &name));
            names.push(name);
            index += 1;
        }
        log.push('\n');
        fs::write(events_path, &log).unwrap();
        names
    }

    fn batch_names(body: &str) -> Vec<String> {
        body.lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["n"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    fn records(root: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(root.join("reports/stats/steam/events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn channels_have_independent_spools_and_drains() {
        let root = fresh_root();
        let mut steam = TelemetrySpool::open(&root, identity()).unwrap();
        let mut direct_identity = identity();
        direct_identity.channel = "direct".into();
        let mut direct = TelemetrySpool::open(&root, direct_identity).unwrap();

        assert_ne!(steam.events_path, direct.events_path);
        steam.record_stat("steam-event", 1).unwrap();
        direct.record_stat("direct-event", 2).unwrap();

        let mut steam_seen = Vec::new();
        TelemetryDrain::open(&root, "steam")
            .unwrap()
            .run(|body| {
                steam_seen.push(serde_json::from_str::<Value>(body).unwrap()["n"].clone());
                Ok(())
            })
            .unwrap();
        let mut direct_seen = Vec::new();
        TelemetryDrain::open(&root, "direct")
            .unwrap()
            .run(|body| {
                direct_seen.push(serde_json::from_str::<Value>(body).unwrap()["n"].clone());
                Ok(())
            })
            .unwrap();

        assert_eq!(steam_seen, ["steam-event"]);
        assert_eq!(direct_seen, ["direct-event"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cursor_ignores_an_unterminated_tail() {
        let root = fresh_root();
        let drain = TelemetryDrain::open(&root, "steam").unwrap();
        std::fs::write(
            &drain.cursor_path,
            format!(
                "{} 100\n{} 1",
                drain.consent_generation, drain.consent_generation
            ),
        )
        .unwrap();

        assert_eq!(drain.cursor_for_len(200).unwrap(), 100);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cursor_beyond_the_durable_log_falls_back_to_a_safe_offset() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();
        spool.record_stat("first", 1).unwrap();
        let first_len = std::fs::metadata(&spool.events_path).unwrap().len();
        spool.record_stat("second", 2).unwrap();
        let drain = TelemetryDrain::open(&root, "steam").unwrap();
        std::fs::write(
            &drain.cursor_path,
            format!(
                "{} {first_len}\n{} 999999\n",
                drain.consent_generation, drain.consent_generation
            ),
        )
        .unwrap();

        let mut seen = Vec::new();
        drain
            .run(|body| {
                seen.push(serde_json::from_str::<Value>(body).unwrap()["n"].clone());
                Ok(())
            })
            .unwrap();

        assert_eq!(seen, ["second"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cursor_recovery_stops_at_the_first_impossible_complete_entry() {
        let root = fresh_root();
        let drain = TelemetryDrain::open(&root, "steam").unwrap();
        std::fs::write(
            &drain.cursor_path,
            format!(
                "{} 100\n{} 999999\n{} 1\n",
                drain.consent_generation, drain.consent_generation, drain.consent_generation
            ),
        )
        .unwrap();

        assert_eq!(drain.cursor_for_len(200).unwrap(), 100);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opening_a_spool_discards_a_torn_final_event() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();
        spool.record_stat("complete", 1).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&spool.events_path)
            .unwrap()
            .write_all(b"{\"v\":1")
            .unwrap();

        TelemetrySpool::open(&root, identity())
            .unwrap()
            .record_stat("after-recovery", 2)
            .unwrap();
        let rows: Vec<Value> = std::fs::read_to_string(&spool.events_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["n"], "complete");
        assert_eq!(rows[1]["n"], "after-recovery");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opening_a_spool_does_not_persist_or_emit_an_install_id() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();
        spool.record_stat("session_start", 1).unwrap();

        assert!(!root.join("reports/.install").exists());
        let rows = records(&root);
        assert!(rows[0].get("install").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opening_a_session_only_client_discards_legacy_install_rows() {
        let root = fresh_root();
        let consent = begin_consent_generation(&root).unwrap();
        let events_path = root.join("reports/stats/steam/events.jsonl");
        fs::create_dir_all(events_path.parent().unwrap()).unwrap();
        let legacy = serde_json::json!({
            "v": 1,
            "kind": "stat",
            "t": 1,
            "install": Uuid::new_v4().to_string(),
            "session": Uuid::new_v4().to_string(),
            "consent": consent.to_string(),
            "seq": 1,
            "build": "old",
            "platform": "linux",
            "channel": "steam",
            "game": "mag",
            "n": "legacy",
        });
        fs::write(&events_path, format!("{}\n", legacy)).unwrap();

        let mut client = TelemetryClient::open(&root, identity(), "", "", false).unwrap();
        client.record_stat("new", 2).unwrap();
        client.finish(StopMode::Discard);

        let rows = records(&root);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["n"], "new");
        assert!(rows[0].get("install").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recording_never_waits_for_telemetry_io() {
        let root = fresh_root();
        let mut client = TelemetryClient::open(&root, identity(), "", "", false).unwrap();
        let io_lock = client.io_lock_for_test();
        let held = io_lock.lock().unwrap();
        let (returned_tx, returned_rx) = mpsc::channel();
        let recorder = std::thread::spawn(move || {
            let started = Instant::now();
            let data = r#"{"run":1,"spell":"Flamestrike","phase":"started"}"#;
            let accepted = (0..1_024).all(|seq| {
                client
                    .record_stat_data("spell_cast", seq, Some(data))
                    .is_ok()
            });
            returned_tx.send((accepted, started.elapsed())).unwrap();
            client
        });

        let returned_without_io = returned_rx.recv_timeout(Duration::from_millis(100));
        drop(held);
        let client = recorder.join().unwrap();
        client.finish(StopMode::Discard);
        std::fs::remove_dir_all(root).unwrap();

        let (accepted, elapsed) = returned_without_io
            .expect("gameplay-side telemetry recording waited for the persistence lock");
        assert!(
            accepted,
            "the nonblocking queue refused one of 1,024 events"
        );
        eprintln!("queued 1,024 telemetry events in {elapsed:?} while persistence was locked");
    }

    #[test]
    fn queued_bursts_are_durable_and_ordered_at_shutdown() {
        let root = fresh_root();
        let mut client = TelemetryClient::open(&root, identity(), "", "", false).unwrap();
        for seq in 1..=130 {
            client.record_stat(&format!("event-{seq}"), seq).unwrap();
        }
        client.finish(StopMode::Discard);

        let rows = records(&root);
        assert_eq!(rows.len(), 130);
        for (index, row) in rows.iter().enumerate() {
            let seq = index + 1;
            assert_eq!(row["n"], format!("event-{seq}"));
            assert_eq!(row["seq"], seq);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_flush_makes_prior_queued_rows_visible() {
        let root = fresh_root();
        let mut client = TelemetryClient::open(&root, identity(), "", "", false).unwrap();
        client.record_stat("before-flush", 1).unwrap();

        client.flush().unwrap();

        assert_eq!(records(&root)[0]["n"], "before-flush");
        client.finish(StopMode::Discard);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_flush_reports_prior_persistence_failure() {
        // Emits through the throttled spool-full site; hold the lock so the
        // warning cannot land inside the asserting test's window.
        let _guard = TEST_LOCK.lock().unwrap();
        let root = fresh_root();
        let mut client = TelemetryClient::open(&root, identity(), "", "", false).unwrap();
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&client.events_path)
            .unwrap()
            .set_len(MAX_SPOOL_BYTES)
            .unwrap();
        client.record_stat("over-cap", 1).unwrap();

        let error = client.flush().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        client.finish(StopMode::Discard);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capped_append_worker_reports_failure_without_warning_per_row() {
        let _guard = TEST_LOCK.lock().unwrap();
        crate::rock_log_reexport::init();
        crate::rock_log_reexport::reset_dedup();
        let root = fresh_root();
        let mut client = TelemetryClient::open(&root, identity(), "", "", false).unwrap();
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&client.events_path)
            .unwrap()
            .set_len(MAX_SPOOL_BYTES)
            .unwrap();
        let before = capped_spool_warnings();
        for _ in 0..256 {
            client.record_stat("over-cap", 1).unwrap();
        }
        assert!(client.flush().is_err());
        client.finish(StopMode::Discard);
        let warnings = capped_spool_warnings() - before;
        std::fs::remove_dir_all(root).unwrap();

        assert!(
            (1..=2).contains(&warnings),
            "256 capped rows emitted {warnings} warnings"
        );
    }

    #[test]
    fn capped_batch_rejection_workload() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&spool.events_path)
            .unwrap()
            .set_len(MAX_SPOOL_BYTES)
            .unwrap();
        let data = serde_json::json!({"values": vec!["x".repeat(200); 16]}).to_string();
        let events: Vec<_> = (0..64)
            .map(|_| QueuedTelemetryEvent::new("over-cap", 1, Some(&data)).unwrap())
            .collect();
        let start = Instant::now();
        for _ in 0..256 {
            assert_eq!(
                spool.record_batch(&events).unwrap_err().kind(),
                io::ErrorKind::StorageFull
            );
        }
        let elapsed = start.elapsed();
        assert_eq!(spool.next_seq, 1);
        assert_eq!(
            fs::metadata(&spool.events_path).unwrap().len(),
            MAX_SPOOL_BYTES
        );
        std::fs::remove_dir_all(root).unwrap();
        eprintln!(
            "CAPPED_BATCH_WORK batches=256 rows=16384 elapsed_us={}",
            elapsed.as_micros()
        );
    }

    #[test]
    fn near_capacity_worker_still_accepts_a_smaller_row_after_rejection() {
        // Emits through the throttled spool-full site; hold the lock so the
        // warning cannot land inside the asserting test's window.
        let _guard = TEST_LOCK.lock().unwrap();
        let root = fresh_root();
        let mut client = TelemetryClient::open(&root, identity(), "", "", false).unwrap();
        let io_lock = Arc::clone(&client.io_lock);
        let guard = io_lock.lock().unwrap();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(&client.events_path)
            .unwrap();
        file.set_len(MAX_SPOOL_BYTES - 1024).unwrap();
        file.seek(SeekFrom::End(-1)).unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);
        let data = serde_json::json!({"values": vec!["x".repeat(200); 16]}).to_string();
        client
            .record_stat_data("too-large", 1, Some(&data))
            .unwrap();
        client.record_stat("fits", 1).unwrap();
        drop(guard);
        assert!(client.flush().is_err());
        let length = fs::metadata(&client.events_path).unwrap().len();
        let bytes = fs::read(&client.events_path).unwrap();
        let row: serde_json::Value = serde_json::from_slice(
            bytes
                .split(|b| *b == b'\n')
                .rev()
                .find(|row| !row.is_empty())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(row["n"], "fits");
        assert_eq!(row["seq"], 1);
        assert!(length > MAX_SPOOL_BYTES - 1024 && length <= MAX_SPOOL_BYTES);
        assert!(client.flush().is_ok());
        client.finish(StopMode::Discard);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_consent_persistence_cannot_publish_a_generation() {
        let root = fresh_root();

        let error =
            begin_consent_generation_with_persist(&root, |_| Err(io::Error::other("sync failed")))
                .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(consent_path(&root)).unwrap(), b"");
        let generation = begin_consent_generation(&root).unwrap();
        let stored = fs::read_to_string(consent_path(&root)).unwrap();
        assert_eq!(stored, format!("on {generation}\n"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_event_persistence_rolls_back_before_reusing_the_sequence() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();

        let error = spool
            .record_stat_with_persist("failed", 1, None, |_| Err(io::Error::other("sync failed")))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        spool.record_stat("after-recovery", 2).unwrap();

        let rows = records(&root);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["seq"], 1);
        assert_eq!(rows[0]["n"], "after-recovery");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn offline_spool_stops_at_its_disk_cap() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();
        fs::File::create(&spool.events_path)
            .unwrap()
            .set_len(MAX_SPOOL_BYTES)
            .unwrap();

        let error = spool.record_stat("over-cap", 1).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert_eq!(
            fs::metadata(&spool.events_path).unwrap().len(),
            MAX_SPOOL_BYTES
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn draining_a_capped_spool_allows_an_append_without_reopening() {
        let root = fresh_root();
        let io_lock = Arc::new(Mutex::new(()));
        let consent_generation = load_or_create_consent_generation(&root).unwrap();
        let mut spool = TelemetrySpool::open_with_io_lock(
            &root,
            identity(),
            consent_generation,
            Arc::clone(&io_lock),
        )
        .unwrap();
        spool.record_stat("before-cap", 1).unwrap();
        let mut events = OpenOptions::new()
            .write(true)
            .open(&spool.events_path)
            .unwrap();
        events.set_len(MAX_SPOOL_BYTES).unwrap();
        events.seek(SeekFrom::End(-1)).unwrap();
        events.write_all(b"\n").unwrap();
        events.sync_all().unwrap();
        // Windows keeps a deleted-but-open file in its directory, so the teardown
        // below cannot remove `root` while this handle lives.
        drop(events);
        let drain =
            TelemetryDrain::open_with_io_lock(&root, "steam", consent_generation, io_lock).unwrap();

        drain.run_reclaiming(|_| Ok(())).unwrap();
        spool.record_stat("after-drain", 2).unwrap();

        let rows = records(&root);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["n"], "after-drain");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn one_preference_root_has_one_native_owner() {
        if let Some(child_root) = std::env::var_os("TELEMETRY_LOCK_CHILD_ROOT") {
            let contended = TelemetryRootLock::acquire(Path::new(&child_root)).unwrap();
            assert!(contended.is_none());
            return;
        }
        let root = fresh_root();
        let owner = TelemetryRootLock::acquire(&root).unwrap().unwrap();

        let status = Command::new(std::env::current_exe().unwrap())
            .arg("telemetry::tests::one_preference_root_has_one_native_owner")
            .arg("--exact")
            .env("TELEMETRY_LOCK_CHILD_ROOT", &root)
            .status()
            .unwrap();

        assert!(status.success());
        drop(owner);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn another_process_can_revoke_a_retrying_uploader() {
        if let Some(child_root) = std::env::var_os("TELEMETRY_CLEAR_CHILD_ROOT") {
            let gate = std::env::var_os("TELEMETRY_CLEAR_CHILD_GATE").unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !Path::new(&gate).exists() {
                assert!(
                    Instant::now() < deadline,
                    "parent never released revocation child"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            let root = std::ffi::CString::new(child_root.as_encoded_bytes()).unwrap();
            assert!(stone_telemetry_clear(root.as_ptr()));
            return;
        }

        let root = fresh_root();
        let root_lock = TelemetryRootLock::acquire(&root).unwrap().unwrap();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();
        spool.record_stat("revoke-me", 1).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let child_gate = root.join("publish-revocation");
        let revocation = revocation_path(&root);
        let child = Command::new(std::env::current_exe().unwrap())
            .arg("telemetry::tests::another_process_can_revoke_a_retrying_uploader")
            .arg("--exact")
            .env("TELEMETRY_CLEAR_CHILD_ROOT", &root)
            .env("TELEMETRY_CLEAR_CHILD_GATE", &child_gate)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (published_tx, published_rx) = mpsc::channel();
        let stop_server = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_stop = Arc::clone(&stop_server);
        let requests = Arc::new(AtomicU32::new(0));
        let server_requests = Arc::clone(&requests);
        let server = std::thread::spawn(move || {
            while !server_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if read_http_request(&mut stream).is_empty() {
                            continue;
                        }
                        let request = server_requests.fetch_add(1, Ordering::SeqCst) + 1;
                        if request == 1 {
                            fs::write(&child_gate, b"publish").unwrap();
                            let deadline = Instant::now() + Duration::from_secs(10);
                            while !revocation.exists() {
                                assert!(
                                    Instant::now() < deadline,
                                    "child did not publish revocation"
                                );
                                std::thread::sleep(Duration::from_millis(2));
                            }
                            published_tx.send(request).unwrap();
                        }
                        write!(
                            stream,
                            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"
                        )
                        .unwrap();
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("test server failed: {error}"),
                }
            }
        });
        let worker = UploadWorker::start_with_timing(
            &root,
            TelemetryTransport {
                endpoint: format!("http://{address}/v1/events"),
                token: "secret".into(),
                game: "mag".into(),
                channel: "steam".into(),
            },
            test_upload_timing(),
        )
        .unwrap();
        let events_path = spool.events_path.clone();
        let io_lock = Arc::clone(&spool.io_lock);
        let append_worker = AppendWorker::start(spool, worker.waker());
        let client = TelemetryClient {
            append_worker,
            upload_worker: worker,
            events_path,
            io_lock,
            _root_lock: root_lock,
        };

        let requests_at_publication = published_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let child = child.wait_with_output().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while root.join("reports/stats").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        client.finish(StopMode::Drain);
        stop_server.store(true, Ordering::SeqCst);
        server.join().unwrap();
        let requests_after_withdrawal = requests.load(Ordering::SeqCst);

        assert!(
            child.status.success(),
            "child clear failed:\n{}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert_eq!(
            requests_at_publication, 1,
            "test did not expose one initial request"
        );
        assert_eq!(
            requests_after_withdrawal, requests_at_publication,
            "uploader sent another request after root-wide withdrawal"
        );
        assert!(root.join("reports/.telemetry-revoked").exists());
        assert!(!root.join("reports/stats").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_respects_withdrawal_until_explicit_reconsent() {
        let root = fresh_root();
        TelemetrySpool::open(&root, identity())
            .unwrap()
            .record_stat("before-withdrawal", 1)
            .unwrap();
        fs::write(root.join("reports/.telemetry-revoked"), b"").unwrap();

        let error = TelemetryClient::open(&root, identity(), "", "", false)
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(root.join("reports/.telemetry-revoked").exists());
        assert_eq!(records(&root).len(), 1);

        let mut client = TelemetryClient::open(&root, identity(), "", "", true).unwrap();
        client.record_stat("after-reconsent", 2).unwrap();
        client.finish(StopMode::Discard);

        assert!(!root.join("reports/.telemetry-revoked").exists());
        let rows = records(&root);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["n"], "after-reconsent");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reappearing_revoked_rows_never_become_eligible_again() {
        let root = fresh_root();
        let mut before = TelemetrySpool::open(&root, identity()).unwrap();
        before.record_stat("before-withdrawal", 1).unwrap();
        let old_events = fs::read(&before.events_path).unwrap();
        let events_path = before.events_path.clone();

        publish_revocation(&root).unwrap();
        fs::remove_file(consent_path(&root)).unwrap();
        prepare_opt_in(&root).unwrap();
        fs::create_dir_all(events_path.parent().unwrap()).unwrap();
        fs::write(&events_path, old_events).unwrap();

        let mut after = TelemetrySpool::open(&root, identity()).unwrap();
        after.record_stat("after-reconsent", 2).unwrap();
        let mut sent = Vec::new();
        TelemetryDrain::open(&root, "steam")
            .unwrap()
            .run(|body| {
                sent.push(
                    serde_json::from_str::<Value>(body).unwrap()["n"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                );
                Ok(())
            })
            .unwrap();

        assert_eq!(sent, ["after-reconsent"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_missing_consent_journal_with_queued_rows_fails_closed() {
        let root = fresh_root();
        TelemetrySpool::open(&root, identity())
            .unwrap()
            .record_stat("queued", 1)
            .unwrap();
        let _ = fs::remove_file(root.join("reports/.telemetry-consent"));

        let error = TelemetryClient::open(&root, identity(), "", "", false)
            .err()
            .expect("queued rows without their consent generation were accepted");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn withdrawal_journal_blocks_startup_if_the_tombstone_disappears() {
        let root = fresh_root();
        TelemetrySpool::open(&root, identity())
            .unwrap()
            .record_stat("before-withdrawal", 1)
            .unwrap();
        let root_string = std::ffi::CString::new(root.as_os_str().as_encoded_bytes()).unwrap();
        assert!(stone_telemetry_clear(root_string.as_ptr()));
        fs::remove_file(revocation_path(&root)).unwrap();

        let error = TelemetryClient::open(&root, identity(), "", "", false)
            .err()
            .expect("durable withdrawal state was forgotten with the tombstone");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn revocation_stops_an_offline_owner_from_appending() {
        let root = fresh_root();
        let mut client = TelemetryClient::open(&root, identity(), "", "", false).unwrap();
        client.record_stat("before-withdrawal", 1).unwrap();
        publish_revocation(&root).unwrap();

        client.record_stat("after-withdrawal", 2).unwrap();
        client.finish(StopMode::Discard);

        assert!(!root.join("reports/stats").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn another_process_revokes_an_idle_offline_owner() {
        if let Some(child_root) = std::env::var_os("TELEMETRY_OFFLINE_CLEAR_CHILD_ROOT") {
            let root = std::ffi::CString::new(child_root.as_encoded_bytes()).unwrap();
            assert!(stone_telemetry_clear(root.as_ptr()));
            return;
        }

        let root = fresh_root();
        let mut client = TelemetryClient::open(&root, identity(), "", "", false).unwrap();
        client.record_stat("revoke-me", 1).unwrap();

        let child = Command::new(std::env::current_exe().unwrap())
            .arg("telemetry::tests::another_process_revokes_an_idle_offline_owner")
            .arg("--exact")
            .env("TELEMETRY_OFFLINE_CLEAR_CHILD_ROOT", &root)
            .output()
            .unwrap();
        let deadline = Instant::now() + Duration::from_millis(300);
        while root.join("reports/stats").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let stats_remain = root.join("reports/stats").exists();
        client.finish(StopMode::Discard);

        assert!(
            child.status.success(),
            "child clear failed:\n{}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(!stats_remain, "offline owner did not observe withdrawal");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn revocation_during_persistence_discards_the_appended_row() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();

        let error = spool
            .record_stat_with_persist("raced", 1, None, |file| {
                file.sync_all()?;
                publish_revocation(&root)
            })
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!root.join("reports/stats").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    // Absent counts as empty: callers asking "was anything written" do not care
    // which of the two they get, and an eagerly created queue gives the latter.
    fn queue_len(path: &Path) -> u64 {
        match fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => panic!("{}: {error}", path.display()),
        }
    }

    #[test]
    fn opening_the_queue_creates_it_before_any_flush() {
        let root = fresh_root();
        let events = root.join("reports/stats/steam/events.jsonl");
        assert!(!events.exists());
        let client = TelemetryClient::open(&root, identity(), "", "", false).unwrap();
        assert!(events.exists(), "the open materializes the queue");
        assert_eq!(queue_len(&events), 0, "and writes no row of its own");
        client.finish(StopMode::Discard);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn client_rejects_rows_the_ingest_would_refuse() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();

        assert_eq!(
            spool.record_stat(&"x".repeat(129), 1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            spool.record_stat("valid", -1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        // Opening the spool creates the queue, so "nothing was appended" is an
        // empty file rather than an absent one.
        assert_eq!(queue_len(&spool.events_path), 0);
        let mut bad_identity = identity();
        bad_identity.channel = "playtest/other".into();
        assert_eq!(
            TelemetrySpool::open(&root, bad_identity)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        for game in ["", ".", "..", "MAG", "mag/other", "-mag"] {
            let mut bad_identity = identity();
            bad_identity.game = game.into();
            assert_eq!(
                TelemetrySpool::open(&root, bad_identity)
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::InvalidInput,
                "accepted game {game:?}"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn structured_data_reaches_the_emitted_envelope() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();

        spool
            .record_stat_data(
                "death",
                1_754_600_000,
                Some(
                    r#"{"cause":"Skeleton","run":3,"run_ms":41250,"ratio":0.5,
                        "first":true,"lost":[12,3]}"#,
                ),
            )
            .unwrap();

        let rows = records(&root);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["n"], "death");
        let data = &rows[0]["data"];
        assert_eq!(data["cause"], "Skeleton");
        assert_eq!(data["run"], 3);
        assert_eq!(data["run_ms"], 41250);
        assert_eq!(data["ratio"], 0.5);
        assert_eq!(data["first"], true);
        assert_eq!(data["lost"], serde_json::json!([12, 3]));
        // The envelope key set is what ingest validates by exact equality, so a
        // row with data must be exactly the base fields plus `data`.
        let keys: Vec<&String> = rows[0].as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 12, "unexpected envelope keys {keys:?}");

        // Absent data must not emit the key at all, or the exact-set validator
        // sees a different shape for every ordinary row.
        spool.record_stat("session_start", 1_754_600_001).unwrap();
        let rows = records(&root);
        assert!(
            rows[1].as_object().unwrap().get("data").is_none(),
            "a row without data still carried the key"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ffi_validation_is_the_clients_admission() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();
        let cases: [(&str, i64, &str); 6] = [
            ("probe", 1, ""),
            ("probe", 1, r#"{"k":[1,"a"]}"#),
            ("probe", -1, ""),
            (&"x".repeat(129), 1, ""),
            ("probe", 1, r#"{"k":{"nested":1}}"#),
            ("probe", 1, "not json"),
        ];
        for (name, timestamp, data) in cases {
            let c_name = std::ffi::CString::new(name).unwrap();
            let c_data = std::ffi::CString::new(data).unwrap();
            let validated = stone_telemetry_validate(c_name.as_ptr(), timestamp, c_data.as_ptr());
            let payload = Some(data).filter(|data| !data.is_empty());
            let recorded = spool.record_stat_data(name, timestamp, payload).is_ok();
            assert_eq!(validated, recorded, "{name:?} {timestamp} {data:?}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn client_rejects_data_the_ingest_would_refuse() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();

        let long_key = format!(r#"{{"{}":1}}"#, "k".repeat(65));
        let long_string = format!(r#"{{"s":"{}"}}"#, "v".repeat(257));
        let many_keys = format!(
            "{{{}}}",
            (0..33)
                .map(|index| format!(r#""k{index}":1"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let long_array = format!(
            r#"{{"a":[{}]}}"#,
            (0..33).map(|_| "1").collect::<Vec<_>>().join(",")
        );
        let refused: Vec<&str> = vec![
            "not json at all",
            "[1,2,3]",
            "\"a string\"",
            "12",
            "null",
            r#"{"k":null}"#,
            r#"{"k":{"nested":1}}"#,
            r#"{"k":[[1]]}"#,
            r#"{"k":[{"nested":1}]}"#,
            r#"{"k":[null]}"#,
            r#"{"":1}"#,
            // Beyond signed 64-bit, which ingest rejects as an int out of range.
            r#"{"k":18446744073709551615}"#,
            &long_key,
            &long_string,
            &many_keys,
            &long_array,
        ];
        for payload in &refused {
            assert_eq!(
                spool
                    .record_stat_data("probe", 1, Some(payload))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput,
                "accepted data {payload:?}"
            );
        }

        // Both sides of admission: the shapes at the limit must still be taken,
        // or the rejections above could be a validator that refuses everything.
        let max_key = format!(r#"{{"{}":1}}"#, "k".repeat(64));
        let max_string = format!(r#"{{"s":"{}"}}"#, "v".repeat(256));
        let max_keys = format!(
            "{{{}}}",
            (0..32)
                .map(|index| format!(r#""k{index}":1"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let max_array = format!(
            r#"{{"a":[{}]}}"#,
            (0..32).map(|_| "1").collect::<Vec<_>>().join(",")
        );
        let accepted: Vec<&str> = vec![
            "{}",
            r#"{"k":-9223372036854775808}"#,
            r#"{"k":9223372036854775807}"#,
            r#"{"k":false}"#,
            r#"{"k":-1.5}"#,
            r#"{"k":["a",1,true,2.5]}"#,
            r#"{"k":[]}"#,
            &max_key,
            &max_string,
            &max_keys,
            &max_array,
        ];
        for payload in &accepted {
            spool
                .record_stat_data("probe", 1, Some(payload))
                .unwrap_or_else(|error| panic!("refused data {payload:?}: {error}"));
        }
        assert_eq!(records(&root).len(), accepted.len());
        std::fs::remove_dir_all(root).unwrap();
    }

    // No single per-key or per-string bound reaches 8 KiB, so a fixture built from
    // one long value can only ever exercise the 256-char string rule. The row bound
    // lives where many maximal keys accumulate, which is where this builds its arms.
    #[test]
    fn an_oversized_event_is_rejected_at_the_ingest_event_bound() {
        // One row per fresh spool, so every measurement is taken at sequence 1 and
        // a digit rolling over cannot move the length under the arms.
        fn row_len(payload: &str) -> usize {
            let root = fresh_root();
            let mut spool = TelemetrySpool::open(&root, identity()).unwrap();
            spool.record_stat_data("size", 1, Some(payload)).unwrap();
            let len = fs::read_to_string(&spool.events_path)
                .unwrap()
                .trim_end()
                .len();
            std::fs::remove_dir_all(root).unwrap();
            len
        }

        // Each filled key costs less than one maximal string, so the gap left below
        // the bound is always payable by the tuned tail.
        let filler = "v".repeat(240);
        let payload = |keys: usize, tail: usize| {
            let mut fields: Vec<String> = (0..keys)
                .map(|index| format!(r#""k{index:02}":"{filler}""#))
                .collect();
            fields.push(format!(r#""s":"{}""#, "v".repeat(tail)));
            format!("{{{}}}", fields.join(","))
        };

        let mut keys = 0;
        while keys + 1 < MAX_DATA_KEYS && row_len(&payload(keys + 1, 0)) <= MAX_EVENT_BYTES {
            keys += 1;
        }
        let fits = MAX_EVENT_BYTES - row_len(&payload(keys, 0));
        assert!(
            fits > 0 && fits <= MAX_DATA_STRING_CHARS,
            "the bound is not reachable by tuning one legal string ({fits} to go)"
        );

        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();
        spool
            .record_stat_data("size", 1, Some(&payload(keys, fits)))
            .expect("a row exactly at the bound must be accepted");
        assert_eq!(
            fs::read_to_string(&spool.events_path)
                .unwrap()
                .trim_end()
                .len(),
            MAX_EVENT_BYTES
        );
        assert_eq!(
            spool
                .record_stat_data("size", 1, Some(&payload(keys, fits + 1)))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(records(&root).len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refused_data_neither_appends_nor_advances_the_sequence() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();

        spool.record_stat("first", 1).unwrap();
        assert!(spool
            .record_stat_data("refused", 2, Some(r#"{"k":{"nested":1}}"#))
            .is_err());
        assert!(spool
            .record_stat_data("refused", 2, Some("not json at all"))
            .is_err());
        spool.record_stat("second", 3).unwrap();

        let rows = records(&root);
        assert_eq!(
            rows.iter().map(|row| row["n"].clone()).collect::<Vec<_>>(),
            vec!["first", "second"],
            "a refused row was appended"
        );
        assert_eq!(rows[0]["seq"], 1);
        assert_eq!(
            rows[1]["seq"], 2,
            "a refused row consumed a sequence number"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_upload_retries_without_another_event() {
        let root = fresh_root();
        TelemetrySpool::open(&root, identity())
            .unwrap()
            .record_stat("retry-me", 1)
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(800);
            let mut requests = 0;
            while requests < 2 && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if read_http_request(&mut stream).is_empty() {
                            continue;
                        }
                        requests += 1;
                        let status = if requests == 1 {
                            "503 Service Unavailable"
                        } else {
                            "204 No Content"
                        };
                        write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").unwrap();
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("test server failed: {error}"),
                }
            }
            requests
        });
        let worker = UploadWorker::start_with_timing(
            &root,
            TelemetryTransport {
                endpoint: format!("http://{address}/v1/events"),
                token: "secret".into(),
                game: "mag".into(),
                channel: "steam".into(),
            },
            test_upload_timing(),
        )
        .unwrap();

        let requests = server.join().unwrap();
        worker.finish(StopMode::Drain);

        assert_eq!(requests, 2);
        assert!(!root.join("reports/stats/steam/events.jsonl").exists());
        assert!(!root.join("reports/stats/steam/cursor.log").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repeated_wakes_keep_the_first_flush_deadline() {
        let first = Instant::now();
        let window = Duration::from_secs(60);
        let mut deadline = None;

        schedule_flush(&mut deadline, first, window, false);
        schedule_flush(
            &mut deadline,
            first + Duration::from_secs(45),
            window,
            false,
        );

        assert_eq!(deadline, Some(first + window));
    }

    #[test]
    fn flush_wait_is_capped_by_the_revocation_poll() {
        let now = Instant::now();
        assert_eq!(
            worker_wait(
                Some(now + Duration::from_secs(60)),
                now,
                Duration::from_millis(100),
            ),
            Duration::from_millis(100)
        );
    }

    #[test]
    fn live_events_wait_for_one_flush_window() {
        let root = fresh_root();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (uploaded_tx, uploaded_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let body = read_http_request(&mut stream);
                        if body.is_empty() {
                            continue;
                        }
                        write!(
                            stream,
                            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"
                        )
                        .unwrap();
                        uploaded_tx.send(body).unwrap();
                        return;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "flush window never expired");
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("test server failed: {error}"),
                }
            }
        });
        let mut timing = test_upload_timing();
        timing.flush_window = Duration::from_millis(500);
        let mut client = TelemetryClient::open_with_timing(
            &root,
            identity(),
            &format!("http://{address}/v1/events"),
            "secret",
            false,
            timing,
        )
        .unwrap();

        client.record_stat("first", 1).unwrap();
        assert!(uploaded_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        client.record_stat("second", 2).unwrap();
        let body = uploaded_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        client.finish(StopMode::Drain);
        server.join().unwrap();

        assert_eq!(batch_names(&body), ["first", "second"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_backlog_does_not_wait_for_a_new_flush_window() {
        let root = fresh_root();
        TelemetrySpool::open(&root, identity())
            .unwrap()
            .record_stat("from-last-run", 1)
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (uploaded_tx, uploaded_rx) = mpsc::channel();
        let server = std::thread::spawn(move || loop {
            let (mut stream, _) = listener.accept().unwrap();
            let body = read_http_request(&mut stream);
            if body.is_empty() {
                continue;
            }
            write!(
                stream,
                "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"
            )
            .unwrap();
            uploaded_tx.send(body).unwrap();
            return;
        });
        let mut timing = test_upload_timing();
        timing.flush_window = Duration::from_secs(5);
        let worker = UploadWorker::start_with_timing(
            &root,
            TelemetryTransport {
                endpoint: format!("http://{address}/v1/events"),
                token: "secret".into(),
                game: "mag".into(),
                channel: "steam".into(),
            },
            timing,
        )
        .unwrap();

        let body = uploaded_rx
            .recv_timeout(Duration::from_millis(300))
            .unwrap();
        worker.finish(StopMode::Drain);
        server.join().unwrap();

        assert_eq!(batch_names(&body), ["from-last-run"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_full_request_body_flushes_before_the_time_window() {
        let root = fresh_root();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (uploaded_tx, uploaded_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let body = read_http_request(&mut stream);
                        if body.is_empty() {
                            continue;
                        }
                        write!(
                            stream,
                            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"
                        )
                        .unwrap();
                        uploaded_tx.send(body).unwrap();
                        return;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "full body never flushed");
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("test server failed: {error}"),
                }
            }
        });
        let mut timing = test_upload_timing();
        timing.flush_window = Duration::from_secs(30);
        let mut client = TelemetryClient::open_with_timing(
            &root,
            identity(),
            &format!("http://{address}/v1/events"),
            "secret",
            false,
            timing,
        )
        .unwrap();
        for seq in 1_i64..=512 {
            let mut name = format!("row-{seq}");
            name.push_str(&"x".repeat(MAX_FIELD_CHARS - name.len()));
            client.record_stat(&name, seq).unwrap();
        }

        let uploaded = uploaded_rx.recv_timeout(Duration::from_secs(1));
        client.finish(StopMode::Discard);
        server.join().unwrap();
        let body = uploaded.unwrap();

        assert!(body.len() <= MAX_BODY_BYTES);
        assert!(body.len() > MAX_BODY_BYTES / 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shutdown_flushes_a_live_partial_batch() {
        let root = fresh_root();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop_server = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_stop = Arc::clone(&stop_server);
        let server = std::thread::spawn(move || {
            let mut received = None;
            while received.is_none() && !server_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let body = read_http_request(&mut stream);
                        if body.is_empty() {
                            continue;
                        }
                        write!(
                            stream,
                            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"
                        )
                        .unwrap();
                        received = Some(body);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("test server failed: {error}"),
                }
            }
            received
        });
        let mut timing = test_upload_timing();
        timing.flush_window = Duration::from_secs(30);
        let mut client = TelemetryClient::open_with_timing(
            &root,
            identity(),
            &format!("http://{address}/v1/events"),
            "secret",
            false,
            timing,
        )
        .unwrap();

        client.record_stat("before-close", 1).unwrap();
        client.finish(StopMode::Drain);
        stop_server.store(true, Ordering::SeqCst);
        let body = server
            .join()
            .unwrap()
            .expect("shutdown skipped the partial batch");

        assert_eq!(batch_names(&body), ["before-close"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn revocation_polling_continues_during_the_flush_window() {
        let root = fresh_root();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut timing = test_upload_timing();
        timing.flush_window = Duration::from_secs(30);
        let mut client = TelemetryClient::open_with_timing(
            &root,
            identity(),
            &format!("http://{address}/v1/events"),
            "secret",
            false,
            timing,
        )
        .unwrap();

        client.record_stat("revoke-before-flush", 1).unwrap();
        publish_revocation(&root).unwrap();
        let deadline = Instant::now() + Duration::from_millis(200);
        while root.join("reports/stats").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        client.finish(StopMode::Discard);

        assert!(!root.join("reports/stats").exists());
        // The worker is joined, so nothing further can upload; drain anything
        // already queued and fail only on a real request. An empty read is a
        // stray port probe from another test process, not an upload attempt.
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    assert!(
                        read_http_request(&mut stream).is_empty(),
                        "revoked rows were uploaded during the flush window"
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("test server failed: {error}"),
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn permanent_rejection_quarantines_the_batch_and_drains_its_successor() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();
        spool.record_stat("row-0", 1).unwrap();
        let expected = grow_log_past_one_batch(&spool.events_path);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(800);
            let mut bodies = Vec::new();
            while bodies.len() < 2 && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let body = read_http_request(&mut stream);
                        if body.is_empty() {
                            continue;
                        }
                        bodies.push(body);
                        let status = if bodies.len() == 1 {
                            "400 Bad Request"
                        } else {
                            "204 No Content"
                        };
                        write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").unwrap();
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("test server failed: {error}"),
                }
            }
            bodies
        });
        let worker = UploadWorker::start_with_timing(
            &root,
            TelemetryTransport {
                endpoint: format!("http://{address}/v1/events"),
                token: "secret".into(),
                game: "mag".into(),
                channel: "steam".into(),
            },
            test_upload_timing(),
        )
        .unwrap();

        let bodies = server.join().unwrap();
        worker.finish(StopMode::Drain);

        assert_eq!(bodies.len(), 2);
        let first = batch_names(&bodies[0]);
        let second = batch_names(&bodies[1]);
        assert!(first.len() < expected.len(), "fixture fit in one request");
        assert_eq!(
            first.iter().chain(&second).cloned().collect::<Vec<_>>(),
            expected
        );
        let quarantine =
            fs::read_to_string(root.join("reports/stats/steam/quarantine.jsonl")).unwrap();
        let rejected: Value = serde_json::from_str(quarantine.trim_end()).unwrap();
        assert_eq!(rejected["status"], 400);
        assert_eq!(batch_names(rejected["body"].as_str().unwrap()), first);
        assert!(!root.join("reports/stats/steam/events.jsonl").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn authentication_failure_stays_queued_without_busy_retrying() {
        let root = fresh_root();
        TelemetrySpool::open(&root, identity())
            .unwrap()
            .record_stat("keep-for-a-new-build", 1)
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (first_tx, first_rx) = mpsc::channel();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            let mut requests = 0;
            while !server_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if read_http_request(&mut stream).is_empty() {
                            continue;
                        }
                        requests += 1;
                        write!(
                            stream,
                            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n"
                        )
                        .unwrap();
                        if requests == 1 {
                            first_tx.send(()).unwrap();
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("test server failed: {error}"),
                }
            }
            requests
        });
        let worker = UploadWorker::start_with_timing(
            &root,
            TelemetryTransport {
                endpoint: format!("http://{address}/v1/events"),
                token: "wrong".into(),
                game: "mag".into(),
                channel: "steam".into(),
            },
            test_upload_timing(),
        )
        .unwrap();

        first_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert!(root.join("reports/stats/steam/events.jsonl").exists());
        publish_revocation(&root).unwrap();
        let deadline = Instant::now() + Duration::from_millis(200);
        while root.join("reports/stats").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        worker.finish(StopMode::Discard);
        stop.store(true, Ordering::SeqCst);
        let requests = server.join().unwrap();

        assert_eq!(requests, 1);
        assert!(!root.join("reports/stats").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shutdown_does_not_resend_an_in_flight_authentication_failure() {
        let root = fresh_root();
        TelemetrySpool::open(&root, identity())
            .unwrap()
            .record_stat("keep-for-a-new-build", 1)
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (received_tx, received_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            let mut requests = 0;
            while !server_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if read_http_request(&mut stream).is_empty() {
                            continue;
                        }
                        requests += 1;
                        if requests == 1 {
                            received_tx.send(()).unwrap();
                            release_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                        }
                        write!(
                            stream,
                            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n"
                        )
                        .unwrap();
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("test server failed: {error}"),
                }
            }
            requests
        });
        let worker = UploadWorker::start_with_timing(
            &root,
            TelemetryTransport {
                endpoint: format!("http://{address}/v1/events"),
                token: "wrong".into(),
                game: "mag".into(),
                channel: "steam".into(),
            },
            test_upload_timing(),
        )
        .unwrap();

        received_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let finisher = std::thread::spawn(move || worker.finish(StopMode::Drain));
        release_tx.send(()).unwrap();
        finisher.join().unwrap();
        stop.store(true, Ordering::SeqCst);
        let requests = server.join().unwrap();

        assert_eq!(requests, 1);
        assert!(root.join("reports/stats/steam/events.jsonl").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn http_statuses_have_explicit_retry_reject_and_block_classes() {
        assert!(matches!(classify_http_status(408), HttpFailureClass::Retry));
        assert!(matches!(classify_http_status(429), HttpFailureClass::Retry));
        assert!(matches!(classify_http_status(503), HttpFailureClass::Retry));
        for status in [400, 409, 413, 422] {
            assert!(matches!(
                classify_http_status(status),
                HttpFailureClass::Reject
            ));
        }
        for status in [401, 403, 404, 405] {
            assert!(matches!(
                classify_http_status(status),
                HttpFailureClass::Blocked
            ));
        }
    }

    #[test]
    fn quarantine_must_be_durable_before_the_rejected_batch_is_acknowledged() {
        let root = fresh_root();
        TelemetrySpool::open(&root, identity())
            .unwrap()
            .record_stat("must-replay", 1)
            .unwrap();
        let drain = TelemetryDrain::open(&root, "steam").unwrap();
        fs::create_dir(&drain.quarantine_path).unwrap();

        let error = match drain.run_while(|| true, |_| UploadDisposition::Reject(400)) {
            Err(error) => error,
            Ok(_) => panic!("an unwritable quarantine was acknowledged"),
        };
        assert!(error.raw_os_error().is_some());
        assert!(!drain.cursor_path.exists());

        fs::remove_dir(&drain.quarantine_path).unwrap();
        let mut replayed = Vec::new();
        drain
            .run(|body| {
                replayed.extend(batch_names(body));
                Ok(())
            })
            .unwrap();
        assert_eq!(replayed, ["must-replay"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn quarantine_storage_is_bounded() {
        let root = fresh_root();
        TelemetrySpool::open(&root, identity())
            .unwrap()
            .record_stat("rejected", 1)
            .unwrap();
        let drain = TelemetryDrain::open(&root, "steam").unwrap();
        let mut quarantine = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&drain.quarantine_path)
            .unwrap();
        quarantine.set_len(MAX_SPOOL_BYTES - 1).unwrap();
        quarantine.seek(SeekFrom::End(-1)).unwrap();
        quarantine.write_all(b"\n").unwrap();
        quarantine.sync_all().unwrap();
        drop(quarantine);

        let outcome = drain
            .run_while(|| true, |_| UploadDisposition::Reject(413))
            .unwrap();

        assert!(matches!(outcome, DrainOutcome::Complete));
        let stored = fs::read_to_string(&drain.quarantine_path).unwrap();
        assert!(stored.len() < MAX_BODY_BYTES * 2);
        let rejected: Value = serde_json::from_str(stored.trim_end()).unwrap();
        assert_eq!(rejected["status"], 413);
        assert_eq!(
            batch_names(rejected["body"].as_str().unwrap()),
            ["rejected"]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retry_backoff_does_not_delay_revocation_polling() {
        let root = fresh_root();
        TelemetrySpool::open(&root, identity())
            .unwrap()
            .record_stat("revoke-during-backoff", 1)
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (failed_tx, failed_rx) = mpsc::channel();
        let stop_server = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_stop = Arc::clone(&stop_server);
        let server = std::thread::spawn(move || {
            let mut requests = 0;
            while !server_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if read_http_request(&mut stream).is_empty() {
                            continue;
                        }
                        requests += 1;
                        write!(
                            stream,
                            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"
                        )
                        .unwrap();
                        if requests == 1 {
                            failed_tx.send(()).unwrap();
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("test server failed: {error}"),
                }
            }
            requests
        });
        let mut timing = test_upload_timing();
        timing.retry_initial = Duration::from_secs(5);
        timing.retry_max = Duration::from_secs(5);
        timing.revocation_poll = Duration::from_millis(10);
        let worker = UploadWorker::start_with_timing(
            &root,
            TelemetryTransport {
                endpoint: format!("http://{address}/v1/events"),
                token: "secret".into(),
                game: "mag".into(),
                channel: "steam".into(),
            },
            timing,
        )
        .unwrap();

        failed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        publish_revocation(&root).unwrap();
        let deadline = Instant::now() + Duration::from_millis(200);
        while root.join("reports/stats").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let stats_remain = root.join("reports/stats").exists();
        worker.finish(StopMode::Discard);
        stop_server.store(true, Ordering::SeqCst);
        let requests = server.join().unwrap();

        assert!(!stats_remain, "retry sleep hid the withdrawal marker");
        assert_eq!(requests, 1, "revocation polling bypassed retry backoff");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discard_does_not_retry_a_failed_upload() {
        let root = fresh_root();
        TelemetrySpool::open(&root, identity())
            .unwrap()
            .record_stat("discard-me", 1)
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (failed_tx, failed_rx) = mpsc::channel();
        let stop_server = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_stop = Arc::clone(&stop_server);
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let mut requests = 0;
            while !server_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if read_http_request(&mut stream).is_empty() {
                            continue;
                        }
                        requests += 1;
                        write!(
                            stream,
                            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"
                        )
                        .unwrap();
                        if requests == 1 {
                            failed_tx.send(()).unwrap();
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("test server failed: {error}"),
                }
            }
            requests
        });
        let mut timing = test_upload_timing();
        timing.retry_initial = Duration::from_secs(5);
        timing.retry_max = Duration::from_secs(5);
        let worker = UploadWorker::start_with_timing(
            &root,
            TelemetryTransport {
                endpoint: format!("http://{address}/v1/events"),
                token: "secret".into(),
                game: "mag".into(),
                channel: "steam".into(),
            },
            timing,
        )
        .unwrap();

        // A bounded wait on the condition, not a latency assertion: the bound
        // only fires when the worker never attempts, however loaded the host.
        failed_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        worker.finish(StopMode::Discard);
        stop_server.store(true, Ordering::SeqCst);
        let requests = server.join().unwrap();

        assert_eq!(requests, 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn close_bounds_the_whole_shutdown_with_a_backlog() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();
        for seq in 0..30 {
            spool.record_stat(&format!("row-{seq}"), seq).unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop_server = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_stop = std::sync::Arc::clone(&stop_server);
        let requests = std::sync::Arc::new(AtomicU32::new(0));
        let server_requests = std::sync::Arc::clone(&requests);
        let server = std::thread::spawn(move || {
            while !server_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if read_http_request(&mut stream).is_empty() {
                            continue;
                        }
                        server_requests.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(250));
                        let _ = write!(
                            stream,
                            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"
                        );
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("test server failed: {error}"),
                }
            }
        });
        // Drive the production shutdown drain on this thread. Routing it
        // through the spawned upload worker races the worker's first
        // scheduling against the live-request timeout: a worker that enters
        // its blocking send before the Finish lands fails the bound without
        // production misbehaving. The through-worker Finish wiring stays
        // covered by `shutdown_flushes_a_live_partial_batch`.
        let drain = TelemetryDrain::open(&root, "steam").unwrap();
        let transport = TelemetryTransport {
            endpoint: format!("http://{address}/v1/events"),
            token: "secret".into(),
            game: "mag".into(),
            channel: "steam".into(),
        };

        let started = Instant::now();
        finish_upload(
            &root,
            StopMode::Drain,
            started + Duration::from_millis(80),
            &drain,
            &transport,
            Duration::from_millis(500),
            false,
        );
        let elapsed = started.elapsed();
        stop_server.store(true, Ordering::SeqCst);
        server.join().unwrap();

        let sent = requests.load(Ordering::SeqCst);
        assert!(sent > 0, "shutdown did not attempt any pending row");
        assert!(sent < 30, "shutdown drained the unbounded backlog");
        assert!(
            elapsed < Duration::from_millis(180),
            "shutdown took {elapsed:?}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The native client's whole path -- a queued spool row, the upload worker,
    /// the signed HTTP POST -- against a fixture server that records the
    /// request. Asserts the records the client EMITS; the dev ingest's own
    /// acceptance and storage belong to that ingest's tests.
    #[test]
    fn native_client_posts_the_documented_batch() {
        let root = fresh_root();
        let client_root = root.join("client");
        fs::create_dir_all(&client_root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop_server = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_stop = std::sync::Arc::clone(&stop_server);
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let server_captured = std::sync::Arc::clone(&captured);
        let server = std::thread::spawn(move || {
            while !server_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_full_http_request(&mut stream);
                        if request.is_empty() {
                            continue;
                        }
                        server_captured.lock().unwrap().push(request);
                        let _ = write!(
                            stream,
                            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"
                        );
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("test server failed: {error}"),
                }
            }
        });

        let mut dev_identity = identity();
        dev_identity.channel = "dev".into();
        let mut timing = test_upload_timing();
        timing.flush_window = Duration::from_millis(20);
        let mut client = TelemetryClient::open_with_timing(
            &client_root,
            dev_identity,
            &format!("http://{address}/v1/events"),
            "development-secret",
            false,
            timing,
        )
        .unwrap();
        client
            .record_stat_data(
                "death",
                1_754_600_000,
                Some(r#"{"cause":"Skeleton","run":3,"gold_lost":42,"ratio":0.5,"first":true}"#),
            )
            .unwrap();
        let queued = client.events_path.clone();
        let deadline = Instant::now() + Duration::from_secs(30);
        while captured.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        client.finish(StopMode::Drain);
        stop_server.store(true, Ordering::SeqCst);
        server.join().unwrap();

        let posted = captured.lock().unwrap().clone();
        assert_eq!(posted.len(), 1, "one spool row is one posted batch");
        let request = &posted[0];
        assert!(request.starts_with("POST /v1/events HTTP/1.1\r\n"));
        let headers = request.to_ascii_lowercase();
        assert!(headers.contains("x-rock-game: mag\r\n"));
        assert!(headers.contains("x-rock-channel: dev\r\n"));
        let body = http_body(request);
        let expected = hmac_sha256_hex("development-secret", body);
        assert!(
            headers.contains(&format!("x-rock-signature: sha256={expected}\r\n")),
            "the batch is signed with this client's channel token"
        );

        // The row is the one the game recorded, in the envelope the ingest
        // documents: one JSON object per line, still carrying the structured
        // data rather than the flattened name.
        let rows: Vec<Value> = body
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row["v"], 2);
        assert_eq!(row["kind"], "stat");
        assert_eq!(row["game"], "mag");
        assert_eq!(row["channel"], "dev");
        assert_eq!(row["n"], "death");
        assert_eq!(row["t"], 1_754_600_000_i64);
        assert_eq!(row["data"]["cause"], "Skeleton");
        assert_eq!(row["data"]["run"], 3);
        assert_eq!(row["data"]["gold_lost"], 42);
        assert_eq!(row["data"]["ratio"], 0.5);
        assert_eq!(row["data"]["first"], true);
        assert!(
            !queued.exists(),
            "the client did not observe and reclaim the accepted event"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn records_ordered_minimal_stat_envelopes() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();

        spool
            .record_stat("found:\"Orpiment\"", 1_754_600_000)
            .unwrap();
        spool.record_stat("died:any", 1_754_600_001).unwrap();

        let rows = records(&root);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["v"], 2);
        assert_eq!(rows[0]["kind"], "stat");
        assert_eq!(rows[0]["t"], 1_754_600_000_i64);
        assert!(Uuid::parse_str(rows[0]["consent"].as_str().unwrap()).is_ok());
        assert_eq!(rows[0]["seq"], 1);
        assert_eq!(rows[0]["build"], "abc123");
        assert_eq!(rows[0]["platform"], "linux");
        assert_eq!(rows[0]["game"], "mag");
        assert_eq!(rows[0]["channel"], "steam");
        assert_eq!(rows[0]["n"], "found:\"Orpiment\"");
        assert_eq!(rows[1]["seq"], 2);
        assert_eq!(rows[1]["n"], "died:any");
        assert!(rows[0]["session"].as_str().unwrap().len() >= 32);
        assert!(rows[0].get("install").is_none());
        assert!(rows[1].get("install").is_none());
        assert_eq!(rows[0]["session"], rows[1]["session"]);
        assert_eq!(rows[0]["consent"], rows[1]["consent"]);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_rotates_without_a_persistent_identity() {
        let root = fresh_root();
        TelemetrySpool::open(&root, identity())
            .unwrap()
            .record_stat("first", 1)
            .unwrap();
        TelemetrySpool::open(&root, identity())
            .unwrap()
            .record_stat("second", 2)
            .unwrap();

        let rows = records(&root);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].get("install").is_none());
        assert!(rows[1].get("install").is_none());
        assert_ne!(rows[0]["session"], rows[1]["session"]);
        assert_eq!(rows[0]["seq"], 1);
        assert_eq!(rows[1]["seq"], 1);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn drain_advances_a_persisted_cursor_only_after_acceptance() {
        let root = fresh_root();
        let upload_marker = root.join("reports").join(LAST_UPLOAD_FILE);
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();
        spool.record_stat("first", 1).unwrap();
        spool.record_stat("second", 2).unwrap();

        let mut sends = Vec::new();
        let drain = TelemetryDrain::open(&root, "steam").unwrap();
        let error = drain
            .run(|body| {
                sends.push(batch_names(body));
                Err(io::Error::other("server refused the batch"))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(sends, [["first", "second"]], "both rows ride one request");
        assert!(!upload_marker.exists(), "a refused batch is not an upload");

        // The batch is the unit of acknowledgement, so a refused batch replays
        // whole. Re-delivering a row is safe because the ingest keys each event
        // by id and a repeat is a no-op there.
        let mut retried = Vec::new();
        TelemetryDrain::open(&root, "steam")
            .unwrap()
            .run(|body| {
                retried.push(batch_names(body));
                Ok(())
            })
            .unwrap();
        assert_eq!(retried, [["first", "second"]]);

        spool.record_stat("third", 3).unwrap();
        let mut appended = Vec::new();
        TelemetryDrain::open(&root, "steam")
            .unwrap()
            .run(|body| {
                appended.push(batch_names(body));
                Ok(())
            })
            .unwrap();
        assert_eq!(
            appended,
            [["third"]],
            "an accepted batch advances the cursor"
        );
        let uploaded_at = fs::read_to_string(upload_marker).unwrap();
        assert!(uploaded_at.trim().parse::<u64>().unwrap() > 0);

        std::fs::remove_dir_all(root).unwrap();
    }

    // The mid-loop flush is a separate code path from the one at end-of-log, and
    // only this arm refuses a send on it. Without it, acknowledging before
    // sending there survives the whole suite.
    #[test]
    fn a_refused_first_batch_replays_from_the_start() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();
        spool.record_stat("row-0", 1).unwrap();
        let expected = grow_log_past_one_batch(&spool.events_path);

        let mut refused = Vec::new();
        let error = TelemetryDrain::open(&root, "steam")
            .unwrap()
            .run(|body| {
                refused.push(batch_names(body));
                Err(io::Error::other("server refused the batch"))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(refused.len(), 1, "the drain kept going past a refusal");
        assert!(
            refused[0].len() < expected.len(),
            "the log did not span two request bodies"
        );

        let mut retried = Vec::new();
        TelemetryDrain::open(&root, "steam")
            .unwrap()
            .run(|body| {
                retried.push(batch_names(body));
                Ok(())
            })
            .unwrap();
        let delivered: Vec<String> = retried.into_iter().flatten().collect();
        assert_eq!(delivered, expected, "a refused first batch lost rows");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_accepted_batch_is_not_replayed_when_a_later_batch_is_refused() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();
        spool.record_stat("row-0", 1).unwrap();

        let expected = grow_log_past_one_batch(&spool.events_path);

        let mut sends = Vec::new();
        let error = TelemetryDrain::open(&root, "steam")
            .unwrap()
            .run(|body| {
                sends.push(batch_names(body));
                if sends.len() == 2 {
                    return Err(io::Error::other("server refused the batch"));
                }
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(sends.len(), 2, "the log did not span two request bodies");

        let mut retried = Vec::new();
        TelemetryDrain::open(&root, "steam")
            .unwrap()
            .run(|body| {
                retried.push(batch_names(body));
                Ok(())
            })
            .unwrap();

        let delivered: Vec<String> = sends[0]
            .iter()
            .chain(retried.iter().flatten())
            .cloned()
            .collect();
        assert_eq!(delivered, expected, "every row lands exactly once");
        assert!(
            !retried.iter().flatten().any(|name| sends[0].contains(name)),
            "the accepted first batch was replayed"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opening_a_client_reclaims_a_fully_acknowledged_log() {
        let root = fresh_root();
        let mut spool = TelemetrySpool::open(&root, identity()).unwrap();
        spool.record_stat("first", 1).unwrap();
        TelemetryDrain::open(&root, "steam")
            .unwrap()
            .run(|_| Ok(()))
            .unwrap();
        assert!(root.join("reports/stats/steam/events.jsonl").exists());
        assert!(root.join("reports/stats/steam/cursor.log").exists());

        let client = TelemetryClient::open(&root, identity(), "", "", false).unwrap();

        // The reclaim removed both; reopening the spool recreates an empty
        // queue, and the absent cursor is the reclamation witness.
        assert_eq!(queue_len(&root.join("reports/stats/steam/events.jsonl")), 0);
        assert!(!root.join("reports/stats/steam/cursor.log").exists());

        client.finish(StopMode::Discard);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transport_posts_the_body_with_channel_hmac() {
        assert_eq!(
            hmac_sha256_hex("key", "The quick brown fox jumps over the lazy dog"),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || loop {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0_u8; 4096];
            let header_end = loop {
                let read = stream.read(&mut buf).unwrap();
                if read == 0 {
                    break None;
                }
                request.extend_from_slice(&buf[..read]);
                if let Some(index) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(index + 4);
                }
            };
            let Some(header_end) = header_end else {
                continue;
            };
            let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap();
            while request.len() < header_end + content_length {
                let read = stream.read(&mut buf).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
            }
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            return String::from_utf8(request).unwrap();
        });

        let body = r#"{"v":1,"kind":"stat"}"#;
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(2)))
            .build();
        let agent: ureq::Agent = config.into();
        let result = TelemetryTransport {
            endpoint: format!("http://{address}/v1/events"),
            token: "secret".into(),
            game: "mag".into(),
            channel: "steam".into(),
        }
        .send(&agent, body);
        assert!(matches!(result, UploadDisposition::Accepted));

        let request = server.join().unwrap();
        let expected = hmac_sha256_hex("secret", body);
        assert!(request.starts_with("POST /v1/events HTTP/1.1\r\n"));
        assert!(request
            .to_ascii_lowercase()
            .contains("x-rock-channel: steam\r\n"));
        assert!(request
            .to_ascii_lowercase()
            .contains("x-rock-game: mag\r\n"));
        assert!(request
            .to_ascii_lowercase()
            .contains(&format!("x-rock-signature: sha256={expected}\r\n")));
        assert!(request.ends_with(body));
    }
}
