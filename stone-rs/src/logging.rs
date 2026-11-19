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

pub(crate) fn init() {
    rock_log::init();
}

#[no_mangle]
pub extern "C" fn stone_log_set_level(level: StoneLogLevel) {
    rock_log::set_level(level as u8);
}

#[no_mangle]
pub extern "C" fn stone_log_get_level() -> StoneLogLevel {
    rock_log::init();
    from_u8(rock_log::level())
}

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
