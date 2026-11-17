//! `log_once!` / `log_throttled!` — per-call-site dedup / throttle.
//!
//! These are the spam-guard macros for engine-side failures that would
//! otherwise repeat 60×/sec. The dedup/throttle *state* and the actual
//! emit now live in the one logger (`rock-log` + the `log` facade).
//! These macros only keep their
//! historical call-site signatures (`log_once!("CAT", "fmt", args)` /
//! `log_throttled!("CAT", Duration, "fmt", args)`) so existing sites
//! compile unchanged; they delegate the dedup/throttle decision to
//! `rock_log` and emit via `log::` at `warn` level (spam-guard =
//! recoverable failure; visible at the default `info`).

/// Rock's level numbering (0 error, 1 warn); only those two keep records.
fn record_level(level: std::os::raw::c_int) -> Option<::log::Level> {
    match level {
        0 => Some(::log::Level::Error),
        1 => Some(::log::Level::Warn),
        _ => None,
    }
}

/// Every record at `level` (0 error, 1 warn) this process emitted, Rust and
/// Rock alike, saturating. What the development error badge counts.
#[no_mangle]
pub extern "C" fn stone_log_record_total(level: std::os::raw::c_int) -> std::os::raw::c_int {
    record_level(level).map_or(0, |l| {
        rock_log::records_total(l).min(std::os::raw::c_int::MAX as u64) as std::os::raw::c_int
    })
}

/// The most recently seen distinct records at `level`, at most `max_rows`, one
/// per line as `count<TAB>target<TAB>message`, then one `evicted<TAB>N` line
/// when the bounded table dropped rows. Valid until the next call on this
/// thread.
#[no_mangle]
pub extern "C" fn stone_log_records_text(
    level: std::os::raw::c_int,
    max_rows: std::os::raw::c_int,
) -> *const std::os::raw::c_char {
    let mut out = String::new();
    if let Some(level) = record_level(level) {
        for row in rock_log::records(level)
            .iter()
            .take(max_rows.max(0) as usize)
        {
            let clean = |s: &str| s.replace(['\t', '\0'], " ");
            out.push_str(&format!(
                "{}\t{}\t{}\n",
                row.count,
                clean(&row.target),
                clean(&row.message)
            ));
        }
        let evicted = rock_log::records_evicted(level);
        if evicted > 0 {
            out.push_str(&format!("evicted\t{evicted}\n"));
        }
    }
    LOG_RECORDS_REPLY.with(|reply| {
        let text = std::ffi::CString::new(out).expect("NUL bytes were replaced");
        let ptr = text.as_ptr();
        *reply.borrow_mut() = Some(text);
        ptr
    })
}

/// Start collecting error records for a checked run, emptying the list.
#[no_mangle]
pub extern "C" fn stone_log_collect_errors() {
    rock_log::collect_errors();
}

/// Collected error records, then a final line with the overflow count, as
/// `record<NUL>...<NUL>overflow`. A record may span lines, so NUL separates.
/// Returns the byte length; the text is valid until the next call on this
/// thread.
#[no_mangle]
pub extern "C" fn stone_log_collected_errors(len: *mut usize) -> *const std::os::raw::c_char {
    collected_error_reply(rock_log::collected_errors(), len)
}

#[no_mangle]
pub extern "C" fn stone_log_scoped_errors(len: *mut usize) -> *const std::os::raw::c_char {
    collected_error_reply(rock_log::scoped_errors(), len)
}

fn collected_error_reply(records: Vec<String>, len: *mut usize) -> *const std::os::raw::c_char {
    let mut out = String::new();
    for record in records {
        out.push_str(&record.replace('\0', " "));
        out.push('\0');
    }
    out.push_str(&rock_log::collected_error_overflow().to_string());
    if !len.is_null() {
        unsafe { *len = out.len() };
    }
    COLLECTED_ERRORS_REPLY.with(|reply| {
        let mut reply = reply.borrow_mut();
        *reply = out.into_bytes();
        reply.as_ptr() as *const std::os::raw::c_char
    })
}

/// Remove the first collected record equal to `record` (`len` bytes); 0 when
/// none is.
#[no_mangle]
pub extern "C" fn stone_log_take_collected_error(
    record: *const u8,
    len: usize,
) -> std::os::raw::c_int {
    take_collected_error(record, len, rock_log::take_collected_error)
}

#[no_mangle]
pub extern "C" fn stone_log_take_scoped_error(
    record: *const u8,
    len: usize,
) -> std::os::raw::c_int {
    take_collected_error(record, len, rock_log::take_scoped_error)
}

fn take_collected_error(
    record: *const u8,
    len: usize,
    take: fn(&str) -> bool,
) -> std::os::raw::c_int {
    if record.is_null() {
        return 0;
    }
    let record = unsafe { std::slice::from_raw_parts(record, len) };
    take(&String::from_utf8_lossy(record)) as std::os::raw::c_int
}

/// Drop every collected record after the first `len`.
#[no_mangle]
pub extern "C" fn stone_log_truncate_collected_errors(len: std::os::raw::c_int) {
    rock_log::truncate_collected_errors(len.max(0) as usize);
}

thread_local! {
    static LOG_RECORDS_REPLY: std::cell::RefCell<Option<std::ffi::CString>> =
        const { std::cell::RefCell::new(None) };
    static COLLECTED_ERRORS_REPLY: std::cell::RefCell<Vec<u8>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Emit at most once per call site, through the one logger.
///
/// ```ignore
/// log_once!("AUDIO", "failed to load music: {}", path);
/// ```
#[macro_export]
macro_rules! log_once {
    ($category:expr, $($arg:tt)*) => {{
        if $crate::rock_log_reexport::once_should_emit(concat!(file!(), ":", line!())) {
            ::log::warn!(target: $category, $($arg)*);
        }
    }};
}

/// Rate-limit log lines from a single call site. `interval` is a
/// `std::time::Duration`; first call emits, rest drop until elapsed.
///
/// ```ignore
/// log_throttled!("RENDER", std::time::Duration::from_secs(1),
///                "shader handle is 0; draw is a no-op");
/// ```
#[macro_export]
macro_rules! log_throttled {
    ($category:expr, $interval:expr, $($arg:tt)*) => {{
        if $crate::rock_log_reexport::throttle_should_emit(
            concat!(file!(), ":", line!()), $interval)
        {
            ::log::warn!(target: $category, $($arg)*);
        }
    }};
}

#[cfg(test)]
mod tests {
    use once_cell::sync::Lazy;
    use std::sync::Mutex;
    use std::time::Duration;

    // Tests share global dedup state; serialize them.
    static TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    #[test]
    fn log_once_dedups_same_site() {
        let _g = TEST_LOCK.lock().unwrap();
        rock_log::reset_dedup();
        let before = rock_log::dedup_count();
        for _ in 0..100 {
            log_once!("TEST", "spam {}", 42);
        }
        assert_eq!(
            rock_log::dedup_count() - before,
            1,
            "100 calls from one site -> 1 dedup entry"
        );
    }

    #[test]
    fn log_once_distinct_sites_emit_separately() {
        let _g = TEST_LOCK.lock().unwrap();
        rock_log::reset_dedup();
        let before = rock_log::dedup_count();
        log_once!("TEST", "site A");
        log_once!("TEST", "site B");
        assert_eq!(rock_log::dedup_count() - before, 2);
    }

    #[test]
    fn log_throttled_first_call_emits() {
        let _g = TEST_LOCK.lock().unwrap();
        rock_log::reset_dedup();
        for _ in 0..100 {
            log_throttled!("TEST", Duration::from_secs(60), "throttled {}", 1);
        }
        assert_eq!(rock_log::throttle_count(), 1);
    }

    #[test]
    fn log_reset_clears_state() {
        let _g = TEST_LOCK.lock().unwrap();
        log_once!("TEST", "before reset");
        log_throttled!("TEST", Duration::from_secs(60), "before reset");
        rock_log::reset_dedup();
        assert_eq!(rock_log::dedup_count(), 0);
        assert_eq!(rock_log::throttle_count(), 0);
    }
}
