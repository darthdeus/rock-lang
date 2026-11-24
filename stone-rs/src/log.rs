use std::ffi::{c_char, c_int, CStr};

fn to_log_level(level: c_int) -> ::log::Level {
    match level {
        ..=0 => ::log::Level::Error,
        1 => ::log::Level::Warn,
        2 => ::log::Level::Info,
        3 => ::log::Level::Debug,
        _ => ::log::Level::Trace,
    }
}

#[no_mangle]
pub extern "C" fn stone_log_set_level(level: c_int) {
    rock_log::set_level(level.clamp(0, 4) as u8);
}

#[no_mangle]
pub extern "C" fn stone_log_get_level() -> c_int {
    rock_log::init();
    rock_log::level().min(4) as c_int
}

#[no_mangle]
pub extern "C" fn stone_log(
    level: c_int,
    category: *const c_char,
    msg: *const c_char,
    wall_clock: c_int,
) {
    let category = if category.is_null() {
        ""
    } else {
        unsafe { CStr::from_ptr(category) }.to_str().unwrap_or("")
    };
    let message = if msg.is_null() {
        std::borrow::Cow::Borrowed("")
    } else {
        unsafe { CStr::from_ptr(msg) }.to_string_lossy()
    };
    rock_log::init();
    if level.clamp(0, 4) as u8 > rock_log::level() {
        return;
    }
    let level = to_log_level(level);
    if wall_clock != 0 {
        rock_log::emit_wall_clock(level, category, format_args!("{message}"));
        return;
    }
    ::log::log!(target: category, level, "{message}");
}

/// Rock's level numbering (0 error, 1 warn); only those two keep records.
fn record_level(level: c_int) -> Option<::log::Level> {
    match level {
        0 => Some(::log::Level::Error),
        1 => Some(::log::Level::Warn),
        _ => None,
    }
}

/// Every record at `level` (0 error, 1 warn) this process emitted, Rust and
/// Rock alike, saturating. What the development error badge counts.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_log_record_total(level: c_int) -> c_int {
    record_level(level).map_or(0, |l| {
        rock_log::records_total(l).min(c_int::MAX as u64) as c_int
    })
}

/// Valid until the next call on this thread.
#[no_mangle]
pub extern "C" fn stone_log_records_text(level: c_int, max_rows: c_int) -> *const c_char {
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

/// NUL-separated records, then the overflow count. Valid until the next call on this
/// thread.
#[no_mangle]
pub extern "C" fn stone_log_collected_errors(len: *mut usize) -> *const c_char {
    collected_error_reply(rock_log::collected_errors(), len)
}

#[no_mangle]
pub extern "C" fn stone_log_scoped_errors(len: *mut usize) -> *const c_char {
    collected_error_reply(rock_log::scoped_errors(), len)
}

fn collected_error_reply(records: Vec<String>, len: *mut usize) -> *const c_char {
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
        reply.as_ptr() as *const c_char
    })
}

/// Remove the first collected record equal to `record` (`len` bytes); 0 when
/// none is.
#[no_mangle]
pub extern "C" fn stone_log_take_collected_error(record: *const u8, len: usize) -> c_int {
    take_collected_error(record, len, rock_log::take_collected_error)
}

#[no_mangle]
pub extern "C" fn stone_log_take_scoped_error(record: *const u8, len: usize) -> c_int {
    take_collected_error(record, len, rock_log::take_scoped_error)
}

fn take_collected_error(record: *const u8, len: usize, take: fn(&str) -> bool) -> c_int {
    if record.is_null() {
        return 0;
    }
    let record = unsafe { std::slice::from_raw_parts(record, len) };
    take(&String::from_utf8_lossy(record)) as c_int
}

thread_local! {
    static LOG_RECORDS_REPLY: std::cell::RefCell<Option<std::ffi::CString>> =
        const { std::cell::RefCell::new(None) };
    static COLLECTED_ERRORS_REPLY: std::cell::RefCell<Vec<u8>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[macro_export]
macro_rules! log_once {
    ($category:expr, $($arg:tt)*) => {{
        if $crate::rock_log_reexport::once_should_emit(concat!(file!(), ":", line!())) {
            ::log::warn!(target: $category, $($arg)*);
        }
    }};
}

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

    // Tests share global dedup state.
    static TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

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
