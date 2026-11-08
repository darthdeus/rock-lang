//! RenderDoc in-app capture API (`STONE_RENDERDOC=<template>`).
//!
//! The runner preloads `librenderdoc.so` into the game process (see
//! `rock_host::project::apply_renderdoc_gate`); this module grabs the
//! already-loaded copy via `dlopen(RTLD_NOW | RTLD_NOLOAD)` + `dlsym` of
//! `RENDERDOC_GetAPI` and exposes capture through FFI externs plus an
//! optional auto-trigger (`STONE_RENDERDOC_FRAME=<n>` fires once, right
//! after the nth swap; RenderDoc captures the next presented frame).
//!
//! Off-by-default cost is one relaxed load + predictable branch in
//! `on_swap`. The API table is transcribed from RenderDoc 1.45's
//! `renderdoc_app.h` (`RENDERDOC_API_1_7_0`, which every older version
//! typedefs); the layout asserts below fail the build on drift.

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Requested in-app API version: 1.6.0 (`eRENDERDOC_API_Version_1_6_0`).
/// Only the Linux `load_api` passes it to `RENDERDOC_GetAPI`.
#[cfg(target_os = "linux")]
const API_VERSION_1_6_0: c_int = 10600;

const ENV_TEMPLATE: &str = "STONE_RENDERDOC";
const ENV_FRAME: &str = "STONE_RENDERDOC_FRAME";

/// `void *` device / window handles in RenderDoc's C API.
type DevicePointer = *mut c_void;
type WindowHandle = *mut c_void;

/// Transcription of `RENDERDOC_API_1_7_0`, field order verbatim. Only the
/// fields stone calls carry exact signatures; the two 1.7.0 annotation
/// slots are width placeholders (never called under a 1.6.0 request).
#[repr(C)]
struct RenderdocApi {
    get_api_version: Option<unsafe extern "C" fn(*mut c_int, *mut c_int, *mut c_int)>,
    set_capture_option_u32: Option<unsafe extern "C" fn(c_uint, c_uint)>,
    set_capture_option_f32: Option<unsafe extern "C" fn(c_uint, f32)>,
    get_capture_option_u32: Option<unsafe extern "C" fn(c_uint) -> c_uint>,
    get_capture_option_f32: Option<unsafe extern "C" fn(c_uint) -> f32>,
    set_focus_toggle_keys: Option<unsafe extern "C" fn(*mut c_void, c_int)>,
    set_capture_keys: Option<unsafe extern "C" fn(*mut c_void, c_int)>,
    get_overlay_bits: Option<unsafe extern "C" fn() -> c_uint>,
    mask_overlay_bits: Option<unsafe extern "C" fn(c_uint, c_uint)>,
    remove_hooks: Option<unsafe extern "C" fn()>,
    unload_crash_handler: Option<unsafe extern "C" fn()>,
    set_capture_file_path_template: Option<unsafe extern "C" fn(*const c_char)>,
    get_capture_file_path_template: Option<unsafe extern "C" fn() -> *const c_char>,
    get_num_captures: Option<unsafe extern "C" fn() -> c_uint>,
    get_capture: Option<unsafe extern "C" fn(c_uint, *mut c_char, *mut c_uint, *mut u64) -> c_uint>,
    trigger_capture: Option<unsafe extern "C" fn()>,
    is_target_control_connected: Option<unsafe extern "C" fn() -> c_uint>,
    launch_replay_ui: Option<unsafe extern "C" fn(c_uint, *const c_char) -> c_uint>,
    set_active_window: Option<unsafe extern "C" fn(DevicePointer, WindowHandle)>,
    start_frame_capture: Option<unsafe extern "C" fn(DevicePointer, WindowHandle)>,
    is_frame_capturing: Option<unsafe extern "C" fn() -> c_uint>,
    end_frame_capture: Option<unsafe extern "C" fn(DevicePointer, WindowHandle) -> c_uint>,
    trigger_multi_frame_capture: Option<unsafe extern "C" fn(c_uint)>,
    set_capture_file_comments: Option<unsafe extern "C" fn(*const c_char, *const c_char)>,
    discard_frame_capture: Option<unsafe extern "C" fn(DevicePointer, WindowHandle) -> c_uint>,
    show_replay_ui: Option<unsafe extern "C" fn() -> c_uint>,
    set_capture_title: Option<unsafe extern "C" fn(*const c_char)>,
    set_object_annotation: *const c_void,
    set_command_annotation: *const c_void,
}

const PTR: usize = size_of::<*const ()>();
const _: () = assert!(size_of::<RenderdocApi>() == 29 * PTR);
const _: () = assert!(std::mem::offset_of!(RenderdocApi, set_capture_keys) == 6 * PTR);
const _: () =
    assert!(std::mem::offset_of!(RenderdocApi, set_capture_file_path_template) == 11 * PTR);
const _: () = assert!(std::mem::offset_of!(RenderdocApi, get_num_captures) == 13 * PTR);
const _: () = assert!(std::mem::offset_of!(RenderdocApi, get_capture) == 14 * PTR);
const _: () = assert!(std::mem::offset_of!(RenderdocApi, trigger_capture) == 15 * PTR);

/// Loaded API table address, 0 when absent. Set once by `init`, never
/// cleared; it points at RenderDoc's static table (preloaded lib =
/// process lifetime).
static API: AtomicUsize = AtomicUsize::new(0);
/// True once `init` saw `STONE_RENDERDOC` with a working API.
static ARMED: AtomicBool = AtomicBool::new(false);
/// Swap count after which to auto-trigger; `u64::MAX` = manual only.
static FRAME_TARGET: AtomicU64 = AtomicU64::new(u64::MAX);
/// Auto-trigger fired (once).
static TRIGGERED: AtomicBool = AtomicBool::new(false);

fn api() -> Option<&'static RenderdocApi> {
    let addr = API.load(Ordering::Relaxed);
    if addr == 0 {
        return None;
    }
    // SAFETY: set once from `load_api` to RenderDoc's static table, which
    // outlives the process; never cleared, never freed.
    Some(unsafe { &*(addr as *const RenderdocApi) })
}

/// Strip one trailing `.rdc` so the literal `STONE_RENDERDOC=<out.rdc>`
/// spelling and the bare-prefix spelling land the same template.
pub(crate) fn normalize_template(raw: &str) -> &str {
    raw.strip_suffix(".rdc").unwrap_or(raw)
}

/// Parse `STONE_RENDERDOC_FRAME`: a positive integer, else `None`.
fn parse_frame_target(raw: Option<&str>) -> Option<u64> {
    match raw?.parse::<u64>() {
        Ok(n) if n > 0 => Some(n),
        _ => None,
    }
}

/// Grab the preloaded API (Linux only) and arm capture. Called early from
/// `stone_init`, after logging init. Absent lib + requested capture is a
/// loud error, never a silent no-op.
pub(crate) fn init() {
    let raw = match std::env::var(ENV_TEMPLATE).ok().filter(|v| !v.is_empty()) {
        Some(v) => v,
        None => return,
    };
    // SAFETY: `dlsym` of a well-known symbol; NOLOAD maps nothing new.
    let table = unsafe { load_api() };
    let Some(table) = table else {
        ::log::error!(target: "STONE",
            "renderdoc: {ENV_TEMPLATE} is set but librenderdoc.so is not loaded; \
             run through the rock runner so it is preloaded into the game process");
        return;
    };
    // SAFETY: `table` is RenderDoc's static struct; the layout asserts above
    // pin every field this module reads.
    let table = unsafe { &*table };
    let (Some(set_template), Some(_trigger)) =
        (table.set_capture_file_path_template, table.trigger_capture)
    else {
        ::log::error!(target: "STONE", "renderdoc: loaded API is missing capture functions");
        return;
    };
    API.store(table as *const RenderdocApi as usize, Ordering::Relaxed);
    // Route all captures through our trigger: RenderDoc's built-in F12 hook
    // would double-fire with stone's F12 screenshot.
    unsafe {
        if let Some(set_keys) = table.set_capture_keys {
            set_keys(std::ptr::null_mut(), 0);
        }
        let template = normalize_template(&raw);
        match std::ffi::CString::new(template) {
            Ok(c) => set_template(c.as_ptr()),
            Err(_) => ::log::warn!(target: "STONE",
                "renderdoc: template has an interior NUL, using RenderDoc's default path"),
        }
    }
    match std::env::var(ENV_FRAME).ok().filter(|v| !v.is_empty()) {
        None => {}
        Some(raw_frame) => match parse_frame_target(Some(&raw_frame)) {
            Some(n) => {
                FRAME_TARGET.store(n, Ordering::Relaxed);
            }
            None => ::log::warn!(target: "STONE",
                "renderdoc: ignoring invalid {ENV_FRAME}={raw_frame:?}, want a positive integer"),
        },
    }
    ARMED.store(true, Ordering::Relaxed);
    ::log::info!(target: "STONE", "renderdoc: active, captures -> {}", normalize_template(&raw));
}

#[cfg(target_os = "linux")]
unsafe fn load_api() -> Option<*const RenderdocApi> {
    let handle = libc::dlopen(
        c"librenderdoc.so".as_ptr(),
        libc::RTLD_NOW | libc::RTLD_NOLOAD,
    );
    if handle.is_null() {
        return None;
    }
    let sym = libc::dlsym(handle, c"RENDERDOC_GetAPI".as_ptr());
    if sym.is_null() {
        return None;
    }
    let get_api: unsafe extern "C" fn(c_int, *mut *mut c_void) -> c_int = std::mem::transmute(sym);
    let mut out: *mut c_void = std::ptr::null_mut();
    if get_api(API_VERSION_1_6_0, &mut out) != 1 || out.is_null() {
        return None;
    }
    Some(out as *const RenderdocApi)
}

#[cfg(not(target_os = "linux"))]
unsafe fn load_api() -> Option<*const RenderdocApi> {
    None
}

/// Frame-boundary hook from `stone_swap_window`, called with the completed
/// swap count (1-based). Off by default: one relaxed load + branch.
#[inline]
pub(crate) fn on_swap(completed: u64) {
    if !ARMED.load(Ordering::Relaxed) {
        return;
    }
    if completed == FRAME_TARGET.load(Ordering::Relaxed) && !TRIGGERED.swap(true, Ordering::SeqCst)
    {
        trigger_capture();
    }
}

/// Capture the next presented frame. False + a loud log when the library
/// is absent -- never a silent no-op.
pub(crate) fn trigger_capture() -> bool {
    let Some(table) = api() else {
        ::log::error!(target: "STONE",
            "renderdoc: capture requested but librenderdoc.so is absent; \
             run with STONE_RENDERDOC=<out> so the runner preloads it");
        return false;
    };
    match table.trigger_capture {
        // SAFETY: pinned layout, non-null checked, called on the GL thread.
        Some(trigger) => unsafe {
            trigger();
            ::log::info!(target: "STONE", "renderdoc: capture triggered");
            true
        },
        None => {
            ::log::error!(target: "STONE", "renderdoc: loaded API has no TriggerCapture");
            false
        }
    }
}

/// Report landed captures (and a trigger that never finalized) at teardown.
pub(crate) fn on_teardown() {
    let Some(table) = api() else { return };
    let (Some(num_captures), Some(get_capture)) = (table.get_num_captures, table.get_capture)
    else {
        return;
    };
    // SAFETY: pinned layout, called once on the exiting game thread.
    let n = unsafe { num_captures() };
    if TRIGGERED.load(Ordering::Relaxed) && n == 0 {
        ::log::warn!(target: "STONE",
            "renderdoc: trigger fired but no capture finalized ({ENV_FRAME} beyond the run end?)");
    }
    for idx in 0..n {
        let mut buf = vec![0 as c_char; 1024];
        let mut len: c_uint = buf.len() as c_uint;
        let mut ts: u64 = 0;
        let ok = unsafe { get_capture(idx, buf.as_mut_ptr(), &mut len, &mut ts) };
        if ok == 1 {
            let path = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
            ::log::info!(target: "STONE", "renderdoc: capture {idx}: {path}");
        }
    }
}

/// Capture the next presented frame. Returns false + logs when the library
/// is absent -- never a silent no-op.
#[no_mangle]
pub extern "C" fn stone_renderdoc_trigger_capture() -> bool {
    trigger_capture()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_strips_one_rdc_suffix() {
        assert_eq!(normalize_template("cap/mag-crypt"), "cap/mag-crypt");
        assert_eq!(normalize_template("cap/mag-crypt.rdc"), "cap/mag-crypt");
        assert_eq!(
            normalize_template("cap/mag-crypt.rdc.rdc"),
            "cap/mag-crypt.rdc"
        );
    }

    #[test]
    fn frame_target_parses_positive_ints() {
        assert_eq!(parse_frame_target(None), None);
        assert_eq!(parse_frame_target(Some("")), None);
        assert_eq!(parse_frame_target(Some("60")), Some(60));
        assert_eq!(parse_frame_target(Some("0")), None);
        assert_eq!(parse_frame_target(Some("abc")), None);
    }

    #[test]
    fn trigger_without_lib_returns_false() {
        // The test process never runs `init`, so the API stays absent: the
        // absent-library error path, deterministically.
        assert!(api().is_none());
        assert!(!trigger_capture());
    }
}
