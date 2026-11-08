//! C-ABI level bridge for the game process. The level itself lives in
//! the one logger (`rock-log`); this only exposes it across the FFI
//! boundary and installs the logger when the engine starts. No second
//! `ROCK_LOG` parser and no
//! second level store — both used to exist here and silently drifted.

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum StoneLogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

fn from_u8(v: u8) -> StoneLogLevel {
    match v {
        0 => StoneLogLevel::Error,
        1 => StoneLogLevel::Warn,
        2 => StoneLogLevel::Info,
        3 => StoneLogLevel::Debug,
        _ => StoneLogLevel::Trace,
    }
}

/// Called at engine startup (FFI). Installs the one logger and parses
/// `ROCK_LOG`. Idempotent.
#[no_mangle]
pub extern "C" fn stone_log_init() {
    rock_log::init();
}

#[no_mangle]
pub extern "C" fn stone_log_set_level(level: StoneLogLevel) {
    rock_log::set_level(level as u8);
}

#[no_mangle]
pub extern "C" fn stone_log_get_level() -> StoneLogLevel {
    // Idempotent: covers Rock reading the level before stone_init()
    // (Lua global-init logging).
    rock_log::init();
    from_u8(rock_log::level())
}

/// Rock-side log emit bridge (Phase 4): the one sink for the whole
/// game process. Rock's `__rock_log` / `log_once` / `log_throttled`
/// call this instead of Lua `print()`. `rock_log::init()` is
/// idempotent and makes early Rock logs (emitted during Lua
/// global-init, before `stone_init()` runs) still install + gate
/// correctly. Caller already level-gated, but we re-gate here so the
/// one logger stays authoritative. `level`: 0=error..4=trace.
///
/// `wall_clock` is the log SITE's declaration that this record's content is a
/// wall-clock measurement (`rock_log::WALL_CLOCK_MARK`). It cannot ride the
/// `log` facade, which has no channel for it, so a declared record goes to the
/// same backend through `rock_log::emit_wall_clock`.
#[no_mangle]
pub extern "C" fn stone_log(
    level: std::os::raw::c_int,
    category: *const std::os::raw::c_char,
    msg: *const std::os::raw::c_char,
    wall_clock: std::os::raw::c_int,
) {
    let cat = if category.is_null() {
        ""
    } else {
        unsafe { std::ffi::CStr::from_ptr(category) }
            .to_str()
            .unwrap_or("")
    };
    let m = if msg.is_null() {
        std::borrow::Cow::Borrowed("")
    } else {
        unsafe { std::ffi::CStr::from_ptr(msg) }.to_string_lossy()
    };
    emit_log(level, cat, &m, wall_clock);
}

#[no_mangle]
pub extern "C" fn stone_log_bytes(
    level: std::os::raw::c_int,
    category: *const u8,
    category_len: usize,
    message: *const u8,
    message_len: usize,
    wall_clock: std::os::raw::c_int,
) {
    let Some(category) = ffi_bytes(category, category_len) else {
        return;
    };
    let Some(message) = ffi_bytes(message, message_len) else {
        return;
    };
    let category = std::str::from_utf8(category).unwrap_or("");
    let message = String::from_utf8_lossy(message);
    emit_log(level, category, &message, wall_clock);
}

fn ffi_bytes<'a>(data: *const u8, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if data.is_null() {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(data, len) })
}

fn emit_log(level: std::os::raw::c_int, cat: &str, message: &str, wall_clock: std::os::raw::c_int) {
    rock_log::init();
    let lvl = level.clamp(0, 4) as u8;
    if lvl > rock_log::level() {
        return;
    }
    // `log::` is shadowed by this crate's local `pub mod log` — the
    // extern crate is `::log`.
    let lv = match lvl {
        0 => ::log::Level::Error,
        1 => ::log::Level::Warn,
        2 => ::log::Level::Info,
        3 => ::log::Level::Debug,
        _ => ::log::Level::Trace,
    };
    if wall_clock != 0 {
        rock_log::emit_wall_clock(lv, cat, format_args!("{}", message));
        return;
    }
    ::log::log!(target: cat, lv, "{}", message);
}
