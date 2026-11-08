use image::ImageEncoder;
use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
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

static FILE_JOBS_TEST_PAUSED: AtomicU8 = AtomicU8::new(0);
static FILE_JOBS_TEST_ENTERED: AtomicU8 = AtomicU8::new(0);
static FILE_JOBS_TEST_PUBLICATION_PAUSED: AtomicU8 = AtomicU8::new(0);
static FILE_JOBS_TEST_PUBLICATION_ENTERED: AtomicU8 = AtomicU8::new(0);

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

fn file_jobs_test_pause_gate() {
    if !file_jobs_test_enabled() {
        return;
    }
    let control = file_jobs_test_control();
    let mut guard = control.state.lock().unwrap();
    if FILE_JOBS_TEST_PAUSED.load(Ordering::Acquire) == 0 {
        return;
    }
    FILE_JOBS_TEST_ENTERED.store(1, Ordering::Release);
    control.wake.notify_all();
    while FILE_JOBS_TEST_PAUSED.load(Ordering::Acquire) != 0 {
        guard = control.wake.wait(guard).unwrap();
    }
    FILE_JOBS_TEST_ENTERED.store(0, Ordering::Release);
}

fn file_jobs_test_publication_pause_gate() {
    if !file_jobs_test_enabled() {
        return;
    }
    let control = file_jobs_test_control();
    let mut guard = control.state.lock().unwrap();
    if FILE_JOBS_TEST_PUBLICATION_PAUSED.load(Ordering::Acquire) == 0 {
        return;
    }
    FILE_JOBS_TEST_PUBLICATION_ENTERED.store(1, Ordering::Release);
    control.wake.notify_all();
    while FILE_JOBS_TEST_PUBLICATION_PAUSED.load(Ordering::Acquire) != 0 {
        guard = control.wake.wait(guard).unwrap();
    }
    FILE_JOBS_TEST_PUBLICATION_ENTERED.store(0, Ordering::Release);
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

struct SingleRequest {
    path: PathBuf,
    bytes: Vec<u8>,
    append: bool,
    source: Option<PathBuf>,
    second: Option<(PathBuf, Vec<u8>)>,
}

impl SingleRequest {
    fn owned_bytes(&self) -> usize {
        self.bytes
            .len()
            .saturating_add(self.second.as_ref().map_or(0, |(_, bytes)| bytes.len()))
    }
}

struct Request {
    id: i32,
    work: Work,
    completion: Arc<Completion>,
    cancellation: Option<Arc<Cancellation>>,
    owned_bytes: usize,
}

enum Work {
    Single(SingleRequest),
    Transaction(Transaction),
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
    Png(PngWrite),
    Publish(AtomicWrite),
    Sync(PathBuf),
    Rename { source: PathBuf, target: PathBuf },
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

#[cfg(test)]
#[derive(Default)]
struct DirectorySyncLog {
    directories: Mutex<Vec<PathBuf>>,
}

#[cfg(test)]
impl DirectorySyncLog {
    fn record(&self, directory: &Path) {
        self.directories
            .lock()
            .unwrap()
            .push(directory.to_path_buf());
    }

    fn take(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.directories.lock().unwrap())
    }
}

#[cfg(test)]
struct DirectorySyncFailure {
    index: usize,
    attempts: AtomicUsize,
}

#[cfg(test)]
impl DirectorySyncFailure {
    fn at(index: usize) -> Self {
        Self {
            index,
            attempts: AtomicUsize::new(0),
        }
    }

    fn should_fail(&self) -> bool {
        self.attempts.fetch_add(1, Ordering::AcqRel) + 1 == self.index
    }
}

// A crash keeps a renamed or created name only after its containing directory is
// synced. Every publication records the directories whose entries it changed and
// each phase syncs that set once, deepest first; ancestors enter the set only as
// parents of directories this transaction created.
#[derive(Default)]
struct DirectoryBarriers {
    changed: Vec<PathBuf>,
    #[cfg(test)]
    log: Option<Arc<DirectorySyncLog>>,
    #[cfg(test)]
    failure: Option<Arc<DirectorySyncFailure>>,
}

impl DirectoryBarriers {
    fn observed(control: &WorkerControl) -> Self {
        #[cfg(not(test))]
        let _ = control;
        Self {
            changed: Vec::new(),
            #[cfg(test)]
            log: control.directory_syncs.clone(),
            #[cfg(test)]
            failure: control.directory_sync_failure.clone(),
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
        let mut outcome = None;
        for directory in &self.changed {
            #[cfg(test)]
            if let Some(log) = &self.log {
                log.record(directory);
            }
            #[cfg(test)]
            if let Some(failure) = &self.failure {
                if failure.should_fail() {
                    outcome = Some(DirectoryBarrierError {
                        directory: directory.clone(),
                        error: std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "injected directory barrier failure",
                        ),
                    });
                    break;
                }
            }
            if let Err(error) = sync_directory(directory) {
                outcome = Some(DirectoryBarrierError {
                    directory: directory.clone(),
                    error,
                });
                break;
            }
        }
        self.changed.clear();
        match outcome {
            Some(failure) => Err(failure),
            None => Ok(()),
        }
    }
}

struct TransactionBuilder {
    operations: Vec<TransactionOp>,
    cleanup_success: Vec<PathBuf>,
    cleanup_failure: Vec<PathBuf>,
    cleanup_directories: Vec<CleanupDirectory>,
    cleanup_entries: usize,
    cleanup_bytes: usize,
    bytes: usize,
    fragments: usize,
    published: bool,
}

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
}

impl TransactionBuilder {
    fn new() -> Self {
        Self {
            operations: Vec::new(),
            cleanup_success: Vec::new(),
            cleanup_failure: Vec::new(),
            cleanup_directories: Vec::new(),
            cleanup_entries: 0,
            cleanup_bytes: 0,
            bytes: 0,
            fragments: 0,
            published: false,
        }
    }

    fn add_fragment(
        &mut self,
        path: PathBuf,
        bytes: Vec<u8>,
        append: bool,
        publish: bool,
        max_ops: usize,
        max_fragments: usize,
    ) -> bool {
        if self.fragments >= max_fragments {
            return false;
        }
        if publish {
            if self.published {
                if !append {
                    return false;
                }
                let accepted = Self::append_to_last(&mut self.operations, &path, bytes, true);
                if accepted {
                    self.fragments += 1;
                }
                return accepted;
            }
            if append || self.operations.len() >= max_ops {
                return false;
            }
            self.operations.push(TransactionOp::Publish(AtomicWrite {
                path,
                fragments: vec![bytes],
            }));
            self.published = true;
            self.fragments += 1;
            return true;
        }

        if self.published {
            return false;
        }
        if append {
            let accepted = Self::append_to_last(&mut self.operations, &path, bytes, false);
            if accepted {
                self.fragments += 1;
            }
            return accepted;
        }
        if self.operations.len() >= max_ops {
            return false;
        }
        self.operations.push(TransactionOp::Write(AtomicWrite {
            path,
            fragments: vec![bytes],
        }));
        self.fragments += 1;
        true
    }

    fn append_to_last(
        operations: &mut [TransactionOp],
        path: &Path,
        bytes: Vec<u8>,
        publish: bool,
    ) -> bool {
        let Some(last) = operations.last_mut() else {
            return false;
        };
        let write = match last {
            TransactionOp::Write(write) if !publish => write,
            TransactionOp::Publish(write) if publish => write,
            _ => return false,
        };
        if write.path != path {
            return false;
        }
        write.fragments.push(bytes);
        true
    }

    fn add_sync(&mut self, path: PathBuf, max_ops: usize) -> bool {
        if self.published || self.operations.len() >= max_ops {
            return false;
        }
        self.operations.push(TransactionOp::Sync(path));
        true
    }

    fn add_rename(&mut self, source: PathBuf, target: PathBuf, max_ops: usize) -> bool {
        if self.published || self.operations.len() >= max_ops {
            return false;
        }
        self.operations
            .push(TransactionOp::Rename { source, target });
        true
    }

    fn add_cleanup(&mut self, path: PathBuf, on_success: bool, max_cleanups: usize) -> bool {
        let paths = if on_success {
            &mut self.cleanup_success
        } else {
            &mut self.cleanup_failure
        };
        if paths.len() >= max_cleanups {
            return false;
        }
        paths.push(path);
        true
    }

    fn finish(self) -> Transaction {
        Transaction {
            operations: self.operations,
            cleanup_success: self.cleanup_success,
            cleanup_failure: self.cleanup_failure,
            cleanup_directories: self.cleanup_directories,
            cleanup_entries: self.cleanup_entries,
            cleanup_bytes: self.cleanup_bytes,
            bytes: self.bytes,
            fragments: self.fragments,
            published: self.published,
        }
    }
}

#[derive(Clone, Default)]
struct WorkerControl {
    #[cfg(test)]
    gate: Option<Arc<TestGate>>,
    #[cfg(test)]
    publication_gate: Option<Arc<TestGate>>,
    #[cfg(test)]
    completion_gate: Option<Arc<TestGate>>,
    #[cfg(test)]
    publish_sync_failure: Option<Arc<AtomicBool>>,
    #[cfg(test)]
    directory_syncs: Option<Arc<DirectorySyncLog>>,
    #[cfg(test)]
    directory_sync_failure: Option<Arc<DirectorySyncFailure>>,
}

struct Jobs {
    sender: mpsc::SyncSender<Request>,
    pending: HashMap<i32, Pending>,
    transactions: HashMap<i32, TransactionBuilder>,
    next: i32,
    reserved_jobs: usize,
    accounting: Arc<AtomicUsize>,
    limits: Limits,
}

#[cfg(test)]
struct TestGate {
    state: Mutex<(bool, bool)>,
    ready: Condvar,
}

#[cfg(test)]
impl TestGate {
    fn new() -> Self {
        Self {
            state: Mutex::new((false, false)),
            ready: Condvar::new(),
        }
    }

    fn block_request(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.ready.notify_all();
        while !state.1 {
            state = self.ready.wait(state).unwrap();
        }
    }

    fn wait_until_entered(&self) {
        let mut state = self.state.lock().unwrap();
        while !state.0 {
            state = self.ready.wait(state).unwrap();
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap();
        state.1 = true;
        self.ready.notify_all();
    }
}

impl Jobs {
    fn new(limits: Limits) -> Self {
        Self::new_with_control(limits, WorkerControl::default())
    }

    #[cfg(test)]
    fn new_with_gate(limits: Limits, gate: Arc<TestGate>) -> Self {
        Self::new_with_control(
            limits,
            WorkerControl {
                gate: Some(gate),
                publication_gate: None,
                publish_sync_failure: None,
                completion_gate: None,
                directory_syncs: None,
                directory_sync_failure: None,
            },
        )
    }

    #[cfg(test)]
    fn new_with_publication_gate(limits: Limits, gate: Arc<TestGate>) -> Self {
        Self::new_with_control(
            limits,
            WorkerControl {
                gate: None,
                publication_gate: Some(gate),
                publish_sync_failure: None,
                completion_gate: None,
                directory_syncs: None,
                directory_sync_failure: None,
            },
        )
    }

    #[cfg(test)]
    fn new_with_publish_sync_failure(limits: Limits, failure: Arc<AtomicBool>) -> Self {
        Self::new_with_control(
            limits,
            WorkerControl {
                gate: None,
                publication_gate: None,
                publish_sync_failure: Some(failure),
                completion_gate: None,
                directory_syncs: None,
                directory_sync_failure: None,
            },
        )
    }

    #[cfg(test)]
    fn new_with_directory_syncs(limits: Limits, log: Arc<DirectorySyncLog>) -> Self {
        Self::new_with_control(
            limits,
            WorkerControl {
                directory_syncs: Some(log),
                ..WorkerControl::default()
            },
        )
    }

    #[cfg(test)]
    fn new_with_directory_sync_failure(limits: Limits, failure: Arc<DirectorySyncFailure>) -> Self {
        Self::new_with_control(
            limits,
            WorkerControl {
                directory_sync_failure: Some(failure),
                ..WorkerControl::default()
            },
        )
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
            reserved_jobs: 0,
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

    fn begin_transaction(&mut self) -> Option<i32> {
        if self.transactions.len() >= self.limits.max_open_transactions {
            return None;
        }
        let id = self.next_id()?;
        self.transactions.insert(id, TransactionBuilder::new());
        Some(id)
    }

    fn reserve_transaction_bytes(&self, id: i32, bytes: usize) -> bool {
        let Some(transaction) = self.transactions.get(&id) else {
            return false;
        };
        transaction.fragments < self.limits.max_transaction_fragments
            && bytes <= self.limits.max_transaction_fragment_bytes
            && self.reserve_bytes(bytes)
    }

    fn add_transaction_fragment_reserved(
        &mut self,
        id: i32,
        path: PathBuf,
        bytes: Vec<u8>,
        append: bool,
        publish: bool,
    ) -> bool {
        let length = bytes.len();
        let Some(transaction) = self.transactions.get_mut(&id) else {
            self.release_bytes(length);
            return false;
        };
        if !transaction.add_fragment(
            path,
            bytes,
            append,
            publish,
            self.limits.max_transaction_ops,
            self.limits.max_transaction_fragments,
        ) {
            self.release_bytes(length);
            return false;
        }
        transaction.bytes += length;
        true
    }

    #[cfg(test)]
    fn add_transaction_fragment_owned(
        &mut self,
        id: i32,
        path: PathBuf,
        bytes: Vec<u8>,
        append: bool,
        publish: bool,
    ) -> bool {
        let length = bytes.len();
        if !self.reserve_transaction_bytes(id, length) {
            return false;
        }
        self.add_transaction_fragment_reserved(id, path, bytes, append, publish)
    }

    fn add_transaction_png_reserved(
        &mut self,
        id: i32,
        path: PathBuf,
        pixels: Vec<u8>,
        width: u32,
        height: u32,
    ) -> bool {
        let length = pixels.len();
        let Some(transaction) = self.transactions.get_mut(&id) else {
            self.release_bytes(length);
            return false;
        };
        if transaction.published
            || transaction.operations.len() >= self.limits.max_transaction_ops
            || transaction.fragments >= self.limits.max_transaction_fragments
        {
            self.release_bytes(length);
            return false;
        }
        transaction.operations.push(TransactionOp::Png(PngWrite {
            path,
            pixels,
            width,
            height,
        }));
        transaction.bytes += length;
        transaction.fragments += 1;
        true
    }

    fn add_transaction_sync(&mut self, id: i32, path: PathBuf) -> bool {
        let Some(transaction) = self.transactions.get_mut(&id) else {
            return false;
        };
        transaction.add_sync(path, self.limits.max_transaction_ops)
    }

    fn add_transaction_rename(&mut self, id: i32, source: PathBuf, target: PathBuf) -> bool {
        let Some(transaction) = self.transactions.get_mut(&id) else {
            return false;
        };
        transaction.add_rename(source, target, self.limits.max_transaction_ops)
    }

    fn add_transaction_cleanup(&mut self, id: i32, path: PathBuf, on_success: bool) -> bool {
        let Some(transaction) = self.transactions.get_mut(&id) else {
            return false;
        };
        transaction.add_cleanup(path, on_success, self.limits.max_transaction_cleanups)
    }

    fn reserve_transaction_cleanup_directory(&self, id: i32, bytes: usize) -> bool {
        let Some(transaction) = self.transactions.get(&id) else {
            return false;
        };
        if transaction.cleanup_directories.len() >= self.limits.max_transaction_cleanup_directories
            || bytes > self.limits.max_transaction_cleanup_bytes
            || transaction.cleanup_bytes > self.limits.max_transaction_cleanup_bytes - bytes
        {
            return false;
        }
        self.reserve_bytes(bytes)
    }

    fn reserve_transaction_keep(&self, id: i32, directory: &Path, bytes: usize) -> bool {
        let Some(transaction) = self.transactions.get(&id) else {
            return false;
        };
        if !transaction
            .cleanup_directories
            .iter()
            .any(|entry| entry.directory == directory)
            || transaction.cleanup_entries >= self.limits.max_transaction_keep_entries
            || bytes > self.limits.max_transaction_cleanup_bytes
            || transaction.cleanup_bytes > self.limits.max_transaction_cleanup_bytes - bytes
        {
            return false;
        }
        self.reserve_bytes(bytes)
    }

    fn add_transaction_cleanup_directory_reserved(
        &mut self,
        id: i32,
        directory: PathBuf,
        bytes: usize,
    ) -> bool {
        let Some(transaction) = self.transactions.get_mut(&id) else {
            self.release_bytes(bytes);
            return false;
        };
        if bytes > self.limits.max_transaction_cleanup_bytes
            || transaction.cleanup_directories.len()
                >= self.limits.max_transaction_cleanup_directories
            || transaction.cleanup_bytes > self.limits.max_transaction_cleanup_bytes - bytes
            || transaction
                .cleanup_directories
                .iter()
                .any(|entry| entry.directory == directory)
        {
            self.release_bytes(bytes);
            return false;
        }
        transaction.cleanup_directories.push(CleanupDirectory {
            directory,
            keep: Vec::new(),
        });
        transaction.cleanup_bytes += bytes;
        transaction.bytes += bytes;
        true
    }

    #[cfg(test)]
    fn add_transaction_cleanup_directory_owned(&mut self, id: i32, directory: PathBuf) -> bool {
        let bytes = directory.as_os_str().len();
        if !self.reserve_transaction_cleanup_directory(id, bytes) {
            return false;
        }
        self.add_transaction_cleanup_directory_reserved(id, directory, bytes)
    }

    fn add_transaction_keep_reserved(
        &mut self,
        id: i32,
        directory: &Path,
        filename: String,
        bytes: usize,
    ) -> bool {
        let Some(transaction) = self.transactions.get_mut(&id) else {
            self.release_bytes(bytes);
            return false;
        };
        let Some(directory_index) = transaction
            .cleanup_directories
            .iter()
            .position(|entry| entry.directory == directory)
        else {
            self.release_bytes(bytes);
            return false;
        };
        if transaction.cleanup_entries >= self.limits.max_transaction_keep_entries
            || bytes > self.limits.max_transaction_cleanup_bytes
            || transaction.cleanup_bytes > self.limits.max_transaction_cleanup_bytes - bytes
        {
            self.release_bytes(bytes);
            return false;
        }
        transaction.cleanup_directories[directory_index]
            .keep
            .push(filename);
        transaction.cleanup_entries += 1;
        transaction.cleanup_bytes += bytes;
        transaction.bytes += bytes;
        true
    }

    #[cfg(test)]
    fn add_transaction_keep_owned(
        &mut self,
        id: i32,
        directory: PathBuf,
        filename: String,
    ) -> bool {
        if !is_plain_filename(&filename) {
            return false;
        }
        let bytes = filename.len();
        if !self.reserve_transaction_keep(id, &directory, bytes) {
            return false;
        }
        self.add_transaction_keep_reserved(id, &directory, filename, bytes)
    }

    fn reserve_single_submission(&mut self, bytes: usize) -> bool {
        if self.pending.len() + self.reserved_jobs >= self.limits.max_pending_jobs
            || !self.reserve_bytes(bytes)
        {
            return false;
        }
        self.reserved_jobs += 1;
        true
    }

    fn release_reserved_single(&mut self, bytes: usize) {
        self.reserved_jobs = self.reserved_jobs.saturating_sub(1);
        self.release_bytes(bytes);
    }

    fn enqueue_single(&mut self, request: SingleRequest) -> Option<i32> {
        let owned_bytes = request.owned_bytes();
        if !self.reserve_single_submission(owned_bytes) {
            return None;
        }
        self.enqueue_single_reserved(request)
    }

    fn enqueue_single_reserved(&mut self, request: SingleRequest) -> Option<i32> {
        let owned_bytes = request.owned_bytes();
        let Some(id) = self.next_id() else {
            self.release_reserved_single(owned_bytes);
            return None;
        };
        let completion = Arc::new(Completion::new());
        let pending = Pending {
            completion: Arc::clone(&completion),
            cancellation: None,
        };
        let request = Request {
            id,
            work: Work::Single(request),
            completion,
            cancellation: None,
            owned_bytes,
        };
        match self.sender.try_send(request) {
            Ok(()) => {
                self.reserved_jobs = self.reserved_jobs.saturating_sub(1);
                self.pending.insert(id, pending);
                Some(id)
            }
            Err(mpsc::TrySendError::Full(request))
            | Err(mpsc::TrySendError::Disconnected(request)) => {
                self.release_reserved_single(request.owned_bytes);
                None
            }
        }
    }

    fn commit_transaction(&mut self, id: i32) -> Option<i32> {
        if self.pending.len() + self.reserved_jobs >= self.limits.max_pending_jobs {
            return None;
        }
        let builder = self.transactions.remove(&id)?;
        let job_id = match self.next_id() {
            Some(job_id) => job_id,
            None => {
                self.transactions.insert(id, builder);
                return None;
            }
        };
        let transaction = builder.finish();
        let completion = Arc::new(Completion::new());
        let cancellation = Arc::new(Cancellation::new());
        let pending = Pending {
            completion: Arc::clone(&completion),
            cancellation: Some(Arc::clone(&cancellation)),
        };
        let owned_bytes = transaction.bytes;
        let request = Request {
            id: job_id,
            work: Work::Transaction(transaction),
            completion,
            cancellation: Some(cancellation),
            owned_bytes,
        };
        match self.sender.try_send(request) {
            Ok(()) => {
                self.pending.insert(job_id, pending);
                Some(job_id)
            }
            Err(mpsc::TrySendError::Full(request)) => {
                if let Work::Transaction(transaction) = request.work {
                    self.transactions.insert(
                        id,
                        TransactionBuilder {
                            operations: transaction.operations,
                            cleanup_success: transaction.cleanup_success,
                            cleanup_failure: transaction.cleanup_failure,
                            cleanup_directories: transaction.cleanup_directories,
                            cleanup_entries: transaction.cleanup_entries,
                            cleanup_bytes: transaction.cleanup_bytes,
                            bytes: transaction.bytes,
                            fragments: transaction.fragments,
                            published: transaction.published,
                        },
                    );
                }
                None
            }
            Err(mpsc::TrySendError::Disconnected(request)) => {
                self.release_bytes(request.owned_bytes);
                None
            }
        }
    }

    fn abort_transaction(&mut self, id: i32) -> bool {
        let Some(transaction) = self.transactions.remove(&id) else {
            return false;
        };
        self.release_bytes(transaction.bytes);
        true
    }

    fn reset_transactions(&mut self) -> usize {
        let mut reset_count = 0;
        let mut released_bytes = 0;
        for (_, transaction) in self.transactions.drain() {
            reset_count += 1;
            released_bytes += transaction.bytes;
        }
        self.release_bytes(released_bytes);
        self.pending.retain(|_, pending| {
            let Some(cancellation) = &pending.cancellation else {
                return true;
            };
            let _ = cancellation.cancel();
            reset_count += 1;
            false
        });
        reset_count
    }
}

impl WorkerControl {
    fn take_publish_sync_failure(&self) -> bool {
        #[cfg(test)]
        if let Some(failure) = &self.publish_sync_failure {
            return failure.swap(false, Ordering::AcqRel);
        }
        false
    }
}

fn worker_loop(
    receiver: mpsc::Receiver<Request>,
    accounting: Arc<AtomicUsize>,
    control: WorkerControl,
) {
    #[cfg(not(test))]
    let _ = &control;
    for request in receiver {
        file_jobs_test_pause_gate();
        #[cfg(test)]
        if let Some(gate) = &control.gate {
            gate.block_request();
        }
        let result = match (&request.work, &request.cancellation) {
            (Work::Single(single), None) => match write_single(single, request.id, &control) {
                Ok(()) => JobResult::Success,
                Err(error) => {
                    let operation = if single.source.is_some() {
                        "rename"
                    } else if single.append {
                        "append"
                    } else {
                        "write"
                    };
                    log_io_error(operation, &single.path, &error);
                    JobResult::Failure
                }
            },
            (Work::Transaction(transaction), Some(cancellation)) => {
                execute_transaction(transaction, cancellation, request.id, &control)
            }
            _ => JobResult::Failure,
        };
        // Completion admits a successor, so release its predecessor's payload first.
        drop(request.work);
        accounting.fetch_sub(request.owned_bytes, Ordering::AcqRel);
        request.completion.finish(result);
        #[cfg(test)]
        if let Some(gate) = &control.completion_gate {
            gate.block_request();
        }
    }
}

fn write_single(request: &SingleRequest, id: i32, control: &WorkerControl) -> std::io::Result<()> {
    let mut barriers = DirectoryBarriers::observed(control);
    if request.second.is_some() {
        return write_pair(request, id, false, &mut barriers);
    }
    ensure_parent(&request.path, &mut barriers)?;
    if let Some(source) = &request.source {
        std::fs::rename(source, &request.path)?;
        barriers.record_parent(&request.path);
        return barriers.flush().map_err(|failure| failure.error);
    }
    if request.append {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&request.path)?;
        let previous = file.metadata()?.len();
        if let Err(error) = file.write_all(&request.bytes).and_then(|_| file.sync_all()) {
            file.set_len(previous)?;
            file.sync_all()?;
            return Err(error);
        }
        // Journal appends keep their own weaker contract: the data is synced,
        // the directory name is not a publication barrier.
        return Ok(());
    }
    write_atomic(
        &request.path,
        std::slice::from_ref(&request.bytes),
        id,
        0,
        &mut barriers,
    )?;
    barriers.flush().map_err(|failure| failure.error)
}

fn read_previous(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn restore_previous(
    path: &Path,
    before: &Option<Vec<u8>>,
    id: i32,
    sequence: usize,
    barriers: &mut DirectoryBarriers,
) -> std::io::Result<()> {
    let result = match before {
        Some(bytes) => write_atomic(path, std::slice::from_ref(bytes), id, sequence, barriers),
        None => std::fs::remove_file(path).map(|()| barriers.record_parent(path)),
    };
    let result = result.and_then(|()| barriers.flush().map_err(|failure| failure.error));
    if let Err(error) = &result {
        log_io_error("paired rollback", path, error);
    }
    result
}

fn write_pair(
    request: &SingleRequest,
    id: i32,
    fail_second_sync: bool,
    barriers: &mut DirectoryBarriers,
) -> std::io::Result<()> {
    let Some((second, bytes)) = &request.second else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "missing paired destination",
        ));
    };
    if second == &request.path {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "paired destinations must differ",
        ));
    }
    let before = read_previous(&request.path)?;
    write_atomic(
        &request.path,
        std::slice::from_ref(&request.bytes),
        id,
        0,
        barriers,
    )?;
    let result = (|| {
        let second_before = read_previous(second)?;
        write_atomic(second, std::slice::from_ref(bytes), id, 1, barriers)?;
        if fail_second_sync {
            // Injected: the second name is renamed into place but its durability
            // acknowledgement fails, so both destinations roll back.
            restore_previous(second, &second_before, id, 3, barriers)?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "injected post-publish sync failure",
            ));
        }
        Ok(())
    })();
    if result.is_err() {
        restore_previous(&request.path, &before, id, 2, barriers)?;
        return result;
    }
    barriers.flush().map_err(|failure| failure.error)
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

// create_dir_all does not report what it created; the ancestors of a created
// directory are the only ones that still need a barrier.
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
    let mut barriers = DirectoryBarriers::observed(control);
    for (sequence, operation) in transaction.operations.iter().enumerate() {
        match operation {
            TransactionOp::Publish(write) => {
                if !cancellation.claim_publication() {
                    cleanup(&transaction.cleanup_failure);
                    cancellation.complete();
                    return JobResult::Cancelled;
                }
                #[cfg(test)]
                if let Some(gate) = &control.publication_gate {
                    gate.block_request();
                }
                file_jobs_test_publication_pause_gate();
                // Everything the root reaches is renamed into place by now, so
                // those names must be durable before the root name can be.
                if let Err(failure) = barriers.flush() {
                    log_io_error("directory barrier", &failure.directory, &failure.error);
                    cleanup(&transaction.cleanup_failure);
                    cancellation.complete();
                    return JobResult::Failure;
                }
                if let Err(error) =
                    write_atomic(&write.path, &write.fragments, id, sequence, &mut barriers)
                {
                    log_io_error("publish", &write.path, &error);
                    cleanup(&transaction.cleanup_failure);
                    cancellation.complete();
                    return JobResult::Failure;
                }
                if control.take_publish_sync_failure() {
                    let error = std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "injected post-publish sync failure",
                    );
                    log_io_error("publish durability", &write.path, &error);
                    cancellation.complete();
                    return JobResult::DurabilityUncertain;
                }
                // The commit point: the root name reaches durability here and
                // nowhere earlier, which is what acknowledgement reports.
                if let Err(failure) = barriers.flush() {
                    log_io_error("publish durability", &write.path, &failure.error);
                    cancellation.complete();
                    return JobResult::DurabilityUncertain;
                }
            }
            TransactionOp::Write(write) => {
                if cancellation.is_cancelled() {
                    cleanup(&transaction.cleanup_failure);
                    cancellation.complete();
                    return JobResult::Cancelled;
                }
                if let Err(error) =
                    write_atomic(&write.path, &write.fragments, id, sequence, &mut barriers)
                {
                    log_io_error("write", &write.path, &error);
                    cleanup(&transaction.cleanup_failure);
                    cancellation.complete();
                    return JobResult::Failure;
                }
            }
            TransactionOp::Png(png) => {
                if cancellation.is_cancelled() {
                    cleanup(&transaction.cleanup_failure);
                    cancellation.complete();
                    return JobResult::Cancelled;
                }
                let encoded = match encode_png(&png.pixels, png.width, png.height) {
                    Ok(encoded) => encoded,
                    Err(error) => {
                        log_io_error("png encode", &png.path, &error);
                        cleanup(&transaction.cleanup_failure);
                        cancellation.complete();
                        return JobResult::Failure;
                    }
                };
                if let Err(error) = write_atomic(
                    &png.path,
                    std::slice::from_ref(&encoded),
                    id,
                    sequence,
                    &mut barriers,
                ) {
                    log_io_error("png write", &png.path, &error);
                    cleanup(&transaction.cleanup_failure);
                    cancellation.complete();
                    return JobResult::Failure;
                }
            }
            TransactionOp::Sync(path) => {
                if cancellation.is_cancelled() {
                    cleanup(&transaction.cleanup_failure);
                    cancellation.complete();
                    return JobResult::Cancelled;
                }
                if let Err(error) = sync_file(path) {
                    log_io_error("sync", path, &error);
                    cleanup(&transaction.cleanup_failure);
                    cancellation.complete();
                    return JobResult::Failure;
                }
            }
            TransactionOp::Rename { source, target } => {
                if cancellation.is_cancelled() {
                    cleanup(&transaction.cleanup_failure);
                    cancellation.complete();
                    return JobResult::Cancelled;
                }
                if let Err(error) = ensure_parent(target, &mut barriers) {
                    log_io_error("rename prepare", target, &error);
                    cleanup(&transaction.cleanup_failure);
                    cancellation.complete();
                    return JobResult::Failure;
                }
                if let Err(error) = std::fs::rename(source, target) {
                    log_io_error("rename", target, &error);
                    cleanup(&transaction.cleanup_failure);
                    cancellation.complete();
                    return JobResult::Failure;
                }
                barriers.record_parent(target);
                barriers.record_parent(source);
            }
        }
    }
    if cancellation.is_cancelled() {
        cleanup(&transaction.cleanup_failure);
        cancellation.complete();
        return JobResult::Cancelled;
    }
    // Only a published transaction flushes at its commit point; every other
    // transaction still owes its directory names a barrier before success.
    if let Err(failure) = barriers.flush() {
        log_io_error("directory barrier", &failure.directory, &failure.error);
        cleanup(&transaction.cleanup_failure);
        cancellation.complete();
        return JobResult::Failure;
    }
    cleanup(&transaction.cleanup_success);
    if transaction.published {
        cleanup_directories(&transaction.cleanup_directories);
    }
    cancellation.complete();
    JobResult::Success
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

fn sync_file(path: &Path) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.sync_all()
}

fn jobs() -> &'static Mutex<Jobs> {
    static JOBS: OnceLock<Mutex<Jobs>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(Jobs::new(LIMITS)))
}

fn parse_path(path: *const c_char) -> Option<PathBuf> {
    with_cstr_text(path, MAX_PATH_BYTES, |path| PathBuf::from(path))
}

fn with_cstr_text<R>(
    value: *const c_char,
    max_len: usize,
    callback: impl FnOnce(&str) -> R,
) -> Option<R> {
    if value.is_null() {
        return None;
    }
    let value = unsafe { CStr::from_ptr(value) }.to_str().ok()?;
    if value.is_empty() || value.len() > max_len {
        return None;
    }
    Some(callback(value))
}

fn is_plain_filename(filename: &str) -> bool {
    !filename.is_empty()
        && filename.len() <= MAX_PATH_BYTES
        && filename != "."
        && filename != ".."
        && !filename.contains('/')
        && !filename.contains('\\')
}

fn copy_bytes(data: *const c_char, len: usize, max_len: usize) -> Option<Vec<u8>> {
    if data.is_null() || len > max_len {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) }.to_vec())
}

fn copy_bytes_u8(data: *const u8, len: usize, max_len: usize) -> Option<Vec<u8>> {
    if data.is_null() || len > max_len {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(data, len) }.to_vec())
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
    if data.is_null() || len > MAX_PENDING_BYTES {
        return -1;
    }
    let mut job_queue = jobs().lock().unwrap();
    if !job_queue.reserve_single_submission(len) {
        return -1;
    }
    drop(job_queue);
    let Some(bytes) = copy_bytes(data, len, MAX_PENDING_BYTES) else {
        let mut job_queue = jobs().lock().unwrap();
        job_queue.release_reserved_single(len);
        return -1;
    };
    let mut job_queue = jobs().lock().unwrap();
    job_queue
        .enqueue_single_reserved(SingleRequest {
            path,
            bytes,
            append,
            source: None,
            second: None,
        })
        .unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn stone_file_rename_async(source: *const c_char, target: *const c_char) -> i32 {
    let Some(source) = parse_path(source) else {
        return -1;
    };
    let Some(target) = parse_path(target) else {
        return -1;
    };
    jobs()
        .lock()
        .unwrap()
        .enqueue_single(SingleRequest {
            path: target,
            bytes: Vec::new(),
            append: false,
            source: Some(source),
            second: None,
        })
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
    let Some(path) = parse_path(path) else {
        return -1;
    };
    let Some(second_path) = parse_path(second_path) else {
        return -1;
    };
    let Some(total) = len.checked_add(second_len) else {
        return -1;
    };
    if path == second_path || data.is_null() || second_data.is_null() || total > MAX_PENDING_BYTES {
        return -1;
    }
    let mut queue = jobs().lock().unwrap();
    if !queue.reserve_single_submission(total) {
        return -1;
    }
    drop(queue);
    let payload = copy_bytes(data, len, MAX_PENDING_BYTES).zip(copy_bytes(
        second_data,
        second_len,
        MAX_PENDING_BYTES,
    ));
    let mut queue = jobs().lock().unwrap();
    let Some((bytes, second_bytes)) = payload else {
        queue.release_reserved_single(total);
        return -1;
    };
    queue
        .enqueue_single_reserved(SingleRequest {
            path,
            bytes,
            append: false,
            source: None,
            second: Some((second_path, second_bytes)),
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
    let job_queue = jobs().lock().unwrap();
    if !job_queue.reserve_transaction_bytes(transaction, len) {
        return false;
    }
    drop(job_queue);
    let Some(bytes) = copy_bytes(data, len, MAX_TRANSACTION_FRAGMENT_BYTES) else {
        let job_queue = jobs().lock().unwrap();
        job_queue.release_bytes(len);
        return false;
    };
    let mut job_queue = jobs().lock().unwrap();
    job_queue.add_transaction_fragment_reserved(transaction, path, bytes, append, publish)
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
    if pixels.is_null() || width <= 0 || height <= 0 || width > 128 || height > 128 {
        return false;
    }
    let width = width as usize;
    let height = height as usize;
    let Some(length) = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return false;
    };
    let job_queue = jobs().lock().unwrap();
    if !job_queue.reserve_transaction_bytes(transaction, length) {
        return false;
    }
    drop(job_queue);
    let Some(pixels) = copy_bytes_u8(pixels, length, MAX_TRANSACTION_FRAGMENT_BYTES) else {
        let job_queue = jobs().lock().unwrap();
        job_queue.release_bytes(length);
        return false;
    };
    let mut job_queue = jobs().lock().unwrap();
    job_queue.add_transaction_png_reserved(transaction, path, pixels, width as u32, height as u32)
}

#[no_mangle]
pub extern "C" fn stone_file_transaction_sync(transaction: i32, path: *const c_char) -> bool {
    let Some(path) = parse_path(path) else {
        return false;
    };
    jobs()
        .lock()
        .unwrap()
        .add_transaction_sync(transaction, path)
}

#[no_mangle]
pub extern "C" fn stone_file_transaction_rename(
    transaction: i32,
    source: *const c_char,
    target: *const c_char,
) -> bool {
    let Some(source) = parse_path(source) else {
        return false;
    };
    let Some(target) = parse_path(target) else {
        return false;
    };
    jobs()
        .lock()
        .unwrap()
        .add_transaction_rename(transaction, source, target)
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
        .add_transaction_cleanup(transaction, path, on_success)
}

#[no_mangle]
pub extern "C" fn stone_file_transaction_cleanup_directory(
    transaction: i32,
    directory: *const c_char,
) -> bool {
    with_cstr_text(directory, MAX_PATH_BYTES, |directory| {
        let mut job_queue = jobs().lock().unwrap();
        let bytes = directory.len();
        if !job_queue.reserve_transaction_cleanup_directory(transaction, bytes) {
            return false;
        }
        job_queue.add_transaction_cleanup_directory_reserved(
            transaction,
            PathBuf::from(directory),
            bytes,
        )
    })
    .unwrap_or(false)
}

#[no_mangle]
pub extern "C" fn stone_file_transaction_keep(
    transaction: i32,
    directory: *const c_char,
    filename: *const c_char,
) -> bool {
    with_cstr_text(directory, MAX_PATH_BYTES, |directory| {
        with_cstr_text(filename, MAX_PATH_BYTES, |filename| {
            if !is_plain_filename(filename) {
                return false;
            }
            let mut job_queue = jobs().lock().unwrap();
            let bytes = filename.len();
            if !job_queue.reserve_transaction_keep(transaction, Path::new(directory), bytes) {
                return false;
            }
            job_queue.add_transaction_keep_reserved(
                transaction,
                Path::new(directory),
                filename.to_owned(),
                bytes,
            )
        })
        .unwrap_or(false)
    })
    .unwrap_or(false)
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
pub extern "C" fn stone_file_transactions_reset() -> i32 {
    jobs().lock().unwrap().reset_transactions() as i32
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
    if !file_jobs_test_enabled() {
        return false;
    }
    let control = file_jobs_test_control();
    let _guard = control.state.lock().unwrap();
    FILE_JOBS_TEST_ENTERED.store(0, Ordering::Release);
    FILE_JOBS_TEST_PAUSED.store(1, Ordering::Release);
    true
}

#[no_mangle]
pub extern "C" fn stone_file_jobs_test_pause_publication() -> bool {
    if !file_jobs_test_enabled() {
        return false;
    }
    let control = file_jobs_test_control();
    let _guard = control.state.lock().unwrap();
    FILE_JOBS_TEST_PUBLICATION_ENTERED.store(0, Ordering::Release);
    FILE_JOBS_TEST_PUBLICATION_PAUSED.store(1, Ordering::Release);
    true
}

#[no_mangle]
pub extern "C" fn stone_file_jobs_test_release() -> bool {
    if !file_jobs_test_enabled() {
        return false;
    }
    let control = file_jobs_test_control();
    let _guard = control.state.lock().unwrap();
    FILE_JOBS_TEST_PAUSED.store(0, Ordering::Release);
    FILE_JOBS_TEST_PUBLICATION_PAUSED.store(0, Ordering::Release);
    control.wake.notify_all();
    true
}

#[no_mangle]
pub extern "C" fn stone_file_jobs_test_entered() -> bool {
    file_jobs_test_enabled() && FILE_JOBS_TEST_ENTERED.load(Ordering::Acquire) != 0
}

#[no_mangle]
pub extern "C" fn stone_file_jobs_test_publication_entered() -> bool {
    file_jobs_test_enabled() && FILE_JOBS_TEST_PUBLICATION_ENTERED.load(Ordering::Acquire) != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::thread;
    use std::time::Duration;

    static TEST_SEQUENCE: AtomicU32 = AtomicU32::new(0);

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

    fn test_root() -> PathBuf {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "stone_file_jobs_{}_{}",
            std::process::id(),
            sequence
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn queued_write(queue: &Mutex<Jobs>, path: PathBuf, bytes: &[u8]) -> i32 {
        queue
            .lock()
            .unwrap()
            .enqueue_single(SingleRequest {
                path,
                bytes: bytes.to_vec(),
                append: false,
                source: None,
                second: None,
            })
            .unwrap()
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

    #[test]
    fn queued_writes_preserve_order_and_report_failures() {
        let root = test_root();
        let queue = Mutex::new(Jobs::new(limits(8, 64)));
        let path = root.join("nested/data");
        let first = queued_write(&queue, path.clone(), b"first");
        let second = queue
            .lock()
            .unwrap()
            .enqueue_single(SingleRequest {
                path: path.clone(),
                bytes: b"second".to_vec(),
                append: true,
                source: None,
                second: None,
            })
            .unwrap();
        assert_eq!(poll_queue(&queue, first, true), 1);
        assert_eq!(poll_queue(&queue, second, true), 1);
        assert_eq!(std::fs::read(&path).unwrap(), b"firstsecond");
        let invalid = path.join("child");
        let failed = queued_write(&queue, invalid, b"bad");
        assert_eq!(poll_queue(&queue, failed, true), -1);
        assert_eq!(std::fs::read(&path).unwrap(), b"firstsecond");
        assert_eq!(poll_queue(&queue, failed, false), -1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn waiting_poll_does_not_hold_jobs_mutex_and_submit_is_nonblocking() {
        let root = test_root();
        let gate = Arc::new(TestGate::new());
        let queue = Arc::new(Mutex::new(Jobs::new_with_gate(
            limits(4, 64),
            Arc::clone(&gate),
        )));
        let first = queued_write(&queue, root.join("first"), b"one");
        gate.wait_until_entered();
        let queue_for_poll = Arc::clone(&queue);
        let waiter = thread::spawn(move || poll_queue(&queue_for_poll, first, true));
        thread::sleep(Duration::from_millis(5));
        let second = queued_write(&queue, root.join("second"), b"two");
        assert!(second > 0, "submit must not wait for a blocked worker");
        assert_eq!(poll_queue(&queue, second, false), 0);
        gate.release();
        assert_eq!(waiter.join().unwrap(), 1);
        assert_eq!(poll_queue(&queue, second, true), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn paired_admission_counts_both_payloads() {
        let root = test_root();
        let queue = Mutex::new(Jobs::new(limits(2, 4)));
        let request = SingleRequest {
            path: root.join("first"),
            bytes: b"123".to_vec(),
            append: false,
            source: None,
            second: Some((root.join("second"), b"45".to_vec())),
        };
        assert!(queue.lock().unwrap().enqueue_single(request).is_none());
        assert_eq!(queue.lock().unwrap().accounting.load(Ordering::Acquire), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admission_bounds_jobs_and_owned_bytes() {
        let root = test_root();
        let gate = Arc::new(TestGate::new());
        let queue = Mutex::new(Jobs::new_with_gate(limits(1, 4), Arc::clone(&gate)));
        let first = queued_write(&queue, root.join("first"), b"1234");
        gate.wait_until_entered();
        assert!(queue
            .lock()
            .unwrap()
            .enqueue_single(SingleRequest {
                path: root.join("second"),
                bytes: b"x".to_vec(),
                append: false,
                source: None,
                second: None,
            })
            .is_none());
        gate.release();
        assert_eq!(poll_queue(&queue, first, true), 1);
        let second = queued_write(&queue, root.join("second"), b"x");
        assert_eq!(poll_queue(&queue, second, true), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn completed_job_releases_owned_bytes_before_observation() {
        let root = tempfile::tempdir().unwrap();
        let gate = Arc::new(TestGate::new());
        let queue = Mutex::new(Jobs::new_with_control(
            limits(1, 4),
            WorkerControl {
                completion_gate: Some(Arc::clone(&gate)),
                ..WorkerControl::default()
            },
        ));
        let first = queued_write(&queue, root.path().join("first"), b"1234");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let entered = {
                let mut state = gate.state.lock().unwrap();
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                loop {
                    if state.0 {
                        break true;
                    }
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        break false;
                    }
                    state = gate.ready.wait_timeout(state, remaining).unwrap().0;
                }
            };
            assert!(entered, "worker did not publish completion");
            assert_eq!(poll_queue(&queue, first, false), 1);
            assert_eq!(std::fs::read(root.path().join("first")).unwrap(), b"1234");
            // Successful completion must admit the next maximum-sized payload.
            assert!(
                queue
                    .lock()
                    .unwrap()
                    .enqueue_single(SingleRequest {
                        path: root.path().join("second"),
                        bytes: b"5678".to_vec(),
                        append: false,
                        source: None,
                        second: None,
                    })
                    .is_some(),
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
    fn transaction_writes_fragments_then_publishes_manifest() {
        let root = test_root();
        let queue = Mutex::new(Jobs::new(limits(4, 64)));
        let snapshot = root.join("snapshot.png");
        let manifest = root.join("manifest.json");
        let transaction_id = queue.lock().unwrap().begin_transaction().unwrap();
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            transaction_id,
            snapshot.clone(),
            b"snap".to_vec(),
            false,
            false
        ));
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            transaction_id,
            manifest.clone(),
            b"{\"a\":".to_vec(),
            false,
            true
        ));
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            transaction_id,
            manifest.clone(),
            b"1}".to_vec(),
            true,
            true
        ));
        let job = queue
            .lock()
            .unwrap()
            .commit_transaction(transaction_id)
            .unwrap();
        assert_eq!(poll_queue(&queue, job, true), 1);
        assert_eq!(std::fs::read(snapshot).unwrap(), b"snap");
        assert_eq!(std::fs::read(manifest).unwrap(), b"{\"a\":1}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn transaction_encodes_rgba8_png_on_worker() {
        let root = test_root();
        let queue = Mutex::new(Jobs::new(limits(4, 64)));
        let png = root.join("blood.png");
        let pixels = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let transaction_id = queue.lock().unwrap().begin_transaction().unwrap();
        assert!(queue
            .lock()
            .unwrap()
            .reserve_transaction_bytes(transaction_id, pixels.len()));
        assert!(queue.lock().unwrap().add_transaction_png_reserved(
            transaction_id,
            png.clone(),
            pixels,
            2,
            2
        ));
        let job = queue
            .lock()
            .unwrap()
            .commit_transaction(transaction_id)
            .unwrap();
        assert_eq!(poll_queue(&queue, job, true), 1);
        let decoded = image::load_from_memory(&std::fs::read(png).unwrap())
            .unwrap()
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (2, 2));
        assert_eq!(
            decoded.as_raw(),
            &[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    // The traced MAG compaction batch: 91 snapshot PNG publications across 12
    // existing directories plus the root publication that makes them reachable.
    fn compaction_limits() -> Limits {
        Limits {
            max_pending_jobs: 4,
            max_pending_bytes: 1024 * 1024,
            max_open_transactions: 4,
            max_transaction_ops: 256,
            max_transaction_cleanups: 16,
            max_transaction_cleanup_directories: 4,
            max_transaction_keep_entries: 16,
            max_transaction_cleanup_bytes: 64 * 1024,
            max_transaction_fragment_bytes: 64 * 1024,
            max_transaction_fragments: 256,
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

    // The published root names exactly what it reaches, so a test can check
    // reachability from the durable root instead of from the fixture.
    fn compaction_batch(
        queue: &Mutex<Jobs>,
        save: &Path,
        directories: &[PathBuf],
        generation: u8,
        publications: usize,
    ) -> (i32, PathBuf, Vec<PathBuf>) {
        let transaction = queue.lock().unwrap().begin_transaction().unwrap();
        let mut files = Vec::new();
        for index in 0..publications {
            let path = directories[index % directories.len()]
                .join(format!("chunk-{index:03}-gen-{generation}.png"));
            let pixels = vec![generation, index as u8, 0, 255];
            assert!(queue
                .lock()
                .unwrap()
                .reserve_transaction_bytes(transaction, pixels.len()));
            assert!(queue.lock().unwrap().add_transaction_png_reserved(
                transaction,
                path.clone(),
                pixels,
                1,
                1
            ));
            files.push(path);
        }
        let manifest = save.join("manifest.json");
        let listing = manifest_listing(save, &files);
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            transaction,
            manifest.clone(),
            listing.into_bytes(),
            false,
            true
        ));
        let job = queue
            .lock()
            .unwrap()
            .commit_transaction(transaction)
            .unwrap();
        (job, manifest, files)
    }

    fn assert_reachable_png(path: &Path, index: usize, generation: u8) {
        let decoded = image::load_from_memory(&std::fs::read(path).unwrap())
            .unwrap()
            .to_rgba8();
        assert_eq!(
            decoded.as_raw(),
            &[generation, index as u8, 0, 255],
            "{path:?}"
        );
    }

    #[test]
    fn compaction_batch_barriers_follow_changed_directories() {
        let root = test_root();
        let (save, directories) = compaction_fixture(&root);
        let log = Arc::new(DirectorySyncLog::default());
        let queue = Mutex::new(Jobs::new_with_directory_syncs(
            compaction_limits(),
            Arc::clone(&log),
        ));
        std::fs::write(save.join("manifest.json"), b"previous root").unwrap();
        let (job, manifest, files) = compaction_batch(&queue, &save, &directories, 1, 91);
        assert_eq!(poll_queue(&queue, job, true), 1);
        let synced = log.take();
        // One barrier per changed snapshot directory, plus one for the root name
        // after its rename. The fixture has 12 directories and 92 publications.
        assert_eq!(synced.len(), directories.len() + 1, "{synced:?}");
        assert!(synced.len() <= 24, "{} barriers", synced.len());
        for directory in &directories {
            assert_eq!(
                synced.iter().filter(|entry| *entry == directory).count(),
                1,
                "{directory:?} synced once per phase in {synced:?}",
            );
        }
        assert_eq!(
            synced.last().unwrap(),
            &save,
            "the root barrier follows its rename"
        );
        assert!(
            synced
                .iter()
                .all(|directory| directory == &save || directory.starts_with(&save)),
            "no unchanged ancestor is re-synced: {synced:?}",
        );
        assert_eq!(
            std::fs::read_to_string(&manifest).unwrap(),
            manifest_listing(&save, &files)
        );
        for (index, file) in files.iter().enumerate() {
            assert_reachable_png(file, index, 1);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn created_directory_barriers_stop_at_the_existing_parent() {
        let root = test_root();
        let save = root.join("slot");
        std::fs::create_dir_all(&save).unwrap();
        let log = Arc::new(DirectorySyncLog::default());
        let queue = Mutex::new(Jobs::new_with_directory_syncs(
            limits(4, 64 * 1024),
            Arc::clone(&log),
        ));
        let snapshots = save.join("snapshots");
        let path = snapshots.join("level-0").join("face-1").join("chunk.png");
        let manifest = save.join("manifest.json");
        let transaction = queue.lock().unwrap().begin_transaction().unwrap();
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            transaction,
            path.clone(),
            b"png".to_vec(),
            false,
            false
        ));
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            transaction,
            manifest.clone(),
            b"manifest".to_vec(),
            false,
            true
        ));
        let job = queue
            .lock()
            .unwrap()
            .commit_transaction(transaction)
            .unwrap();
        assert_eq!(poll_queue(&queue, job, true), 1);
        let face = snapshots.join("level-0").join("face-1");
        let level = snapshots.join("level-0");
        // Deepest first, then the created parents, then the root barrier: the
        // chain stops at the directory that already existed before this save.
        assert_eq!(
            log.take(),
            vec![face, level, snapshots.clone(), save.clone(), save.clone()],
        );
        assert!(path.exists());
        assert_eq!(std::fs::read(&manifest).unwrap(), b"manifest");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn injected_directory_barrier_failures_keep_a_reachable_root() {
        let measure_root = test_root();
        let (save, directories) = compaction_fixture(&measure_root);
        let log = Arc::new(DirectorySyncLog::default());
        let queue = Mutex::new(Jobs::new_with_directory_syncs(
            compaction_limits(),
            Arc::clone(&log),
        ));
        std::fs::write(save.join("manifest.json"), b"previous root").unwrap();
        let (job, _, _) = compaction_batch(&queue, &save, &directories, 1, 91);
        assert_eq!(poll_queue(&queue, job, true), 1);
        let cuts = log.take().len();
        assert_eq!(cuts, directories.len() + 1);
        let _ = std::fs::remove_dir_all(measure_root);

        for cut in 1..=cuts {
            let root = test_root();
            let (save, directories) = compaction_fixture(&root);
            let manifest = save.join("manifest.json");
            std::fs::write(&manifest, b"previous root").unwrap();
            let queue = Mutex::new(Jobs::new_with_directory_sync_failure(
                compaction_limits(),
                Arc::new(DirectorySyncFailure::at(cut)),
            ));
            // Barrier cuts are per directory, so one publication per directory
            // reaches every cut; the full 91-file batch is measured above.
            let (job, _, files) =
                compaction_batch(&queue, &save, &directories, 2, directories.len());
            match poll_queue(&queue, job, true) {
                -1 => assert_eq!(
                    std::fs::read(&manifest).unwrap(),
                    b"previous root",
                    "cut {cut} replaced the root before its commit barrier",
                ),
                -3 => {
                    assert_eq!(cut, cuts, "only the commit barrier is uncertain");
                    assert_eq!(
                        std::fs::read_to_string(&manifest).unwrap(),
                        manifest_listing(&save, &files),
                    );
                    for (index, file) in files.iter().enumerate() {
                        assert_reachable_png(file, index, 2);
                    }
                }
                other => panic!("cut {cut} reported {other}"),
            }
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn transaction_fragment_admission_bounds_retained_fragment_overhead() {
        let root = test_root();
        let queue = Mutex::new(Jobs::new(limits(4, 128 * 1024)));
        let transaction_id = queue.lock().unwrap().begin_transaction().unwrap();
        let chunk = vec![0u8; 4 * 1024];
        for index in 0..16 {
            assert!(queue.lock().unwrap().add_transaction_fragment_owned(
                transaction_id,
                root.join("snapshot"),
                chunk.clone(),
                index != 0,
                false,
            ));
        }
        assert!(!queue.lock().unwrap().add_transaction_fragment_owned(
            transaction_id,
            root.join("snapshot"),
            Vec::new(),
            true,
            false,
        ));
        assert!(!queue
            .lock()
            .unwrap()
            .reserve_transaction_bytes(transaction_id, 1));
        assert_eq!(
            queue
                .lock()
                .unwrap()
                .transactions
                .get(&transaction_id)
                .unwrap()
                .fragments,
            16,
        );
        assert!(queue.lock().unwrap().abort_transaction(transaction_id));
        assert_eq!(queue.lock().unwrap().accounting.load(Ordering::Acquire), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn durability_uncertain_result_has_distinct_failure_code() {
        assert_eq!(JobResult::Failure.code(), -1);
        assert_eq!(JobResult::DurabilityUncertain.code(), -3);
    }

    #[cfg(unix)]
    #[test]
    fn directory_barriers_handle_bare_and_nested_paths() {
        let mut bare = DirectoryBarriers::default();
        bare.record_parent(Path::new("file"));
        assert_eq!(bare.changed, vec![PathBuf::from(".")]);
        assert!(bare.flush().is_ok());
        let root = test_root();
        let path = root.join("new").join("nested").join("file");
        let mut nested = DirectoryBarriers::default();
        ensure_parent(&path, &mut nested).unwrap();
        nested.record_parent(&path);
        let root_nested = root.join("new").join("nested");
        assert!(nested.changed.contains(&root_nested));
        assert!(nested.flush().is_ok());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn successful_manifest_publish_prunes_orphans_but_keeps_referenced_files() {
        let root = test_root();
        let queue = Mutex::new(Jobs::new(limits(4, 64 * 1024)));
        let snapshots = root.join("snapshots");
        std::fs::create_dir_all(snapshots.join("nested")).unwrap();
        std::fs::write(snapshots.join("keep.png"), b"keep").unwrap();
        std::fs::write(snapshots.join("orphan.png"), b"orphan").unwrap();
        std::fs::write(snapshots.join("nested/child.png"), b"child").unwrap();
        let manifest = root.join("manifest.json");
        let transaction_id = queue.lock().unwrap().begin_transaction().unwrap();
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            transaction_id,
            manifest,
            b"new".to_vec(),
            false,
            true
        ));
        assert!(queue
            .lock()
            .unwrap()
            .add_transaction_cleanup_directory_owned(transaction_id, snapshots.clone()));
        assert!(queue.lock().unwrap().add_transaction_keep_owned(
            transaction_id,
            snapshots.clone(),
            "keep.png".to_owned()
        ));
        let job = queue
            .lock()
            .unwrap()
            .commit_transaction(transaction_id)
            .unwrap();
        assert_eq!(poll_queue(&queue, job, true), 1);
        assert!(snapshots.join("keep.png").exists());
        assert!(!snapshots.join("orphan.png").exists());
        assert!(snapshots.join("nested/child.png").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_manifest_publish_preserves_all_gc_files_and_old_manifest() {
        let root = test_root();
        let queue = Mutex::new(Jobs::new(limits(4, 64 * 1024)));
        let snapshots = root.join("snapshots");
        std::fs::create_dir_all(&snapshots).unwrap();
        std::fs::write(snapshots.join("keep.png"), b"keep").unwrap();
        std::fs::write(snapshots.join("orphan.png"), b"orphan").unwrap();
        let manifest = root.join("manifest.json");
        std::fs::write(&manifest, b"old").unwrap();
        let transaction_id = queue.lock().unwrap().begin_transaction().unwrap();
        assert!(queue.lock().unwrap().add_transaction_rename(
            transaction_id,
            root.join("missing"),
            root.join("unused")
        ));
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            transaction_id,
            manifest.clone(),
            b"new".to_vec(),
            false,
            true
        ));
        assert!(queue
            .lock()
            .unwrap()
            .add_transaction_cleanup_directory_owned(transaction_id, snapshots.clone()));
        assert!(queue.lock().unwrap().add_transaction_keep_owned(
            transaction_id,
            snapshots.clone(),
            "keep.png".to_owned()
        ));
        let job = queue
            .lock()
            .unwrap()
            .commit_transaction(transaction_id)
            .unwrap();
        assert_eq!(poll_queue(&queue, job, true), -1);
        assert_eq!(std::fs::read(&manifest).unwrap(), b"old");
        assert!(snapshots.join("keep.png").exists());
        assert!(snapshots.join("orphan.png").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn uncertain_manifest_publish_retains_dependencies_and_plain_write_recovers() {
        let root = test_root();
        let failure = Arc::new(AtomicBool::new(true));
        let queue = Mutex::new(Jobs::new_with_publish_sync_failure(
            limits(4, 64 * 1024),
            Arc::clone(&failure),
        ));
        let snapshots = root.join("snapshots");
        std::fs::create_dir_all(&snapshots).unwrap();
        let previous = snapshots.join("previous.png");
        let pixels = vec![255, 0, 0, 255];
        let encoded = encode_png(&pixels, 1, 1).unwrap();
        std::fs::write(&previous, &encoded).unwrap();
        let next = snapshots.join("next.png");
        let manifest = root.join("manifest.json");
        std::fs::write(&manifest, b"old").unwrap();

        let transaction_id = queue.lock().unwrap().begin_transaction().unwrap();
        assert!(queue
            .lock()
            .unwrap()
            .add_transaction_cleanup(transaction_id, next.clone(), false));
        assert!(queue
            .lock()
            .unwrap()
            .reserve_transaction_bytes(transaction_id, pixels.len()));
        assert!(queue.lock().unwrap().add_transaction_png_reserved(
            transaction_id,
            next.clone(),
            pixels,
            1,
            1
        ));
        assert!(queue.lock().unwrap().add_transaction_cleanup(
            transaction_id,
            previous.clone(),
            true
        ));
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            transaction_id,
            manifest.clone(),
            b"new".to_vec(),
            false,
            true
        ));
        let job = queue
            .lock()
            .unwrap()
            .commit_transaction(transaction_id)
            .unwrap();
        assert_eq!(poll_queue(&queue, job, true), -3);
        assert_eq!(std::fs::read(&manifest).unwrap(), b"new");
        let previous_pixels = image::load_from_memory(&std::fs::read(&previous).unwrap())
            .unwrap()
            .to_rgba8();
        assert_eq!(previous_pixels.as_raw(), &[255, 0, 0, 255]);
        let next_pixels = image::load_from_memory(&std::fs::read(&next).unwrap())
            .unwrap()
            .to_rgba8();
        assert_eq!(next_pixels.as_raw(), &[255, 0, 0, 255]);

        let recovery = queue.lock().unwrap().begin_transaction().unwrap();
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            recovery,
            manifest.clone(),
            b"recovered".to_vec(),
            false,
            true
        ));
        let recovery_job = queue.lock().unwrap().commit_transaction(recovery).unwrap();
        assert_eq!(poll_queue(&queue, recovery_job, true), 1);
        assert_eq!(std::fs::read(&manifest).unwrap(), b"recovered");
        assert!(previous.exists());
        assert!(next.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_keep_requires_plain_filenames_and_registered_directories() {
        let root = test_root();
        let queue = Mutex::new(Jobs::new(limits(4, 64 * 1024)));
        let snapshots = root.join("snapshots");
        let transaction_id = queue.lock().unwrap().begin_transaction().unwrap();
        assert!(!queue.lock().unwrap().add_transaction_keep_owned(
            transaction_id,
            snapshots.clone(),
            "keep.png".to_owned(),
        ));
        assert!(queue
            .lock()
            .unwrap()
            .add_transaction_cleanup_directory_owned(transaction_id, snapshots.clone()));
        assert!(!queue.lock().unwrap().add_transaction_keep_owned(
            transaction_id,
            snapshots.clone(),
            "nested/keep.png".to_owned(),
        ));
        assert!(!queue.lock().unwrap().add_transaction_keep_owned(
            transaction_id,
            snapshots.clone(),
            "nested\\keep.png".to_owned(),
        ));
        assert!(!queue.lock().unwrap().add_transaction_keep_owned(
            transaction_id,
            snapshots.clone(),
            "..".to_owned(),
        ));
        assert!(queue.lock().unwrap().add_transaction_keep_owned(
            transaction_id,
            snapshots,
            "keep.png".to_owned(),
        ));
        assert!(queue.lock().unwrap().abort_transaction(transaction_id));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_admission_bounds_directories_items_and_metadata_bytes() {
        let root = test_root();
        let snapshots = root.join("snapshots");
        let cleanup_bytes = snapshots.as_os_str().len() + 1;
        let queue = Mutex::new(Jobs::new(Limits {
            max_pending_jobs: 4,
            max_pending_bytes: 64 * 1024,
            max_open_transactions: 4,
            max_transaction_ops: 16,
            max_transaction_cleanups: 16,
            max_transaction_cleanup_directories: 1,
            max_transaction_keep_entries: 1,
            max_transaction_cleanup_bytes: cleanup_bytes,
            max_transaction_fragment_bytes: 64 * 1024,
            max_transaction_fragments: 16,
        }));
        let transaction_id = queue.lock().unwrap().begin_transaction().unwrap();
        assert!(queue
            .lock()
            .unwrap()
            .add_transaction_cleanup_directory_owned(transaction_id, snapshots.clone(),));
        assert!(!queue
            .lock()
            .unwrap()
            .add_transaction_cleanup_directory_owned(transaction_id, root.join("other"),));
        assert!(queue.lock().unwrap().add_transaction_keep_owned(
            transaction_id,
            snapshots.clone(),
            "x".to_owned(),
        ));
        assert!(!queue.lock().unwrap().add_transaction_keep_owned(
            transaction_id,
            snapshots,
            "y".to_owned(),
        ));
        assert!(queue.lock().unwrap().abort_transaction(transaction_id));
        assert_eq!(queue.lock().unwrap().accounting.load(Ordering::Acquire), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn transaction_cleanup_runs_for_success_and_failure() {
        let root = test_root();
        let queue = Mutex::new(Jobs::new(limits(4, 64)));
        let success_cleanup = root.join("success.tmp");
        std::fs::write(&success_cleanup, b"stale").unwrap();
        let success_id = queue.lock().unwrap().begin_transaction().unwrap();
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            success_id,
            root.join("success"),
            b"ok".to_vec(),
            false,
            false
        ));
        assert!(queue.lock().unwrap().add_transaction_cleanup(
            success_id,
            success_cleanup.clone(),
            true
        ));
        let success_job = queue
            .lock()
            .unwrap()
            .commit_transaction(success_id)
            .unwrap();
        assert_eq!(poll_queue(&queue, success_job, true), 1);
        assert!(!success_cleanup.exists());

        let failure_cleanup = root.join("failure.tmp");
        std::fs::write(&failure_cleanup, b"stale").unwrap();
        let failure_id = queue.lock().unwrap().begin_transaction().unwrap();
        assert!(queue.lock().unwrap().add_transaction_rename(
            failure_id,
            root.join("missing"),
            root.join("unused")
        ));
        assert!(queue.lock().unwrap().add_transaction_cleanup(
            failure_id,
            failure_cleanup.clone(),
            false
        ));
        let failure_job = queue
            .lock()
            .unwrap()
            .commit_transaction(failure_id)
            .unwrap();
        assert_eq!(poll_queue(&queue, failure_job, true), -1);
        assert!(!failure_cleanup.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_transaction_does_not_replace_existing_manifest() {
        let root = test_root();
        let queue = Mutex::new(Jobs::new(limits(4, 64)));
        let manifest = root.join("manifest.json");
        std::fs::write(&manifest, b"old").unwrap();
        let transaction_id = queue.lock().unwrap().begin_transaction().unwrap();
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            transaction_id,
            root.join("snapshot.png"),
            b"snapshot".to_vec(),
            false,
            false
        ));
        assert!(queue.lock().unwrap().add_transaction_rename(
            transaction_id,
            root.join("missing"),
            root.join("unused")
        ));
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            transaction_id,
            manifest.clone(),
            b"new".to_vec(),
            false,
            true
        ));
        let job = queue
            .lock()
            .unwrap()
            .commit_transaction(transaction_id)
            .unwrap();
        assert_eq!(poll_queue(&queue, job, true), -1);
        assert_eq!(std::fs::read(manifest).unwrap(), b"old");
        assert_eq!(
            std::fs::read(root.join("snapshot.png")).unwrap(),
            b"snapshot"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cancellation_before_publication_never_replaces_manifest() {
        let root = test_root();
        let gate = Arc::new(TestGate::new());
        let queue = Mutex::new(Jobs::new_with_gate(limits(4, 64 * 1024), Arc::clone(&gate)));
        let manifest = root.join("manifest.json");
        let snapshots = root.join("snapshots");
        std::fs::create_dir_all(&snapshots).unwrap();
        std::fs::write(snapshots.join("keep.png"), b"keep").unwrap();
        std::fs::write(snapshots.join("orphan.png"), b"orphan").unwrap();
        std::fs::write(&manifest, b"old").unwrap();
        let transaction_id = queue.lock().unwrap().begin_transaction().unwrap();
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            transaction_id,
            manifest.clone(),
            b"new".to_vec(),
            false,
            true
        ));
        assert!(queue
            .lock()
            .unwrap()
            .add_transaction_cleanup_directory_owned(transaction_id, snapshots.clone()));
        assert!(queue.lock().unwrap().add_transaction_keep_owned(
            transaction_id,
            snapshots.clone(),
            "keep.png".to_owned()
        ));
        let job = queue
            .lock()
            .unwrap()
            .commit_transaction(transaction_id)
            .unwrap();
        gate.wait_until_entered();
        assert_eq!(
            queue
                .lock()
                .unwrap()
                .pending
                .get(&job)
                .unwrap()
                .cancellation
                .as_ref()
                .unwrap()
                .cancel(),
            1
        );
        gate.release();
        assert_eq!(poll_queue(&queue, job, true), -2);
        assert_eq!(std::fs::read(manifest).unwrap(), b"old");
        assert!(snapshots.join("keep.png").exists());
        assert!(snapshots.join("orphan.png").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cancellation_reports_too_late_after_publication_claim() {
        let cancellation = Cancellation::new();
        assert!(cancellation.claim_publication());
        assert_eq!(cancellation.cancel(), 0);
    }

    #[test]
    fn cancellation_after_publication_claim_still_publishes_manifest() {
        let root = test_root();
        let gate = Arc::new(TestGate::new());
        let queue = Mutex::new(Jobs::new_with_publication_gate(
            limits(4, 64 * 1024),
            Arc::clone(&gate),
        ));
        let snapshots = root.join("snapshots");
        std::fs::create_dir_all(&snapshots).unwrap();
        std::fs::write(snapshots.join("keep.png"), b"keep").unwrap();
        std::fs::write(snapshots.join("orphan.png"), b"orphan").unwrap();
        let manifest = root.join("manifest.json");
        std::fs::write(&manifest, b"old").unwrap();
        let transaction_id = queue.lock().unwrap().begin_transaction().unwrap();
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            transaction_id,
            manifest.clone(),
            b"new".to_vec(),
            false,
            true
        ));
        assert!(queue
            .lock()
            .unwrap()
            .add_transaction_cleanup_directory_owned(transaction_id, snapshots.clone()));
        assert!(queue.lock().unwrap().add_transaction_keep_owned(
            transaction_id,
            snapshots.clone(),
            "keep.png".to_owned()
        ));
        let job = queue
            .lock()
            .unwrap()
            .commit_transaction(transaction_id)
            .unwrap();
        gate.wait_until_entered();
        let cancellation = queue
            .lock()
            .unwrap()
            .pending
            .get(&job)
            .unwrap()
            .cancellation
            .as_ref()
            .unwrap()
            .clone();
        assert_eq!(cancellation.cancel(), 0);
        gate.release();
        assert_eq!(poll_queue(&queue, job, true), 1);
        assert_eq!(std::fs::read(&manifest).unwrap(), b"new");
        assert!(snapshots.join("keep.png").exists());
        assert!(!snapshots.join("orphan.png").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn copied_payload_is_independent_of_caller_buffer() {
        let root = test_root();
        let gate = Arc::new(TestGate::new());
        let queue = Mutex::new(Jobs::new_with_gate(limits(4, 64), Arc::clone(&gate)));
        let path = root.join("payload");
        let mut caller_bytes = b"before".to_vec();
        let copied = copy_bytes(caller_bytes.as_ptr().cast(), caller_bytes.len(), 64).unwrap();
        let job = queue
            .lock()
            .unwrap()
            .enqueue_single(SingleRequest {
                path: path.clone(),
                bytes: copied,
                append: false,
                source: None,
                second: None,
            })
            .unwrap();
        caller_bytes.copy_from_slice(b"after!");
        gate.wait_until_entered();
        gate.release();
        assert_eq!(poll_queue(&queue, job, true), 1);
        assert_eq!(std::fs::read(path).unwrap(), b"before");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn transaction_reset_detaches_transactions_and_preserves_legacy_jobs() {
        let root = test_root();
        let gate = Arc::new(TestGate::new());
        let mut test_limits = limits(2, 64);
        test_limits.max_open_transactions = 2;
        let queue = Mutex::new(Jobs::new_with_gate(test_limits, Arc::clone(&gate)));
        let open_transaction = queue.lock().unwrap().begin_transaction().unwrap();
        let builder_bytes = b"builder".to_vec();
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            open_transaction,
            root.join("builder"),
            builder_bytes.clone(),
            false,
            false,
        ));
        let committed_transaction = queue.lock().unwrap().begin_transaction().unwrap();
        let transaction_bytes = b"transaction".to_vec();
        let transaction_path = root.join("transaction");
        assert!(queue.lock().unwrap().add_transaction_fragment_owned(
            committed_transaction,
            transaction_path.clone(),
            transaction_bytes.clone(),
            false,
            false,
        ));
        let job = queue
            .lock()
            .unwrap()
            .commit_transaction(committed_transaction)
            .unwrap();
        gate.wait_until_entered();
        let legacy_path = root.join("legacy");
        let legacy_bytes = b"legacy";
        let legacy_job = queued_write(&queue, legacy_path.clone(), legacy_bytes);
        let reset_count = queue.lock().unwrap().reset_transactions();
        assert_eq!(reset_count, 2);
        assert!(queue.lock().unwrap().transactions.is_empty());
        assert!(!queue.lock().unwrap().pending.contains_key(&job));
        assert!(queue.lock().unwrap().pending.contains_key(&legacy_job));
        assert_eq!(poll_queue(&queue, job, false), -1);
        assert_eq!(
            queue.lock().unwrap().accounting.load(Ordering::Acquire),
            transaction_bytes.len() + legacy_bytes.len(),
        );
        let replacement = queue.lock().unwrap().begin_transaction().unwrap();
        assert!(queue.lock().unwrap().abort_transaction(replacement));
        gate.release();
        assert_eq!(poll_queue(&queue, legacy_job, true), 1);
        assert_eq!(queue.lock().unwrap().accounting.load(Ordering::Acquire), 0);
        assert!(!transaction_path.exists());
        assert!(!root.join("builder").exists());
        assert_eq!(std::fs::read(legacy_path).unwrap(), legacy_bytes);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod pair_tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn paired_write_restores_both_after_publish_sync_failure() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("level");
        let second = root.path().join("quest");
        std::fs::write(&first, b"old level").unwrap();
        std::fs::write(&second, b"old quest").unwrap();
        let request = SingleRequest {
            path: first.clone(),
            bytes: b"new level".to_vec(),
            append: false,
            source: None,
            second: Some((second.clone(), b"new quest".to_vec())),
        };
        assert!(write_pair(&request, 1, true, &mut DirectoryBarriers::default()).is_err());
        assert_eq!(std::fs::read(&first).unwrap(), b"old level");
        assert_eq!(std::fs::read(&second).unwrap(), b"old quest");
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
        let failed = stone_file_write_pair_async(
            first.as_ptr(),
            c"edit".as_ptr(),
            4,
            bad.as_ptr(),
            c"quest".as_ptr(),
            5,
        );
        assert_eq!(stone_file_write_poll(failed, true), -1);
        assert_eq!(std::fs::read(&first_path).unwrap(), b"original");
        std::fs::remove_file(&first_path).unwrap();
        let absent = stone_file_write_pair_async(
            first.as_ptr(),
            c"edit".as_ptr(),
            4,
            bad.as_ptr(),
            c"quest".as_ptr(),
            5,
        );
        assert_eq!(stone_file_write_poll(absent, true), -1);
        assert!(!first_path.exists());
        let second_path = root.path().join("quest");
        let second = CString::new(second_path.to_str().unwrap()).unwrap();
        let ok = stone_file_write_pair_async(
            first.as_ptr(),
            c"edit".as_ptr(),
            4,
            second.as_ptr(),
            c"quest".as_ptr(),
            5,
        );
        assert_eq!(stone_file_write_poll(ok, true), 1);
        assert_eq!(std::fs::read(first_path).unwrap(), b"edit");
        assert_eq!(std::fs::read(second_path).unwrap(), b"quest");
    }
}
