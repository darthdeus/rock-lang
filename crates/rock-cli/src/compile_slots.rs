//! Machine-wide limit on concurrent full compiles (docs/machine-coordination.md).
//!
//! A slot is an exclusive advisory lock on a file in a per-user directory, so
//! the kernel frees it when its holder exits or dies. Batch compiles take
//! `batch-<i>`; the author's interactive compiles try `author-0` first, which
//! batch work never takes. A process must not hold a slot while it waits on
//! another process that may need one.

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

const AUTHOR_SLOT: &str = "author-0";
const RETRY_INTERVAL: Duration = Duration::from_millis(50);
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
    pub batch_slots: usize,
    pub class: CompileClass,
}

impl SlotConfig {
    fn candidates(&self) -> Vec<String> {
        let batch = (0..self.batch_slots).map(|index| format!("batch-{index}"));
        match self.class {
            CompileClass::Interactive => std::iter::once(AUTHOR_SLOT.to_string()).chain(batch).collect(),
            CompileClass::Batch => batch.collect(),
        }
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

/// One compile's slot; dropping it frees the slot.
#[must_use = "the slot is freed when this guard drops"]
pub(crate) struct CompileSlot {
    file: Option<File>,
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
        let Some(file) = self.file.take() else { return };
        // Closing releases the lock too; unlocking first also covers a
        // descriptor that leaked into a child.
        if let Err(error) = file.unlock() {
            log::warn!(target: "COMPILE", "unlocking compile slot {:?}: {error}", self.acquisition.slot);
        }
    }
}

/// Take a compile slot for `what` (named in the wait log), waiting while every
/// candidate slot is busy.
pub(crate) fn acquire(what: &str) -> Result<CompileSlot> {
    if HOLDING.with(|holding| holding.get()) {
        return Err(anyhow!(
            "this thread already holds a compile slot; a nested acquire for {what} would wait on itself"
        ));
    }
    let mut slot = match current_config()? {
        Some(config) => acquire_with(&config, what)?,
        None => CompileSlot {
            file: None,
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
    static RAM_SLOTS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let inputs = EnvInputs {
        slots: env_var(SLOTS_ENV)?,
        class: env_var(CLASS_ENV)?,
        dir: env_var(SLOT_DIR_ENV)?,
        home: std::env::var_os("HOME").filter(|home| !home.is_empty()).map(PathBuf::from),
        job_id_set: std::env::var_os("ROCK_JOB_ID").is_some_and(|id| !id.is_empty()),
        stderr_is_terminal: std::io::stderr().is_terminal(),
    };
    config_from(&inputs, || *RAM_SLOTS.get_or_init(|| ram_batch_slots(crate::common::total_system_memory_mib())))
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

/// `None` when the limit is off (`ROCK_COMPILE_SLOTS=0`).
fn config_from(inputs: &EnvInputs, ram_slots: impl FnOnce() -> usize) -> Result<Option<SlotConfig>> {
    let batch_slots = match &inputs.slots {
        Some(value) => value.trim().parse::<usize>().map_err(|_| {
            anyhow!("{SLOTS_ENV} must be a non-negative integer, got `{value}`")
        })?,
        None => ram_slots(),
    };
    if batch_slots == 0 {
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
    Ok(Some(SlotConfig { dir, batch_slots, class }))
}

/// Batch slots from total RAM: 40% of it at one compile's planned peak.
fn ram_batch_slots(total_ram_mib: Option<u64>) -> usize {
    let Some(total) = total_ram_mib else {
        log::warn!(target: "COMPILE", "no total RAM reading; allowing one batch compile at a time");
        return 1;
    };
    usize::try_from(total.saturating_mul(4) / (10 * COMPILE_PEAK_MIB)).unwrap_or(usize::MAX).max(1)
}

pub(crate) fn acquire_with(config: &SlotConfig, what: &str) -> Result<CompileSlot> {
    let started = Instant::now();
    fs_err::create_dir_all(&config.dir).context("creating the compile slot directory")?;
    let names = config.candidates();
    let mut files = names
        .iter()
        .map(|name| open_slot(&config.dir.join(name)))
        .collect::<Result<Vec<_>>>()?;
    let mut waiting = false;
    loop {
        for index in 0..files.len() {
            match files[index].try_lock() {
                Ok(()) => {
                    let mut file = files.swap_remove(index);
                    let waited = started.elapsed();
                    if let Err(error) = write_holder(&mut file, what, std::env::args()) {
                        log::warn!(target: "COMPILE", "recording the holder of compile slot {}: {error}", names[index]);
                    }
                    if waiting {
                        log::info!(target: "COMPILE", "took compile slot {} for {what} after waiting {:.1}s",
                            names[index], waited.as_secs_f64());
                    } else {
                        log::debug!(target: "COMPILE", "took compile slot {} for {what} in {waited:?}", names[index]);
                    }
                    let acquisition = SlotAcquisition { slot: Some(names[index].clone()), waited };
                    return Ok(CompileSlot { file: Some(file), acquisition, marks_thread: false });
                }
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => {
                    return Err(error).with_context(|| format!("locking compile slot {}", config.dir.join(&names[index]).display()));
                }
            }
        }
        if !waiting {
            waiting = true;
            log::info!(target: "COMPILE", "waiting for a compile slot for {what}; busy: {}", describe_holders(&config.dir, &names));
            #[cfg(test)]
            WAIT_OBSERVER.with(|observer| {
                if let Some(sender) = observer.borrow_mut().take() {
                    sender.send(()).expect("slot-wait observer disconnected");
                }
            });
        }
        std::thread::sleep(RETRY_INTERVAL);
    }
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

fn write_holder(file: &mut File, what: &str, args: impl IntoIterator<Item = String>) -> std::io::Result<()> {
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let cmdline = crate::invocation_log::redact_var_values(args).join(" ");
    file.set_len(0)?;
    file.rewind()?;
    writeln!(file, "pid={} start={started} for={what} cmd={cmdline}", std::process::id())
}

fn describe_holders(dir: &Path, names: &[String]) -> String {
    names
        .iter()
        .map(|name| match std::fs::read_to_string(dir.join(name)) {
            Ok(holder) => format!("{name} ({})", holder.trim()),
            Err(error) => format!("{name} (unreadable: {error})"),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Run `compile` on a thread whose only batch slot is held, and check it
/// waits for the slot and takes it once freed. Returns the compile's result.
#[cfg(test)]
pub(crate) fn assert_compile_waits_for_slot<T: Send + 'static>(
    compile: impl FnOnce() -> T + Send + 'static,
) -> T {
    let slots = tempfile::tempdir().unwrap();
    let config = SlotConfig { dir: slots.path().to_path_buf(), batch_slots: 1, class: CompileClass::Batch };
    let held = acquire_with(&config, "test holder").unwrap();
    let (worker, waiting) = spawn_slot_waiter(move || {
        set_thread_config(Some(config));
        let result = compile();
        (result, last_acquisition())
    });
    let ((result, acquisition), held_for) = finish_slot_waiter(held, worker, waiting);
    let acquisition = acquisition.expect("the compile took no slot");
    assert_eq!(acquisition.slot.as_deref(), Some("batch-0"));
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
    assert!(!finished_while_held, "the compile ran while the only batch slot was held");
    (result.unwrap(), held_for)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_holder_record_never_contains_var_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch-0");
        let mut file = super::open_slot(&path).unwrap();
        let args = ["rock", "package", "mag", "--var", "TELEMETRY_TOKEN=holder-secret-1", "--var=OTHER=holder-secret-2"];
        super::write_holder(&mut file, "package", args.iter().map(|arg| arg.to_string())).unwrap();
        let record = std::fs::read_to_string(&path).unwrap();
        assert!(!record.contains("holder-secret"), "holder record leaked a --var value: {record}");
        assert!(record.contains("TELEMETRY_TOKEN") && record.contains("OTHER"), "holder record keeps the names: {record}");
        assert!(super::describe_holders(dir.path(), &["batch-0".to_string()]).find("holder-secret").is_none());
    }

    use super::*;

    fn config(dir: &Path, batch_slots: usize, class: CompileClass) -> SlotConfig {
        SlotConfig { dir: dir.to_path_buf(), batch_slots, class }
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

    #[test]
    fn second_acquirer_waits_until_the_first_guard_drops() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 1, CompileClass::Batch);
        let first = acquire_with(&config, "first").unwrap();
        assert_eq!(first.acquisition().slot.as_deref(), Some("batch-0"));
        let (waiter, waiting) = {
            let config = config.clone();
            spawn_slot_waiter(move || acquire_with(&config, "second").map(|slot| slot.acquisition().clone()))
        };
        let (second, held_for) = finish_slot_waiter(first, waiter, waiting);
        let second = second.unwrap();
        assert_eq!(second.slot.as_deref(), Some("batch-0"));
        assert!(second.waited >= held_for, "held after observing contention: {held_for:?}; {second:?}");
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
        assert_eq!(acquisition.slot.as_deref(), Some("batch-0"));
        assert!(acquisition.waited >= held_for, "held after observing contention: {held_for:?}; {acquisition:?}");
    }

    #[test]
    fn interactive_prefers_the_author_slot_then_falls_back_to_batch() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 2, CompileClass::Interactive);
        let first = acquire_with(&config, "first").unwrap();
        assert_eq!(first.acquisition().slot.as_deref(), Some(AUTHOR_SLOT));
        let second = acquire_with(&config, "second").unwrap();
        assert_eq!(second.acquisition().slot.as_deref(), Some("batch-0"));
        assert!(!slot_is_free(dir.path(), AUTHOR_SLOT));
        assert!(!slot_is_free(dir.path(), "batch-0"));
        assert!(slot_is_free(dir.path(), "batch-1"));
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
        let author = config_from(&inputs(None, None, false, true), || 2).unwrap().unwrap();
        assert_eq!((author.batch_slots, author.class), (2, CompileClass::Interactive));
        let agent = config_from(&inputs(None, None, false, false), || 2).unwrap().unwrap();
        assert_eq!(agent.class, CompileClass::Batch);
        let forced = config_from(&inputs(None, Some("interactive"), true, false), || 2).unwrap().unwrap();
        assert_eq!(forced.class, CompileClass::Interactive);
        assert!(config_from(&inputs(Some("many"), None, true, false), || 2).is_err());
        assert!(config_from(&inputs(None, Some("urgent"), true, false), || 2).is_err());
    }

    #[test]
    fn ram_budget_matches_the_machines() {
        let gib = |n: u64| Some(n * 1024);
        assert_eq!(ram_batch_slots(gib(62)), 5, "host-a");
        assert_eq!(ram_batch_slots(gib(36)), 3, "Studio");
        assert_eq!(ram_batch_slots(gib(24)), 2);
        assert_eq!(ram_batch_slots(gib(16)), 1, "m1key");
        assert_eq!(ram_batch_slots(gib(4)), 1);
        assert_eq!(ram_batch_slots(None), 1);
    }

    const CHILD_DIR_ENV: &str = "ROCK_COMPILE_SLOT_TEST_CHILD_DIR";

    /// The child half of `a_killed_holder_frees_its_slot`: holds batch-0
    /// until it is killed.
    #[test]
    #[ignore = "spawned by a_killed_holder_frees_its_slot"]
    fn hold_slot_until_killed() {
        let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else { return };
        let _slot = acquire_with(&config(Path::new(&dir), 1, CompileClass::Batch), "child").unwrap();
        // Serial libtest prints its test-name prefix without a trailing newline.
        println!("\nheld");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    fn a_killed_holder_frees_its_slot() {
        use std::io::BufRead;
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "compile_slots::tests::hold_slot_until_killed", "--ignored", "--nocapture", "--test-threads=1"])
            .env(CHILD_DIR_ENV, dir.path())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let held = std::io::BufReader::new(stdout).lines().any(|line| line.unwrap().trim() == "held");
        assert!(held, "the child never reported holding the slot");
        assert!(!slot_is_free(dir.path(), "batch-0"), "the child's slot is not locked");
        let holder = std::fs::read_to_string(dir.path().join("batch-0")).unwrap();
        assert!(holder.contains(&format!("pid={}", child.id())), "{holder}");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(slot_is_free(dir.path(), "batch-0"), "a killed holder kept its slot");
    }
}
