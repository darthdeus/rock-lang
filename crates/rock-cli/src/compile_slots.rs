//! Machine-wide memory budget for concurrent full compiles
//! (docs/agent-coordination.md).
//!
//! The slot directory holds a pool of memory. Each compile reserves its
//! estimated peak ([`estimate_reserve`]), takes a numbered ticket in `queue/`
//! and, once admitted, holds a `held-<ticket>` file at the top level until it
//! drops its guard. An owner keeps an exclusive advisory lock on its ticket
//! and held file for their whole life, so a file whose lock can be taken
//! belongs to a dead owner and the next caller removes it. Any other locked
//! file at the top level also counts as a holder, reserving what its record
//! names or else a full compile.
//!
//! The author's interactive compiles try `author-0` first, outside the pool,
//! which batch work never takes. A process must not hold a slot while it
//! waits on another process that may need one.

use anyhow::{anyhow, Context, Result};
#[cfg(test)]
use std::cell::RefCell;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) const SLOT_DIR_ENV: &str = "ROCK_COMPILE_SLOT_DIR";
pub(crate) const SLOTS_ENV: &str = "ROCK_COMPILE_SLOTS";
pub(crate) const CLASS_ENV: &str = "ROCK_COMPILE_CLASS";
pub(crate) const SPAWN_PHASE_FILE_ENV: &str = "ROCK_SPAWN_PHASE_FILE";

const AUTHOR_SLOT: &str = "author-0";
const HELD_PREFIX: &str = "held-";
const QUEUE_DIR: &str = "queue";
const PEAKS_DIR: &str = "peaks";
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Measured peaks kept per package; the reservation covers the largest.
const PEAK_SAMPLES: usize = 8;

pub(crate) fn report_spawn_phase(phase: &str) {
    let Some(path) = std::env::var_os(SPAWN_PHASE_FILE_ENV) else { return };
    if let Err(error) = fs_err::write(&path, phase) {
        log::warn!(target: "CLI", "cannot record spawn phase {phase} at {}: {error}", Path::new(&path).display());
    }
}
/// Planned peak of one full mag compile (measured 4.3-5.4 GB).
const COMPILE_PEAK_MIB: u64 = 4608;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompileClass {
    Interactive,
    Batch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SlotConfig {
    pub dir: PathBuf,
    pub pool_mib: u64,
    pub class: CompileClass,
}

impl SlotConfig {
    /// A pool that fits `compiles` full compiles.
    #[cfg(test)]
    pub(crate) fn for_compiles(dir: &Path, compiles: u64, class: CompileClass) -> Self {
        Self { dir: dir.to_path_buf(), pool_mib: compiles * COMPILE_PEAK_MIB, class }
    }
}

/// How a compile got its slot: `slot` is `None` when the limit is off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SlotAcquisition {
    pub slot: Option<String>,
    pub waited: Duration,
}

thread_local! {
    static HOLDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    #[cfg(test)]
    static LAST_ACQUISITION: RefCell<Option<SlotAcquisition>> = const { RefCell::new(None) };
    #[cfg(test)]
    static TEST_CONFIG: RefCell<Option<SlotConfig>> = const { RefCell::new(None) };
    #[cfg(test)]
    static WAIT_OBSERVER: RefCell<Option<std::sync::mpsc::Sender<()>>> = const { RefCell::new(None) };
}

/// The last slot this thread took, for tests that see only a compile's result.
/// Every covered compile acquires on the thread that runs it.
#[cfg(test)]
pub(crate) fn last_acquisition() -> Option<SlotAcquisition> {
    LAST_ACQUISITION.with(|last| last.borrow().clone())
}

/// A held slot file: `held-<ticket>` (removed on release) or `author-0`.
struct Held {
    file: File,
    path: PathBuf,
    remove: bool,
    dir: PathBuf,
    peak_key: Option<String>,
    peak_at_admission: Option<u64>,
}

/// One compile's slot; dropping it frees the slot.
#[must_use = "the slot is freed when this guard drops"]
pub(crate) struct CompileSlot {
    held: Option<Held>,
    acquisition: SlotAcquisition,
    /// Set for guards from [`acquire`], which marks the thread as holding.
    marks_thread: bool,
}

impl CompileSlot {
    pub(crate) fn acquisition(&self) -> &SlotAcquisition {
        &self.acquisition
    }
}

impl Drop for CompileSlot {
    fn drop(&mut self) {
        if self.marks_thread {
            HOLDING.with(|holding| holding.set(false));
        }
        let Some(held) = self.held.take() else { return };
        match QueueGuard::lock(&held.dir) {
            Ok(_guard) => {
                if let Some(key) = &held.peak_key {
                    record_peak(&held.dir, key, held.peak_at_admission, process_peak_mib());
                }
                if held.remove {
                    if let Err(error) = std::fs::remove_file(&held.path) {
                        log::warn!(target: "COMPILE", "removing compile slot {}: {error}", held.path.display());
                    }
                }
            }
            // The unlock below still frees the slot; the next caller removes the file.
            Err(error) => log::warn!(target: "COMPILE", "releasing compile slot {}: {error:#}", held.path.display()),
        }
        // Closing releases the lock too; unlocking first also covers a
        // descriptor that leaked into a child.
        if let Err(error) = held.file.unlock() {
            log::warn!(target: "COMPILE", "unlocking compile slot {}: {error}", held.path.display());
        }
    }
}

/// Take a compile slot for `what` (named in the wait log), waiting while the
/// pool cannot fit its reservation.
pub(crate) fn acquire(what: &str) -> Result<CompileSlot> {
    if HOLDING.with(|holding| holding.get()) {
        return Err(anyhow!(
            "this thread already holds a compile slot; a nested acquire for {what} would wait on itself"
        ));
    }
    let mut slot = match current_config()? {
        Some(config) => acquire_with(&config, what)?,
        None => CompileSlot {
            held: None,
            acquisition: SlotAcquisition { slot: None, waited: Duration::ZERO },
            marks_thread: false,
        },
    };
    slot.marks_thread = true;
    HOLDING.with(|holding| holding.set(true));
    #[cfg(test)]
    LAST_ACQUISITION.with(|last| *last.borrow_mut() = Some(slot.acquisition.clone()));
    Ok(slot)
}

#[cfg(test)]
fn current_config() -> Result<Option<SlotConfig>> {
    // Unit tests never contend with the machine's real compiles; a test that
    // exercises the limit installs its own configuration on its thread.
    Ok(TEST_CONFIG.with(|config| config.borrow().clone()))
}

#[cfg(not(test))]
fn current_config() -> Result<Option<SlotConfig>> {
    use std::io::IsTerminal;
    static RAM_POOL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let inputs = EnvInputs {
        slots: env_var(SLOTS_ENV)?,
        class: env_var(CLASS_ENV)?,
        dir: env_var(SLOT_DIR_ENV)?,
        home: std::env::var_os("HOME").filter(|home| !home.is_empty()).map(PathBuf::from),
        job_id_set: std::env::var_os("ROCK_JOB_ID").is_some_and(|id| !id.is_empty()),
        stderr_is_terminal: std::io::stderr().is_terminal(),
    };
    config_from(&inputs, || *RAM_POOL.get_or_init(|| ram_pool_mib(crate::common::total_system_memory_mib())))
}

#[cfg(test)]
pub(crate) fn set_thread_config(config: Option<SlotConfig>) {
    TEST_CONFIG.with(|current| *current.borrow_mut() = config);
}

#[cfg_attr(test, allow(dead_code))]
fn env_var(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(anyhow!("{name}: {error}")),
    }
}

struct EnvInputs {
    slots: Option<String>,
    class: Option<String>,
    dir: Option<String>,
    home: Option<PathBuf>,
    job_id_set: bool,
    stderr_is_terminal: bool,
}

/// `None` when the limit is off (`ROCK_COMPILE_SLOTS=0`). `ROCK_COMPILE_SLOTS=N`
/// sizes the pool for N full compiles.
fn config_from(inputs: &EnvInputs, ram_pool: impl FnOnce() -> u64) -> Result<Option<SlotConfig>> {
    let pool_mib = match &inputs.slots {
        Some(value) => value.trim().parse::<u64>().map_err(|_| {
            anyhow!("{SLOTS_ENV} must be a non-negative integer, got `{value}`")
        })?.saturating_mul(COMPILE_PEAK_MIB),
        None => ram_pool(),
    };
    if pool_mib == 0 {
        return Ok(None);
    }
    let class = match inputs.class.as_deref() {
        Some("interactive") => CompileClass::Interactive,
        Some("batch") => CompileClass::Batch,
        Some(other) => return Err(anyhow!("{CLASS_ENV} must be `interactive` or `batch`, got `{other}`")),
        // Agents and rock-jobs run without a terminal; the author types in one.
        None if !inputs.job_id_set && inputs.stderr_is_terminal => CompileClass::Interactive,
        None => CompileClass::Batch,
    };
    let dir = match (&inputs.dir, &inputs.home) {
        (Some(dir), _) => PathBuf::from(dir),
        (None, Some(home)) => home.join("tmp/rock-compile-slots"),
        (None, None) => return Err(anyhow!("HOME is unset; set {SLOT_DIR_ENV} or {SLOTS_ENV}=0")),
    };
    Ok(Some(SlotConfig { dir, pool_mib, class }))
}

/// The batch pool: 40% of total RAM, at least one full compile.
fn ram_pool_mib(total_ram_mib: Option<u64>) -> u64 {
    let Some(total) = total_ram_mib else {
        log::warn!(target: "COMPILE", "no total RAM reading; allowing one full compile at a time");
        return COMPILE_PEAK_MIB;
    };
    (total.saturating_mul(4) / 10).max(COMPILE_PEAK_MIB)
}

pub(crate) fn acquire_with(config: &SlotConfig, what: &str) -> Result<CompileSlot> {
    let peak_key = package_key(what);
    let reserve = estimate_reserve(&config.dir, peak_key.as_deref());
    acquire_reserving(config, what, reserve, peak_key)
}

fn acquire_reserving(config: &SlotConfig, what: &str, reserve_mib: u64, peak_key: Option<String>) -> Result<CompileSlot> {
    report_spawn_phase("slot-wait");
    let started = Instant::now();
    fs_err::create_dir_all(&config.dir).context("creating the compile slot directory")?;
    let reserve_mib = reserve_mib.min(config.pool_mib);
    let peak_at_admission = peak_key.as_ref().and_then(|_| process_peak_mib());
    let held = |file, path: PathBuf, remove| Held {
        file, path, remove, dir: config.dir.clone(), peak_key: peak_key.clone(), peak_at_admission,
    };
    if config.class == CompileClass::Interactive {
        let path = config.dir.join(AUTHOR_SLOT);
        let mut file = open_slot(&path)?;
        match file.try_lock() {
            Ok(()) => {
                if let Err(error) = write_holder(&mut file, what, reserve_mib, std::env::args()) {
                    log::warn!(target: "COMPILE", "recording the holder of compile slot {AUTHOR_SLOT}: {error}");
                }
                log::debug!(target: "COMPILE", "took compile slot {AUTHOR_SLOT} for {what}");
                report_spawn_phase("compile");
                return Ok(CompileSlot {
                    held: Some(held(file, path, false)),
                    acquisition: SlotAcquisition { slot: Some(AUTHOR_SLOT.to_string()), waited: started.elapsed() },
                    marks_thread: false,
                });
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("locking compile slot {}", path.display()));
            }
        }
    }
    let mut ticket = {
        let _guard = QueueGuard::lock(&config.dir)?;
        Ticket::enqueue(&config.dir, what, reserve_mib)?
    };
    let mut waiting = false;
    loop {
        let busy = {
            let _guard = QueueGuard::lock(&config.dir)?;
            let pool = PoolState::scan(&config.dir, config.pool_mib)?;
            let position = pool.queue.iter().position(|entry| entry.name == ticket.name)
                .ok_or_else(|| anyhow!("compile slot ticket {} vanished from {}", ticket.name, config.dir.display()))?;
            if position < admitted_prefix(config.pool_mib, pool.held_mib(), &pool.queue) {
                let name = format!("{HELD_PREFIX}{}", ticket.name);
                let (file, path) = ticket.admit(&config.dir.join(&name), what, reserve_mib)?;
                let waited = started.elapsed();
                if waiting {
                    log::info!(target: "COMPILE", "took compile slot {name} ({reserve_mib} MiB) for {what} after waiting {:.1}s",
                        waited.as_secs_f64());
                } else {
                    log::debug!(target: "COMPILE", "took compile slot {name} ({reserve_mib} MiB) for {what} in {waited:?}");
                }
                report_spawn_phase("compile");
                return Ok(CompileSlot {
                    held: Some(held(file, path, true)),
                    acquisition: SlotAcquisition { slot: Some(name), waited },
                    marks_thread: false,
                });
            }
            (!waiting).then(|| pool.describe(position))
        };
        if let Some(busy) = busy {
            waiting = true;
            log::info!(target: "COMPILE", "waiting for a compile slot ({reserve_mib} MiB) for {what}; busy: {busy}");
            #[cfg(test)]
            WAIT_OBSERVER.with(|observer| {
                if let Some(sender) = observer.borrow_mut().take() {
                    sender.send(()).expect("slot-wait observer disconnected");
                }
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// How many waiters at the head of `queue` may hold the pool now.
///
/// Head-of-line rule: strict FIFO. Waiters are admitted in ticket order, each
/// once the holders and every older waiter leave room for its reservation;
/// no waiter passes an older one that does not fit. A large request at the
/// head therefore waits only for holders to finish, never for later small
/// ones, and small requests share the pool whenever they are next in line.
fn admitted_prefix(pool_mib: u64, held_mib: u64, queue: &[Entry]) -> usize {
    let mut used = held_mib;
    queue.iter().take_while(|entry| {
        let fits = used + entry.reserve_mib <= pool_mib;
        used += entry.reserve_mib;
        fits
    }).count()
}

/// The queue lock: every ticket, admission, release and stale removal happens
/// under it.
struct QueueGuard(File);

impl QueueGuard {
    fn lock(dir: &Path) -> Result<Self> {
        let queue = dir.join(QUEUE_DIR);
        fs_err::create_dir_all(&queue).context("creating the compile slot queue")?;
        let file = open_slot(&queue.join("guard"))?;
        file.lock().with_context(|| format!("locking the compile slot queue in {}", queue.display()))?;
        Ok(Self(file))
    }
}

impl Drop for QueueGuard {
    fn drop(&mut self) {
        if let Err(error) = self.0.unlock() {
            log::warn!(target: "COMPILE", "unlocking the compile slot queue: {error}");
        }
    }
}

/// A waiter's place in the queue; dropping it before admission leaves the queue.
struct Ticket {
    name: String,
    path: PathBuf,
    file: Option<File>,
}

impl Ticket {
    /// Called under the queue lock, so no scan sees the ticket before it is locked.
    fn enqueue(dir: &Path, what: &str, reserve_mib: u64) -> Result<Self> {
        let seq_path = dir.join(QUEUE_DIR).join("seq");
        let seq = match std::fs::read_to_string(&seq_path) {
            Ok(text) => text.trim().parse::<u64>()
                .with_context(|| format!("reading compile slot sequence {}: `{}`", seq_path.display(), text.trim()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error).with_context(|| format!("reading {}", seq_path.display())),
        } + 1;
        fs_err::write(&seq_path, format!("{seq}\n"))?;
        let name = format!("{seq:012}");
        let path = dir.join(QUEUE_DIR).join(&name);
        let file = open_slot(&path)?;
        file.lock().with_context(|| format!("locking compile slot ticket {}", path.display()))?;
        let mut ticket = Self { name, path, file: Some(file) };
        let file = ticket.file.as_mut().expect("just set");
        write_holder(file, what, reserve_mib, std::env::args())
            .with_context(|| format!("writing compile slot ticket {}", ticket.path.display()))?;
        Ok(ticket)
    }

    /// Moves the locked ticket to `held` with the holder record, keeping its lock.
    fn admit(&mut self, held: &Path, what: &str, reserve_mib: u64) -> Result<(File, PathBuf)> {
        let mut file = self.file.take().expect("a ticket is admitted once");
        if let Err(error) = write_holder(&mut file, what, reserve_mib, std::env::args()) {
            log::warn!(target: "COMPILE", "recording the holder of compile slot {}: {error}", held.display());
        }
        if let Err(error) = std::fs::rename(&self.path, held) {
            self.file = Some(file);
            return Err(error).with_context(|| format!("moving compile slot ticket to {}", held.display()));
        }
        Ok((file, held.to_path_buf()))
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let Some(file) = self.file.take() else { return };
        if let Err(error) = std::fs::remove_file(&self.path) {
            log::warn!(target: "COMPILE", "leaving the compile slot queue {}: {error}", self.path.display());
        }
        if let Err(error) = file.unlock() {
            log::warn!(target: "COMPILE", "unlocking compile slot ticket {}: {error}", self.path.display());
        }
    }
}

#[derive(Debug)]
struct Entry {
    name: String,
    reserve_mib: u64,
    record: String,
}

/// The live holders and waiters, oldest ticket first. Scanning removes the
/// files of dead owners.
struct PoolState {
    pool_mib: u64,
    holders: Vec<Entry>,
    queue: Vec<Entry>,
}

impl PoolState {
    fn scan(dir: &Path, pool_mib: u64) -> Result<Self> {
        let live = |dir: &Path, owned: &dyn Fn(&str) -> bool, removable: &dyn Fn(&str) -> bool| -> Result<Vec<Entry>> {
            let mut entries = Vec::new();
            for item in fs_err::read_dir(dir)? {
                let item = item?;
                let name = item.file_name().to_string_lossy().into_owned();
                // Dot files include the AppleDouble `._*` files macOS writes on ExFAT.
                if name.starts_with('.') || !owned(&name) || name == AUTHOR_SLOT || !item.file_type()?.is_file() {
                    continue;
                }
                match live_record(&item.path())? {
                    Some(record) => {
                        let reserve_mib = record_reserve(&record).unwrap_or(COMPILE_PEAK_MIB).min(pool_mib);
                        entries.push(Entry { name, reserve_mib, record });
                    }
                    None if removable(&name) => {
                        log::warn!(target: "COMPILE", "removing compile slot file {} of a dead owner ({})",
                            item.path().display(), std::fs::read_to_string(item.path()).unwrap_or_default().trim());
                        if let Err(error) = std::fs::remove_file(item.path()) {
                            if error.kind() != std::io::ErrorKind::NotFound {
                                return Err(error).with_context(|| format!("removing {}", item.path().display()));
                            }
                        }
                    }
                    // An unlocked file this scheme does not own holds nothing.
                    None => {}
                }
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(entries)
        };
        let is_ticket = |name: &str| name.len() == 12 && name.bytes().all(|b| b.is_ascii_digit());
        let holders = live(dir, &|_| true, &|name| name.strip_prefix(HELD_PREFIX).is_some_and(is_ticket))?;
        let queue = live(&dir.join(QUEUE_DIR), &is_ticket, &|_| true)?;
        Ok(Self { pool_mib, holders, queue })
    }

    fn held_mib(&self) -> u64 {
        self.holders.iter().map(|holder| holder.reserve_mib).sum()
    }

    fn describe(&self, position: usize) -> String {
        let holders = self.holders.iter()
            .map(|holder| format!("{} ({})", holder.name, holder.record.trim()))
            .collect::<Vec<_>>();
        format!("{}; {} of {} MiB reserved; queue position {} of {}",
            if holders.is_empty() { "no holders".to_string() } else { holders.join("; ") },
            self.held_mib(), self.pool_mib, position + 1, self.queue.len())
    }
}

/// The record of a slot file whose owner still holds its lock; `None` when
/// the file is gone or unlocked.
fn live_record(path: &Path) -> Result<Option<String>> {
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("opening compile slot {}", path.display())),
    };
    match file.try_lock() {
        Ok(()) => {
            file.unlock().with_context(|| format!("unlocking probed compile slot {}", path.display()))?;
            Ok(None)
        }
        Err(TryLockError::WouldBlock) => Ok(Some(std::fs::read_to_string(path).unwrap_or_default())),
        Err(TryLockError::Error(error)) => Err(error).with_context(|| format!("probing compile slot {}", path.display())),
    }
}

fn record_reserve(record: &str) -> Option<u64> {
    record.split_whitespace().find_map(|field| field.strip_prefix("reserve_mib=")?.parse().ok())
}

fn open_slot(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening compile slot {}", path.display()))
}

fn write_holder(file: &mut File, what: &str, reserve_mib: u64, args: impl IntoIterator<Item = String>) -> std::io::Result<()> {
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let cmdline = crate::invocation_log::redact_var_values(args).join(" ");
    file.set_len(0)?;
    file.rewind()?;
    writeln!(file, "pid={} start={started} for={what} reserve_mib={reserve_mib} cmd={cmdline}", std::process::id())
}

/// The peak-measurement key of a compile of `what`: its package name and the
/// `rock` subcommand, so every worktree's copy of a package and every run of
/// a test fixture share one measurement, while `rock test mag` (tests
/// included, about 4.9 GB) stays apart from `rock compile mag` (about 1.4 GB).
fn package_key(what: &str) -> Option<String> {
    let manifest = Path::new(what).join("stick.toml");
    match rock_host::stick::StickConfig::from_path_optional(&manifest) {
        Ok(config) => config.map(|config| {
            format!("{}.{}", config.package.name, crate::compile_measurements::command()).chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
                .collect()
        }),
        Err(error) => {
            log::debug!(target: "COMPILE", "no package name for {what}, reserving a full compile: {error:#}");
            None
        }
    }
}

/// A compile's reservation: the largest recent peak measured for its package
/// with 25% headroom (one package's peaks vary that much: mag measured
/// 4.3-5.4 GB), capped at a full compile. An unmeasured package reserves a
/// full compile.
fn estimate_reserve(dir: &Path, key: Option<&str>) -> u64 {
    key.and_then(|key| read_peaks(dir, key).into_iter().max())
        .map_or(COMPILE_PEAK_MIB, |peak| (peak + peak / 4).min(COMPILE_PEAK_MIB))
}

fn read_peaks(dir: &Path, key: &str) -> Vec<u64> {
    let path = dir.join(PEAKS_DIR).join(key);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            log::warn!(target: "COMPILE", "reading compile peaks {}: {error}", path.display());
            return Vec::new();
        }
    };
    let mut peaks = Vec::new();
    for line in text.lines() {
        match line.trim().parse() {
            Ok(peak) => peaks.push(peak),
            Err(error) => log::warn!(target: "COMPILE", "ignoring compile peak `{line}` in {}: {error}", path.display()),
        }
    }
    peaks
}

/// Records the process peak as `key`'s compile peak when it rose while the
/// slot was held; an earlier, larger peak bounds nothing about this compile.
/// Called under the queue lock.
fn record_peak(dir: &Path, key: &str, at_admission: Option<u64>, at_release: Option<u64>) {
    let (Some(before), Some(peak)) = (at_admission, at_release) else { return };
    if peak <= before {
        return;
    }
    let mut peaks = read_peaks(dir, key);
    peaks.push(peak);
    let keep = &peaks[peaks.len().saturating_sub(PEAK_SAMPLES)..];
    let text: String = keep.iter().map(|peak| format!("{peak}\n")).collect();
    let path = dir.join(PEAKS_DIR).join(key);
    if let Err(error) = fs_err::create_dir_all(dir.join(PEAKS_DIR)).and_then(|()| fs_err::write(&path, text)) {
        log::warn!(target: "COMPILE", "recording compile peak {peak} MiB for {key}: {error}");
    }
}

fn process_peak_mib() -> Option<u64> {
    crate::allocator::process_memory()["peak_rss_bytes"].as_u64().map(|bytes| bytes / (1024 * 1024))
}

/// Run `compile` on a thread whose pool is full, and check it waits for the
/// holder and takes a slot once freed. Returns the compile's result.
#[cfg(test)]
pub(crate) fn assert_compile_waits_for_slot<T: Send + 'static>(
    compile: impl FnOnce() -> T + Send + 'static,
) -> T {
    let slots = tempfile::tempdir().unwrap();
    let config = SlotConfig::for_compiles(slots.path(), 1, CompileClass::Batch);
    let held = acquire_with(&config, "test holder").unwrap();
    let (worker, waiting) = spawn_slot_waiter(move || {
        set_thread_config(Some(config));
        let result = compile();
        (result, last_acquisition())
    });
    let ((result, acquisition), held_for) = finish_slot_waiter(held, worker, waiting);
    let acquisition = acquisition.expect("the compile took no slot");
    assert!(acquisition.slot.as_deref().is_some_and(|slot| slot.starts_with(HELD_PREFIX)), "{acquisition:?}");
    assert!(acquisition.waited >= held_for, "held after observing contention: {held_for:?}; {acquisition:?}");
    result
}

#[cfg(test)]
pub(crate) fn spawn_slot_waiter<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> (std::thread::JoinHandle<T>, std::sync::mpsc::Receiver<()>) {
    let (sender, waiting) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        WAIT_OBSERVER.with(|observer| *observer.borrow_mut() = Some(sender));
        work()
    });
    (worker, waiting)
}

#[cfg(test)]
pub(crate) fn finish_slot_waiter<T>(
    held: CompileSlot,
    worker: std::thread::JoinHandle<T>,
    waiting: std::sync::mpsc::Receiver<()>,
) -> (T, Duration) {
    let observed = waiting.recv_timeout(Duration::from_secs(30));
    let started = Instant::now();
    if observed.is_ok() {
        std::thread::sleep(Duration::from_millis(300));
    }
    let held_for = started.elapsed();
    let finished_while_held = worker.is_finished();
    // Release and join before any assertion, including a missing contention signal.
    drop(held);
    let result = worker.join();
    observed.expect("the compile never reported slot contention");
    assert!(!finished_while_held, "the compile ran while the pool was full");
    (result.unwrap(), held_for)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn a_holder_record_never_contains_var_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("held-000000000001");
        let mut file = open_slot(&path).unwrap();
        file.lock().unwrap();
        let args = ["rock", "package", "mag", "--var", "TELEMETRY_TOKEN=holder-secret-1", "--var=OTHER=holder-secret-2"];
        write_holder(&mut file, "package", 4608, args.iter().map(|arg| arg.to_string())).unwrap();
        let record = std::fs::read_to_string(&path).unwrap();
        assert!(!record.contains("holder-secret"), "holder record leaked a --var value: {record}");
        assert!(record.contains("TELEMETRY_TOKEN") && record.contains("OTHER"), "holder record keeps the names: {record}");
        std::fs::create_dir(dir.path().join(QUEUE_DIR)).unwrap();
        let busy = PoolState::scan(dir.path(), 4608).unwrap().describe(0);
        assert!(busy.contains("held-000000000001") && !busy.contains("holder-secret"), "{busy}");
    }

    fn config(dir: &Path, compiles: u64, class: CompileClass) -> SlotConfig {
        SlotConfig::for_compiles(dir, compiles, class)
    }

    fn pool(dir: &Path, pool_mib: u64) -> SlotConfig {
        SlotConfig { dir: dir.to_path_buf(), pool_mib, class: CompileClass::Batch }
    }

    fn inputs(slots: Option<&str>, class: Option<&str>, job_id_set: bool, tty: bool) -> EnvInputs {
        EnvInputs {
            slots: slots.map(str::to_string),
            class: class.map(str::to_string),
            dir: None,
            home: Some(PathBuf::from("/home/someone")),
            job_id_set,
            stderr_is_terminal: tty,
        }
    }

    /// Whether `name` is free right now, without waiting.
    fn slot_is_free(dir: &Path, name: &str) -> bool {
        let file = open_slot(&dir.join(name)).unwrap();
        match file.try_lock() {
            Ok(()) => true,
            Err(TryLockError::WouldBlock) => false,
            Err(TryLockError::Error(error)) => panic!("{error}"),
        }
    }

    fn held_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap()
            .map(|item| item.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(HELD_PREFIX))
            .collect();
        names.sort();
        names
    }

    fn queued_tickets(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.join(QUEUE_DIR)).unwrap()
            .map(|item| item.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "guard" && name != "seq")
            .collect();
        names.sort();
        names
    }

    #[test]
    fn second_acquirer_waits_until_the_first_guard_drops() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 1, CompileClass::Batch);
        let first = acquire_with(&config, "first").unwrap();
        assert_eq!(first.acquisition().slot.as_deref(), Some("held-000000000001"));
        let (waiter, waiting) = {
            let config = config.clone();
            spawn_slot_waiter(move || acquire_with(&config, "second").map(|slot| slot.acquisition().clone()))
        };
        let (second, held_for) = finish_slot_waiter(first, waiter, waiting);
        let second = second.unwrap();
        assert_eq!(second.slot.as_deref(), Some("held-000000000002"));
        assert!(second.waited >= held_for, "held after observing contention: {held_for:?}; {second:?}");
        assert!(held_files(dir.path()).is_empty() && queued_tickets(dir.path()).is_empty(), "released slots leave no files");
    }

    #[test]
    fn waiters_are_admitted_in_arrival_order() {
        let dir = tempfile::tempdir().unwrap();
        let config = pool(dir.path(), 1000);
        let held = acquire_reserving(&config, "holder", 1000, None).unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut workers = Vec::new();
        for label in ["first", "second", "third", "fourth"] {
            let (config, order) = (config.clone(), order.clone());
            let (worker, waiting) = spawn_slot_waiter(move || {
                let slot = acquire_reserving(&config, label, 1000, None).unwrap();
                order.lock().unwrap().push(label);
                std::thread::sleep(Duration::from_millis(20));
                drop(slot);
            });
            waiting.recv_timeout(Duration::from_secs(10)).expect("waiter never queued");
            workers.push(worker);
        }
        drop(held);
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(*order.lock().unwrap(), ["first", "second", "third", "fourth"]);
    }

    #[test]
    fn small_compiles_share_the_pool() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 1, CompileClass::Batch);
        let (sender, taken) = std::sync::mpsc::channel();
        {
            let config = config.clone();
            // On a thread, so a pool that cannot share fails the test instead of hanging it.
            std::thread::spawn(move || {
                let small: Vec<_> = (0..4).map(|_| acquire_reserving(&config, "small", 1000, None).unwrap()).collect();
                sender.send(small).unwrap();
            });
        }
        let mut small = taken.recv_timeout(Duration::from_secs(10)).expect("four small compiles did not fit together");
        assert_eq!(held_files(dir.path()).len(), 4, "four small compiles hold the pool at once");
        let (waiter, waiting) = {
            let config = config.clone();
            spawn_slot_waiter(move || acquire_reserving(&config, "fifth", 1000, None).map(|slot| slot.acquisition().clone()))
        };
        let (fifth, _) = finish_slot_waiter(small.pop().unwrap(), waiter, waiting);
        assert!(fifth.unwrap().slot.is_some());
    }

    #[test]
    fn a_large_head_request_is_not_passed_by_smaller_ones() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 1, CompileClass::Batch);
        let held = acquire_reserving(&config, "small holder", 1000, None).unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut workers = Vec::new();
        for (label, reserve) in [("large", COMPILE_PEAK_MIB), ("small", 1000)] {
            let (config, order) = (config.clone(), order.clone());
            let (worker, waiting) = spawn_slot_waiter(move || {
                let slot = acquire_reserving(&config, label, reserve, None).unwrap();
                order.lock().unwrap().push(label);
                std::thread::sleep(Duration::from_millis(200));
                drop(slot);
            });
            // The small request fits beside the holder, but the large one is ahead.
            waiting.recv_timeout(Duration::from_secs(10)).expect("waiter never queued");
            workers.push(worker);
        }
        std::thread::sleep(Duration::from_millis(300));
        assert!(order.lock().unwrap().is_empty(), "a waiter passed the head: {:?}", order.lock().unwrap());
        drop(held);
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(*order.lock().unwrap(), ["large", "small"]);
    }

    #[test]
    fn a_foreign_locked_file_occupies_its_recorded_or_full_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 1, CompileClass::Batch);
        let barrier = open_slot(&dir.path().join("batch-0")).unwrap();
        barrier.lock().unwrap();
        let (waiter, waiting) = {
            let config = config.clone();
            spawn_slot_waiter(move || acquire_reserving(&config, "small", 1000, None).map(|slot| slot.acquisition().clone()))
        };
        let barrier = CompileSlot {
            held: Some(Held { file: barrier, path: dir.path().join("batch-0"), remove: false,
                dir: dir.path().to_path_buf(), peak_key: None, peak_at_admission: None }),
            acquisition: SlotAcquisition { slot: None, waited: Duration::ZERO },
            marks_thread: false,
        };
        let (small, _) = finish_slot_waiter(barrier, waiter, waiting);
        assert!(small.unwrap().slot.is_some());
        assert!(dir.path().join("batch-0").exists(), "an unlocked foreign file is left alone");
    }

    #[test]
    fn files_outside_the_naming_scheme_are_never_removed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(QUEUE_DIR)).unwrap();
        let foreign = ["._held-000000000001", "queue/._guard", "queue/notes", "held-notes"];
        for name in foreign {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        std::fs::write(dir.path().join("queue/000000000001"), "pid=1").unwrap();
        let pool = PoolState::scan(dir.path(), 4608).unwrap();
        assert!(pool.holders.is_empty() && pool.queue.is_empty());
        assert!(foreign.iter().all(|name| dir.path().join(name).exists()), "a foreign file was removed");
        assert!(!dir.path().join("queue/000000000001").exists(), "a dead ticket stays");
    }

    #[test]
    fn batch_never_takes_the_author_slot() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 1, CompileClass::Batch);
        let held = acquire_with(&config, "held").unwrap();
        assert!(slot_is_free(dir.path(), AUTHOR_SLOT));
        let (waiter, waiting) = {
            let config = config.clone();
            spawn_slot_waiter(move || acquire_with(&config, "batch").map(|slot| slot.acquisition().clone()))
        };
        let (acquisition, held_for) = finish_slot_waiter(held, waiter, waiting);
        let acquisition = acquisition.unwrap();
        assert!(acquisition.slot.as_deref().is_some_and(|slot| slot.starts_with(HELD_PREFIX)), "{acquisition:?}");
        assert!(acquisition.waited >= held_for, "held after observing contention: {held_for:?}; {acquisition:?}");
    }

    #[test]
    fn interactive_prefers_the_author_slot_then_falls_back_to_the_pool() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 2, CompileClass::Interactive);
        let first = acquire_with(&config, "first").unwrap();
        assert_eq!(first.acquisition().slot.as_deref(), Some(AUTHOR_SLOT));
        let second = acquire_with(&config, "second").unwrap();
        assert_eq!(second.acquisition().slot.as_deref(), Some("held-000000000001"));
        assert!(!slot_is_free(dir.path(), AUTHOR_SLOT));
        let third = acquire_with(&config, "third").unwrap();
        assert_eq!(third.acquisition().slot.as_deref(), Some("held-000000000002"));
        drop(first);
        assert!(slot_is_free(dir.path(), AUTHOR_SLOT));
    }

    #[test]
    fn zero_slots_disable_the_limit() {
        assert_eq!(config_from(&inputs(Some("0"), Some("batch"), true, false), || 5).unwrap(), None);
        set_thread_config(None);
        let slot = acquire("unlimited").unwrap();
        assert_eq!(slot.acquisition().slot, None);
    }

    #[test]
    fn a_thread_cannot_take_a_second_slot_while_holding_one() {
        set_thread_config(None);
        let outer = acquire("outer").unwrap();
        let nested = acquire("inner").err().expect("a nested acquire would wait on itself");
        assert!(format!("{nested:#}").contains("already holds"), "{nested:#}");
        drop(outer);
        let _after = acquire("after").unwrap();
    }

    #[test]
    fn configuration_reads_count_class_and_directory() {
        let batch = config_from(&inputs(Some("3"), None, true, true), || unreachable!()).unwrap().unwrap();
        assert_eq!(batch, config(Path::new("/home/someone/tmp/rock-compile-slots"), 3, CompileClass::Batch));
        let author = config_from(&inputs(None, None, false, true), || 9000).unwrap().unwrap();
        assert_eq!((author.pool_mib, author.class), (9000, CompileClass::Interactive));
        let agent = config_from(&inputs(None, None, false, false), || 9000).unwrap().unwrap();
        assert_eq!(agent.class, CompileClass::Batch);
        let forced = config_from(&inputs(None, Some("interactive"), true, false), || 9000).unwrap().unwrap();
        assert_eq!(forced.class, CompileClass::Interactive);
        assert!(config_from(&inputs(Some("many"), None, true, false), || 9000).is_err());
        assert!(config_from(&inputs(None, Some("urgent"), true, false), || 9000).is_err());
    }

    #[test]
    fn ram_budget_matches_the_machines() {
        let full_compiles = |gib: Option<u64>| ram_pool_mib(gib.map(|n| n * 1024)) / COMPILE_PEAK_MIB;
        assert_eq!(full_compiles(Some(62)), 5, "host-a");
        assert_eq!(full_compiles(Some(36)), 3, "Studio");
        assert_eq!(full_compiles(Some(24)), 2);
        assert_eq!(full_compiles(Some(16)), 1, "m1key");
        assert_eq!(full_compiles(Some(4)), 1);
        assert_eq!(full_compiles(None), 1);
        assert_eq!(ram_pool_mib(Some(62 * 1024)), 25395, "the pool is 40% of RAM, not whole compiles");
    }

    #[test]
    fn reservations_follow_measured_package_peaks() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(estimate_reserve(dir.path(), None), COMPILE_PEAK_MIB);
        assert_eq!(estimate_reserve(dir.path(), Some("fixture")), COMPILE_PEAK_MIB, "unmeasured");
        record_peak(dir.path(), "fixture", Some(100), Some(400));
        assert_eq!(estimate_reserve(dir.path(), Some("fixture")), 500);
        record_peak(dir.path(), "fixture", Some(900), Some(900));
        assert_eq!(read_peaks(dir.path(), "fixture"), [400], "a peak that did not rise bounds nothing");
        record_peak(dir.path(), "mag", Some(50), Some(5000));
        assert_eq!(estimate_reserve(dir.path(), Some("mag")), COMPILE_PEAK_MIB, "capped at a full compile");
        for _ in 0..PEAK_SAMPLES {
            record_peak(dir.path(), "fixture", Some(0), Some(80));
        }
        assert_eq!(estimate_reserve(dir.path(), Some("fixture")), 100, "old samples age out");

        let package = tempfile::tempdir().unwrap();
        assert_eq!(package_key(package.path().to_str().unwrap()), None);
        std::fs::write(package.path().join("stick.toml"), "[package]\nname = \"slot/probe\"\ntype = \"lib\"\n").unwrap();
        assert_eq!(package_key(package.path().to_str().unwrap()).as_deref(), Some("slot_probe.other"));
    }

    const CHILD_DIR_ENV: &str = "ROCK_COMPILE_SLOT_TEST_CHILD_DIR";
    const CHILD_MODE_ENV: &str = "ROCK_COMPILE_SLOT_TEST_CHILD_MODE";

    /// The child half of the dead-owner tests: holds (or queues for) the
    /// only full-compile slot until it is killed.
    #[test]
    #[ignore = "spawned by the dead-owner tests"]
    fn hold_slot_until_killed() {
        let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else { return };
        let config = config(Path::new(&dir), 1, CompileClass::Batch);
        let _slot = if std::env::var_os(CHILD_MODE_ENV).is_some_and(|mode| mode == "queue") {
            let (_worker, waiting) = spawn_slot_waiter(move || acquire_with(&config, "child"));
            waiting.recv_timeout(Duration::from_secs(30)).unwrap();
            None
        } else {
            Some(acquire_with(&config, "child").unwrap())
        };
        // Serial libtest prints its test-name prefix without a trailing newline.
        println!("\nready");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    fn spawn_child(dir: &Path, mode: &str) -> std::process::Child {
        use std::io::BufRead;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "compile_slots::tests::hold_slot_until_killed", "--ignored", "--nocapture", "--test-threads=1"])
            .env(CHILD_DIR_ENV, dir)
            .env(CHILD_MODE_ENV, mode)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let ready = std::io::BufReader::new(stdout).lines().any(|line| line.unwrap().trim() == "ready");
        assert!(ready, "the child never reported its slot state");
        child
    }

    #[test]
    fn a_killed_holder_frees_its_slot() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = spawn_child(dir.path(), "hold");
        let held = held_files(dir.path());
        assert_eq!(held, ["held-000000000001"]);
        assert!(!slot_is_free(dir.path(), &held[0]), "the child's slot is not locked");
        let holder = std::fs::read_to_string(dir.path().join(&held[0])).unwrap();
        assert!(holder.contains(&format!("pid={}", child.id())), "{holder}");
        child.kill().unwrap();
        child.wait().unwrap();
        let next = acquire_with(&config(dir.path(), 1, CompileClass::Batch), "next").unwrap();
        assert!(next.acquisition().waited < Duration::from_secs(1), "{:?}", next.acquisition());
        assert_eq!(held_files(dir.path()), ["held-000000000002"], "the dead holder's file is removed");
    }

    #[test]
    fn a_killed_waiter_leaves_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 1, CompileClass::Batch);
        let held = acquire_with(&config, "parent").unwrap();
        let mut child = spawn_child(dir.path(), "queue");
        assert_eq!(queued_tickets(dir.path()), ["000000000002"]);
        child.kill().unwrap();
        child.wait().unwrap();
        let (sender, admitted) = std::sync::mpsc::channel();
        let (_worker, waiting) = spawn_slot_waiter(move || {
            let slot = acquire_with(&config, "after the dead waiter");
            sender.send(slot.map(|slot| slot.acquisition().clone())).unwrap();
        });
        waiting.recv_timeout(Duration::from_secs(10)).expect("the next waiter never queued");
        drop(held);
        let acquisition = admitted.recv_timeout(Duration::from_secs(10))
            .expect("a dead waiter's ticket blocked the queue").unwrap();
        assert_eq!(acquisition.slot.as_deref(), Some("held-000000000003"));
        assert!(queued_tickets(dir.path()).is_empty(), "{:?}", queued_tickets(dir.path()));
    }
}
