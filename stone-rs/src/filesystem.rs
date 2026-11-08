use fs_err as fs;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ffi::{c_char, c_int, CStr, CString};
use std::io::Write;
use std::path::Path;
use std::ptr;
use std::time::UNIX_EPOCH;

thread_local! {
    static WORKING_DIR: RefCell<Option<CString>> = const { RefCell::new(None) };
    static EXECUTABLE_PATH: RefCell<Option<CString>> = const { RefCell::new(None) };
    static HOSTNAME: RefCell<Option<CString>> = const { RefCell::new(None) };
    static PREF_PATH: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Decode a path argument. A null pointer and invalid UTF-8 are the same
/// answer here -- neither names a file -- so each entry point states only the
/// sentinel it reports, not the two ways of getting there.
///
/// # Safety
/// `p` is null or points at a NUL-terminated string that outlives `'a`.
unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok()
}

#[no_mangle]
pub extern "C" fn stone_get_working_directory() -> *const c_char {
    WORKING_DIR.with(|wd| {
        let mut wd_ref = wd.borrow_mut();
        if wd_ref.is_none() {
            if let Ok(cwd) = std::env::current_dir() {
                if let Some(s) = cwd.to_str() {
                    if let Ok(cs) = CString::new(s) {
                        *wd_ref = Some(cs);
                    }
                }
            }
            if wd_ref.is_none() {
                *wd_ref = Some(CString::new(".").unwrap());
            }
        }
        wd_ref.as_ref().map(|s| s.as_ptr()).unwrap_or(c".".as_ptr())
    })
}

#[no_mangle]
pub extern "C" fn stone_get_executable_path() -> *const c_char {
    EXECUTABLE_PATH.with(|ep| {
        let mut ep_ref = ep.borrow_mut();
        if ep_ref.is_none() {
            if let Ok(exe) = std::env::current_exe() {
                if let Some(parent) = exe.parent() {
                    if let Some(s) = parent.to_str() {
                        if let Ok(cs) = CString::new(s) {
                            *ep_ref = Some(cs);
                        }
                    }
                }
            }
            if ep_ref.is_none() {
                *ep_ref = Some(CString::new(".").unwrap());
            }
        }
        ep_ref.as_ref().map(|s| s.as_ptr()).unwrap_or(c".".as_ptr())
    })
}

#[no_mangle]
pub extern "C" fn stone_get_pref_path(
    organization: *const c_char,
    application: *const c_char,
) -> *const c_char {
    if organization.is_null() || application.is_null() {
        return ptr::null();
    }
    let Ok(organization) = unsafe { CStr::from_ptr(organization) }.to_str() else {
        return ptr::null();
    };
    let Ok(application) = unsafe { CStr::from_ptr(application) }.to_str() else {
        return ptr::null();
    };
    let Ok(path) = sdl2::filesystem::pref_path(organization, application) else {
        return ptr::null();
    };
    let Ok(path) = CString::new(path) else {
        return ptr::null();
    };

    PREF_PATH.with(|slot| {
        *slot.borrow_mut() = Some(path);
        slot.borrow()
            .as_ref()
            .map_or(ptr::null(), |path| path.as_ptr())
    })
}

/// Get file modification time as Unix timestamp (seconds since epoch)
/// Returns -1.0 on error
#[no_mangle]
pub extern "C" fn stone_file_modtime(path: *const c_char) -> f64 {
    let Some(path_str) = (unsafe { cstr(path) }) else {
        return -1.0;
    };

    // Embedded assets (fire release mode) have no on-disk mtime. Return a
    // fixed positive sentinel so file_modtime().unwrap() succeeds and the
    // hot-reload watcher never fires on them.
    if crate::asset_registry::contains_asset(path_str) {
        return 1.0;
    }

    match fs::metadata(path_str) {
        Ok(meta) => match meta.modified() {
            Ok(time) => match time.duration_since(UNIX_EPOCH) {
                Ok(duration) => duration.as_secs_f64(),
                Err(_) => -1.0,
            },
            Err(_) => -1.0,
        },
        Err(_) => -1.0,
    }
}

/// Write data to a file (creates parent directories if needed)
/// Returns true on success
#[no_mangle]
pub extern "C" fn stone_file_write(path: *const c_char, data: *const c_char, len: usize) -> bool {
    if path.is_null() || data.is_null() {
        return false;
    }
    let Some(path_str) = (unsafe { cstr(path) }) else {
        return false;
    };

    // Create parent directories if needed
    if let Some(parent) = Path::new(path_str).parent() {
        if !parent.as_os_str().is_empty() && fs::create_dir_all(parent).is_err() {
            // Ignore error - directory might already exist
        }
    }

    // Write the data
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, len) };
    match fs::File::create(path_str) {
        Ok(mut file) => file.write_all(bytes).is_ok(),
        Err(_) => false,
    }
}

/// Append data to a file, creating it (and parent dirs) if absent. The single
/// write is atomic w.r.t. the append offset (O_APPEND), so a concurrent or
/// interrupted writer never interleaves bytes into another record. Returns
/// true on success.
#[no_mangle]
pub extern "C" fn stone_file_append(path: *const c_char, data: *const c_char, len: usize) -> bool {
    if path.is_null() || data.is_null() {
        return false;
    }
    let Some(path_str) = (unsafe { cstr(path) }) else {
        return false;
    };

    if let Some(parent) = Path::new(path_str).parent() {
        if !parent.as_os_str().is_empty() && fs::create_dir_all(parent).is_err() {
            // Ignore error - directory might already exist
        }
    }

    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, len) };
    match fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path_str)
    {
        Ok(mut file) => file.write_all(bytes).is_ok(),
        Err(_) => false,
    }
}

/// Delete a single file. Returns true on success, false if the path is
/// missing or points at a directory.
#[no_mangle]
pub extern "C" fn stone_remove_file(path: *const c_char) -> bool {
    let Some(path_str) = (unsafe { cstr(path) }) else {
        return false;
    };
    fs::remove_file(path_str).is_ok()
}

/// Delete a directory and everything under it. Returns true on success.
#[no_mangle]
pub extern "C" fn stone_remove_dir(path: *const c_char) -> bool {
    let Some(path_str) = (unsafe { cstr(path) }) else {
        return false;
    };
    fs::remove_dir_all(path_str).is_ok()
}

/// Flush a file's data + metadata to stable storage (fsync). Opens the path
/// for writing (no truncate) so the flush is portable to Windows, which
/// requires write access for FlushFileBuffers. Returns true on success.
#[no_mangle]
pub extern "C" fn stone_file_sync(path: *const c_char) -> bool {
    let Some(path_str) = (unsafe { cstr(path) }) else {
        return false;
    };
    sync_file(path_str)
}

fn sync_file(path: &str) -> bool {
    match fs::OpenOptions::new().write(true).open(path) {
        Ok(file) => file.sync_all().is_ok(),
        Err(_) => false,
    }
}

fn sync_paths_with(paths: &[String], sync: impl Fn(&str) -> bool + Sync) -> bool {
    use rayon::prelude::*;
    // Join every operation before the caller can remove an aborted transaction.
    paths
        .par_iter()
        .map(|path| sync(path))
        .collect::<Vec<_>>()
        .into_iter()
        .all(|ok| ok)
}

#[no_mangle]
pub extern "C" fn stone_files_sync(paths_json: *const c_char) -> bool {
    let Some(input) = (unsafe { cstr(paths_json) }) else {
        return false;
    };
    let Ok(paths) = serde_json::from_str::<Vec<String>>(input) else {
        return false;
    };
    sync_paths_with(&paths, sync_file)
}

/// Read entire file into a malloc'd buffer.
/// Checks the in-memory asset registry first, falls back to disk.
/// Caller must free the returned buffer with libc::free.
/// Returns NULL on failure, sets out_len to bytes read on success.
#[no_mangle]
pub extern "C" fn stone_file_read(path: *const c_char, out_len: *mut usize) -> *mut c_char {
    let Some(path_str) = (unsafe { cstr(path) }) else {
        return ptr::null_mut();
    };

    // Override dir, then embedded registry, then disk.
    let contents = match crate::asset_registry::resolve_asset(path_str) {
        Some(data) => data,
        None => return ptr::null_mut(),
    };

    let len = contents.len();
    let buffer = unsafe { libc::malloc(len + 1) as *mut c_char };
    if buffer.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(contents.as_ptr(), buffer as *mut u8, len);
        *buffer.add(len) = 0;
        if !out_len.is_null() {
            *out_len = len;
        }
    }
    buffer
}

/// Free a buffer returned by stone_file_read. Frees through Rust's libc so the
/// alloc and free use the same CRT -- required on Windows, where ffi.C.free
/// can't resolve the exe's CRT free (cross-CRT free is UB regardless).
#[no_mangle]
pub extern "C" fn stone_free(ptr: *mut std::ffi::c_void) {
    if !ptr.is_null() {
        unsafe { libc::free(ptr) };
    }
}

/// Machine hostname (trimmed), cached. Returns "" on failure.
/// The returned pointer is owned by Stone -- do not free.
#[no_mangle]
pub extern "C" fn stone_get_hostname() -> *const c_char {
    HOSTNAME.with(|h| {
        let mut h_ref = h.borrow_mut();
        if h_ref.is_none() {
            *h_ref = Some(CString::new(read_hostname()).unwrap_or_default());
        }
        h_ref.as_ref().map(|s| s.as_ptr()).unwrap_or(c"".as_ptr())
    })
}

// libc::gethostname is POSIX-only; on Windows the hostname is COMPUTERNAME.
#[cfg(unix)]
fn read_hostname() -> String {
    let mut buf = [0 as c_char; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) } == 0 {
        buf[buf.len() - 1] = 0; // gethostname may not NUL-terminate on truncation
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_str()
            .unwrap_or("")
            .trim()
            .to_string()
    } else {
        String::new()
    }
}

#[cfg(windows)]
fn read_hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

/// Probe a file or directory: -1 is an inspection error, 0 is absent and 1
/// is present. Registry-aware so it agrees with stone_file_read: an embedded
/// asset must report as existing even with no loose file on disk.
#[no_mangle]
pub extern "C" fn stone_file_exists_status(path: *const c_char) -> i32 {
    let Some(path_str) = (unsafe { cstr(path) }) else {
        return -1;
    };
    if crate::asset_registry::contains_asset(path_str) {
        return 1;
    }
    match path_exists(Path::new(path_str)) {
        Ok(exists) => i32::from(exists),
        Err(error) => {
            ::log::error!(
                target: "FILESYSTEM",
                "failed to inspect path {path_str}: {error}"
            );
            -1
        }
    }
}

fn path_exists(path: &Path) -> Result<bool, std::io::Error> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) if is_not_a_directory(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

/// A path routed through a non-directory cannot name anything: ENOTDIR is
/// definitive absence, unlike ELOOP or EACCES, which stay errors.
#[cfg(unix)]
fn is_not_a_directory(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENOTDIR)
}

#[cfg(not(unix))]
fn is_not_a_directory(_error: &std::io::Error) -> bool {
    // Windows reports a missing path component as NotFound already.
    false
}

/// Probe whether a path is a directory: -1 is an inspection error, 0 is not a
/// directory and 1 is a directory. Embedded directories count as present.
#[no_mangle]
pub extern "C" fn stone_is_directory_status(path: *const c_char) -> i32 {
    let Some(path_str) = (unsafe { cstr(path) }) else {
        return -1;
    };
    if crate::asset_registry::contains_directory(path_str) {
        return 1;
    }
    match path_is_directory(Path::new(path_str)) {
        Ok(is_directory) => i32::from(is_directory),
        Err(error) => {
            ::log::error!(
                target: "FILESYSTEM",
                "failed to inspect directory path {path_str}: {error}"
            );
            -1
        }
    }
}

fn path_is_directory(path: &Path) -> Result<bool, std::io::Error> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) if is_not_a_directory(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

/// File size in bytes. Returns -1 on error / nonexistent.
#[no_mangle]
pub extern "C" fn stone_file_size(path: *const c_char) -> i64 {
    let Some(path_str) = (unsafe { cstr(path) }) else {
        return -1;
    };
    if let Some(size) = crate::asset_registry::asset_size(path_str) {
        return size;
    }
    std::fs::metadata(path_str)
        .map(|m| m.len() as i64)
        .unwrap_or(-1)
}

/// Create a directory (creates all parent directories as needed)
/// Returns true on success
#[no_mangle]
pub extern "C" fn stone_create_directory(path: *const c_char) -> bool {
    let Some(path_str) = (unsafe { cstr(path) }) else {
        return false;
    };
    fs::create_dir_all(path_str).is_ok()
}

/// Copy a single file. Contents always; metadata best-effort
/// (std::fs::copy semantics). Returns false on any error.
#[no_mangle]
pub extern "C" fn stone_copy_file(src: *const c_char, dst: *const c_char) -> bool {
    let (Some(src_str), Some(dst_str)) = (unsafe { (cstr(src), cstr(dst)) }) else {
        return false;
    };
    fs::copy(src_str, dst_str).is_ok()
}

/// Recursively copy `src` (file or directory tree) onto `dst`.
/// Symlinks are copied via `fs::copy` (their target, for files) and
/// never recursed into -- a directory symlink pointing at an ancestor
/// would otherwise loop forever. `symlink_metadata` does not follow
/// the link, so the `is_dir()` branch only fires for real dirs.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    let file_type = fs::symlink_metadata(src)?.file_type();
    if file_type.is_dir() {
        fs::create_dir_all(dst)?;
        // A single un-copyable entry (e.g. a directory symlink, which
        // fs::copy cannot dereference) must not abort copying its
        // siblings. Attempt every entry, then surface the first failure
        // -- this honors the documented "partial tree left as-is,
        // returns false on any failure" contract instead of bailing
        // mid-directory in readdir order.
        let mut first_err: Option<std::io::Error> = None;
        for entry in fs::read_dir(src)? {
            let name = entry?.file_name();
            if let Err(e) = copy_tree(&src.join(&name), &dst.join(&name)) {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        if file_type.is_symlink() {
            // Recreate the link itself; following it could cycle (and
            // fs::copy on a dir-symlink errors outright).
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
            #[cfg(not(unix))]
            fs::copy(src, dst)?;
        } else {
            fs::copy(src, dst)?;
        }
        Ok(())
    }
}

/// Recursively copy `src` (file or whole directory tree) to `dst`.
/// `dst` must not already exist (caller's overwrite guard). Symlinks
/// are copied shallowly, never recursed into (cycle-safe -- see
/// copy_tree). Returns false on any failure (partial tree left as-is).
#[no_mangle]
pub extern "C" fn stone_copy_recursive(src: *const c_char, dst: *const c_char) -> bool {
    let (Some(src_str), Some(dst_str)) = (unsafe { (cstr(src), cstr(dst)) }) else {
        return false;
    };
    copy_tree(Path::new(src_str), Path::new(dst_str)).is_ok()
}

/// Move/rename a path (file or directory tree). Tries an atomic
/// same-filesystem rename first; on a cross-device failure (EXDEV)
/// falls back to a recursive copy + delete so moves across mount
/// points still work. Returns false if both the rename and the
/// fallback fail. A failed copy leaves the source intact (we never
/// delete a source we couldn't fully copy).
#[no_mangle]
pub extern "C" fn stone_rename(src: *const c_char, dst: *const c_char) -> bool {
    let (Some(src_str), Some(dst_str)) = (unsafe { (cstr(src), cstr(dst)) }) else {
        return false;
    };
    if fs::rename(src_str, dst_str).is_ok() {
        return true;
    }
    let src_p = Path::new(src_str);
    let dst_p = Path::new(dst_str);
    if copy_tree(src_p, dst_p).is_err() {
        return false;
    }
    let removed = if src_p.is_dir() {
        fs::remove_dir_all(src_p)
    } else {
        fs::remove_file(src_p)
    };
    removed.is_ok()
}

/// List all files in a directory (excluding . and ..)
/// Returns malloc'd array of malloc'd strings
/// Caller must free with stone_free_string_array()
/// Returns true on success, false on error
#[no_mangle]
pub extern "C" fn stone_list_directory(
    path: *const c_char,
    out_files: *mut *mut *mut c_char,
    out_count: *mut c_int,
) -> bool {
    if path.is_null() || out_files.is_null() || out_count.is_null() {
        return false;
    }
    let Some(path_str) = (unsafe { cstr(path) }) else {
        return false;
    };

    let mut entries: BTreeSet<String> = crate::asset_registry::directory_entries(path_str)
        .into_iter()
        .collect();
    match fs::read_dir(path_str) {
        Ok(dir) => {
            for entry in dir.filter_map(Result::ok) {
                let Some(name) = entry.file_name().into_string().ok() else {
                    continue;
                };
                entries.insert(name);
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !entries.is_empty() => {}
        Err(_) => return false,
    }
    let entries: Vec<_> = entries.into_iter().collect();

    let count = entries.len();
    if count == 0 {
        unsafe {
            *out_files = ptr::null_mut();
            *out_count = 0;
        }
        return true;
    }

    // Allocate array of pointers
    let arr =
        unsafe { libc::malloc(count * std::mem::size_of::<*mut c_char>()) as *mut *mut c_char };
    if arr.is_null() {
        return false;
    }

    // Allocate and copy each string
    for (i, name) in entries.iter().enumerate() {
        match CString::new(name.as_str()) {
            Ok(cstr) => {
                let bytes = cstr.as_bytes_with_nul();
                let str_ptr = unsafe { libc::malloc(bytes.len()) as *mut c_char };
                if str_ptr.is_null() {
                    // Free previously allocated strings
                    for j in 0..i {
                        unsafe { libc::free(*arr.add(j) as *mut std::ffi::c_void) };
                    }
                    unsafe { libc::free(arr as *mut std::ffi::c_void) };
                    return false;
                }
                unsafe {
                    ptr::copy_nonoverlapping(bytes.as_ptr(), str_ptr as *mut u8, bytes.len());
                    *arr.add(i) = str_ptr;
                }
            }
            Err(_) => {
                // Free previously allocated strings
                for j in 0..i {
                    unsafe { libc::free(*arr.add(j) as *mut std::ffi::c_void) };
                }
                unsafe { libc::free(arr as *mut std::ffi::c_void) };
                return false;
            }
        }
    }

    unsafe {
        *out_files = arr;
        *out_count = count as c_int;
    }
    true
}

/// Free a string array returned by stone_list_directory
#[no_mangle]
pub extern "C" fn stone_free_string_array(arr: *mut *mut c_char, count: c_int) {
    if arr.is_null() || count <= 0 {
        return;
    }
    unsafe {
        for i in 0..count as usize {
            let str_ptr = *arr.add(i);
            if !str_ptr.is_null() {
                libc::free(str_ptr as *mut std::ffi::c_void);
            }
        }
        libc::free(arr as *mut std::ffi::c_void);
    }
}

#[cfg(test)]
mod xfer_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static UNIQ: AtomicU32 = AtomicU32::new(0);

    fn fresh_root() -> std::path::PathBuf {
        let n = UNIQ.fetch_add(1, Ordering::SeqCst);
        let p =
            std::env::temp_dir().join(format!("stone_fsxfer_test_{}_{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn cs(p: &Path) -> CString {
        CString::new(p.to_str().unwrap()).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn file_exists_status_distinguishes_metadata_errors_from_misses() {
        let root = fresh_root();
        let existing = std::fs::metadata(&root).unwrap();
        let existing_exists = stone_file_exists_status(cs(&root).as_ptr());
        let existing_is_directory = stone_is_directory_status(cs(&root).as_ptr());
        let path = root.join("loop");
        std::os::unix::fs::symlink(&path, &path).unwrap();
        let observed = std::fs::metadata(&path).unwrap_err();
        let expected = observed.raw_os_error() == Some(libc::ELOOP);
        let exists = stone_file_exists_status(cs(&path).as_ptr());
        let is_directory = stone_is_directory_status(cs(&path).as_ptr());
        let missing_path = root.join("missing");
        let missing_error = std::fs::metadata(&missing_path).unwrap_err();
        let missing_oracle = missing_error.kind() == std::io::ErrorKind::NotFound;
        let missing_exists = stone_file_exists_status(cs(&missing_path).as_ptr());
        let missing_is_directory = stone_is_directory_status(cs(&missing_path).as_ptr());
        fs::remove_dir_all(root).unwrap();

        assert!(
            expected,
            "symlink loop must produce an independent ELOOP oracle: {observed}"
        );
        assert!(
            existing.is_dir(),
            "fixture root must be a directory according to OS metadata"
        );
        assert_eq!(
            existing_exists, 1,
            "an OS-confirmed existing path is present"
        );
        assert_eq!(
            existing_is_directory, 1,
            "an OS-confirmed directory is a directory"
        );
        assert_eq!(
            exists, -1,
            "metadata failure must have a distinct error status"
        );
        assert_eq!(
            is_directory, -1,
            "directory metadata failure must have a distinct error status"
        );
        assert!(
            missing_oracle,
            "missing path must produce an independent ENOENT oracle: {missing_error}"
        );
        assert_eq!(missing_exists, 0, "a missing path remains absent");
        assert_eq!(
            missing_is_directory, 0,
            "a missing path remains non-directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_exists_status_treats_not_a_directory_as_absent() {
        let root = fresh_root();
        let blocker = root.join("blocker");
        std::fs::write(&blocker, "block").unwrap();
        let child = blocker.join("quests.json");
        let observed = std::fs::metadata(&child).unwrap_err();
        assert_eq!(
            observed.raw_os_error(),
            Some(libc::ENOTDIR),
            "a path under a regular file must produce an independent ENOTDIR oracle: {observed}"
        );
        assert_eq!(
            stone_file_exists_status(cs(&child).as_ptr()),
            0,
            "a path under a non-directory cannot exist"
        );
        assert_eq!(
            stone_is_directory_status(cs(&child).as_ptr()),
            0,
            "a path under a non-directory is not a directory"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_sync_overlaps_independent_files_and_joins_failures() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let entered = AtomicUsize::new(0);
        let completed = AtomicUsize::new(0);
        let paths = vec!["first".to_string(), "second".to_string()];
        let result = pool.install(|| {
            sync_paths_with(&paths, |_| {
                entered.fetch_add(1, Ordering::SeqCst);
                let deadline = Instant::now() + Duration::from_secs(2);
                while entered.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let overlapped = entered.load(Ordering::SeqCst) == 2;
                completed.fetch_add(1, Ordering::SeqCst);
                overlapped
            })
        });
        assert!(
            result,
            "durability waits must overlap rather than serialize"
        );
        assert_eq!(completed.load(Ordering::SeqCst), 2);
        completed.store(0, Ordering::SeqCst);
        assert!(!pool.install(|| sync_paths_with(&paths, |path| {
            completed.fetch_add(1, Ordering::SeqCst);
            path != "first"
        })));
        assert_eq!(
            completed.load(Ordering::SeqCst),
            2,
            "failure must join every file operation"
        );
    }

    #[test]
    fn snapshot_sync_reports_missing_files_and_preserves_contents() {
        let root = fresh_root();
        let path = root.join("snapshot.png");
        fs::write(&path, b"snapshot payload").unwrap();
        let input = CString::new(serde_json::to_string(&vec![&path]).unwrap()).unwrap();
        assert!(stone_files_sync(input.as_ptr()));
        assert_eq!(fs::read(&path).unwrap(), b"snapshot payload");
        let missing = root.join("missing.png");
        let input = CString::new(serde_json::to_string(&vec![&path, &missing]).unwrap()).unwrap();
        assert!(!stone_files_sync(input.as_ptr()));
        assert!(!stone_files_sync(c"{}".as_ptr()));
        assert!(!stone_files_sync(std::ptr::null()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pref_path_returns_an_existing_absolute_directory() {
        let org = CString::new("rock-pref-path-test").unwrap();
        let app = CString::new(format!("stone-rs-{}", std::process::id())).unwrap();

        let raw = stone_get_pref_path(org.as_ptr(), app.as_ptr());

        assert!(!raw.is_null());
        let path = Path::new(unsafe { CStr::from_ptr(raw) }.to_str().unwrap());
        assert!(
            path.is_absolute(),
            "pref path must not depend on the process cwd"
        );
        assert!(
            path.is_dir(),
            "SDL pref_path must create the returned directory"
        );
    }

    #[test]
    fn pref_path_rejects_null_and_non_utf8_names() {
        let valid = CString::new("stone-rs").unwrap();
        assert!(stone_get_pref_path(ptr::null(), valid.as_ptr()).is_null());
        assert!(stone_get_pref_path(valid.as_ptr(), ptr::null()).is_null());

        let invalid = [0xff_u8, 0];
        assert!(stone_get_pref_path(invalid.as_ptr().cast(), valid.as_ptr()).is_null());
        assert!(stone_get_pref_path(valid.as_ptr(), invalid.as_ptr().cast()).is_null());
    }

    #[test]
    fn embedded_asset_metadata_is_available_without_a_disk_file() {
        let path = CString::new("embedded-only/filesystem-test.bin").unwrap();
        let bytes = b"embedded";
        crate::asset_registry::stone_register_asset(path.as_ptr(), bytes.as_ptr(), bytes.len());

        assert_eq!(stone_file_size(path.as_ptr()), bytes.len() as i64);
        assert!(stone_file_modtime(path.as_ptr()) > 0.0);
    }

    #[test]
    fn copy_file_copies_contents() {
        let root = fresh_root();
        let src = root.join("a.txt");
        let dst = root.join("b.txt");
        fs::write(&src, b"hello").unwrap();

        assert!(stone_copy_file(cs(&src).as_ptr(), cs(&dst).as_ptr()));
        assert_eq!(fs::read(&dst).unwrap(), b"hello");
        assert!(src.exists(), "copy must not remove the source");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn copy_file_rejects_missing_source() {
        let root = fresh_root();
        let src = root.join("nope.txt");
        let dst = root.join("out.txt");
        assert!(!stone_copy_file(cs(&src).as_ptr(), cs(&dst).as_ptr()));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rename_moves_file_and_removes_source() {
        let root = fresh_root();
        let src = root.join("song.wav");
        let dst = root.join("moved.wav");
        fs::write(&src, b"pcm").unwrap();

        assert!(stone_rename(cs(&src).as_ptr(), cs(&dst).as_ptr()));
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"pcm");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rename_moves_directory_tree() {
        let root = fresh_root();
        let src = root.join("Pack");
        let sub = src.join("Sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(src.join("kick.wav"), b"k").unwrap();
        fs::write(sub.join("snare.wav"), b"s").unwrap();
        let dst = root.join("dest").join("Pack");
        fs::create_dir_all(dst.parent().unwrap()).unwrap();

        assert!(stone_rename(cs(&src).as_ptr(), cs(&dst).as_ptr()));
        assert!(!src.exists());
        assert_eq!(fs::read(dst.join("kick.wav")).unwrap(), b"k");
        assert_eq!(fs::read(dst.join("Sub").join("snare.wav")).unwrap(), b"s");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn copy_recursive_ffi_copies_file_and_tree() {
        let root = fresh_root();
        // File case.
        fs::write(root.join("one.wav"), b"1").unwrap();
        assert!(stone_copy_recursive(
            cs(&root.join("one.wav")).as_ptr(),
            cs(&root.join("one_copy.wav")).as_ptr()
        ));
        assert_eq!(fs::read(root.join("one_copy.wav")).unwrap(), b"1");
        // Tree case.
        let pack = root.join("Pack");
        fs::create_dir_all(pack.join("Sub")).unwrap();
        fs::write(pack.join("k.wav"), b"k").unwrap();
        fs::write(pack.join("Sub").join("s.wav"), b"s").unwrap();
        let dst = root.join("PackCopy");
        assert!(stone_copy_recursive(cs(&pack).as_ptr(), cs(&dst).as_ptr()));
        assert_eq!(fs::read(dst.join("k.wav")).unwrap(), b"k");
        assert_eq!(fs::read(dst.join("Sub").join("s.wav")).unwrap(), b"s");
        assert!(pack.exists(), "copy must not remove the source");
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_does_not_recurse_symlink_cycle() {
        // A dir containing a symlink back to itself must not loop.
        let root = fresh_root();
        let src = root.join("pack");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.wav"), b"a").unwrap();
        std::os::unix::fs::symlink(&src, src.join("loop")).unwrap();
        let dst = root.join("out");

        // Terminates (no stack overflow / hang). The cyclic symlink
        // entry copies shallowly or errors -- either way, bounded.
        let _ = copy_tree(&src, &dst);
        assert_eq!(fs::read(dst.join("a.wav")).unwrap(), b"a");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn copy_tree_mirrors_a_nested_tree() {
        // Directly exercises the EXDEV fallback's core.
        let root = fresh_root();
        let src = root.join("src");
        let deep = src.join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        fs::write(src.join("top.txt"), b"1").unwrap();
        fs::write(deep.join("leaf.txt"), b"2").unwrap();
        let dst = root.join("dst");

        copy_tree(&src, &dst).unwrap();
        assert_eq!(fs::read(dst.join("top.txt")).unwrap(), b"1");
        assert_eq!(
            fs::read(dst.join("a").join("b").join("leaf.txt")).unwrap(),
            b"2"
        );
        assert!(src.exists(), "copy_tree must not remove the source");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn directory_ffi_lists_embedded_registry_entries() {
        let n = UNIQ.fetch_add(1, Ordering::SeqCst);
        let dir = format!("game/assets/embedded-list-{n}");
        let key = CString::new(format!("{dir}/sound.ogg")).unwrap();
        let bytes = b"sound";
        crate::asset_registry::stone_register_asset(key.as_ptr(), bytes.as_ptr(), bytes.len());
        let dir = CString::new(dir).unwrap();

        assert_eq!(stone_is_directory_status(dir.as_ptr()), 1);
        let mut files = ptr::null_mut();
        let mut count = 0;
        assert!(stone_list_directory(dir.as_ptr(), &mut files, &mut count));
        let names: Vec<_> = (0..count)
            .map(|index| unsafe {
                CStr::from_ptr(*files.add(index as usize))
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        stone_free_string_array(files, count);

        assert_eq!(names, vec!["sound.ogg"]);
    }

    #[test]
    fn embedded_entries_do_not_hide_disk_listing_errors() {
        let root = fresh_root();
        let collision = root.join("not-a-directory");
        fs::write(&collision, b"file").unwrap();
        let key = CString::new(format!("{}/sound.ogg", collision.display())).unwrap();
        crate::asset_registry::stone_register_asset(key.as_ptr(), b"sound".as_ptr(), 5);
        let collision = cs(&collision);
        let mut files = ptr::null_mut();
        let mut count = 0;

        assert!(!stone_list_directory(
            collision.as_ptr(),
            &mut files,
            &mut count
        ));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn directory_listing_keeps_valid_names_visible() {
        let root = fresh_root();
        fs::write(root.join("visible.txt"), b"visible").unwrap();
        let dir = cs(&root);
        let mut files = ptr::null_mut();
        let mut count = 0;

        assert!(stone_list_directory(dir.as_ptr(), &mut files, &mut count));
        let names: Vec<_> = (0..count)
            .map(|index| unsafe {
                CStr::from_ptr(*files.add(index as usize))
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        stone_free_string_array(files, count);
        assert_eq!(names, vec!["visible.txt"]);

        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn directory_listing_skips_non_utf8_entries_without_hiding_valid_names() {
        use std::os::unix::ffi::OsStringExt;

        let root = fresh_root();
        fs::write(root.join("visible.txt"), b"visible").unwrap();
        fs::write(
            root.join(std::ffi::OsString::from_vec(vec![b'h', b'i', 0xff])),
            b"opaque",
        )
        .unwrap();
        let dir = cs(&root);
        let mut files = ptr::null_mut();
        let mut count = 0;

        assert!(stone_list_directory(dir.as_ptr(), &mut files, &mut count));
        let names: Vec<_> = (0..count)
            .map(|index| unsafe {
                CStr::from_ptr(*files.add(index as usize))
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        stone_free_string_array(files, count);
        assert_eq!(names, vec!["visible.txt"]);

        fs::remove_dir_all(&root).unwrap();
    }
}
