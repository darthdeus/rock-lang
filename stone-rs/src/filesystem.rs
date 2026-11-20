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

    if let Some(parent) = Path::new(path_str).parent() {
        if !parent.as_os_str().is_empty() && fs::create_dir_all(parent).is_err() {}
    }

    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, len) };
    match fs::File::create(path_str) {
        Ok(mut file) => file.write_all(bytes).is_ok(),
        Err(_) => false,
    }
}

/// One O_APPEND write, so concurrent writers never interleave bytes within a record.
#[no_mangle]
pub extern "C" fn stone_file_append(path: *const c_char, data: *const c_char, len: usize) -> bool {
    if path.is_null() || data.is_null() {
        return false;
    }
    let Some(path_str) = (unsafe { cstr(path) }) else {
        return false;
    };

    if let Some(parent) = Path::new(path_str).parent() {
        if !parent.as_os_str().is_empty() && fs::create_dir_all(parent).is_err() {}
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

/// The caller frees the returned buffer with libc::free. NULL on failure.
#[no_mangle]
pub extern "C" fn stone_file_read(path: *const c_char, out_len: *mut usize) -> *mut c_char {
    let Some(path_str) = (unsafe { cstr(path) }) else {
        return ptr::null_mut();
    };

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

// Symlinks are recreated, never recursed into: a link to an ancestor would loop.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    let file_type = fs::symlink_metadata(src)?.file_type();
    if file_type.is_dir() {
        fs::create_dir_all(dst)?;
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

/// The caller frees the array with stone_free_string_array().
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

    let arr =
        unsafe { libc::malloc(count * std::mem::size_of::<*mut c_char>()) as *mut *mut c_char };
    if arr.is_null() {
        return false;
    }

    for (i, name) in entries.iter().enumerate() {
        match CString::new(name.as_str()) {
            Ok(cstr) => {
                let bytes = cstr.as_bytes_with_nul();
                let str_ptr = unsafe { libc::malloc(bytes.len()) as *mut c_char };
                if str_ptr.is_null() {
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
    fn fresh_root() -> crate::test_support::TestRoot {
        crate::test_support::fresh_root("stone_fsxfer_test_")
    }

    fn list_names(dir: &CStr) -> Vec<String> {
        let mut files = ptr::null_mut();
        let mut count = 0;
        assert!(stone_list_directory(dir.as_ptr(), &mut files, &mut count));
        let names = (0..count)
            .map(|index| unsafe {
                CStr::from_ptr(*files.add(index as usize))
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        stone_free_string_array(files, count);
        names
    }

    fn cs(p: &Path) -> CString {
        CString::new(p.to_str().unwrap()).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn existence_status_separates_hits_misses_and_metadata_errors() {
        let root = fresh_root();
        let blocker = root.join("blocker");
        std::fs::write(&blocker, "block").unwrap();
        let symlink_loop = root.join("loop");
        std::os::unix::fs::symlink(&symlink_loop, &symlink_loop).unwrap();
        let cases = [
            (root.to_path_buf(), None, 1),
            (root.join("missing"), Some(libc::ENOENT), 0),
            (blocker.join("quests.json"), Some(libc::ENOTDIR), 0),
            (symlink_loop, Some(libc::ELOOP), -1),
        ];
        for (path, errno, status) in cases {
            let observed = std::fs::metadata(&path)
                .err()
                .and_then(|e| e.raw_os_error());
            assert_eq!(observed, errno, "fixture {path:?}");
            assert_eq!(
                stone_file_exists_status(cs(&path).as_ptr()),
                status,
                "{path:?}"
            );
            assert_eq!(
                stone_is_directory_status(cs(&path).as_ptr()),
                status,
                "{path:?}"
            );
        }
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
    fn copy_file_copies_contents_and_rejects_a_missing_source() {
        let root = fresh_root();
        let src = root.join("a.txt");
        let dst = root.join("b.txt");
        fs::write(&src, b"hello").unwrap();

        assert!(stone_copy_file(cs(&src).as_ptr(), cs(&dst).as_ptr()));
        assert_eq!(fs::read(&dst).unwrap(), b"hello");
        assert!(src.exists(), "copy must not remove the source");
        let missing = root.join("nope.txt");
        assert!(!stone_copy_file(cs(&missing).as_ptr(), cs(&dst).as_ptr()));
    }

    fn write_pack(pack: &Path) {
        fs::create_dir_all(pack.join("Sub")).unwrap();
        fs::write(pack.join("kick.wav"), b"k").unwrap();
        fs::write(pack.join("Sub").join("snare.wav"), b"s").unwrap();
    }

    fn assert_pack(pack: &Path) {
        assert_eq!(fs::read(pack.join("kick.wav")).unwrap(), b"k");
        assert_eq!(fs::read(pack.join("Sub").join("snare.wav")).unwrap(), b"s");
    }

    #[test]
    fn rename_moves_files_and_directory_trees() {
        let root = fresh_root();
        let src = root.join("song.wav");
        let dst = root.join("moved.wav");
        fs::write(&src, b"pcm").unwrap();
        assert!(stone_rename(cs(&src).as_ptr(), cs(&dst).as_ptr()));
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"pcm");

        let src = root.join("Pack");
        write_pack(&src);
        let dst = root.join("dest").join("Pack");
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        assert!(stone_rename(cs(&src).as_ptr(), cs(&dst).as_ptr()));
        assert!(!src.exists());
        assert_pack(&dst);
    }

    #[test]
    fn copy_tree_copies_a_file_or_a_nested_tree() {
        let root = fresh_root();
        let file = root.join("one.wav");
        let file_copy = root.join("one_copy.wav");
        fs::write(&file, b"1").unwrap();
        copy_tree(&file, &file_copy).unwrap();
        assert_eq!(fs::read(&file_copy).unwrap(), b"1");

        let pack = root.join("Pack");
        write_pack(&pack);
        let dst = root.join("PackCopy");
        copy_tree(&pack, &dst).unwrap();
        assert_pack(&dst);
        assert_pack(&pack);
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_does_not_recurse_symlink_cycle() {
        let root = fresh_root();
        let src = root.join("pack");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.wav"), b"a").unwrap();
        std::os::unix::fs::symlink(&src, src.join("loop")).unwrap();
        let dst = root.join("out");

        let _ = copy_tree(&src, &dst);
        assert_eq!(fs::read(dst.join("a.wav")).unwrap(), b"a");
    }

    #[test]
    fn directory_ffi_lists_embedded_registry_entries() {
        let dir = "game/assets/embedded-list";
        let key = CString::new(format!("{dir}/sound.ogg")).unwrap();
        let bytes = b"sound";
        crate::asset_registry::stone_register_asset(key.as_ptr(), bytes.as_ptr(), bytes.len());
        let dir = CString::new(dir).unwrap();

        assert_eq!(stone_is_directory_status(dir.as_ptr()), 1);
        let names = list_names(&dir);

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
    }

    #[test]
    fn directory_listing_keeps_valid_names_visible() {
        let root = fresh_root();
        fs::write(root.join("visible.txt"), b"visible").unwrap();
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStringExt;
            let opaque = std::ffi::OsString::from_vec(vec![b'h', b'i', 0xff]);
            fs::write(root.join(opaque), b"opaque").unwrap();
        }
        assert_eq!(list_names(&cs(&root)), vec!["visible.txt"]);
    }
}
