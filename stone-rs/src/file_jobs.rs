use image::ImageEncoder;
use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};

const MAX_PENDING_JOBS: usize = 128;
const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;
const MAX_OPEN_TRANSACTIONS: usize = 32;
const MAX_TRANSACTION_OPS: usize = 512;
const MAX_TRANSACTION_CLEANUPS: usize = 512;
const MAX_TRANSACTION_CLEANUP_DIRECTORIES: usize = 128;
const MAX_TRANSACTION_KEEP_ENTRIES: usize = 4096;
const MAX_TRANSACTION_CLEANUP_BYTES: usize = 512 * 1024;
const MAX_TRANSACTION_FRAGMENT_BYTES: usize = 64 * 1024;
const MAX_TRANSACTION_FRAGMENTS: usize = MAX_PENDING_BYTES / (4 * 1024);
const MAX_PATH_BYTES: usize = 4096;

struct PausePoint {
    paused: AtomicU8,
    entered: AtomicU8,
}

impl PausePoint {
    const fn new() -> Self {
        Self {
            paused: AtomicU8::new(0),
            entered: AtomicU8::new(0),
        }
    }

    fn gate(&self) {
        if !file_jobs_test_enabled() {
            return;
        }
        let control = file_jobs_test_control();
        let mut guard = control.state.lock().unwrap();
        if self.paused.load(Ordering::Acquire) == 0 {
            return;
        }
        self.entered.store(1, Ordering::Release);
        control.wake.notify_all();
        while self.paused.load(Ordering::Acquire) != 0 {
            guard = control.wake.wait(guard).unwrap();
        }
        self.entered.store(0, Ordering::Release);
    }

    fn pause(&self) -> bool {
        if !file_jobs_test_enabled() {
            return false;
        }
        let control = file_jobs_test_control();
        let _guard = control.state.lock().unwrap();
        self.entered.store(0, Ordering::Release);
        self.paused.store(1, Ordering::Release);
        true
    }

    fn entered(&self) -> bool {
        file_jobs_test_enabled() && self.entered.load(Ordering::Acquire) != 0
    }
}

static WORKER_PAUSE: PausePoint = PausePoint::new();
static PUBLICATION_PAUSE: PausePoint = PausePoint::new();

struct FileJobsTestControl {
    state: Mutex<()>,
    wake: Condvar,
}

fn file_jobs_test_control() -> &'static FileJobsTestControl {
    static CONTROL: OnceLock<FileJobsTestControl> = OnceLock::new();
    CONTROL.get_or_init(|| FileJobsTestControl {
        state: Mutex::new(()),
        wake: Condvar::new(),
    })
}

fn file_jobs_test_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("STONE_FILE_JOBS_TEST_CONTROL").as_deref() == Ok("1"))
}

#[derive(Clone, Copy)]
struct Limits {
    max_pending_jobs: usize,
    max_pending_bytes: usize,
    max_open_transactions: usize,
    max_transaction_ops: usize,
    max_transaction_cleanups: usize,
    max_transaction_cleanup_directories: usize,
    max_transaction_keep_entries: usize,
    max_transaction_cleanup_bytes: usize,
    max_transaction_fragment_bytes: usize,
    max_transaction_fragments: usize,
}

const LIMITS: Limits = Limits {
    max_pending_jobs: MAX_PENDING_JOBS,
    max_pending_bytes: MAX_PENDING_BYTES,
    max_open_transactions: MAX_OPEN_TRANSACTIONS,
    max_transaction_ops: MAX_TRANSACTION_OPS,
    max_transaction_cleanups: MAX_TRANSACTION_CLEANUPS,
    max_transaction_cleanup_directories: MAX_TRANSACTION_CLEANUP_DIRECTORIES,
    max_transaction_keep_entries: MAX_TRANSACTION_KEEP_ENTRIES,
    max_transaction_cleanup_bytes: MAX_TRANSACTION_CLEANUP_BYTES,
    max_transaction_fragment_bytes: MAX_TRANSACTION_FRAGMENT_BYTES,
    max_transaction_fragments: MAX_TRANSACTION_FRAGMENTS,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobResult {
    Success,
    Failure,
    Cancelled,
    DurabilityUncertain,
}

impl JobResult {
    fn code(self) -> i32 {
        match self {
            Self::Success => 1,
            Self::Failure => -1,
            Self::Cancelled => -2,
            Self::DurabilityUncertain => -3,
        }
    }
}

struct Completion {
    result: Mutex<Option<JobResult>>,
    ready: Condvar,
}

impl Completion {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    fn finish(&self, result: JobResult) {
        let mut slot = self.result.lock().unwrap();
        *slot = Some(result);
        self.ready.notify_all();
    }

    fn poll(&self, wait: bool) -> Option<JobResult> {
        let mut slot = self.result.lock().unwrap();
        if wait {
            while slot.is_none() {
                slot = self.ready.wait(slot).unwrap();
            }
        }
        *slot
    }
}

const CANCEL_ACTIVE: u8 = 0;
const CANCEL_REQUESTED: u8 = 1;
const CANCEL_PUBLISHING: u8 = 2;
const CANCEL_COMPLETE: u8 = 3;

struct Cancellation {
    state: AtomicU8,
}

impl Cancellation {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(CANCEL_ACTIVE),
        }
    }

    fn cancel(&self) -> i32 {
        loop {
            match self.state.load(Ordering::Acquire) {
                CANCEL_ACTIVE => {
                    if self
                        .state
                        .compare_exchange(
                            CANCEL_ACTIVE,
                            CANCEL_REQUESTED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return 1;
                    }
                }
                CANCEL_REQUESTED => return 1,
                CANCEL_PUBLISHING | CANCEL_COMPLETE => return 0,
                _ => return -1,
            }
        }
    }

    fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == CANCEL_REQUESTED
    }

    fn claim_publication(&self) -> bool {
        self.state
            .compare_exchange(
                CANCEL_ACTIVE,
                CANCEL_PUBLISHING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn complete(&self) {
        self.state.store(CANCEL_COMPLETE, Ordering::Release);
    }
}

struct Request {
    id: i32,
    transaction: Transaction,
    completion: Arc<Completion>,
    cancellation: Arc<Cancellation>,
}

#[derive(Clone)]
struct Pending {
    completion: Arc<Completion>,
    cancellation: Option<Arc<Cancellation>>,
}

struct AtomicWrite {
    path: PathBuf,
    fragments: Vec<Vec<u8>>,
}

enum TransactionOp {
    Write(AtomicWrite),
    Append(AtomicWrite),
    Rename { source: PathBuf, target: PathBuf },
    Png(PngWrite),
    Publish(AtomicWrite),
}

struct PngWrite {
    path: PathBuf,
    pixels: Vec<u8>,
    width: u32,
    height: u32,
}

struct CleanupDirectory {
    directory: PathBuf,
    keep: Vec<String>,
}

struct DirectoryBarrierError {
    directory: PathBuf,
    error: std::io::Error,
}

/// Points where the worker consults the test probe; a probe returning true injects a
/// failure at `Published` and `DirectorySync`.
#[derive(Clone, Copy)]
#[cfg_attr(not(test), allow(dead_code))]
enum Probe<'a> {
    Request,
    Publication,
    Published,
    Completed,
    DirectorySync(&'a Path),
}

#[cfg(test)]
type ProbeHook = dyn Fn(Probe) -> bool + Send + Sync;

#[derive(Clone, Default)]
struct WorkerControl {
    #[cfg(test)]
    probe: Option<Arc<ProbeHook>>,
}

impl WorkerControl {
    fn probe(&self, probe: Probe) -> bool {
        #[cfg(test)]
        if let Some(hook) = &self.probe {
            return hook(probe);
        }
        let _ = probe;
        false
    }
}

// A crash keeps a created or renamed name only after its directory is synced. Each phase
// syncs the changed directories once, deepest first.
struct DirectoryBarriers<'a> {
    changed: Vec<PathBuf>,
    control: &'a WorkerControl,
}

impl<'a> DirectoryBarriers<'a> {
    fn new(control: &'a WorkerControl) -> Self {
        Self {
            changed: Vec::new(),
            control,
        }
    }

    fn record_entry_change(&mut self, directory: &Path) {
        if !self.changed.iter().any(|changed| changed == directory) {
            self.changed.push(directory.to_path_buf());
        }
    }

    fn record_parent(&mut self, path: &Path) {
        self.record_entry_change(directory_of(path));
    }

    fn record_created(&mut self, directory: &Path) {
        self.record_entry_change(directory);
        self.record_entry_change(directory_of(directory));
    }

    fn flush(&mut self) -> Result<(), DirectoryBarrierError> {
        self.changed.sort_by(|left, right| {
            path_depth(right)
                .cmp(&path_depth(left))
                .then_with(|| left.cmp(right))
        });
        let changed = std::mem::take(&mut self.changed);
        for directory in changed {
            let result = if self.control.probe(Probe::DirectorySync(&directory)) {
                Err(std::io::Error::other("injected directory barrier failure"))
            } else {
                sync_directory(&directory)
            };
            if let Err(error) = result {
                return Err(DirectoryBarrierError { directory, error });
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct Transaction {
    operations: Vec<TransactionOp>,
    cleanup_success: Vec<PathBuf>,
    cleanup_failure: Vec<PathBuf>,
    cleanup_directories: Vec<CleanupDirectory>,
    cleanup_entries: usize,
    cleanup_bytes: usize,
    bytes: usize,
    fragments: usize,
    published: bool,
    restore_on_failure: bool,
}

impl Transaction {
    fn admits_payload(&self, limits: &Limits, bytes: usize) -> bool {
        self.fragments < limits.max_transaction_fragments
            && bytes <= limits.max_transaction_fragment_bytes
    }

    fn add_fragment(
        &mut self,
        limits: &Limits,
        path: PathBuf,
        bytes: Vec<u8>,
        append: bool,
        publish: bool,
    ) -> bool {
        if !self.admits_payload(limits, bytes.len()) || self.published && !(publish && append) {
            return false;
        }
        let accepted = if append {
            match self.operations.last_mut() {
                Some(TransactionOp::Write(write)) if !publish && write.path == path => {
                    write.fragments.push(bytes);
                    true
                }
                Some(TransactionOp::Publish(write)) if publish && write.path == path => {
                    write.fragments.push(bytes);
                    true
                }
                _ => false,
            }
        } else if self.operations.len() >= limits.max_transaction_ops {
            false
        } else {
            let write = AtomicWrite {
                path,
                fragments: vec![bytes],
            };
            self.operations.push(if publish {
                TransactionOp::Publish(write)
            } else {
                TransactionOp::Write(write)
            });
            self.published = publish;
            true
        };
        self.fragments += usize::from(accepted);
        accepted
    }

    fn add_png(&mut self, limits: &Limits, png: PngWrite) -> bool {
        if !self.admits_payload(limits, png.pixels.len())
            || self.published
            || self.operations.len() >= limits.max_transaction_ops
        {
            return false;
        }
        self.operations.push(TransactionOp::Png(png));
        self.fragments += 1;
        true
    }

    fn add_cleanup(&mut self, limits: &Limits, path: PathBuf, on_success: bool) -> bool {
        let paths = if on_success {
            &mut self.cleanup_success
        } else {
            &mut self.cleanup_failure
        };
        if paths.len() >= limits.max_transaction_cleanups {
            return false;
        }
        paths.push(path);
        true
    }

    fn admits_cleanup_bytes(&self, limits: &Limits, bytes: usize) -> bool {
        bytes <= limits.max_transaction_cleanup_bytes
            && self.cleanup_bytes <= limits.max_transaction_cleanup_bytes - bytes
    }

    fn add_cleanup_directory(&mut self, limits: &Limits, directory: PathBuf) -> bool {
        let bytes = directory.as_os_str().len();
        if !self.admits_cleanup_bytes(limits, bytes)
            || self.cleanup_directories.len() >= limits.max_transaction_cleanup_directories
            || self
                .cleanup_directories
                .iter()
                .any(|entry| entry.directory == directory)
        {
            return false;
        }
        self.cleanup_directories.push(CleanupDirectory {
            directory,
            keep: Vec::new(),
        });
        self.cleanup_bytes += bytes;
        true
    }

    fn add_keep(&mut self, limits: &Limits, directory: &Path, filename: &str) -> bool {
        if !is_plain_filename(filename)
            || !self.admits_cleanup_bytes(limits, filename.len())
            || self.cleanup_entries >= limits.max_transaction_keep_entries
        {
            return false;
        }
        let Some(entry) = self
            .cleanup_directories
            .iter_mut()
            .find(|entry| entry.directory == directory)
        else {
            return false;
        };
        entry.keep.push(filename.to_owned());
        self.cleanup_entries += 1;
        self.cleanup_bytes += filename.len();
        true
    }
}

struct Jobs {
    sender: mpsc::SyncSender<Request>,
    pending: HashMap<i32, Pending>,
    transactions: HashMap<i32, Transaction>,
    next: i32,
    accounting: Arc<AtomicUsize>,
    limits: Limits,
}

impl Jobs {
    fn new(limits: Limits) -> Self {
        Self::new_with_control(limits, WorkerControl::default())
    }

    fn new_with_control(limits: Limits, control: WorkerControl) -> Self {
        let channel_capacity = limits.max_pending_jobs.max(1);
        let (sender, receiver) = mpsc::sync_channel(channel_capacity);
        let accounting = Arc::new(AtomicUsize::new(0));
        let worker_accounting = Arc::clone(&accounting);
        std::thread::Builder::new()
            .name("stone-file-writer".into())
            .spawn(move || {
                worker_loop(receiver, worker_accounting, control);
            })
            .expect("start file writer");
        Self {
            sender,
            pending: HashMap::new(),
            transactions: HashMap::new(),
            next: 1,
            accounting,
            limits,
        }
    }

    fn next_id(&mut self) -> Option<i32> {
        let id = self.next;
        self.next = self.next.checked_add(1)?;
        Some(id)
    }

    fn reserve_bytes(&self, bytes: usize) -> bool {
        let current = self.accounting.load(Ordering::Acquire);
        if bytes > self.limits.max_pending_bytes || current > self.limits.max_pending_bytes - bytes
        {
            return false;
        }
        self.accounting.fetch_add(bytes, Ordering::AcqRel);
        true
    }

    fn release_bytes(&self, bytes: usize) {
        if bytes > 0 {
            self.accounting.fetch_sub(bytes, Ordering::AcqRel);
        }
    }

    fn queue_full(&self) -> bool {
        self.pending.len() >= self.limits.max_pending_jobs
    }

    fn begin_transaction(&mut self) -> Option<i32> {
        if self.transactions.len() >= self.limits.max_open_transactions {
            return None;
        }
        let id = self.next_id()?;
        self.transactions.insert(id, Transaction::default());
        Some(id)
    }

    /// Charges `bytes` against the queue budget, then lets `add` stage into the open
    /// transaction; a rejected stage returns the charge.
    fn stage(
        &mut self,
        id: i32,
        bytes: usize,
        add: impl FnOnce(&mut Transaction, &Limits) -> bool,
    ) -> bool {
        if !self.transactions.contains_key(&id) || !self.reserve_bytes(bytes) {
            return false;
        }
        let transaction = self.transactions.get_mut(&id).unwrap();
        if !add(transaction, &self.limits) {
            self.release_bytes(bytes);
            return false;
        }
        transaction.bytes += bytes;
        true
    }

    /// A job outside the transaction API: not cancellable, admitted on the queue budget
    /// alone, and built only once admitted.
    fn submit(
        &mut self,
        bytes: usize,
        restore_on_failure: bool,
        operations: impl FnOnce() -> Vec<TransactionOp>,
    ) -> Option<i32> {
        if self.queue_full() || !self.reserve_bytes(bytes) {
            return None;
        }
        let transaction = Transaction {
            operations: operations(),
            bytes,
            restore_on_failure,
            ..Transaction::default()
        };
        self.send(transaction, None)
    }

    /// Queues `transaction`. A committed transaction is `open` under its id: it is
    /// cancellable and goes back to the open set if the queue refuses it.
    fn send(&mut self, transaction: Transaction, open: Option<i32>) -> Option<i32> {
        let Some(id) = self.next_id() else {
            self.refuse(transaction, open);
            return None;
        };
        let completion = Arc::new(Completion::new());
        let cancellation = Arc::new(Cancellation::new());
        let pending = Pending {
            completion: Arc::clone(&completion),
            cancellation: open.map(|_| Arc::clone(&cancellation)),
        };
        let request = Request {
            id,
            transaction,
            completion,
            cancellation,
        };
        match self.sender.try_send(request) {
            Ok(()) => {
                self.pending.insert(id, pending);
                Some(id)
            }
            Err(mpsc::TrySendError::Full(request))
            | Err(mpsc::TrySendError::Disconnected(request)) => {
                self.refuse(request.transaction, open);
                None
            }
        }
    }

    fn refuse(&mut self, transaction: Transaction, open: Option<i32>) {
        match open {
            Some(id) => {
                self.transactions.insert(id, transaction);
            }
            None => self.release_bytes(transaction.bytes),
        }
    }

    fn commit_transaction(&mut self, id: i32) -> Option<i32> {
        if self.queue_full() {
            return None;
        }
        let transaction = self.transactions.remove(&id)?;
        self.send(transaction, Some(id))
    }

    fn abort_transaction(&mut self, id: i32) -> bool {
        let Some(transaction) = self.transactions.remove(&id) else {
            return false;
        };
        self.release_bytes(transaction.bytes);
        true
    }
}

fn worker_loop(
    receiver: mpsc::Receiver<Request>,
    accounting: Arc<AtomicUsize>,
    control: WorkerControl,
) {
    for request in receiver {
        WORKER_PAUSE.gate();
        control.probe(Probe::Request);
        let result = execute_transaction(
            &request.transaction,
            &request.cancellation,
            request.id,
            &control,
        );
        let owned_bytes = request.transaction.bytes;
        // Completion admits a successor, so release this payload first.
        drop(request.transaction);
        accounting.fetch_sub(owned_bytes, Ordering::AcqRel);
        request.completion.finish(result);
        control.probe(Probe::Completed);
    }
}

fn read_previous(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Puts back what each written path held before, newest first, stopping at the first
/// path that cannot be restored.
fn restore_previous(
    written: &[(PathBuf, Option<Vec<u8>>)],
    id: i32,
    first_sequence: usize,
    barriers: &mut DirectoryBarriers,
) {
    for (index, (path, before)) in written.iter().enumerate().rev() {
        let result = match before {
            Some(bytes) => write_atomic(
                path,
                std::slice::from_ref(bytes),
                id,
                first_sequence + index,
                barriers,
            ),
            None => std::fs::remove_file(path).map(|()| barriers.record_parent(path)),
        };
        if let Err(error) = result.and_then(|()| barriers.flush().map_err(|failure| failure.error))
        {
            log_io_error("paired rollback", path, &error);
            return;
        }
    }
}

fn write_atomic(
    path: &Path,
    fragments: &[Vec<u8>],
    id: i32,
    sequence: usize,
    barriers: &mut DirectoryBarriers,
) -> std::io::Result<()> {
    ensure_parent(path, barriers)?;
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(format!(
        ".stone-{}-{}-{}.tmp",
        std::process::id(),
        id,
        sequence
    ));
    let temporary = PathBuf::from(temporary);
    let result = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        for fragment in fragments {
            file.write_all(fragment)?;
        }
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        barriers.record_parent(path);
        Ok(())
    })();
    if result.is_err() {
        if let Err(error) = std::fs::remove_file(&temporary) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log_io_error("temporary cleanup", &temporary, &error);
            }
        }
    }
    result
}

fn append_synced(path: &Path, fragments: &[Vec<u8>]) -> std::io::Result<()> {
    // Created directories are not synced: an append does not promise its name survives.
    ensure_parent(path, &mut DirectoryBarriers::new(&WorkerControl::default()))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let previous = file.metadata()?.len();
    let result = fragments
        .iter()
        .try_for_each(|fragment| file.write_all(fragment))
        .and_then(|()| file.sync_all());
    if result.is_err() {
        file.set_len(previous)?;
        file.sync_all()?;
    }
    result
}

fn log_io_error(operation: &str, path: &Path, error: &std::io::Error) {
    ::log::error!(target: "FILE", "file transaction {} failed for {}: {}", operation, path.display(), error);
}
fn directory_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn path_depth(path: &Path) -> usize {
    path.components().count()
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> std::io::Result<()> {
    std::fs::File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> std::io::Result<()> {
    Ok(())
}

fn ensure_directory(directory: &Path, barriers: &mut DirectoryBarriers) -> std::io::Result<()> {
    if directory.as_os_str().is_empty() {
        return Ok(());
    }
    match std::fs::create_dir(directory) {
        Ok(()) => {
            barriers.record_created(directory);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            match std::fs::metadata(directory) {
                Ok(metadata) if metadata.is_dir() => Ok(()),
                _ => Err(error),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let Some(parent) = directory.parent() else {
                return Err(error);
            };
            if parent.as_os_str().is_empty() {
                return Err(error);
            }
            ensure_directory(parent, barriers)?;
            match std::fs::create_dir(directory) {
                Ok(()) => {
                    barriers.record_created(directory);
                    Ok(())
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn ensure_parent(path: &Path, barriers: &mut DirectoryBarriers) -> std::io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => ensure_directory(parent, barriers),
        _ => Ok(()),
    }
}

fn execute_transaction(
    transaction: &Transaction,
    cancellation: &Cancellation,
    id: i32,
    control: &WorkerControl,
) -> JobResult {
    let result = match run_transaction(transaction, cancellation, id, control) {
        Ok(()) => {
            cleanup(&transaction.cleanup_success);
            if transaction.published {
                cleanup_directories(&transaction.cleanup_directories);
            }
            JobResult::Success
        }
        // The root name may already be visible, so the failure cleanup could delete live state.
        Err(JobResult::DurabilityUncertain) => JobResult::DurabilityUncertain,
        Err(result) => {
            cleanup(&transaction.cleanup_failure);
            result
        }
    };
    cancellation.complete();
    result
}

fn run_transaction(
    transaction: &Transaction,
    cancellation: &Cancellation,
    id: i32,
    control: &WorkerControl,
) -> Result<(), JobResult> {
    let mut barriers = DirectoryBarriers::new(control);
    let mut written = Vec::new();
    for (sequence, operation) in transaction.operations.iter().enumerate() {
        let publication = matches!(operation, TransactionOp::Publish(_));
        if publication && !cancellation.claim_publication()
            || !publication && cancellation.is_cancelled()
        {
            return Err(JobResult::Cancelled);
        }
        let outcome = match operation {
            TransactionOp::Publish(write) => {
                control.probe(Probe::Publication);
                PUBLICATION_PAUSE.gate();
                // Everything the root reaches must be durable before the root name can be.
                if let Err(failure) = barriers.flush() {
                    log_io_error("directory barrier", &failure.directory, &failure.error);
                    return Err(JobResult::Failure);
                }
                if let Err(error) =
                    write_atomic(&write.path, &write.fragments, id, sequence, &mut barriers)
                {
                    log_io_error("publish", &write.path, &error);
                    return Err(JobResult::Failure);
                }
                if control.probe(Probe::Published) {
                    let error = std::io::Error::other("injected post-publish sync failure");
                    log_io_error("publish durability", &write.path, &error);
                    return Err(JobResult::DurabilityUncertain);
                }
                // Commit point: the root name becomes durable here and nowhere earlier.
                if let Err(failure) = barriers.flush() {
                    log_io_error("publish durability", &write.path, &failure.error);
                    return Err(JobResult::DurabilityUncertain);
                }
                Ok(())
            }
            TransactionOp::Write(write) => {
                let before = if transaction.restore_on_failure {
                    read_previous(&write.path).map(Some)
                } else {
                    Ok(None)
                };
                let result = before.and_then(|before| {
                    write_atomic(&write.path, &write.fragments, id, sequence, &mut barriers)?;
                    if let Some(before) = before {
                        written.push((write.path.clone(), before));
                    }
                    Ok(())
                });
                result.map_err(|error| ("write", &write.path, error))
            }
            TransactionOp::Append(write) => append_synced(&write.path, &write.fragments)
                .map_err(|error| ("append", &write.path, error)),
            TransactionOp::Rename { source, target } => ensure_parent(target, &mut barriers)
                .and_then(|()| std::fs::rename(source, target))
                .map(|()| barriers.record_parent(target))
                .map_err(|error| ("rename", target, error)),
            TransactionOp::Png(png) => match encode_png(&png.pixels, png.width, png.height) {
                Ok(encoded) => write_atomic(
                    &png.path,
                    std::slice::from_ref(&encoded),
                    id,
                    sequence,
                    &mut barriers,
                )
                .map_err(|error| ("png write", &png.path, error)),
                Err(error) => Err(("png encode", &png.path, error)),
            },
        };
        if let Err((operation, path, error)) = outcome {
            log_io_error(operation, path, &error);
            restore_previous(&written, id, transaction.operations.len(), &mut barriers);
            return Err(JobResult::Failure);
        }
    }
    if cancellation.is_cancelled() {
        return Err(JobResult::Cancelled);
    }
    if let Err(failure) = barriers.flush() {
        log_io_error("directory barrier", &failure.directory, &failure.error);
        return Err(JobResult::Failure);
    }
    Ok(())
}
fn encode_png(pixels: &[u8], width: u32, height: u32) -> std::io::Result<Vec<u8>> {
    let mut encoded = Vec::new();
    image::codecs::png::PngEncoder::new(&mut encoded)
        .write_image(pixels, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    Ok(encoded)
}

fn cleanup(paths: &[PathBuf]) {
    for path in paths {
        if let Err(error) = std::fs::remove_file(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                ::log::error!(target: "FILE", "file transaction cleanup failed for {}: {}", path.display(), error);
            }
        }
    }
}

fn cleanup_directories(directories: &[CleanupDirectory]) {
    for cleanup in directories {
        let entries = match std::fs::read_dir(&cleanup.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                log_io_error("cleanup directory enumeration", &cleanup.directory, &error);
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    log_io_error("cleanup directory enumeration", &cleanup.directory, &error);
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    log_io_error("cleanup directory inspect", &path, &error);
                    continue;
                }
            };
            if !file_type.is_file() {
                continue;
            }
            let filename = entry.file_name();
            if filename
                .to_str()
                .is_some_and(|filename| cleanup.keep.iter().any(|keep| keep == filename))
            {
                continue;
            }
            if let Err(error) = std::fs::remove_file(&path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    log_io_error("cleanup directory remove", &path, &error);
                }
            }
        }
    }
}

fn jobs() -> &'static Mutex<Jobs> {
    static JOBS: OnceLock<Mutex<Jobs>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(Jobs::new(LIMITS)))
}

fn c_text<'a>(value: *const c_char) -> Option<&'a str> {
    if value.is_null() {
        return None;
    }
    let value = unsafe { CStr::from_ptr(value) }.to_str().ok()?;
    (!value.is_empty() && value.len() <= MAX_PATH_BYTES).then_some(value)
}

fn parse_path(path: *const c_char) -> Option<PathBuf> {
    c_text(path).map(PathBuf::from)
}

fn is_plain_filename(filename: &str) -> bool {
    !filename.is_empty()
        && filename.len() <= MAX_PATH_BYTES
        && filename != "."
        && filename != ".."
        && !filename.contains('/')
        && !filename.contains('\\')
}

/// # Safety
/// `data` is non-null and points at `len` readable bytes.
unsafe fn copy_bytes<T>(data: *const T, len: usize) -> Vec<u8> {
    std::slice::from_raw_parts(data.cast::<u8>(), len).to_vec()
}

fn poll_job(id: i32, wait: bool) -> i32 {
    let pending = {
        let jobs = jobs().lock().unwrap();
        jobs.pending.get(&id).cloned()
    };
    let Some(pending) = pending else {
        return -1;
    };
    let Some(result) = pending.completion.poll(wait) else {
        return 0;
    };
    jobs().lock().unwrap().pending.remove(&id);
    result.code()
}

#[no_mangle]
pub extern "C" fn stone_file_write_async(
    path: *const c_char,
    data: *const c_char,
    len: usize,
    append: bool,
) -> i32 {
    let Some(path) = parse_path(path) else {
        return -1;
    };
    if data.is_null() {
        return -1;
    }
    jobs()
        .lock()
        .unwrap()
        .submit(len, false, || {
            let write = AtomicWrite {
                path,
                fragments: vec![unsafe { copy_bytes(data, len) }],
            };
            vec![if append {
                TransactionOp::Append(write)
            } else {
                TransactionOp::Write(write)
            }]
        })
        .unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn stone_file_rename_async(source: *const c_char, target: *const c_char) -> i32 {
    let (Some(source), Some(target)) = (parse_path(source), parse_path(target)) else {
        return -1;
    };
    jobs()
        .lock()
        .unwrap()
        .submit(0, false, || vec![TransactionOp::Rename { source, target }])
        .unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn stone_file_write_pair_async(
    path: *const c_char,
    data: *const c_char,
    len: usize,
    second_path: *const c_char,
    second_data: *const c_char,
    second_len: usize,
) -> i32 {
    let (Some(path), Some(second_path)) = (parse_path(path), parse_path(second_path)) else {
        return -1;
    };
    let Some(total) = len.checked_add(second_len) else {
        return -1;
    };
    if path == second_path || data.is_null() || second_data.is_null() {
        return -1;
    }
    jobs()
        .lock()
        .unwrap()
        .submit(total, true, || {
            let write = |path, data, len| {
                TransactionOp::Write(AtomicWrite {
                    path,
                    fragments: vec![unsafe { copy_bytes(data, len) }],
                })
            };
            vec![
                write(path, data, len),
                write(second_path, second_data, second_len),
            ]
        })
        .unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn stone_file_write_poll(id: i32, wait: bool) -> i32 {
    poll_job(id, wait)
}

#[no_mangle]
pub extern "C" fn stone_file_transaction_begin() -> i32 {
    jobs().lock().unwrap().begin_transaction().unwrap_or(-1)
}

fn transaction_write(
    transaction: i32,
    path: *const c_char,
    data: *const c_char,
    len: usize,
    append: bool,
    publish: bool,
) -> bool {
    let Some(path) = parse_path(path) else {
        return false;
    };
    if data.is_null() || len > MAX_TRANSACTION_FRAGMENT_BYTES {
        return false;
    }
    jobs()
        .lock()
        .unwrap()
        .stage(transaction, len, |staged, limits| {
            let bytes = unsafe { copy_bytes(data, len) };
            staged.add_fragment(limits, path, bytes, append, publish)
        })
}

#[no_mangle]
pub extern "C" fn stone_file_transaction_write(
    transaction: i32,
    path: *const c_char,
    data: *const c_char,
    len: usize,
    append: bool,
) -> bool {
    transaction_write(transaction, path, data, len, append, false)
}

#[no_mangle]
pub extern "C" fn stone_file_transaction_publish(
    transaction: i32,
    path: *const c_char,
    data: *const c_char,
    len: usize,
    append: bool,
) -> bool {
    transaction_write(transaction, path, data, len, append, true)
}

#[no_mangle]
pub extern "C" fn stone_file_transaction_write_png(
    transaction: i32,
    path: *const c_char,
    pixels: *const u8,
    width: i32,
    height: i32,
) -> bool {
    let Some(path) = parse_path(path) else {
        return false;
    };
    if pixels.is_null() || !(1..=128).contains(&width) || !(1..=128).contains(&height) {
        return false;
    }
    let (width, height) = (width as u32, height as u32);
    let length = width as usize * height as usize * 4;
    jobs()
        .lock()
        .unwrap()
        .stage(transaction, length, |staged, limits| {
            let pixels = unsafe { copy_bytes(pixels, length) };
            staged.add_png(
                limits,
                PngWrite {
                    path,
                    pixels,
                    width,
                    height,
                },
            )
        })
}

#[no_mangle]
pub extern "C" fn stone_file_transaction_cleanup(
    transaction: i32,
    path: *const c_char,
    on_success: bool,
) -> bool {
    let Some(path) = parse_path(path) else {
        return false;
    };
    jobs()
        .lock()
        .unwrap()
        .stage(transaction, 0, |staged, limits| {
            staged.add_cleanup(limits, path, on_success)
        })
}

#[no_mangle]
pub extern "C" fn stone_file_transaction_cleanup_directory(
    transaction: i32,
    directory: *const c_char,
) -> bool {
    let Some(directory) = parse_path(directory) else {
        return false;
    };
    let bytes = directory.as_os_str().len();
    jobs()
        .lock()
        .unwrap()
        .stage(transaction, bytes, |staged, limits| {
            staged.add_cleanup_directory(limits, directory)
        })
}

#[no_mangle]
pub extern "C" fn stone_file_transaction_keep(
    transaction: i32,
    directory: *const c_char,
    filename: *const c_char,
) -> bool {
    let (Some(directory), Some(filename)) = (parse_path(directory), c_text(filename)) else {
        return false;
    };
    jobs()
        .lock()
        .unwrap()
        .stage(transaction, filename.len(), |staged, limits| {
            staged.add_keep(limits, &directory, filename)
        })
}

#[no_mangle]
pub extern "C" fn stone_file_transaction_commit(transaction: i32) -> i32 {
    jobs()
        .lock()
        .unwrap()
        .commit_transaction(transaction)
        .unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn stone_file_transaction_abort(transaction: i32) -> bool {
    jobs().lock().unwrap().abort_transaction(transaction)
}

#[no_mangle]
pub extern "C" fn stone_file_transaction_cancel(job: i32) -> i32 {
    let cancellation = {
        let jobs = jobs().lock().unwrap();
        jobs.pending
            .get(&job)
            .and_then(|pending| pending.cancellation.clone())
    };
    cancellation
        .map(|cancellation| cancellation.cancel())
        .unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn stone_file_transaction_poll(job: i32, wait: bool) -> i32 {
    poll_job(job, wait)
}

#[no_mangle]
pub extern "C" fn stone_file_jobs_test_pause() -> bool {
    WORKER_PAUSE.pause()
}

#[no_mangle]
pub extern "C" fn stone_file_jobs_test_pause_publication() -> bool {
    PUBLICATION_PAUSE.pause()
}

#[no_mangle]
pub extern "C" fn stone_file_jobs_test_release() -> bool {
    if !file_jobs_test_enabled() {
        return false;
    }
    let control = file_jobs_test_control();
    let _guard = control.state.lock().unwrap();
    WORKER_PAUSE.paused.store(0, Ordering::Release);
    PUBLICATION_PAUSE.paused.store(0, Ordering::Release);
    control.wake.notify_all();
    true
}

#[no_mangle]
pub extern "C" fn stone_file_jobs_test_entered() -> bool {
    WORKER_PAUSE.entered()
}

#[no_mangle]
pub extern "C" fn stone_file_jobs_test_publication_entered() -> bool {
    PUBLICATION_PAUSE.entered()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    fn limits(jobs: usize, bytes: usize) -> Limits {
        Limits {
            max_pending_jobs: jobs,
            max_pending_bytes: bytes,
            max_open_transactions: 4,
            max_transaction_ops: 16,
            max_transaction_cleanups: 16,
            max_transaction_cleanup_directories: 4,
            max_transaction_keep_entries: 16,
            max_transaction_cleanup_bytes: 64 * 1024,
            max_transaction_fragment_bytes: 64 * 1024,
            max_transaction_fragments: 16,
        }
    }

    fn test_root() -> crate::test_support::TestRoot {
        crate::test_support::fresh_root("stone_file_jobs_")
    }

    struct TestGate {
        state: Mutex<(bool, bool)>,
        ready: Condvar,
    }

    impl TestGate {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new((false, false)),
                ready: Condvar::new(),
            })
        }

        fn block_request(&self) {
            let mut state = self.state.lock().unwrap();
            state.0 = true;
            self.ready.notify_all();
            while !state.1 {
                state = self.ready.wait(state).unwrap();
            }
        }

        fn wait_until_entered(&self) -> bool {
            let state = self.state.lock().unwrap();
            let (state, _) = self
                .ready
                .wait_timeout_while(state, Duration::from_secs(5), |state| !state.0)
                .unwrap();
            state.0
        }

        fn release(&self) {
            let mut state = self.state.lock().unwrap();
            state.1 = true;
            self.ready.notify_all();
        }
    }

    fn probed(
        limits: Limits,
        probe: impl Fn(Probe) -> bool + Send + Sync + 'static,
    ) -> Mutex<Jobs> {
        Mutex::new(Jobs::new_with_control(
            limits,
            WorkerControl {
                probe: Some(Arc::new(probe)),
            },
        ))
    }

    fn gated(limits: Limits, gate: &Arc<TestGate>, at: fn(&Probe) -> bool) -> Mutex<Jobs> {
        let gate = Arc::clone(gate);
        probed(limits, move |probe| {
            if at(&probe) {
                gate.block_request();
            }
            false
        })
    }

    fn logging_syncs(limits: Limits) -> (Mutex<Jobs>, Arc<Mutex<Vec<PathBuf>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&log);
        let queue = probed(limits, move |probe| {
            if let Probe::DirectorySync(directory) = probe {
                recorder.lock().unwrap().push(directory.to_path_buf());
            }
            false
        });
        (queue, log)
    }

    fn begin(queue: &Mutex<Jobs>) -> i32 {
        queue.lock().unwrap().begin_transaction().unwrap()
    }

    fn commit(queue: &Mutex<Jobs>, transaction: i32) -> i32 {
        queue
            .lock()
            .unwrap()
            .commit_transaction(transaction)
            .unwrap()
    }

    fn stage(
        queue: &Mutex<Jobs>,
        transaction: i32,
        bytes: usize,
        add: impl FnOnce(&mut Transaction, &Limits) -> bool,
    ) -> bool {
        queue.lock().unwrap().stage(transaction, bytes, add)
    }

    fn fragment(
        queue: &Mutex<Jobs>,
        transaction: i32,
        path: &Path,
        bytes: &[u8],
        append: bool,
        manifest: bool,
    ) -> bool {
        stage(queue, transaction, bytes.len(), |staged, limits| {
            staged.add_fragment(limits, path.to_path_buf(), bytes.to_vec(), append, manifest)
        })
    }

    fn stage_write(queue: &Mutex<Jobs>, transaction: i32, path: &Path, bytes: &[u8]) {
        assert!(fragment(queue, transaction, path, bytes, false, false));
    }

    fn stage_manifest(queue: &Mutex<Jobs>, transaction: i32, path: &Path, bytes: &[u8]) {
        assert!(fragment(queue, transaction, path, bytes, false, true));
    }

    fn stage_png(queue: &Mutex<Jobs>, transaction: i32, path: &Path, pixels: Vec<u8>) {
        let png = PngWrite {
            path: path.to_path_buf(),
            width: 1,
            height: 1,
            pixels,
        };
        assert!(stage(
            queue,
            transaction,
            png.pixels.len(),
            |staged, limits| { staged.add_png(limits, png) }
        ));
    }

    fn cleanup(queue: &Mutex<Jobs>, transaction: i32, path: &Path, on_success: bool) {
        assert!(stage(queue, transaction, 0, |staged, limits| {
            staged.add_cleanup(limits, path.to_path_buf(), on_success)
        }));
    }

    fn keep(queue: &Mutex<Jobs>, transaction: i32, directory: &Path, name: &str) -> bool {
        stage(queue, transaction, name.len(), |staged, limits| {
            staged.add_keep(limits, directory, name)
        })
    }

    fn cleanup_dir(queue: &Mutex<Jobs>, transaction: i32, directory: &Path) -> bool {
        stage(
            queue,
            transaction,
            directory.as_os_str().len(),
            |staged, limits| staged.add_cleanup_directory(limits, directory.to_path_buf()),
        )
    }

    fn write_op(path: PathBuf, bytes: &[u8]) -> TransactionOp {
        TransactionOp::Write(AtomicWrite {
            path,
            fragments: vec![bytes.to_vec()],
        })
    }

    fn submit(queue: &Mutex<Jobs>, path: PathBuf, bytes: &[u8], append: bool) -> Option<i32> {
        queue.lock().unwrap().submit(bytes.len(), false, || {
            let write = AtomicWrite {
                path,
                fragments: vec![bytes.to_vec()],
            };
            vec![if append {
                TransactionOp::Append(write)
            } else {
                TransactionOp::Write(write)
            }]
        })
    }

    fn queued_write(queue: &Mutex<Jobs>, path: PathBuf, bytes: &[u8]) -> i32 {
        submit(queue, path, bytes, false).unwrap()
    }

    fn poll_queue(queue: &Mutex<Jobs>, id: i32, wait: bool) -> i32 {
        let pending = {
            let jobs = queue.lock().unwrap();
            jobs.pending.get(&id).cloned()
        };
        let Some(pending) = pending else {
            return -1;
        };
        let Some(result) = pending.completion.poll(wait) else {
            return 0;
        };
        queue.lock().unwrap().pending.remove(&id);
        result.code()
    }

    fn cancellation(queue: &Mutex<Jobs>, job: i32) -> Arc<Cancellation> {
        let jobs = queue.lock().unwrap();
        let pending = jobs.pending.get(&job).unwrap();
        Arc::clone(pending.cancellation.as_ref().unwrap())
    }

    fn decoded_png(path: &Path) -> Vec<u8> {
        image::load_from_memory(&std::fs::read(path).unwrap())
            .unwrap()
            .to_rgba8()
            .into_raw()
    }

    #[test]
    fn queued_writes_preserve_order_and_report_failures() {
        let root = test_root();
        let queue = Mutex::new(Jobs::new(limits(8, 64)));
        let path = root.join("nested/data");
        let first = queued_write(&queue, path.clone(), b"first");
        let second = submit(&queue, path.clone(), b"second", true).unwrap();
        assert_eq!(poll_queue(&queue, first, true), 1);
        assert_eq!(poll_queue(&queue, second, true), 1);
        assert_eq!(std::fs::read(&path).unwrap(), b"firstsecond");
        let invalid = path.join("child");
        let failed = queued_write(&queue, invalid, b"bad");
        assert_eq!(poll_queue(&queue, failed, true), -1);
        assert_eq!(std::fs::read(&path).unwrap(), b"firstsecond");
        assert_eq!(poll_queue(&queue, failed, false), -1);
    }

    #[test]
    fn waiting_poll_does_not_hold_jobs_mutex_and_submit_is_nonblocking() {
        let root = test_root();
        let gate = TestGate::new();
        let queue = Arc::new(gated(limits(4, 64), &gate, |probe| {
            matches!(probe, Probe::Request)
        }));
        let first = queued_write(&queue, root.join("first"), b"one");
        assert!(gate.wait_until_entered());
        let queue_for_poll = Arc::clone(&queue);
        let waiter = thread::spawn(move || poll_queue(&queue_for_poll, first, true));
        thread::sleep(Duration::from_millis(5));
        let second = queued_write(&queue, root.join("second"), b"two");
        assert!(second > 0, "submit must not wait for a blocked worker");
        assert_eq!(poll_queue(&queue, second, false), 0);
        gate.release();
        assert_eq!(waiter.join().unwrap(), 1);
        assert_eq!(poll_queue(&queue, second, true), 1);
    }

    #[test]
    fn admission_bounds_jobs_and_owned_bytes() {
        let root = test_root();
        let gate = TestGate::new();
        let queue = gated(limits(1, 4), &gate, |probe| matches!(probe, Probe::Request));
        let first = queued_write(&queue, root.join("first"), b"1234");
        assert!(gate.wait_until_entered());
        assert!(submit(&queue, root.join("second"), b"x", false).is_none());
        gate.release();
        assert_eq!(poll_queue(&queue, first, true), 1);
        let second = queued_write(&queue, root.join("second"), b"x");
        assert_eq!(poll_queue(&queue, second, true), 1);
    }

    #[test]
    fn completed_job_releases_owned_bytes_before_observation() {
        let root = tempfile::tempdir().unwrap();
        let gate = TestGate::new();
        let queue = gated(limits(1, 4), &gate, |probe| {
            matches!(probe, Probe::Completed)
        });
        let first = queued_write(&queue, root.path().join("first"), b"1234");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(
                gate.wait_until_entered(),
                "worker did not publish completion"
            );
            assert_eq!(poll_queue(&queue, first, false), 1);
            assert_eq!(std::fs::read(root.path().join("first")).unwrap(), b"1234");
            assert!(
                submit(&queue, root.path().join("second"), b"5678", false).is_some(),
                "completed payload still consumes the byte budget"
            );
        }));
        gate.release();
        let pending: Vec<_> = queue.lock().unwrap().pending.keys().copied().collect();
        for id in pending {
            assert_eq!(poll_queue(&queue, id, true), 1);
        }
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }

    #[test]
    fn transaction_creates_directories_and_syncs_up_to_the_existing_parent() {
        let root = test_root();
        let save = root.join("slot");
        std::fs::create_dir_all(&save).unwrap();
        let (queue, log) = logging_syncs(limits(4, 64 * 1024));
        let snapshots = save.join("snapshots");
        let level = snapshots.join("level-0");
        let face = level.join("face-1");
        let path = face.join("chunk.png");
        let manifest = save.join("manifest.json");
        let transaction = begin(&queue);
        stage_write(&queue, transaction, &path, b"png");
        stage_manifest(&queue, transaction, &manifest, b"{\"a\":");
        assert!(fragment(&queue, transaction, &manifest, b"1}", true, true));
        let job = commit(&queue, transaction);
        assert_eq!(poll_queue(&queue, job, true), 1);
        assert_eq!(
            std::mem::take(&mut *log.lock().unwrap()),
            vec![face, level, snapshots, save.clone(), save.clone()],
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"png");
        assert_eq!(std::fs::read(&manifest).unwrap(), b"{\"a\":1}");
    }

    fn compaction_limits() -> Limits {
        Limits {
            max_pending_jobs: 4,
            max_pending_bytes: 1024 * 1024,
            max_transaction_ops: 256,
            max_transaction_fragments: 256,
            ..limits(4, 0)
        }
    }

    fn compaction_fixture(root: &Path) -> (PathBuf, Vec<PathBuf>) {
        let save = root.join("saves").join("slot-1");
        let mut directories = Vec::new();
        for level in 0..3 {
            for face in 0..2 {
                for region in 0..2 {
                    let directory = save
                        .join("snapshots")
                        .join(format!("level-{level}"))
                        .join(format!("face-{face}"))
                        .join(format!("region-{region}"));
                    std::fs::create_dir_all(&directory).unwrap();
                    directories.push(directory);
                }
            }
        }
        std::fs::write(save.join("manifest.json"), b"previous root").unwrap();
        (save, directories)
    }

    fn manifest_listing(save: &Path, files: &[PathBuf]) -> String {
        let mut listing = String::new();
        for file in files {
            listing.push_str(file.strip_prefix(save).unwrap().to_str().unwrap());
            listing.push('\n');
        }
        listing
    }

    fn compaction_batch(
        queue: &Mutex<Jobs>,
        save: &Path,
        directories: &[PathBuf],
        generation: u8,
        publications: usize,
    ) -> (i32, Vec<PathBuf>) {
        let transaction = begin(queue);
        let mut files = Vec::new();
        for index in 0..publications {
            let path = directories[index % directories.len()]
                .join(format!("chunk-{index:03}-gen-{generation}.png"));
            stage_png(
                queue,
                transaction,
                &path,
                vec![generation, index as u8, 0, 255],
            );
            files.push(path);
        }
        let listing = manifest_listing(save, &files);
        stage_manifest(
            queue,
            transaction,
            &save.join("manifest.json"),
            listing.as_bytes(),
        );
        (commit(queue, transaction), files)
    }

    fn assert_compaction_published(save: &Path, files: &[PathBuf], generation: u8) {
        assert_eq!(
            std::fs::read_to_string(save.join("manifest.json")).unwrap(),
            manifest_listing(save, files)
        );
        for (index, file) in files.iter().enumerate() {
            assert_eq!(
                decoded_png(file),
                [generation, index as u8, 0, 255],
                "{file:?}"
            );
        }
    }

    #[test]
    fn compaction_syncs_each_changed_directory_once_and_barrier_failures_keep_a_reachable_root() {
        let measure_root = test_root();
        let (save, directories) = compaction_fixture(&measure_root);
        let (queue, log) = logging_syncs(compaction_limits());
        let (job, files) = compaction_batch(&queue, &save, &directories, 1, 91);
        assert_eq!(poll_queue(&queue, job, true), 1);
        assert_compaction_published(&save, &files, 1);
        let synced = std::mem::take(&mut *log.lock().unwrap());
        assert_eq!(
            synced.last(),
            Some(&save),
            "the root barrier follows its rename"
        );
        let mut synced_once = synced.clone();
        synced_once.sort();
        let mut expected = directories.clone();
        expected.push(save.clone());
        expected.sort();
        assert_eq!(synced_once, expected, "each changed directory synced once");

        let cuts = synced.len();
        for cut in 1..=cuts {
            let root = test_root();
            let (save, directories) = compaction_fixture(&root);
            let attempts = AtomicUsize::new(0);
            let queue = probed(compaction_limits(), move |probe| {
                matches!(probe, Probe::DirectorySync(_))
                    && attempts.fetch_add(1, Ordering::AcqRel) + 1 == cut
            });
            let (job, files) = compaction_batch(&queue, &save, &directories, 2, directories.len());
            match poll_queue(&queue, job, true) {
                -1 => assert_eq!(
                    std::fs::read(save.join("manifest.json")).unwrap(),
                    b"previous root",
                    "cut {cut} replaced the root before its commit barrier",
                ),
                -3 => {
                    assert_eq!(cut, cuts, "only the commit barrier is uncertain");
                    assert_compaction_published(&save, &files, 2);
                }
                other => panic!("cut {cut} reported {other}"),
            }
        }
    }

    #[test]
    fn transaction_fragment_admission_bounds_retained_fragment_overhead() {
        let root = test_root();
        let queue = Mutex::new(Jobs::new(limits(4, 128 * 1024)));
        let transaction_id = begin(&queue);
        let snapshot = root.join("snapshot");
        let chunk = vec![0u8; 4 * 1024];
        for index in 0..16 {
            assert!(fragment(
                &queue,
                transaction_id,
                &snapshot,
                &chunk,
                index != 0,
                false
            ));
        }
        assert!(!fragment(
            &queue,
            transaction_id,
            &snapshot,
            &[],
            true,
            false
        ));
        assert!(!fragment(
            &queue,
            transaction_id,
            &root.join("other"),
            b"x",
            false,
            false
        ));
        assert!(queue.lock().unwrap().abort_transaction(transaction_id));
        assert_eq!(queue.lock().unwrap().accounting.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn directory_barrier_of_a_bare_path_is_the_current_directory() {
        let control = WorkerControl::default();
        let mut bare = DirectoryBarriers::new(&control);
        bare.record_parent(Path::new("file"));
        assert_eq!(bare.changed, vec![PathBuf::from(".")]);
        assert!(bare.flush().is_ok());
    }

    fn gc_fixture(root: &Path) -> (PathBuf, PathBuf) {
        let snapshots = root.join("snapshots");
        std::fs::create_dir_all(snapshots.join("nested")).unwrap();
        std::fs::write(snapshots.join("keep.png"), b"keep").unwrap();
        std::fs::write(snapshots.join("orphan.png"), b"orphan").unwrap();
        std::fs::write(snapshots.join("nested/child.png"), b"child").unwrap();
        let manifest = root.join("manifest.json");
        std::fs::write(&manifest, b"old").unwrap();
        (snapshots, manifest)
    }

    fn stage_gc_manifest(queue: &Mutex<Jobs>, transaction: i32, snapshots: &Path, manifest: &Path) {
        stage_manifest(queue, transaction, manifest, b"new");
        assert!(cleanup_dir(queue, transaction, snapshots));
        assert!(keep(queue, transaction, snapshots, "keep.png"));
    }

    fn assert_gc_outcome(snapshots: &Path, manifest: &Path, published: bool) {
        let expected: &[u8] = if published { b"new" } else { b"old" };
        assert_eq!(std::fs::read(manifest).unwrap(), expected);
        assert!(snapshots.join("keep.png").exists());
        assert!(snapshots.join("nested/child.png").exists());
        assert_eq!(snapshots.join("orphan.png").exists(), !published);
    }

    #[test]
    fn failed_transaction_keeps_old_manifest_gc_files_and_earlier_writes() {
        let root = test_root();
        let queue = Mutex::new(Jobs::new(limits(4, 64 * 1024)));
        let (snapshots, manifest) = gc_fixture(&root);
        let snapshot = root.join("snapshot.png");
        let transaction_id = begin(&queue);
        stage_write(&queue, transaction_id, &snapshot, b"snapshot");
        stage_write(&queue, transaction_id, &snapshots, b"bad write");
        stage_gc_manifest(&queue, transaction_id, &snapshots, &manifest);
        let job = commit(&queue, transaction_id);
        assert_eq!(poll_queue(&queue, job, true), -1);
        assert_gc_outcome(&snapshots, &manifest, false);
        assert_eq!(std::fs::read(snapshot).unwrap(), b"snapshot");
    }

    #[test]
    fn uncertain_manifest_publish_retains_dependencies_and_plain_write_recovers() {
        let root = test_root();
        let fired = std::sync::atomic::AtomicBool::new(false);
        let queue = probed(limits(4, 64 * 1024), move |probe| {
            matches!(probe, Probe::Published) && !fired.swap(true, Ordering::AcqRel)
        });
        let snapshots = root.join("snapshots");
        std::fs::create_dir_all(&snapshots).unwrap();
        let previous = snapshots.join("previous.png");
        let pixels = vec![255, 0, 0, 255];
        std::fs::write(&previous, encode_png(&pixels, 1, 1).unwrap()).unwrap();
        let next = snapshots.join("next.png");
        let manifest = root.join("manifest.json");
        std::fs::write(&manifest, b"old").unwrap();

        let transaction_id = begin(&queue);
        cleanup(&queue, transaction_id, &next, false);
        stage_png(&queue, transaction_id, &next, pixels.clone());
        cleanup(&queue, transaction_id, &previous, true);
        stage_manifest(&queue, transaction_id, &manifest, b"new");
        let job = commit(&queue, transaction_id);
        assert_eq!(poll_queue(&queue, job, true), -3);
        assert_eq!(std::fs::read(&manifest).unwrap(), b"new");
        assert_eq!(decoded_png(&previous), pixels);
        assert_eq!(decoded_png(&next), pixels);

        let recovery = begin(&queue);
        stage_manifest(&queue, recovery, &manifest, b"recovered");
        let recovery_job = commit(&queue, recovery);
        assert_eq!(poll_queue(&queue, recovery_job, true), 1);
        assert_eq!(std::fs::read(&manifest).unwrap(), b"recovered");
        assert!(previous.exists());
        assert!(next.exists());
    }

    #[test]
    fn cleanup_keep_requires_plain_filenames_and_registered_directories() {
        let root = test_root();
        let queue = Mutex::new(Jobs::new(limits(4, 64 * 1024)));
        let snapshots = root.join("snapshots");
        let transaction_id = begin(&queue);
        assert!(!keep(&queue, transaction_id, &snapshots, "keep.png"));
        assert!(cleanup_dir(&queue, transaction_id, &snapshots));
        for name in ["nested/keep.png", "nested\\keep.png", ".."] {
            assert!(!keep(&queue, transaction_id, &snapshots, name), "{name}");
        }
        assert!(keep(&queue, transaction_id, &snapshots, "keep.png"));
        assert!(queue.lock().unwrap().abort_transaction(transaction_id));
    }

    #[test]
    fn cleanup_admission_bounds_directories_items_and_metadata_bytes() {
        let root = test_root();
        let snapshots = root.join("snapshots");
        let queue = Mutex::new(Jobs::new(Limits {
            max_transaction_cleanup_directories: 1,
            max_transaction_keep_entries: 1,
            max_transaction_cleanup_bytes: snapshots.as_os_str().len() + 1,
            ..limits(4, 64 * 1024)
        }));
        let transaction_id = begin(&queue);
        assert!(cleanup_dir(&queue, transaction_id, &snapshots));
        assert!(!cleanup_dir(&queue, transaction_id, &root.join("other")));
        assert!(keep(&queue, transaction_id, &snapshots, "x"));
        assert!(!keep(&queue, transaction_id, &snapshots, "y"));
        assert!(queue.lock().unwrap().abort_transaction(transaction_id));
        assert_eq!(queue.lock().unwrap().accounting.load(Ordering::Acquire), 0);
    }

    #[test]
    fn transaction_cleanup_runs_for_success_and_failure() {
        let root = test_root();
        let queue = Mutex::new(Jobs::new(limits(4, 64)));
        let blocked = root.join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        for (target, code) in [(root.join("success"), 1), (blocked, -1)] {
            let stale = root.join("stale.tmp");
            std::fs::write(&stale, b"stale").unwrap();
            let transaction = begin(&queue);
            stage_write(&queue, transaction, &target, b"write");
            cleanup(&queue, transaction, &stale, code == 1);
            let job = commit(&queue, transaction);
            assert_eq!(poll_queue(&queue, job, true), code);
            assert!(!stale.exists(), "{target:?}");
        }
    }

    #[test]
    fn cancellation_before_publication_never_replaces_manifest() {
        let root = test_root();
        let gate = TestGate::new();
        let queue = gated(limits(4, 64 * 1024), &gate, |probe| {
            matches!(probe, Probe::Request)
        });
        let (snapshots, manifest) = gc_fixture(&root);
        let transaction_id = begin(&queue);
        stage_gc_manifest(&queue, transaction_id, &snapshots, &manifest);
        let job = commit(&queue, transaction_id);
        assert!(gate.wait_until_entered());
        assert_eq!(cancellation(&queue, job).cancel(), 1);
        gate.release();
        assert_eq!(poll_queue(&queue, job, true), -2);
        assert_gc_outcome(&snapshots, &manifest, false);
    }

    #[test]
    fn cancellation_after_publication_claim_still_publishes_and_prunes() {
        let root = test_root();
        let gate = TestGate::new();
        let queue = gated(limits(4, 64 * 1024), &gate, |probe| {
            matches!(probe, Probe::Publication)
        });
        let (snapshots, manifest) = gc_fixture(&root);
        let transaction_id = begin(&queue);
        stage_gc_manifest(&queue, transaction_id, &snapshots, &manifest);
        let job = commit(&queue, transaction_id);
        assert!(gate.wait_until_entered());
        assert_eq!(cancellation(&queue, job).cancel(), 0);
        gate.release();
        assert_eq!(poll_queue(&queue, job, true), 1);
        assert_gc_outcome(&snapshots, &manifest, true);
    }

    #[test]
    fn failed_restoring_write_puts_back_every_earlier_path() {
        let root = tempfile::tempdir().unwrap();
        let level = root.path().join("level");
        let quest = root.path().join("quest");
        let blocker = root.path().join("blocker");
        std::fs::write(&level, b"old level").unwrap();
        std::fs::write(&blocker, b"blocker").unwrap();
        let queue = Mutex::new(Jobs::new(limits(4, 64)));
        let job = queue.lock().unwrap().submit(0, true, || {
            vec![
                write_op(level.clone(), b"new level"),
                write_op(quest.clone(), b"new quest"),
                write_op(blocker.join("child"), b"bad"),
            ]
        });
        assert_eq!(poll_queue(&queue, job.unwrap(), true), -1);
        assert_eq!(std::fs::read(&level).unwrap(), b"old level");
        assert!(!quest.exists());
    }
}

#[cfg(test)]
mod pair_tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn paired_admission_counts_both_payloads() {
        let half = vec![0u8; MAX_PENDING_BYTES / 2 + 1];
        let first = CString::new("first").unwrap();
        let second = CString::new("second").unwrap();
        let data = half.as_ptr().cast();
        let id = stone_file_write_pair_async(
            first.as_ptr(),
            data,
            half.len(),
            second.as_ptr(),
            data,
            half.len(),
        );
        assert_eq!(id, -1);
    }

    fn write_edit_and_quest(first: &CString, second: &CString) -> i32 {
        stone_file_write_pair_async(
            first.as_ptr(),
            c"edit".as_ptr(),
            4,
            second.as_ptr(),
            c"quest".as_ptr(),
            5,
        )
    }

    #[test]
    fn paired_writes_roll_back_first_and_keep_queue_running() {
        let root = tempfile::tempdir().unwrap();
        let first_path = root.path().join("level");
        let blocker = root.path().join("blocker");
        std::fs::write(&first_path, b"original").unwrap();
        std::fs::write(&blocker, b"blocker").unwrap();
        let first = CString::new(first_path.to_str().unwrap()).unwrap();
        let bad = CString::new(blocker.join("quest").to_str().unwrap()).unwrap();
        let failed = write_edit_and_quest(&first, &bad);
        assert_eq!(stone_file_write_poll(failed, true), -1);
        assert_eq!(std::fs::read(&first_path).unwrap(), b"original");
        std::fs::remove_file(&first_path).unwrap();
        let absent = write_edit_and_quest(&first, &bad);
        assert_eq!(stone_file_write_poll(absent, true), -1);
        assert!(!first_path.exists());
        let second_path = root.path().join("quest");
        let second = CString::new(second_path.to_str().unwrap()).unwrap();
        let ok = write_edit_and_quest(&first, &second);
        assert_eq!(stone_file_write_poll(ok, true), 1);
        assert_eq!(std::fs::read(first_path).unwrap(), b"edit");
        assert_eq!(std::fs::read(second_path).unwrap(), b"quest");
    }
}
