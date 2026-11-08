//! Stone - Rust implementation of the Stone game engine library
//!
//! This is a cdylib that exports C-compatible functions for LuaJIT FFI.
//!
//! Uses:
//! - sdl2 for windowing/input
//! - glow for OpenGL
//! - image for image loading (pure Rust, replaces SDL2_image)
//! - ttf-parser + zeno for glyph outlines and rasterization (pure Rust, replaces FreeType)

// This crate is the LuaJIT FFI boundary: every `pub extern "C" fn` is called
// across an unsafe ABI, and pointer-arg derefs are part of the contract.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

mod jit_health;
mod jit_health_abi;
pub use jit_health::luaopen_stone_lua_jit_health;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_double, c_float, c_int, c_void, CStr};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

type LuaAlloc = unsafe extern "C" fn(
    ud: *mut c_void,
    ptr: *mut c_void,
    osize: usize,
    nsize: usize,
) -> *mut c_void;
#[cfg(target_os = "linux")]
type LuaGetAlloc = unsafe extern "C" fn(*mut LuaState, *mut *mut c_void) -> LuaAlloc;
#[cfg(target_os = "linux")]
type LuaSetAlloc = unsafe extern "C" fn(*mut LuaState, LuaAlloc, *mut c_void);

#[repr(C)]
pub struct LuaState {
    _private: [u8; 0],
}

#[cfg(not(target_os = "linux"))]
extern "C" {
    fn lua_getallocf(state: *mut LuaState, ud: *mut *mut c_void) -> LuaAlloc;
    fn lua_setallocf(state: *mut LuaState, f: LuaAlloc, ud: *mut c_void);
}

#[cfg(target_os = "linux")]
struct LuaApi {
    getallocf: LuaGetAlloc,
    setallocf: LuaSetAlloc,
}

#[cfg(target_os = "linux")]
fn lua_api() -> Option<&'static LuaApi> {
    static API: OnceLock<Option<LuaApi>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let getallocf = libc::dlsym(libc::RTLD_DEFAULT, c"lua_getallocf".as_ptr());
        let setallocf = libc::dlsym(libc::RTLD_DEFAULT, c"lua_setallocf".as_ptr());
        if getallocf.is_null() || setallocf.is_null() {
            ::log::error!(target: "STONE", "Lua allocator API is unavailable in the host process");
            return None;
        }
        Some(LuaApi {
            getallocf: std::mem::transmute::<*mut c_void, LuaGetAlloc>(getallocf),
            setallocf: std::mem::transmute::<*mut c_void, LuaSetAlloc>(setallocf),
        })
    })
    .as_ref()
}

#[cfg(target_os = "linux")]
unsafe fn lua_getallocf(state: *mut LuaState, ud: *mut *mut c_void) -> Option<LuaAlloc> {
    lua_api().map(|api| (api.getallocf)(state, ud))
}

unsafe fn set_lua_allocator(state: *mut LuaState, f: LuaAlloc, ud: *mut c_void) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Some(api) = lua_api() else { return false };
        (api.setallocf)(state, f, ud);
    }
    #[cfg(not(target_os = "linux"))]
    lua_setallocf(state, f, ud);
    true
}

struct LuaAllocatorState {
    state: *mut LuaState,
    allocf: LuaAlloc,
    ud: *mut c_void,
    category: Option<jit_health_abi::Category>,
}

unsafe impl Send for LuaAllocatorState {}
unsafe impl Sync for LuaAllocatorState {}

static LUA_ALLOCATOR_STATE: OnceLock<LuaAllocatorState> = OnceLock::new();
static LUA_COUNTER_ACTIVE: AtomicBool = AtomicBool::new(false);
static LUA_ALLOC_GROSS_REQUESTED: AtomicU64 = AtomicU64::new(0);
static LUA_ALLOC_POSITIVE_GROWTH: AtomicU64 = AtomicU64::new(0);
static LUA_ALLOC_EXPLICIT_FREED: AtomicU64 = AtomicU64::new(0);
static LUA_ALLOC_REALLOC_SHRUNK: AtomicU64 = AtomicU64::new(0);
static LUA_ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static LUA_ALLOC_ALLOCATION_CALLS: AtomicU64 = AtomicU64::new(0);
static LUA_ALLOC_REALLOCATION_CALLS: AtomicU64 = AtomicU64::new(0);
static LUA_ALLOC_FREE_CALLS: AtomicU64 = AtomicU64::new(0);

static LUA_ALLOC_ORIGIN_BYTES: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static LUA_ALLOC_ORIGIN_CALLS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

#[no_mangle]
pub extern "C" fn stone_lua_alloc_split_available() -> c_int {
    LUA_ALLOCATOR_STATE
        .get()
        .is_some_and(|s| s.category.is_some()) as c_int
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_origin_bytes(origin: c_int) -> u64 {
    LUA_ALLOC_ORIGIN_BYTES
        .get(origin as usize)
        .map_or(0, |v| v.load(Ordering::Relaxed))
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_origin_calls(origin: c_int) -> u64 {
    LUA_ALLOC_ORIGIN_CALLS
        .get(origin as usize)
        .map_or(0, |v| v.load(Ordering::Relaxed))
}

unsafe extern "C" fn stone_lua_counting_alloc(
    ud: *mut c_void,
    ptr: *mut c_void,
    osize: usize,
    nsize: usize,
) -> *mut c_void {
    let state = &*(ud as *const LuaAllocatorState);
    if LUA_COUNTER_ACTIVE.load(Ordering::Relaxed) {
        if let Some(category) = state.category {
            let origin = category(state.state.cast()) as usize;
            LUA_ALLOC_ORIGIN_BYTES[origin].fetch_add(nsize as u64, Ordering::Relaxed);
            LUA_ALLOC_ORIGIN_CALLS[origin].fetch_add(1, Ordering::Relaxed);
        }
        LUA_ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        if nsize == 0 {
            if !ptr.is_null() {
                // Only nsize == 0 is an unambiguous free request. A realloc
                // may shrink in place, so its old size is not counted as free.
                LUA_ALLOC_FREE_CALLS.fetch_add(1, Ordering::Relaxed);
                LUA_ALLOC_EXPLICIT_FREED.fetch_add(osize as u64, Ordering::Relaxed);
            }
        } else {
            LUA_ALLOC_GROSS_REQUESTED.fetch_add(nsize as u64, Ordering::Relaxed);
            if ptr.is_null() {
                LUA_ALLOC_ALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
                LUA_ALLOC_POSITIVE_GROWTH.fetch_add(nsize as u64, Ordering::Relaxed);
            } else {
                LUA_ALLOC_REALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
                if nsize >= osize {
                    LUA_ALLOC_POSITIVE_GROWTH.fetch_add((nsize - osize) as u64, Ordering::Relaxed);
                } else {
                    LUA_ALLOC_REALLOC_SHRUNK.fetch_add((osize - nsize) as u64, Ordering::Relaxed);
                }
            }
        }
    }
    // begin supplies the process-lifetime OnceLock value as callback userdata.
    // Allocation callbacks cannot unwind or call back into Lua to report errors.
    (state.allocf)(state.ud, ptr, osize, nsize)
}

/// LuaJIT calls this module loader with the live `lua_State*`. Installing the
/// wrapper here avoids changing the bundled LuaJIT ABI or counting native/GPU
/// allocations that are outside the Lua allocator.
///
/// # Safety
/// A non-null `state` must be a live LuaJIT state on its owning thread. Its
/// allocator and allocator userdata must remain valid for the state's lifetime.
#[no_mangle]
pub unsafe extern "C" fn luaopen_stone_lua_alloc_counter(state: *mut LuaState) -> c_int {
    if state.is_null() {
        return 0;
    }
    jit_health::install(state);
    if LUA_ALLOCATOR_STATE.get().is_some() {
        return 0;
    }
    let mut ud = ptr::null_mut();
    #[cfg(target_os = "linux")]
    let Some(allocf) = lua_getallocf(state, &mut ud) else {
        return 0;
    };
    #[cfg(not(target_os = "linux"))]
    let allocf = lua_getallocf(state, &mut ud);
    let _ = LUA_ALLOCATOR_STATE.set(LuaAllocatorState {
        state,
        allocf,
        ud,
        category: jit_health::category(),
    });
    0
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_counter_begin() -> c_int {
    let Some(state) = LUA_ALLOCATOR_STATE.get() else {
        return 0;
    };
    LUA_ALLOC_GROSS_REQUESTED.store(0, Ordering::Relaxed);
    LUA_ALLOC_POSITIVE_GROWTH.store(0, Ordering::Relaxed);
    LUA_ALLOC_EXPLICIT_FREED.store(0, Ordering::Relaxed);
    LUA_ALLOC_REALLOC_SHRUNK.store(0, Ordering::Relaxed);
    LUA_ALLOC_CALLS.store(0, Ordering::Relaxed);
    LUA_ALLOC_ALLOCATION_CALLS.store(0, Ordering::Relaxed);
    LUA_ALLOC_REALLOCATION_CALLS.store(0, Ordering::Relaxed);
    LUA_ALLOC_FREE_CALLS.store(0, Ordering::Relaxed);
    for counter in LUA_ALLOC_ORIGIN_BYTES
        .iter()
        .chain(LUA_ALLOC_ORIGIN_CALLS.iter())
    {
        counter.store(0, Ordering::Relaxed);
    }
    unsafe {
        if !set_lua_allocator(
            state.state,
            stone_lua_counting_alloc,
            state as *const LuaAllocatorState as *mut c_void,
        ) {
            return 0;
        }
    }
    LUA_COUNTER_ACTIVE.store(true, Ordering::Relaxed);
    1
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_counter_end() -> c_int {
    let Some(state) = LUA_ALLOCATOR_STATE.get() else {
        return 0;
    };
    LUA_COUNTER_ACTIVE.store(false, Ordering::Relaxed);
    unsafe {
        if !set_lua_allocator(state.state, state.allocf, state.ud) {
            return 0;
        }
    }
    1
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_gross_request_kb() -> c_double {
    LUA_ALLOC_GROSS_REQUESTED.load(Ordering::Relaxed) as c_double / 1024.0
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_positive_growth_kb() -> c_double {
    LUA_ALLOC_POSITIVE_GROWTH.load(Ordering::Relaxed) as c_double / 1024.0
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_explicit_free_kb() -> c_double {
    LUA_ALLOC_EXPLICIT_FREED.load(Ordering::Relaxed) as c_double / 1024.0
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_reallocation_shrink_kb() -> c_double {
    LUA_ALLOC_REALLOC_SHRUNK.load(Ordering::Relaxed) as c_double / 1024.0
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_call_count() -> u64 {
    LUA_ALLOC_CALLS.load(Ordering::Relaxed)
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_allocation_call_count() -> u64 {
    LUA_ALLOC_ALLOCATION_CALLS.load(Ordering::Relaxed)
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_reallocation_call_count() -> u64 {
    LUA_ALLOC_REALLOCATION_CALLS.load(Ordering::Relaxed)
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_free_call_count() -> u64 {
    LUA_ALLOC_FREE_CALLS.load(Ordering::Relaxed)
}

use sdl2::event::Event;
use sdl2::keyboard::Mod;
use sdl2::video::GLProfile;

pub mod log;
// Re-exported so the `#[macro_export]` log_once!/log_throttled! macros
// can reach rock-log via `$crate` from any caller crate without that
// crate needing a direct rock-log dependency.
#[doc(hidden)]
pub use rock_log as rock_log_reexport;
mod actor_query;
pub mod asset_registry;
mod asset_watcher;
mod audio;
mod cursor;
mod file_jobs;
mod filesystem;
mod flow_jobs;
mod frame_stream;
mod image_data;
pub mod image_decode;
mod image_publication;
mod image_texture;
mod imgui;
mod input;
mod light_table;
mod logging;
mod meadow;
mod nav;
mod relight_jobs;
mod screenshot;
#[cfg(test)]
mod test_fonts;
pub(crate) use image_texture::{
    image_write_target, live_image_bytes, live_mip_bytes, live_texture_bytes, stone_unload_image,
    LoadedImage, IMAGES, WHITE_IMAGE,
};
mod canvas;
mod clipboard;
#[cfg(target_os = "macos")]
mod display_pacer;
mod gl_state;
mod glyph_raster;
mod gpu_timer;
mod graphics_capture;
mod handles;
mod mesh;
mod mesh_capture;
mod native_ledger;
mod pixel_readback;
mod proc;
mod renderdoc;
mod shader;
mod sprite;
mod stream_buffer;
mod system_report;
mod telemetry;
mod text;
mod ui_submission_probe;
mod uniform_block;
mod visual_bank;
pub mod window_placement;
pub mod window_report;
#[cfg(target_os = "macos")]
mod macos_focus {
    use objc2::runtime::AnyObject;
    use objc2::MainThreadMarker;
    use objc2::{class, msg_send};
    use std::cell::Cell;

    // NSApplicationActivationPolicy values (NSInteger)
    const POLICY_ACCESSORY: i64 = 1;
    const POLICY_REGULAR: i64 = 0;

    // Link AppKit so the ObjC classes (NSApplication, NSWorkspace, etc.) are
    // available via the runtime. No wrappers generated — selectors resolve at
    // call time via msg_send!.
    #[link(name = "AppKit", kind = "framework")]
    extern "C" {}

    thread_local! {
        /// PID of the app frontmost before SDL init. Teardown consumes it only
        /// when Stone still owns frontmost status. 0 means "nothing saved".
        static PREVIOUS_FRONTMOST_PID: Cell<i32> = const { Cell::new(0) };

        /// Latch: have we promoted NSApp from Accessory to Regular yet?
        /// Flipped by `promote_to_regular_once`; guarantees the promotion
        /// only runs on the first user interaction, not every event.
        static PROMOTED_TO_REGULAR: Cell<bool> = const { Cell::new(false) };
    }

    /// Keep NSApp in Accessory policy after SDL init. This removes Stone from
    /// Dock and Cmd-Tab, but still permits explicit programmatic activation.
    pub fn set_accessory_policy() {
        let Some(_mtm) = MainThreadMarker::new() else {
            ::log::warn!(target: "STONE", "not on main thread, skipping accessory policy");
            return;
        };
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            if !app.is_null() {
                // -[NSApplication setActivationPolicy:] returns BOOL, not void —
                // mismatched return type panics under objc2's runtime type-check.
                let _: bool = msg_send![app, setActivationPolicy: POLICY_ACCESSORY];
            }
        }
    }

    /// On the first user interaction with our window (focus-gained event),
    /// promote NSApp from Accessory back to Regular. Regular includes Stone in
    /// Dock and Cmd-Tab. The SDL background hint suppresses launch activation;
    /// FocusGained means the user has chosen Stone, so activation is wanted.
    ///
    /// Activation detail: after flipping policy we also explicitly call
    /// `[NSApp activate]`. Without this, macOS's Cmd-Tab MRU list is not
    /// updated — stone never enters the list, so Cmd-Tab from stone jumps
    /// to whatever the previous app's Cmd-Tab target was (e.g. Chrome)
    /// rather than back to the launching terminal. Explicit activation
    /// inserts stone at MRU[0] and demotes the launcher to MRU[1].
    pub fn promote_to_regular_once() {
        if PROMOTED_TO_REGULAR.with(|c| c.get()) {
            return;
        }
        let Some(_mtm) = MainThreadMarker::new() else {
            return;
        };
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            if app.is_null() {
                return;
            }
            let _: bool = msg_send![app, setActivationPolicy: POLICY_REGULAR];
            // macOS 14+ deprecated activateIgnoringOtherApps; plain activate()
            // is the modern equivalent and updates the MRU correctly.
            let _: () = msg_send![app, activate];
        }
        PROMOTED_TO_REGULAR.with(|c| c.set(true));
    }

    /// Record the frontmost PID before SDL init (SDL activation can change
    /// `frontmostApplication`). Teardown restores it only if Stone remains frontmost.
    pub fn capture_previous_frontmost() {
        if let Some(pid) = frontmost_process_id() {
            PREVIOUS_FRONTMOST_PID.with(|c| c.set(pid));
        }
    }

    /// Restore the captured app only while this process remains frontmost.
    /// Consume the saved PID even when focus has moved elsewhere.
    pub fn restore_previous_frontmost() {
        let pid = PREVIOUS_FRONTMOST_PID.with(|c| c.replace(0));
        if pid == 0 {
            return;
        }
        if frontmost_process_id() != Some(std::process::id() as i32) {
            return;
        }
        unsafe {
            let target: *mut AnyObject = msg_send![
                class!(NSRunningApplication),
                runningApplicationWithProcessIdentifier: pid
            ];
            if target.is_null() {
                return;
            }
            // Accessory policy permits this activation; the ownership check
            // above is what keeps teardown from switching away from another app.
            let _: bool = msg_send![target, activateWithOptions: 0u64];
        }
    }

    fn frontmost_process_id() -> Option<i32> {
        unsafe {
            let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
            if workspace.is_null() {
                return None;
            }
            let frontmost: *mut AnyObject = msg_send![workspace, frontmostApplication];
            if frontmost.is_null() {
                None
            } else {
                Some(msg_send![frontmost, processIdentifier])
            }
        }
    }
}

/// X11 request errors on the window path, and the handler that keeps them from
/// ending the process.
///
/// A reparenting WM (i3) owns a frame window around the client and destroys it
/// when the client withdraws. Two SDL X11 paths then name that dead frame:
/// `X11_SetWindowPosition` reads the client's current parent and calls
/// XTranslateCoordinates through it (SDL_x11window.c:823), and the
/// ConfigureNotify handler does the same while pumping events
/// (SDL_x11events.c:1154). A request that lands inside the teardown therefore
/// gets `BadWindow` on X_TranslateCoords.
///
/// SDL's own process handler cannot absorb that: its video init installs
/// X11_SafetyNetErrHandler with the previous handler (Xlib's `_XDefaultError`)
/// saved, and the safety net ends by calling it -- which prints
/// `X Error of failed request: BadWindow ... 40 (X_TranslateCoords)` and exits
/// the process. A game must not die because a WM replaced its frame, so stone
/// takes the handler for its display: every error is logged through the shared
/// logger, and none of them is fatal.
///
/// Tolerated: BadWindow and BadDrawable, the request named a window that was
/// already gone. On this path that is the WM's frame teardown, which the WM owns
/// and stone cannot prevent. `move_window` retries the move for those.
/// Everything else is logged at error level and left alone: exiting from inside
/// an Xlib callback loses the game with no cleanup and no diagnostics.
#[cfg(target_os = "linux")]
pub mod x11_wm {
    use std::cell::Cell;
    use std::ffi::c_int;
    use std::sync::atomic::{AtomicPtr, Ordering};

    /// One X error seen by the handler.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct XError {
        pub error_code: u8,
        pub request_code: u8,
        pub minor_code: u8,
        pub resource: u64,
    }

    impl XError {
        /// The request named a window that no longer exists: what a WM frame
        /// teardown does to the window SDL translated through.
        pub fn resource_gone(&self) -> bool {
            self.error_code == x11::xlib::BadWindow || self.error_code == x11::xlib::BadDrawable
        }
    }

    impl std::fmt::Display for XError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "code={} ({}), request={} minor={}, resource=0x{:x}",
                self.error_code,
                if self.resource_gone() {
                    "window gone"
                } else {
                    "unexpected"
                },
                self.request_code,
                self.minor_code,
                self.resource,
            )
        }
    }

    thread_local! {
        static LAST_ERROR: Cell<Option<XError>> = const { Cell::new(None) };
        static ERROR_COUNT: Cell<u64> = const { Cell::new(0) };
    }

    fn x11_window(window: &sdl2::video::Window) -> Option<*mut x11::xlib::Display> {
        use sdl2::sys;

        let mut wm_info: sys::SDL_SysWMinfo = unsafe { std::mem::zeroed() };
        unsafe { sys::SDL_GetVersion(&mut wm_info.version) };
        if unsafe { sys::SDL_GetWindowWMInfo(window.raw(), &mut wm_info) }
            == sys::SDL_bool::SDL_FALSE
            || wm_info.subsystem != sys::SDL_SYSWM_TYPE::SDL_SYSWM_X11
        {
            return None;
        }
        let display = unsafe { wm_info.info.x11.display } as *mut x11::xlib::Display;
        if display.is_null() {
            None
        } else {
            Some(display)
        }
    }

    unsafe extern "C" fn handle(
        _display: *mut x11::xlib::Display,
        event: *mut x11::xlib::XErrorEvent,
    ) -> c_int {
        let event = &*event;
        let error = XError {
            error_code: event.error_code,
            request_code: event.request_code,
            minor_code: event.minor_code,
            resource: event.resourceid,
        };
        LAST_ERROR.with(|last| last.set(Some(error)));
        ERROR_COUNT.with(|count| count.set(count.get() + 1));
        if error.resource_gone() {
            // One line per call site: a hide/show race can repeat every frame.
            crate::log_once!("STONE", "X error tolerated (the window manager replaced the frame under the request): {error}");
        } else {
            ::log::error!(target: "STONE", "X error surfaced (not fatal): {error}");
        }
        // Xlib discards an errored request either way; returning 0 is what the
        // default handler does before it exits the process.
        0
    }

    /// Install stone's X error handler for this window's display. Called once
    /// per display, so a re-initialized video subsystem gets it again.
    pub fn install_error_handler(window: &sdl2::video::Window) -> bool {
        static HANDLED_DISPLAY: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
        let Some(display) = x11_window(window) else {
            return false;
        };
        if HANDLED_DISPLAY.load(Ordering::SeqCst) == display as *mut std::ffi::c_void {
            return false;
        }
        unsafe { x11::xlib::XSetErrorHandler(Some(handle)) };
        HANDLED_DISPLAY.store(display as *mut std::ffi::c_void, Ordering::SeqCst);
        true
    }

    /// A marker for [`errors_since`].
    pub fn error_marker() -> u64 {
        ERROR_COUNT.with(|count| count.get())
    }

    /// The last error since `marker`, or `None` when nothing was reported.
    pub fn errors_since(marker: u64) -> Option<XError> {
        if ERROR_COUNT.with(|count| count.get()) == marker {
            None
        } else {
            LAST_ERROR.with(|last| last.get())
        }
    }
}

#[cfg(target_os = "linux")]
pub mod x11_focus {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DeferredMapState {
        Unsupported,
        Pending,
        Viewable,
    }

    fn deferred_map(window: &sdl2::video::Window, request: bool) -> DeferredMapState {
        use sdl2::sys;

        let mut wm_info: sys::SDL_SysWMinfo = unsafe { std::mem::zeroed() };
        unsafe { sys::SDL_GetVersion(&mut wm_info.version) };
        if unsafe { sys::SDL_GetWindowWMInfo(window.raw(), &mut wm_info) }
            == sys::SDL_bool::SDL_FALSE
            || wm_info.subsystem != sys::SDL_SYSWM_TYPE::SDL_SYSWM_X11
        {
            return DeferredMapState::Unsupported;
        }
        let xinfo = unsafe { wm_info.info.x11 };
        let dpy = xinfo.display as *mut x11::xlib::Display;
        if dpy.is_null() {
            return DeferredMapState::Unsupported;
        }
        unsafe {
            let mut attrs: x11::xlib::XWindowAttributes = std::mem::zeroed();
            if x11::xlib::XGetWindowAttributes(dpy, xinfo.window, &mut attrs) != 0
                && attrs.map_state == x11::xlib::IsViewable
            {
                return DeferredMapState::Viewable;
            }
            if request {
                x11::xlib::XMapRaised(dpy, xinfo.window);
                x11::xlib::XFlush(dpy);
            }
        }
        DeferredMapState::Pending
    }

    pub fn request_deferred_map(window: &sdl2::video::Window) -> DeferredMapState {
        deferred_map(window, true)
    }

    pub fn poll_deferred_map(window: &sdl2::video::Window) -> DeferredMapState {
        deferred_map(window, false)
    }

    /// Stamp `_NET_WM_USER_TIME = 0` on a still-hidden SDL window so the
    /// WM maps it without giving it focus.
    ///
    /// EWMH: a zero user-time at map time means "this map was not user-
    /// initiated; do not focus it." Same mechanism as GTK's
    /// gtk_window_set_focus_on_map(FALSE); honored by i3, mutter, kwin.
    /// SDL has no equivalent — SDL2's X11_ShowWindow unconditionally
    /// XMapRaised's, and SDL3 reads SDL_HINT_WINDOW_ACTIVATE_WHEN_SHOWN on
    /// X11 only to `(void)`-discard it. So: create hidden, stamp, show.
    ///
    /// The zero only suppresses focus on the *initial* map. SDL updates
    /// the property with a real timestamp on the first user input, so
    /// clicking the window focuses it normally from then on.
    ///
    /// No-op on Wayland (compositors don't let clients steal focus) and on
    /// WMs that predate the EWMH user-time check (they just focus the
    /// window — today's behavior, nothing worse).
    pub fn set_user_time_zero(window: &sdl2::video::Window) {
        use sdl2::sys;

        // Query the native handles via SDL_GetWindowWMInfo directly. sdl2's
        // raw-window-handle wrappers run this same query but panic!() when it
        // fails (e.g. SDL fell back to a non-X11 video driver under a
        // degraded/SSH X session), and the unwind out of our extern "C"
        // caller would hard-abort. The raw call just returns SDL_FALSE.
        let mut wm_info: sys::SDL_SysWMinfo = unsafe { std::mem::zeroed() };
        unsafe { sys::SDL_GetVersion(&mut wm_info.version) };
        if unsafe { sys::SDL_GetWindowWMInfo(window.raw(), &mut wm_info) }
            == sys::SDL_bool::SDL_FALSE
        {
            ::log::warn!(target: "STONE", "focus: window WM info unavailable (non-X11 or degraded video driver); window may steal focus");
            return;
        }
        if wm_info.subsystem != sys::SDL_SYSWM_TYPE::SDL_SYSWM_X11 {
            return; // not X11 (Wayland): nothing to do
        }
        let xinfo = unsafe { wm_info.info.x11 };
        let dpy = xinfo.display as *mut x11::xlib::Display;
        if dpy.is_null() {
            ::log::warn!(target: "STONE", "focus: X11 WM info has null display; window may steal focus");
            return;
        }
        unsafe {
            let atom = x11::xlib::XInternAtom(dpy, c"_NET_WM_USER_TIME".as_ptr(), 0);
            // Format-32 property data is an array of C long per Xlib ABI.
            let zero: std::ffi::c_ulong = 0;
            x11::xlib::XChangeProperty(
                dpy,
                xinfo.window,
                atom,
                x11::xlib::XA_CARDINAL,
                32,
                x11::xlib::PropModeReplace,
                &zero as *const _ as *const u8,
                1,
            );
            // Same connection SDL maps through, so the property is queued
            // ahead of the MapWindow request — no XSync needed.
        }
    }

    /// Mark the window's position as USER-specified (USPosition), not merely
    /// program-specified, so the WM honors it on map instead of relocating the
    /// window onto the pointer's monitor ("place on active monitor" policy --
    /// the multi-monitor drift). Must be set before the map. Preserves SDL's
    /// other size hints (min/max, resize increments) by editing the existing
    /// set rather than replacing it.
    pub fn pin_user_position(window: &sdl2::video::Window, x: i32, y: i32) {
        use sdl2::sys;

        let mut wm_info: sys::SDL_SysWMinfo = unsafe { std::mem::zeroed() };
        unsafe { sys::SDL_GetVersion(&mut wm_info.version) };
        if unsafe { sys::SDL_GetWindowWMInfo(window.raw(), &mut wm_info) }
            == sys::SDL_bool::SDL_FALSE
        {
            return;
        }
        if wm_info.subsystem != sys::SDL_SYSWM_TYPE::SDL_SYSWM_X11 {
            return; // not X11 (Wayland): nothing to do
        }
        let xinfo = unsafe { wm_info.info.x11 };
        let dpy = xinfo.display as *mut x11::xlib::Display;
        if dpy.is_null() {
            return;
        }
        unsafe {
            let hints = x11::xlib::XAllocSizeHints();
            if hints.is_null() {
                return;
            }
            // Fold into the existing hints so SDL's size constraints survive.
            let mut supplied: std::ffi::c_long = 0;
            x11::xlib::XGetWMNormalHints(dpy, xinfo.window, hints, &mut supplied);
            (*hints).flags |= x11::xlib::USPosition | x11::xlib::PPosition;
            (*hints).x = x;
            (*hints).y = y;
            x11::xlib::XSetWMNormalHints(dpy, xinfo.window, hints);
            x11::xlib::XFree(hints as *mut std::ffi::c_void);
        }
    }
}

/// Attempts a single window move gets before stone leaves the position to the
/// WM.
const MOVE_ATTEMPTS: u32 = 3;

/// Move the window through the platform position path.
///
/// X11: SDL reads the window's current parent and translates through it, which
/// on a reparenting WM can name the frame the WM is tearing down (see
/// `x11_wm`). That error is tolerated and logged there; a move that only hit it
/// is repeated so the position request still reaches the WM.
fn move_window(window: &mut sdl2::video::Window, x: i32, y: i32) {
    #[cfg(target_os = "linux")]
    {
        for attempt in 1..=MOVE_ATTEMPTS {
            let marker = x11_wm::error_marker();
            window.set_position(
                sdl2::video::WindowPos::Positioned(x),
                sdl2::video::WindowPos::Positioned(y),
            );
            match x11_wm::errors_since(marker) {
                None => return,
                // Non-window errors are already surfaced by the handler.
                Some(error) if !error.resource_gone() => return,
                Some(error) if attempt == MOVE_ATTEMPTS => {
                    ::log::error!(target: "STONE", "window move to ({},{}) still named a destroyed window after {} attempts ({}); leaving the position to the WM", x, y, MOVE_ATTEMPTS, error);
                    return;
                }
                Some(_) => {
                    ::log::debug!(target: "STONE", "window move to ({},{}) hit a destroyed WM frame; moving again", x, y);
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        window.set_position(
            sdl2::video::WindowPos::Positioned(x),
            sdl2::video::WindowPos::Positioned(y),
        );
    }
}

pub(crate) use stone_gl::{compile_shader, link_program, DebugGl};
use stone_gl::{context_request, GlContextRequest, GlHost, GlProfile};

pub use logging::StoneLogLevel;

struct PendingShow {
    pos: (i32, i32),
    map_requested: bool,
    activate: bool,
}

// =============================================================================
// Types matching stone.h
// =============================================================================

/// Opaque window handle - wraps SDL2 window + GL context + glow
pub struct StoneWindow {
    _sdl_context: sdl2::Sdl,
    _video: sdl2::VideoSubsystem,
    pub(crate) window: sdl2::video::Window,
    _gl_context: sdl2::video::GLContext,
    gl: Rc<DebugGl>,
    event_pump: RefCell<sdl2::EventPump>,
    controller_manager: RefCell<input::SdlControllerManager>,
    // A windowed window is created hidden and shown only after init, at this
    // position -- a mapped X11 window is relocated by the WM to the pointer's
    // monitor during the event-starved init, so we keep it unmapped until we
    // can map it at the right spot with the loop already pumping. Some => a
    // deferred show is still pending; None once shown (or never deferred).
    pending_show: Option<PendingShow>,
    // Retained runtime visibility. This is the LIVE answer to "is the window
    // visible", not a startup environment variable: headless boot initializes
    // it to hidden, runtime hide/show flips it, and rendering, input, and
    // captures keep working either way. Read through stone_is_window_hidden.
    runtime_hidden: bool,
    // Window title stashed at creation so geometry save and restore know the game key.
    title: String,
    // Current display mode (windowed, maximized, fullscreen).
    window_mode: Cell<window_placement::WindowMode>,
    // Last normal windowed bounds (for returning from fullscreen/maximized or across hide/show).
    windowed_bounds: Cell<window_placement::WindowedBounds>,
    // Captured window state when hidden.
    saved_state_on_hide: RefCell<Option<window_placement::HiddenWindowState>>,
    // Whether this process started hidden and has never been shown yet.
    initial_hidden_boot: Cell<bool>,
    // The size the GAME asked for (GameConfig), not the size the window ended
    // up at. Saved alongside the geometry so a later launch can tell a
    // remembered size from a stale one.
    requested_size: (i32, i32),
    requested_pos: (i32, i32),
    requested_display: Option<i32>,
    // Machine config `imgui_scale`, read once with the geometry at creation.
    config_imgui_scale: Option<f32>,
    // Raw SDL pointers for cimgui -- its only reader, so they compile out
    // with the feature.
    sdl_window_raw: *mut c_void,
    gl_context_raw: *mut c_void,
}

fn apply_gl_context_request(gl_attr: &sdl2::video::gl_attr::GLAttr<'_>, request: GlContextRequest) {
    gl_attr.set_context_profile(match request.profile {
        GlProfile::Core => GLProfile::Core,
    });
    gl_attr.set_context_version(request.major, request.minor);
}

/// Event types
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoneEventType {
    None = 0,
    Quit = 1,
    KeyDown = 2,
    KeyUp = 3,
    MouseButtonDown = 4,
    MouseButtonUp = 5,
    MouseWheel = 6,
    PadDeviceAdded = 7,
    PadDeviceRemoved = 8,
    PadActiveChanged = 9,
    PadButtonDown = 10,
    PadButtonUp = 11,
    PadAxisMotion = 12,
    TextInput = 13,
}

const STONE_TEXT_INPUT_BYTES: usize = 32;

/// Event structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct StoneEvent {
    pub event_type: StoneEventType,
    pub key: c_int,
    pub scancode: c_int,
    pub modifiers: c_int,
    pub repeat: bool,
    pub mouse_button: c_int,
    pub mouse_x: c_int,
    pub mouse_y: c_int,
    pub pad_instance_id: c_int,
    pub pad_button: *const c_char,
    pub pad_axis: *const c_char,
    pub pad_value: c_int,
    pub pad_sync: bool,
    pub text: [c_char; STONE_TEXT_INPUT_BYTES],
}

#[cfg(test)]
impl StoneEvent {
    fn text(&self) -> &str {
        unsafe { CStr::from_ptr(self.text.as_ptr()) }
            .to_str()
            .unwrap_or("")
    }
}

impl Default for StoneEvent {
    fn default() -> Self {
        Self {
            event_type: StoneEventType::None,
            key: 0,
            scancode: 0,
            modifiers: 0,
            repeat: false,
            mouse_button: 0,
            mouse_x: 0,
            mouse_y: 0,
            pad_instance_id: -1,
            pad_button: ptr::null(),
            pad_axis: ptr::null(),
            pad_value: 0,
            pad_sync: false,
            text: [0; STONE_TEXT_INPUT_BYTES],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct StoneString {
    pub data: *const c_char,
    pub len: usize,
}

#[cfg(any(target_os = "linux", test))]
fn native_video_driver_error(
    headless: bool,
    requested: Option<&str>,
    actual: &str,
) -> Option<String> {
    (!headless && requested.is_none() && matches!(actual, "offscreen" | "dummy")).then(|| {
        format!(
            "SDL silently selected the `{actual}` video driver for an interactive run; check the window-system runtime libraries or set SDL_VIDEODRIVER explicitly"
        )
    })
}

const STONE_MOD_CTRL: c_int = 1;
const STONE_MOD_SHIFT: c_int = 2;
const STONE_MOD_ALT: c_int = 4;
const STONE_MOD_GUI: c_int = 8;

fn normalized_key_modifiers(keymod: Mod) -> c_int {
    let mut modifiers = 0;
    if keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD) {
        modifiers |= STONE_MOD_CTRL;
    }
    if keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD) {
        modifiers |= STONE_MOD_SHIFT;
    }
    if keymod.intersects(Mod::LALTMOD | Mod::RALTMOD) {
        modifiers |= STONE_MOD_ALT;
    }
    if keymod.intersects(Mod::LGUIMOD | Mod::RGUIMOD) {
        modifiers |= STONE_MOD_GUI;
    }
    modifiers
}

fn stone_event_from_sdl(event: &Event) -> Option<StoneEvent> {
    let mut translated = StoneEvent::default();
    match event {
        Event::Quit { .. } => translated.event_type = StoneEventType::Quit,
        Event::KeyDown {
            keycode,
            scancode,
            keymod,
            repeat,
            ..
        } => {
            translated.event_type = StoneEventType::KeyDown;
            translated.key = keycode.map(|value| value.into_i32()).unwrap_or(0);
            translated.scancode = scancode.map(|value| value as c_int).unwrap_or(0);
            translated.modifiers = normalized_key_modifiers(*keymod);
            translated.repeat = *repeat;
        }
        Event::KeyUp {
            keycode,
            scancode,
            keymod,
            repeat,
            ..
        } => {
            translated.event_type = StoneEventType::KeyUp;
            translated.key = keycode.map(|value| value.into_i32()).unwrap_or(0);
            translated.scancode = scancode.map(|value| value as c_int).unwrap_or(0);
            translated.modifiers = normalized_key_modifiers(*keymod);
            translated.repeat = *repeat;
        }
        Event::MouseButtonDown {
            mouse_btn, x, y, ..
        } => {
            translated.event_type = StoneEventType::MouseButtonDown;
            translated.mouse_button = *mouse_btn as c_int;
            translated.mouse_x = *x;
            translated.mouse_y = *y;
        }
        Event::MouseButtonUp {
            mouse_btn, x, y, ..
        } => {
            translated.event_type = StoneEventType::MouseButtonUp;
            translated.mouse_button = *mouse_btn as c_int;
            translated.mouse_x = *x;
            translated.mouse_y = *y;
        }
        Event::MouseWheel { y, .. } => {
            translated.event_type = StoneEventType::MouseWheel;
            translated.mouse_y = *y;
        }
        Event::TextInput { text, .. } => {
            translated.event_type = StoneEventType::TextInput;
            let bytes = text.as_bytes();
            let count = bytes.len().min(STONE_TEXT_INPUT_BYTES - 1);
            for (dst, src) in translated.text[..count].iter_mut().zip(&bytes[..count]) {
                *dst = *src as c_char;
            }
        }
        _ => return None,
    }
    Some(translated)
}

/// Graphics statistics
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct GraphicsStats {
    pub draw_calls: c_int,
    pub text_draw_calls: c_int,
    pub canvas_switches: c_int,
    pub shader_switches: c_int,
    pub stream_upload_calls: c_int,
    pub stream_storage_replacements: c_int,
    pub stream_upload_bytes: c_int,
    pub streamed_vertices: c_int,
    pub retained_upload_calls: c_int,
    pub retained_upload_bytes: c_int,
    pub retained_uploaded_vertices: c_int,
    pub images: c_int,
    pub canvases: c_int,
    pub texture_memory_mib: c_int,
    pub instanced_draws: c_int,
}

// =============================================================================
// Global State
// =============================================================================

/// Per-frame statistics tracking
#[derive(Default)]
struct FrameStats {
    draw_calls: i32,
    text_draw_calls: i32,
    canvas_switches: i32,
    shader_switches: i32,
    stream_upload_calls: i32,
    stream_storage_replacements: i32,
    stream_upload_bytes: i32,
    streamed_vertices: i32,
    retained_upload_calls: i32,
    retained_upload_bytes: i32,
    retained_uploaded_vertices: i32,
    instanced_draws: i32,
}

// We use thread-local storage for the window since SDL2 isn't thread-safe anyway
thread_local! {
    pub(crate) static WINDOW: RefCell<Option<Box<StoneWindow>>> = const { RefCell::new(None) };
    static GPU_HOST_LIFETIME: Cell<GpuHostLifetime> = const { Cell::new(GpuHostLifetime::Unclaimed) };
    pub(crate) static SHADERS: RefCell<HashMap<u64, shader::Shader>> = RefCell::new(HashMap::new());
    pub(crate) static CURRENT_SHADER: RefCell<Option<glow::Program>> = const { RefCell::new(None) };
    static FRAME_STATS: RefCell<FrameStats> = RefCell::new(FrameStats::default());
    static GPU_TIMER: RefCell<gpu_timer::GpuTimer> = RefCell::new(gpu_timer::GpuTimer::new());
    // Borders reported at window creation, re-checked once after the first
    // present (X11 _NET_FRAME_EXTENTS can be 0 until the window is mapped).
    // Set only on a visible window, so headless runs stay quiet.
    static INITIAL_BORDERS: std::cell::Cell<Option<window_report::Borders>> = const { std::cell::Cell::new(None) };
    static POST_FRAME_BORDERS_LOGGED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // Window-change flags set during event polling, drained once per frame by
    // the runner. Each bit is take-and-clear.
    static WINDOW_CHANGE_FLAGS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Copy, PartialEq)]
enum GpuHostLifetime {
    Unclaimed,
    Live,
    Lost,
}

fn release_native_window() {
    WINDOW.with(|window| {
        let mut window = window.borrow_mut();
        if window.is_some() {
            GPU_HOST_LIFETIME.with(|host| {
                if host.get() == GpuHostLifetime::Live {
                    host.set(GpuHostLifetime::Lost);
                }
            });
        }
        *window = None;
    });
}

/// Borrow the process GPU context, creating a hidden host only on first use.
/// A lost host cannot be recreated: its callers retain context-owned resources.
#[no_mangle]
pub extern "C" fn stone_gpu_host_acquire() -> *mut StoneWindow {
    if GPU_HOST_LIFETIME.with(|host| host.get() == GpuHostLifetime::Lost) {
        ::log::error!(target: "STONE", "gpu host: native GL context was destroyed; restart the process before using context-owned resources");
        return ptr::null_mut();
    }
    let window = with_window(|window| window as *const StoneWindow as *mut StoneWindow)
        .unwrap_or_else(|| {
            if !stone_init() {
                ::log::error!(target: "STONE", "gpu host: SDL initialization failed");
                return ptr::null_mut();
            }
            stone_create_window_ex(c"Stone GPU host".as_ptr(), 0, 0, 320, 240, 0, false, 0)
        });
    if window.is_null() {
        ::log::error!(target: "STONE", "gpu host: hidden GL window creation failed");
        return window;
    }
    GPU_HOST_LIFETIME.with(|host| host.set(GpuHostLifetime::Live));
    if !sprite::stone_sprite_init() {
        ::log::error!(target: "STONE", "gpu host: GL sprite system failed to initialize");
        return ptr::null_mut();
    }
    window
}

const WC_RESIZED: u32 = 1;
const WC_MOVED: u32 = 2;
const WC_DISPLAY: u32 = 4;
const SDL_WINDOWPOS_CENTERED: c_int = 0x2fff0000;

fn mark_window_change(bit: u32) {
    WINDOW_CHANGE_FLAGS.with(|c| c.set(c.get() | bit));
}

/// Take-and-clear one window-change bit.
fn take_window_change(bit: u32) -> bool {
    WINDOW_CHANGE_FLAGS.with(|c| {
        let f = c.get();
        c.set(f & !bit);
        f & bit != 0
    })
}

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static GL_SWAP_COUNT: AtomicU64 = AtomicU64::new(0);
// A programmatic `stone_quit()` sets this; the runner's event poll reads it
// (stone_quit_requested) to end the loop after the CURRENT frame finishes.
// Deferred so a quit from an @late_draw system (pause-menu "Quit") does not
// tear down the window mid-frame -- the destructive teardown is stone_teardown(),
// run once after the loop exits.
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn with_window<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&StoneWindow) -> R,
{
    WINDOW.with(|w| w.borrow().as_ref().map(|window| f(window)))
}

pub(crate) fn with_gl<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&DebugGl) -> R,
{
    with_window(|w| f(&w.gl))
}

// =============================================================================
// Core Functions
// =============================================================================

/// Initialize Stone
#[no_mangle]
pub extern "C" fn stone_init() -> bool {
    logging::stone_log_init();
    crate::renderdoc::init();
    QUIT_REQUESTED.store(false, Ordering::SeqCst);
    INITIALIZED.store(true, Ordering::SeqCst);
    true
}

/// SDL's current video driver name (e.g. "x11", "offscreen"), for sims that
/// skip what the offscreen driver cannot do.
#[no_mangle]
pub extern "C" fn stone_sdl_video_driver() -> *const c_char {
    unsafe { sdl2::sys::SDL_GetCurrentVideoDriver() as *const c_char }
}

/// Request quit. Only sets the deferred flag -- it does NOT tear down the
/// window / GL context / ImGui. This is the game-facing quit call, safe from
/// ANY system (@update, @draw, @late_draw): the current frame finishes
/// rendering against a live context, the runner's event poll sees the flag
/// next frame and ends the loop, and the loop-exit path runs stone_teardown()
/// once. Calling this mid-frame used to destroy the window synchronously,
/// crashing the rest of the frame (the pause-menu "Quit" segfault).
#[no_mangle]
pub extern "C" fn stone_quit() {
    QUIT_REQUESTED.store(true, Ordering::SeqCst);
}

/// Whether a programmatic quit has been requested. The runner's event poll
/// reads this to end the main loop cleanly -- there is no SDL Quit event in
/// that path, so without this the loop would spin until the frame budget runs
/// out.
#[no_mangle]
pub extern "C" fn stone_quit_requested() -> bool {
    QUIT_REQUESTED.load(Ordering::SeqCst)
}

/// End the process's window and GL context. Every registry that holds a GL
/// name or a handle into the context is released here, while the context is
/// still current, so nothing outlives the context it belongs to. Run ONCE
/// after the main loop exits (stone_shutdown_window), never mid-frame.
/// Idempotent: safe to call when already torn down or never initialized.
#[no_mangle]
pub extern "C" fn stone_teardown() -> bool {
    crate::flow_jobs::stone_flow_shutdown();
    crate::relight_jobs::shutdown();
    crate::nav::shutdown();
    crate::renderdoc::on_teardown();
    input::test_virtual_pad_cleanup();
    // ImGui's OpenGL3 backend shutdown needs the live GL context. Idempotent:
    // early-returns if ImGui was never initialized.
    crate::imgui::stone_imgui_shutdown();
    crate::pixel_readback::release_all();
    // An SDL_Cursor must be freed before SDL_Quit, and dropping WINDOW drops
    // the sdl2::Sdl context.
    crate::cursor::free_all_cursors();
    text::stone_text_shutdown();
    sprite::stone_sprite_shutdown();
    with_gl(stream_buffer::release_all);
    shader::destroy_all();
    canvas::destroy_all();
    image_texture::unload_all();
    gl_state::forget_samplers();
    mesh::clear_all();
    GPU_TIMER.with(|timer| *timer.borrow_mut() = gpu_timer::GpuTimer::new());
    release_native_window();
    INITIALIZED.store(false, Ordering::SeqCst);
    QUIT_REQUESTED.store(false, Ordering::SeqCst);

    // Return focus to the captured app only while Stone remains frontmost.
    #[cfg(target_os = "macos")]
    macos_focus::restore_previous_frontmost();
    true
}

#[no_mangle]
pub extern "C" fn stone_is_initialized() -> bool {
    INITIALIZED.load(Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn stone_test_virtual_pad_attach() -> c_int {
    input::test_virtual_pad_attach()
}

#[no_mangle]
pub extern "C" fn stone_test_virtual_pad_button(
    index: c_int,
    token: *const c_char,
    down: bool,
) -> bool {
    input::test_virtual_pad_button(index, token, down)
}

#[no_mangle]
pub extern "C" fn stone_test_virtual_pad_axis(
    index: c_int,
    token: *const c_char,
    value: c_int,
) -> bool {
    input::test_virtual_pad_axis(index, token, value)
}

#[no_mangle]
pub extern "C" fn stone_test_virtual_pad_detach(index: c_int) -> bool {
    input::test_virtual_pad_detach(index)
}

#[no_mangle]
pub extern "C" fn stone_test_virtual_pad_cleanup() {
    input::test_virtual_pad_cleanup();
}

#[no_mangle]
pub extern "C" fn stone_pad_connected() -> bool {
    WINDOW.with(|window| {
        window
            .borrow()
            .as_ref()
            .map(|window| window.controller_manager.borrow().connected())
            .unwrap_or(false)
    })
}

#[no_mangle]
pub extern "C" fn stone_pad_type_name() -> *const c_char {
    let type_name = WINDOW.with(|window| {
        window
            .borrow()
            .as_ref()
            .and_then(|window| window.controller_manager.borrow().active_type_name())
    });
    match type_name {
        Some("xbox") => c"xbox".as_ptr(),
        Some("playstation") => c"playstation".as_ptr(),
        Some("nintendo") => c"nintendo".as_ptr(),
        Some("virtual") => c"virtual".as_ptr(),
        Some("unknown") => c"unknown".as_ptr(),
        _ => ptr::null(),
    }
}

#[cfg(any(test, target_os = "linux"))]
fn sdl_graphics_driver_paths(
    search_path: &std::ffi::OsStr,
) -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
    let directories = std::env::split_paths(search_path).collect::<Vec<_>>();
    let find = |name: &str| {
        directories
            .iter()
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    };
    (find("libGL.so.1"), find("libEGL.so.1"))
}

#[cfg(target_os = "linux")]
fn configure_sdl_graphics_drivers() {
    let Some(search_path) = std::env::var_os("NIX_LD_LIBRARY_PATH") else {
        return;
    };
    let (gl, egl) = sdl_graphics_driver_paths(&search_path);
    if std::env::var_os("SDL_VIDEO_GL_DRIVER").is_none() {
        if let Some(path) = gl.and_then(|path| path.into_os_string().into_string().ok()) {
            std::env::set_var("SDL_VIDEO_GL_DRIVER", path);
        }
    }
    if std::env::var_os("SDL_VIDEO_EGL_DRIVER").is_none() {
        if let Some(path) = egl.and_then(|path| path.into_os_string().into_string().ok()) {
            std::env::set_var("SDL_VIDEO_EGL_DRIVER", path);
        }
    }
}

/// Create a window with OpenGL context at the given position on the given
/// display. Pass SDL_WINDOWPOS_CENTERED (0x2FFF0000) for x/y to let SDL
/// auto-center on the chosen display.
#[no_mangle]
pub extern "C" fn stone_create_window_ex(
    title: *const c_char,
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
    _display: c_int,
    resizable: bool,
    fullscreen: c_int,
) -> *mut StoneWindow {
    let title_str = if title.is_null() {
        "Stone"
    } else {
        unsafe { CStr::from_ptr(title).to_str().unwrap_or("Stone") }
    };

    // Single-window engine: the GPU host owns the process window and SDL's
    // EventPump is a singleton. Reject a second window here, before a new GL
    // context would orphan the live one and break subsequent GL calls.
    if WINDOW.with(|w| w.borrow().is_some()) {
        ::log::error!(target: "STONE", "stone_create_window_ex: a window already exists; the engine supports one window per process (use stone_gpu_host_ensure for shared GPU access)");
        return ptr::null_mut();
    }

    // Don't steal focus from the user's current window when we open.
    //
    // Per-platform mechanism:
    //
    //   * macOS — SDL's Cocoa backend intentionally steals focus:
    //     `Cocoa_RegisterApp` calls `[NSApp setActivationPolicy:Regular]`
    //     and `applicationDidFinishLaunching` calls `[NSApp
    //     activateIgnoringOtherApps:YES]` (after briefly activating the
    //     Dock). Both are gated on `SDL_HINT_MAC_BACKGROUND_APP`; setting
    //     the hint to "1" before SDL init suppresses them at the source.
    //     See SDL_cocoaevents.m:289 and :461 in SDL 2.30.x.
    //
    //   * Windows — `SDL_HINT_WINDOW_NO_ACTIVATION_WHEN_SHOWN` (SDL2,
    //     implemented only in the Windows backend).
    //
    //   * Linux — no SDL mechanism exists (the Windows hint above is a
    //     no-op on X11, verified in tools/focus-experiment); mechanism
    //     and Wayland story: `x11_focus::set_user_time_zero`.
    //
    // Accessory policy removes Stone from Dock and Cmd-Tab after SDL init.
    // It still permits explicit activation, so teardown checks ownership.
    //
    // Opt out entirely with STONE_FOCUS_ON_SHOW=1.
    let prevent_focus_steal = std::env::var("STONE_FOCUS_ON_SHOW").is_err();
    if prevent_focus_steal {
        sdl2::hint::set("SDL_MAC_BACKGROUND_APP", "1");
        sdl2::hint::set("SDL_WINDOW_NO_ACTIVATION_WHEN_SHOWN", "1");

        // Capture who was frontmost BEFORE SDL runs, so we can hand focus
        // back on shutdown instead of letting macOS pick an arbitrary app
        // (usually Finder) based on window z-order.
        #[cfg(target_os = "macos")]
        macos_focus::capture_previous_frontmost();
    }

    sdl2::hint::set("SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS", "1");

    // SDL's default minimizes an exclusive-fullscreen window on focus loss,
    // leaving fullscreen first. A tiling WM such as i3 ignores the minimize,
    // so the window stays mapped at its windowed size and never gets the
    // restore event that would re-enter fullscreen; with focus-follows-mouse
    // that happens whenever the cursor leaves the window.
    sdl2::hint::set("SDL_VIDEO_MINIMIZE_ON_FOCUS_LOSS", "0");

    // nix-ld's path resolves ELF dependencies but SDL's dlopen calls do not
    // search it, so give SDL the exact GLVND libraries before video init.
    #[cfg(target_os = "linux")]
    configure_sdl_graphics_drivers();

    // Stable WM_CLASS for window-manager rules (i3 `for_window [class="stone"]`);
    // without it X11 windows inherit the binary name ("luajit-2.1.<ver>").
    // Env var, not sdl2::hint::set (the X11 backend reads it via SDL_getenv),
    // and it must be set before video-subsystem init -- X11_VideoInit captures
    // the classname; by window-creation time it's already latched.
    if std::env::var_os("SDL_VIDEO_X11_WMCLASS").is_none() {
        std::env::set_var("SDL_VIDEO_X11_WMCLASS", "stone");
    }

    // Initialize SDL
    let t_boot = std::time::Instant::now();
    let sdl_context = match sdl2::init() {
        Ok(ctx) => ctx,
        Err(e) => {
            ::log::error!(target: "STONE", "failed to init sdl: {}", e);
            return ptr::null_mut();
        }
    };

    let t_sdl_init = t_boot.elapsed();
    let video = match sdl_context.video() {
        Ok(v) => v,
        Err(e) => {
            ::log::error!(target: "STONE", "failed to init video: {}", e);
            return ptr::null_mut();
        }
    };

    #[cfg(target_os = "linux")]
    {
        let requested = std::env::var("SDL_VIDEODRIVER").ok();
        if let Some(message) = native_video_driver_error(
            std::env::var_os("STONE_HEADLESS").is_some(),
            requested.as_deref(),
            video.current_video_driver(),
        ) {
            ::log::error!(target: "STONE", "{message}");
            return ptr::null_mut();
        }
    }

    let t_video = t_boot.elapsed();
    // (experiment) Skip Cocoa Accessory override to test hint-only behavior.
    #[cfg(target_os = "macos")]
    if prevent_focus_steal && std::env::var("STONE_COCOA_POLICY_OFF").is_err() {
        macos_focus::set_accessory_policy();
    }

    let gl_attr = video.gl_attr();
    let gl_host = if cfg!(target_os = "macos") {
        GlHost::AppleDesktop
    } else {
        GlHost::Desktop
    };
    let gl_request = context_request(gl_host);
    apply_gl_context_request(&gl_attr, gl_request);

    // STONE_GL_DEBUG=1: request a debug context so the driver emits
    // richer KHR_debug diagnostics (deprecated usage, perf warnings,
    // undefined behavior) through the callback installed below. The
    // flag is ignored on platforms that can't honor it (macOS/CGL),
    // and the callback install is separately gated on the extension
    // actually being present.
    let gl_debug = std::env::var("STONE_GL_DEBUG").as_deref() == Ok("1");
    if gl_debug {
        gl_attr.set_context_flags().debug().set();
    }

    // Headless mode: hide the window (screenshots still work via gl.read_pixels)
    let headless = std::env::var("STONE_HEADLESS").is_ok();
    let fit_enabled = window_fit_enabled(headless);

    // Boot window mode: 1 = borderless-desktop, 2 = exclusive. When set (and not
    // headless) the window is born in that mode ON the requested display, shown
    // only after the mode is applied -- so it never flashes windowed on the
    // wrong monitor before the game's display-settings apply catches up.
    // Headless always renders windowed at the requested size (portable shots).
    let boot_fs = if headless {
        None
    } else {
        match fullscreen {
            1 => Some(sdl2::video::FullscreenType::Desktop),
            2 => Some(sdl2::video::FullscreenType::True),
            _ => None,
        }
    };
    let born_fullscreen = boot_fs.is_some();

    // A plain windowed window is created hidden and shown only after init
    // (stone_show_window_if_deferred, once the frame loop is live). A tiling WM
    // (i3) maps a floating window on the *focused* output regardless of the
    // requested position, so we map it late and re-assert the position after
    // the map -- which i3 does honor -- landing it on the intended monitor with
    // no visible relocation. Linux-only: macOS/Windows honor the create-time
    // position, so they keep showing immediately. Fullscreen is pinned to its
    // display by the WM (no defer); headless never shows. STONE_IMMEDIATE_SHOW=1
    // opts out.
    let defer_windowed_show = cfg!(target_os = "linux")
        && !headless
        && !born_fullscreen
        && std::env::var_os("STONE_IMMEDIATE_SHOW").is_none();

    // Remembered placement: env > saved > machine config > the
    // passed game default. Headless renders exactly what was requested, for
    // portable snapshots -- so it ignores REMEMBERED geometry (saved file,
    // machine config) but still honors an explicit STONE_WINDOW size, which is
    // the caller requesting rather than the machine remembering. Position and
    // display mean nothing to a window that is never shown.
    let base = window_placement::ResolvedGeometry {
        size: (width, height),
        pos: (x, y),
        display: if _display >= 0 { Some(_display) } else { None },
    };
    // The hand-written machine config. Headless ignores it, like saved geometry:
    // a portable snapshot must not depend on which machine took it.
    let machine = if headless {
        window_placement::MachineConfig::default()
    } else {
        window_placement::machine_config(&window_placement::game_slug(title_str))
    };
    let resolved = if headless {
        window_placement::ResolvedGeometry {
            size: headless_window_size(base.size),
            ..base
        }
    } else if born_fullscreen {
        // Fullscreen fills a whole display, so windowed placement memory is
        // irrelevant. Target the requested display's usable origin (the windowed
        // rect the mode restores to lands there); saved geometry stays reserved
        // for the windowed mode, restored on the next windowed boot.
        let displays = gather_display_reports(&video);
        let d = displays
            .iter()
            .find(|d| Some(d.index) == base.display)
            .or_else(|| displays.first());
        let pos = d.map(|d| (d.usable.0, d.usable.1)).unwrap_or((x, y));
        window_placement::ResolvedGeometry {
            size: (width, height),
            pos,
            display: base.display,
        }
    } else {
        resolve_window_geometry(title_str, base, &machine, &video)
    };
    let centered =
        resolved.pos.0 == SDL_WINDOWPOS_CENTERED && resolved.pos.1 == SDL_WINDOWPOS_CENTERED;

    // Pre-clamp size (and position, when explicit) against usable_bounds before
    // the window exists -- borders are unknown here, so zero; the
    // post-create pass below re-fits with real/modeled borders.
    let mut build_w = resolved.size.0;
    let mut build_h = resolved.size.1;
    let mut build_x = resolved.pos.0;
    let mut build_y = resolved.pos.1;
    if fit_enabled && !born_fullscreen {
        let displays = gather_display_reports(&video);
        let req = window_report::WindowRequest {
            size: resolved.size,
            pos: if centered { (0, 0) } else { resolved.pos },
            display: resolved.display,
        };
        let p = window_report::resolve_window_placement(req, &displays, ZERO_BORDERS);
        if p.size != resolved.size {
            ::log::info!(
                target: "STONE",
                "window fit (pre-clamp): size {}x{} -> {}x{}",
                resolved.size.0, resolved.size.1, p.size.0, p.size.1,
            );
        }
        build_w = p.size.0;
        build_h = p.size.1;
        if !centered {
            build_x = p.pos.0;
            build_y = p.pos.1;
        }
    }

    // Create window with explicit position (match libstone behavior)
    let mut window_builder = video.window(title_str, build_w as u32, build_h as u32);
    window_builder.opengl();

    if resizable {
        window_builder.resizable();
    }

    // Born-fullscreen and deferred-windowed are created hidden and shown later
    // (after the mode is set / after init), so the window never paints on the
    // wrong monitor.
    if headless || born_fullscreen || defer_windowed_show {
        window_builder.hidden();
    }

    // High-DPI makes the GL drawable size track the host display's scale
    // factor, so a headless image_snapshot renders at 1440x1433 on a 1.0
    // display but 1920x1910 on a 1.333 one -- non-reproducible across dev
    // machines and CI. Headless (sim) runs render at the logical window
    // size for deterministic, portable snapshots; real windowed runs keep
    // high-DPI for crisp output. STONE_NO_HIGHDPI=1 forces 1x anywhere:
    // a 3x retina drawable on a software rasterizer costs ~8x the pixels for
    // a debugging surface.
    let no_highdpi = std::env::var("STONE_NO_HIGHDPI").as_deref() == Ok("1");
    if !headless && !no_highdpi {
        window_builder.allow_highdpi();
    }

    // X11 no-focus-on-map needs the window created hidden so the
    // user-time stamp lands before the first map (see comment above).
    #[cfg(target_os = "linux")]
    let defer_show_for_focus = prevent_focus_steal && !headless;
    #[cfg(target_os = "linux")]
    if defer_show_for_focus {
        window_builder.hidden();
    }

    // Use explicit position if provided, otherwise center.
    if centered {
        window_builder.position_centered();
    } else {
        window_builder.position(build_x, build_y);
    }

    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let t_pre_window = t_boot.elapsed();
    let mut window = match window_builder.build() {
        Ok(w) => w,
        Err(e) => {
            ::log::error!(target: "STONE", "failed to create window: {}", e);
            return ptr::null_mut();
        }
    };

    #[cfg(target_os = "linux")]
    if x11_wm::install_error_handler(&window) {
        ::log::debug!(target: "STONE", "X11 error handler installed: X errors are logged and never fatal");
    }

    #[cfg(target_os = "linux")]
    if defer_show_for_focus && !born_fullscreen && !defer_windowed_show {
        x11_focus::set_user_time_zero(&window);
        window.show();
    }

    // Born-fullscreen: SDL_SetWindowFullscreen(DESKTOP) fullscreens on the
    // display the window currently sits on, and an unmapped window's requested
    // position isn't yet honored -- so map it (show) at the target-display
    // origin FIRST, reassert the position now that it's mapped, then go
    // fullscreen. It comes up fullscreen on the right monitor with no runtime
    // move. The window has no rendered content yet, so the pre-fullscreen mapped
    // frame is blank, not a visible windowed flash. Fit above is skipped -- the
    // mode owns the whole display.
    if born_fullscreen {
        #[cfg(target_os = "linux")]
        if defer_show_for_focus {
            x11_focus::set_user_time_zero(&window);
        }
        window.show();
        move_window(&mut window, build_x, build_y);
        if let Some(mode) = boot_fs {
            if let Err(e) = window.set_fullscreen(mode) {
                ::log::error!(target: "STONE", "boot fullscreen (mode={}) failed: {}", fullscreen, e);
            }
        }
    }

    let t_window = t_boot.elapsed();
    let gl_context = match window.gl_create_context() {
        Ok(context) => context,
        Err(e) => {
            ::log::error!(target: "STONE", "failed to create GL context {:?} {}.{}: {e}", gl_request.profile, gl_request.major, gl_request.minor);
            return ptr::null_mut();
        }
    };

    let t_gl_context = t_boot.elapsed();

    // Create glow context
    let mut gl = unsafe {
        glow::Context::from_loader_function(|s| video.gl_get_proc_address(s) as *const _)
    };
    {
        use glow::HasContext as _;
        unsafe {
            system_report::record_gl_strings(
                gl.get_parameter_string(glow::RENDERER),
                gl.get_parameter_string(glow::VENDOR),
                gl.get_parameter_string(glow::VERSION),
            );
        }
    }
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    ::log::debug!(target: "STONE",
        "boot sdl_init={:.0}ms video={:.0}ms attrs={:.0}ms window_build={:.0}ms gl_context={:.0}ms glow_load={:.0}ms",
        ms(t_sdl_init), ms(t_video - t_sdl_init), ms(t_pre_window - t_video),
        ms(t_window - t_pre_window), ms(t_gl_context - t_window),
        ms(t_boot.elapsed() - t_gl_context));

    // KHR_debug driver diagnostics (STONE_GL_DEBUG=1). Runtime-gated on
    // the extension: available on Mesa/NVIDIA even for 3.3 core
    // contexts, never on macOS (GL caps at 4.1 there) -- absence is a
    // logged no-op, DebugGl's per-call glGetError checks still run.
    if gl_debug {
        use glow::HasContext as _;
        if gl.supported_extensions().contains("GL_KHR_debug") {
            unsafe {
                gl.enable(glow::DEBUG_OUTPUT);
                // Synchronous: the callback fires on the offending call's
                // stack, so STONE_GL_PANIC-style backtraces line up.
                gl.enable(glow::DEBUG_OUTPUT_SYNCHRONOUS);
                gl.debug_message_callback(|source, ty, id, severity, msg| {
                    match severity {
                        glow::DEBUG_SEVERITY_HIGH => {
                            ::log::error!(target: "GL", "[khr src=0x{source:X} type=0x{ty:X} id={id}] {msg}")
                        }
                        glow::DEBUG_SEVERITY_MEDIUM => {
                            ::log::warn!(target: "GL", "[khr src=0x{source:X} type=0x{ty:X} id={id}] {msg}")
                        }
                        glow::DEBUG_SEVERITY_LOW => {
                            ::log::info!(target: "GL", "[khr src=0x{source:X} type=0x{ty:X} id={id}] {msg}")
                        }
                        // Notifications (buffer usage hints etc.) are spammy.
                        _ => ::log::debug!(target: "GL", "[khr id={id}] {msg}"),
                    }
                });
            }
            ::log::info!(target: "GL", "KHR_debug message callback installed (STONE_GL_DEBUG=1)");
        } else {
            ::log::info!(
                target: "GL",
                "STONE_GL_DEBUG=1 but GL_KHR_debug is unavailable on this context (e.g. macOS) -- driver diagnostics off, per-call glGetError checks still active"
            );
        }
    }

    // Wrap in DebugGl for automatic error checking
    let debug_gl = DebugGl::new(gl);

    // The only record of what the driver actually offered. On a device with no
    // debugger attached, a capability that silently differs from the desktop
    // (float canvases, timer queries) is otherwise invisible in a bug report.
    ::log::info!(target: "STONE", "{}", debug_gl.describe());

    canvas::set_default_framebuffer(None);
    if headless {
        let (dw, dh) = window.drawable_size();
        if let Err(e) = canvas::create_headless_screen(&debug_gl, dw as i32, dh as i32) {
            ::log::error!(target: "STONE", "failed to create the headless screen framebuffer: {e}");
            return ptr::null_mut();
        }
        ::log::info!(target: "STONE", "headless window draws into an owned {dw}x{dh} screen framebuffer");
    }

    // Disable vsync
    if let Err(e) = video.gl_set_swap_interval(sdl2::video::SwapInterval::Immediate) {
        log_once!(
            "VIDEO",
            "gl_set_swap_interval(Immediate) at init failed: {}",
            e
        );
    }

    // Post-create re-fit: borders are now known (real, or modeled on macOS),
    // so re-run the clamp against the live size/position and resize/reposition
    // once if the pre-clamp (zero-borders) missed anything. Skipped headless /
    // when opted out.
    if fit_enabled && !born_fullscreen {
        let borders = effective_borders(query_window_borders(&window));
        let (cur_w, cur_h) = window.size();
        let (cur_x, cur_y) = window.position();
        let displays = gather_display_reports(&video);
        let req = window_report::WindowRequest {
            size: (cur_w as i32, cur_h as i32),
            pos: (cur_x, cur_y),
            display: resolved.display,
        };
        let p = window_report::resolve_window_placement(req, &displays, borders);
        let need_resize = p.size != (cur_w as i32, cur_h as i32);
        let need_move = p.pos != (cur_x, cur_y);
        if need_resize {
            if let Err(e) = window.set_size(p.size.0 as u32, p.size.1 as u32) {
                log_once!("STONE", "post-create fit set_size failed: {}", e);
            }
        }
        if need_move {
            move_window(&mut window, p.pos.0, p.pos.1);
        }
        if need_resize || need_move {
            ::log::info!(
                target: "STONE",
                "window fit (post-create): size {}x{} pos ({},{}) -> size {}x{} pos ({},{}) (borders t{} l{} b{} r{})",
                cur_w, cur_h, cur_x, cur_y,
                p.size.0, p.size.1, p.pos.0, p.pos.1,
                borders.top, borders.left, borders.bottom, borders.right,
            );
        }
    }

    // Set viewport
    let (drawable_w, drawable_h) = window.drawable_size();
    let (win_w, win_h) = window.size();
    let (win_x, win_y) = window.position();
    let display_index = window.display_index().unwrap_or(-1);
    let scale = if win_w > 0 {
        drawable_w as f32 / win_w as f32
    } else {
        1.0
    };
    ::log::info!(
        target: "STONE",
        "window created: pos=({},{}) size={}x{} drawable={}x{} dpi_scale={:.2} on display[{}]",
        win_x, win_y, win_w, win_h, drawable_w, drawable_h, scale, display_index,
    );

    // Always-on window/display report.
    // Headless stays at the one-liner above; a visible window emits the full
    // block (human INFO by default, one JSON line under STONE_WINDOW_REPORT=json).
    if !headless {
        let borders = query_window_borders(&window).unwrap_or(window_report::Borders {
            top: 0,
            left: 0,
            bottom: 0,
            right: 0,
        });
        INITIAL_BORDERS.with(|c| c.set(Some(borders)));
        POST_FRAME_BORDERS_LOGGED.with(|c| c.set(false));
        let report = window_report::WindowReport {
            displays: gather_display_reports(&video),
            requested_size: (width, height),
            requested_pos: (x, y),
            actual_size: (win_w as i32, win_h as i32),
            actual_pos: (win_x, win_y),
            landed_display: display_index,
            drawable_size: (drawable_w as i32, drawable_h as i32),
            dpi_scale: scale,
            borders,
        };
        if std::env::var("STONE_WINDOW_REPORT").as_deref() == Ok("json") {
            ::log::info!(target: "STONE", "{}", report.render_json_line());
        } else {
            ::log::info!(target: "STONE", "{}", report.render_human());
        }
    }

    unsafe {
        debug_gl.viewport(0, 0, drawable_w as i32, drawable_h as i32);
    }

    // Create event pump
    let event_pump = match sdl_context.event_pump() {
        Ok(ep) => ep,
        Err(e) => {
            ::log::error!(target: "STONE", "failed to create event pump: {}", e);
            return ptr::null_mut();
        }
    };
    let game_controller = match sdl_context.game_controller() {
        Ok(subsystem) => subsystem,
        Err(e) => {
            ::log::error!(target: "STONE", "failed to initialize SDL game-controller subsystem: {}", e);
            return ptr::null_mut();
        }
    };
    let controller_manager = input::SdlControllerManager::new(game_controller);

    // Get raw pointers for cimgui before moving ownership
    let sdl_window_raw = window.raw() as *mut c_void;
    let gl_context_raw = unsafe { gl_context.raw() };

    // Deferred windowed show: remember where to map it (its resolved, fit
    // position on the still-hidden window) so the frame loop can show it there.
    let pending_show = if defer_windowed_show {
        Some(PendingShow {
            pos: window.position(),
            map_requested: false,
            activate: false,
        })
    } else {
        None
    };

    let initial_mode = if born_fullscreen {
        window_placement::WindowMode::Fullscreen(fullscreen)
    } else {
        window_placement::WindowMode::Windowed
    };
    let initial_windowed_bounds = window_placement::WindowedBounds {
        pos: (win_x, win_y),
        size: (win_w as i32, win_h as i32),
        display: if display_index >= 0 {
            Some(display_index)
        } else {
            None
        },
    };

    let stone_window = Box::new(StoneWindow {
        _sdl_context: sdl_context,
        _video: video,
        window,
        _gl_context: gl_context,
        gl: Rc::new(debug_gl),
        pending_show,
        runtime_hidden: headless,
        title: title_str.to_string(),
        window_mode: Cell::new(initial_mode),
        windowed_bounds: Cell::new(initial_windowed_bounds),
        saved_state_on_hide: RefCell::new(None),
        initial_hidden_boot: Cell::new(headless),
        requested_size: (width, height),
        requested_pos: (x, y),
        requested_display: if _display >= 0 { Some(_display) } else { None },
        config_imgui_scale: machine.imgui_scale,
        event_pump: RefCell::new(event_pump),
        controller_manager: RefCell::new(controller_manager),
        sdl_window_raw,
        gl_context_raw,
    });

    let ptr = Box::into_raw(stone_window);

    // Store in thread-local
    WINDOW.with(|w| {
        *w.borrow_mut() = Some(unsafe { Box::from_raw(ptr) });
    });

    // Return a pointer (but we manage it in thread-local)
    WINDOW.with(|w| {
        w.borrow()
            .as_ref()
            .map(|b| b.as_ref() as *const StoneWindow as *mut StoneWindow)
            .unwrap_or(ptr::null_mut())
    })
}

/// Poll events
#[no_mangle]
pub extern "C" fn stone_poll_event(event: *mut StoneEvent) -> bool {
    if event.is_null() {
        return false;
    }

    WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().as_mut() {
            if let Some(translated) = window.controller_manager.borrow_mut().pop_stone_event() {
                unsafe {
                    (*event) = translated;
                }
                return true;
            }
            // Drop down to raw SDL_PollEvent so we can forward each
            // event to ImGui's SDL2 backend BEFORE we translate it for
            // the game. ImGui needs to see mouse-button-down/up events
            // to drive widget interaction — its NewFrame polls
            // SDL_GetMouseState for position only, so without this
            // clicks inside ImGui windows silently never reach buttons
            // / sliders / colour pickers (hover works because that's
            // a position query; click doesn't because the transient
            // was drained without being seen).
            unsafe { sdl2::sys::SDL_JoystickUpdate() };
            let mut raw_uninit: std::mem::MaybeUninit<sdl2::sys::SDL_Event> =
                std::mem::MaybeUninit::uninit();
            let has_event = unsafe { sdl2::sys::SDL_PollEvent(raw_uninit.as_mut_ptr()) };
            if has_event == 0 {
                unsafe {
                    (*event) = StoneEvent::default();
                }
                return false;
            }
            let raw = unsafe { raw_uninit.assume_init() };
            let sdl_event = sdl2::event::Event::from_ll(raw);

            // Alt+Enter toggle fullscreen (repeat suppressed).
            // Intercept before ImGui and gameplay so it does not inject a Return keypress.
            if let sdl2::event::Event::KeyDown {
                keycode: Some(sdl2::keyboard::Keycode::Return | sdl2::keyboard::Keycode::KpEnter),
                keymod,
                repeat,
                ..
            } = sdl_event
            {
                let alt = keymod.contains(sdl2::keyboard::Mod::LALTMOD)
                    || keymod.contains(sdl2::keyboard::Mod::RALTMOD);
                if alt {
                    if !repeat {
                        let is_fs =
                            window.window.fullscreen_state() != sdl2::video::FullscreenType::Off;
                        let is_max = (window.window.window_flags()
                            & sdl2::sys::SDL_WindowFlags::SDL_WINDOW_MAXIMIZED as u32)
                            != 0;
                        if is_fs || is_max {
                            set_fullscreen_impl(window, 0);
                        } else {
                            set_fullscreen_impl(window, 1);
                        }
                    }
                    unsafe {
                        (*event) = StoneEvent::default();
                    }
                    return true;
                }
            }
            if let sdl2::event::Event::KeyUp {
                keycode: Some(sdl2::keyboard::Keycode::Return | sdl2::keyboard::Keycode::KpEnter),
                keymod,
                ..
            } = sdl_event
            {
                let alt = keymod.contains(sdl2::keyboard::Mod::LALTMOD)
                    || keymod.contains(sdl2::keyboard::Mod::RALTMOD);
                if alt {
                    unsafe {
                        (*event) = StoneEvent::default();
                    }
                    return true;
                }
            }

            // Forward to ImGui first. The handler early-returns if
            // imgui hasn't been initialized yet, so the call is safe
            // before stone_imgui_init too.
            let _ =
                crate::imgui::stone_imgui_process_event(&raw as *const _ as *mut std::ffi::c_void);
            let controller_event = {
                let mut manager = window.controller_manager.borrow_mut();
                let events = manager.handle_sdl_event(&sdl_event);
                manager.enqueue(events);
                manager.pop_stone_event()
            };
            if let Some(translated) = controller_event {
                unsafe {
                    (*event) = translated;
                }
                return true;
            }
            if let Some(translated) = stone_event_from_sdl(&sdl_event) {
                unsafe {
                    (*event) = translated;
                }
                return true;
            }
            unsafe {
                match sdl_event {
                    #[cfg(target_os = "macos")]
                    Event::Window {
                        win_event: sdl2::event::WindowEvent::FocusGained,
                        ..
                    } => {
                        // First time the user focuses our window: promote
                        // to Regular so Cmd-Tab/Dock behave normally.
                        // No surface event — the game doesn't care.
                        macos_focus::promote_to_regular_once();
                        (*event) = StoneEvent::default();
                    }
                    // Window geometry changes are not surfaced as game
                    // events; they set take-and-clear flags the runner
                    // drains each frame. SizeChanged fires for
                    // both user resizes and programmatic set_size; Resized
                    // is the user-driven subset -- track either as resized.
                    Event::Window { win_event, .. } => {
                        use sdl2::event::WindowEvent as WE;
                        match win_event {
                            WE::Resized(..) | WE::SizeChanged(..) => {
                                mark_window_change(WC_RESIZED);
                                if window.window_mode.get()
                                    == window_placement::WindowMode::Windowed
                                    && window.window.fullscreen_state()
                                        == sdl2::video::FullscreenType::Off
                                {
                                    let is_max = (window.window.window_flags()
                                        & sdl2::sys::SDL_WindowFlags::SDL_WINDOW_MAXIMIZED as u32)
                                        != 0;
                                    if !is_max {
                                        let (w, h) = window.window.size();
                                        let mut b = window.windowed_bounds.get();
                                        b.size = (w as i32, h as i32);
                                        window.windowed_bounds.set(b);
                                    }
                                }
                            }
                            WE::Moved(..) => {
                                mark_window_change(WC_MOVED);
                                if window.window_mode.get()
                                    == window_placement::WindowMode::Windowed
                                    && window.window.fullscreen_state()
                                        == sdl2::video::FullscreenType::Off
                                {
                                    let is_max = (window.window.window_flags()
                                        & sdl2::sys::SDL_WindowFlags::SDL_WINDOW_MAXIMIZED as u32)
                                        != 0;
                                    if !is_max {
                                        let (px, py) = window.window.position();
                                        let mut b = window.windowed_bounds.get();
                                        b.pos = (px, py);
                                        b.display = window.window.display_index().ok();
                                        window.windowed_bounds.set(b);
                                    }
                                }
                            }
                            WE::DisplayChanged(..) => mark_window_change(WC_DISPLAY),
                            _ => {}
                        }
                        (*event) = StoneEvent::default();
                    }
                    _ => {
                        (*event) = StoneEvent::default();
                    }
                }
            }
            return true;
        }
        unsafe {
            (*event) = StoneEvent::default();
        }
        false
    })
}

#[no_mangle]
pub extern "C" fn stone_start_text_input() {
    unsafe { sdl2::sys::SDL_StartTextInput() };
}

#[no_mangle]
pub extern "C" fn stone_stop_text_input() {
    unsafe { sdl2::sys::SDL_StopTextInput() };
}

#[no_mangle]
pub extern "C" fn stone_test_push_key_event(token: *const c_char, down: bool) -> bool {
    if token.is_null() {
        return false;
    }
    let token = unsafe { CStr::from_ptr(token) }.to_str().unwrap_or("");
    let (keycode, scancode) = match token {
        "lshift" => (
            sdl2::keyboard::Keycode::LShift,
            sdl2::keyboard::Scancode::LShift,
        ),
        "right" => (
            sdl2::keyboard::Keycode::Right,
            sdl2::keyboard::Scancode::Right,
        ),
        _ => return false,
    };
    let keymod = if token == "right" {
        sdl2::keyboard::Mod::LSHIFTMOD.bits()
    } else {
        0
    };
    let window_id = WINDOW.with(|window| {
        window
            .borrow()
            .as_ref()
            .map(|window| window.window.id())
            .unwrap_or(0)
    });
    let mut raw = sdl2::sys::SDL_Event {
        key: sdl2::sys::SDL_KeyboardEvent {
            type_: if down {
                sdl2::sys::SDL_EventType::SDL_KEYDOWN as u32
            } else {
                sdl2::sys::SDL_EventType::SDL_KEYUP as u32
            },
            timestamp: 0,
            windowID: window_id,
            state: if down {
                sdl2::sys::SDL_PRESSED as u8
            } else {
                sdl2::sys::SDL_RELEASED as u8
            },
            repeat: 0,
            padding2: 0,
            padding3: 0,
            keysym: sdl2::sys::SDL_Keysym {
                scancode: unsafe {
                    std::mem::transmute::<u32, sdl2::sys::SDL_Scancode>(scancode as u32)
                },
                sym: keycode.into_i32(),
                mod_: keymod,
                unused: 0,
            },
        },
    };
    unsafe { sdl2::sys::SDL_PushEvent(&mut raw) == 1 }
}

/// Swap buffers
#[no_mangle]
pub extern "C" fn stone_swap_window(_window: *mut StoneWindow) {
    let completed = GL_SWAP_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
    // RenderDoc frame auto-trigger; one branch when STONE_RENDERDOC is unset.
    crate::renderdoc::on_swap(completed);
    WINDOW.with(|w| {
        if let Some(window) = w.borrow().as_ref() {
            if !window.runtime_hidden && window.pending_show.is_none() {
                canvas::present_headless_screen(&window.window);
            }
            pace_swap(&window.window);
            window.window.gl_swap_window();

            // Re-check borders once after the first present: on X11
            // _NET_FRAME_EXTENTS can read 0 until the window is mapped
            // Only armed on a visible window.
            if !POST_FRAME_BORDERS_LOGGED.with(|c| c.get()) {
                if let Some(before) = INITIAL_BORDERS.with(|c| c.get()) {
                    POST_FRAME_BORDERS_LOGGED.with(|c| c.set(true));
                    let after = query_window_borders(&window.window).unwrap_or(before);
                    let changed = after != before;
                    ::log::info!(
                        target: "STONE",
                        "borders after first present(pt): top={} left={} bottom={} right={} changed={}",
                        after.top, after.left, after.bottom, after.right, changed,
                    );
                }
            }
        }
    });
}

#[no_mangle]
pub extern "C" fn stone_gl_swap_count() -> u64 {
    GL_SWAP_COUNT.load(Ordering::SeqCst)
}

/// Advance a deferred map after each presented frame. X11 map requests remain
/// pending while another window covers the workspace fullscreen; once viewable,
/// synchronize SDL state and reassert the saved position.
#[no_mangle]
pub extern "C" fn stone_show_window_if_deferred() {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().as_mut() {
            if let Some(pending) = window.pending_show.as_mut() {
                let (px, py) = pending.pos;
                if !pending.map_requested {
                    move_window(&mut window.window, px, py);
                    #[cfg(target_os = "linux")]
                    {
                        if std::env::var("STONE_FOCUS_ON_SHOW").is_err() {
                            x11_focus::set_user_time_zero(&window.window);
                        }
                        x11_focus::pin_user_position(&window.window, px, py);
                    }
                }
                #[cfg(target_os = "linux")]
                {
                    let map_state = if pending.map_requested {
                        x11_focus::poll_deferred_map(&window.window)
                    } else {
                        x11_focus::request_deferred_map(&window.window)
                    };
                    if map_state == x11_focus::DeferredMapState::Pending {
                        pending.map_requested = true;
                        return;
                    }
                }
                window.window.show();
                // A tiling WM (i3) maps a floating window on the *focused*
                // output and ignores the pre-map position; it does honor a move
                // once mapped, so re-assert the position now to land it on the
                // intended monitor.
                move_window(&mut window.window, px, py);
                ::log::info!(
                    target: "STONE",
                    "window shown (deferred) at ({},{}) on display[{}]",
                    px, py, window.window.display_index().unwrap_or(-1),
                );
                let activate = pending.activate;
                window.pending_show = None;
                if activate {
                    activate_window_impl(window);
                }
            }
        }
    });
}

/// Hide the native window at runtime. Rendering, command handling, and
/// glReadPixels captures continue exactly as in headless boot: the drawable
/// is unmapped, not destroyed. This flips retained live state, not a startup
/// environment variable.
#[no_mangle]
pub extern "C" fn stone_hide_window() {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().as_mut() {
            if window.pending_show.is_some() {
                // A deferred show is still pending: drop it, the window must
                // stay unmapped.
                window.pending_show = None;
            }
            if !window.runtime_hidden {
                let fs = window.window.fullscreen_state();
                let is_max = (window.window.window_flags()
                    & sdl2::sys::SDL_WindowFlags::SDL_WINDOW_MAXIMIZED as u32)
                    != 0;
                let current_mode = if fs == sdl2::video::FullscreenType::Desktop {
                    window_placement::WindowMode::Fullscreen(1)
                } else if fs == sdl2::video::FullscreenType::True {
                    window_placement::WindowMode::Fullscreen(2)
                } else if is_max {
                    window_placement::WindowMode::Maximized
                } else {
                    window_placement::WindowMode::Windowed
                };
                window.window_mode.set(current_mode);

                let (wx, wy) = window.window.position();
                let (ww, wh) = window.window.size();
                let display = window.window.display_index().ok();

                if current_mode == window_placement::WindowMode::Windowed {
                    window
                        .windowed_bounds
                        .set(window_placement::WindowedBounds {
                            pos: (wx, wy),
                            size: (ww as i32, wh as i32),
                            display,
                        });
                }

                *window.saved_state_on_hide.borrow_mut() =
                    Some(window_placement::HiddenWindowState {
                        mode: current_mode,
                        pos: (wx, wy),
                        size: (ww as i32, wh as i32),
                        display,
                        windowed_bounds: window.windowed_bounds.get(),
                    });
            }
            window.window.hide();
            window.runtime_hidden = true;
            ::log::info!(target: "STONE", "window hidden by request");
        }
    });
}

/// Show a previously hidden window without restarting the game or touching
/// scene state. Rendering and input restore with the same drawable; the map
/// does not steal focus beyond the WM's normal click-to-focus (SDL show on
/// X11 maps without forcing input focus).
#[no_mangle]
pub extern "C" fn stone_show_window() {
    show_window_impl(None);
}

#[no_mangle]
pub extern "C" fn stone_show_window_mode(mode: c_int) {
    if matches!(mode, 0 | 1) {
        show_window_impl(Some(mode));
    }
}

fn show_window_impl(requested_mode: Option<c_int>) {
    // Fullscreen transitions consult SDL's cached mouse focus, including queued leave events.
    unsafe { sdl2::sys::SDL_PumpEvents() };
    WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().as_mut() {
            if !window.runtime_hidden {
                if let Some(mode) = requested_mode {
                    set_fullscreen_impl(window, mode);
                }
                return;
            }
            let displays = gather_display_reports(&window._video);
            let borders = effective_borders(query_window_borders(&window.window));

            let (mut mode, mut target_pos, mut target_size, mut target_display) =
                if window.initial_hidden_boot.get() {
                    window.initial_hidden_boot.set(false);
                    let base = window_placement::ResolvedGeometry {
                        size: window.requested_size,
                        pos: window.requested_pos,
                        display: window.requested_display,
                    };
                    // A headless boot skipped the machine config; the first show reads it.
                    let machine = window_placement::machine_config(&window_placement::game_slug(
                        &window.title,
                    ));
                    let resolved =
                        resolve_window_geometry(&window.title, base, &machine, &window._video);
                    let req = window_report::WindowRequest {
                        size: resolved.size,
                        pos: resolved.pos,
                        display: resolved.display,
                    };
                    let mut p = window_report::resolve_window_placement(req, &displays, borders);
                    const SDL_WINDOWPOS_CENTERED: i32 = 0x2FFF0000u32 as i32;
                    if resolved.pos.0 == SDL_WINDOWPOS_CENTERED
                        && resolved.pos.1 == SDL_WINDOWPOS_CENTERED
                    {
                        if let Some(d) = displays
                            .iter()
                            .find(|d| d.index == p.display)
                            .or_else(|| displays.first())
                        {
                            p.pos.0 = d.usable.0 + (d.usable.2 - p.size.0) / 2;
                            p.pos.1 = d.usable.1 + (d.usable.3 - p.size.1) / 2;
                        }
                    }
                    window
                        .windowed_bounds
                        .set(window_placement::WindowedBounds {
                            pos: p.pos,
                            size: p.size,
                            display: Some(p.display),
                        });
                    (
                        window_placement::WindowMode::Windowed,
                        p.pos,
                        p.size,
                        p.display,
                    )
                } else if let Some(saved) = window.saved_state_on_hide.borrow_mut().take() {
                    let (restored_mode, p, restored_bounds) =
                        window_placement::resolve_hidden_window_restore(saved, &displays, borders);
                    window.windowed_bounds.set(restored_bounds);
                    (restored_mode, p.pos, p.size, p.display)
                } else {
                    let bounds = window.windowed_bounds.get();
                    let mode = window.window_mode.get();
                    (mode, bounds.pos, bounds.size, bounds.display.unwrap_or(0))
                };

            if let Some(requested) = requested_mode {
                mode = if requested == 0 {
                    window_placement::WindowMode::Windowed
                } else {
                    window_placement::WindowMode::Fullscreen(1)
                };
                let bounds = window.windowed_bounds.get();
                target_pos = bounds.pos;
                target_size = bounds.size;
                target_display = bounds.display.unwrap_or(target_display);
            }
            let (px, py) = target_pos;
            let (pw, ph) = target_size;

            if let Err(e) = window.window.set_size(pw as u32, ph as u32) {
                log_once!("STONE", "stone_show_window set_size failed: {}", e);
            }
            move_window(&mut window.window, px, py);

            #[cfg(target_os = "linux")]
            {
                if std::env::var("STONE_FOCUS_ON_SHOW").is_err() {
                    x11_focus::set_user_time_zero(&window.window);
                }
                x11_focus::pin_user_position(&window.window, px, py);
            }

            match mode {
                // SDL can treat a child of an unmapped X11 WM frame as mapped.
                // Fullscreen must wait for actual viewability just like windowed.
                window_placement::WindowMode::Windowed
                | window_placement::WindowMode::Fullscreen(_) => {
                    let fullscreen = match mode {
                        window_placement::WindowMode::Fullscreen(2) => {
                            sdl2::video::FullscreenType::True
                        }
                        window_placement::WindowMode::Fullscreen(_) => {
                            sdl2::video::FullscreenType::Desktop
                        }
                        _ => sdl2::video::FullscreenType::Off,
                    };
                    if let Err(e) = window.window.set_fullscreen(fullscreen) {
                        log_once!("VIDEO", "show window mode failed: {}", e);
                    }
                    window.window_mode.set(mode);
                    window.pending_show = Some(PendingShow {
                        pos: (px, py),
                        map_requested: false,
                        activate: false,
                    });
                }
                window_placement::WindowMode::Maximized => {
                    window.window.show();
                    let _ = window
                        .window
                        .set_fullscreen(sdl2::video::FullscreenType::Off);
                    window.window.maximize();
                    window
                        .window_mode
                        .set(window_placement::WindowMode::Maximized);
                    window.pending_show = None;
                }
            }

            window.runtime_hidden = false;
            ::log::info!(
                target: "STONE",
                "window shown by request: mode={:?} pos=({},{}) size={}x{} display={}",
                mode, px, py, pw, ph, target_display
            );
        }
    });
    stone_show_window_if_deferred();
}

/// Live runtime visibility of the native window. A headless-booted game
/// reports hidden; hide/show at runtime flips it.
#[no_mangle]
pub extern "C" fn stone_is_window_hidden() -> bool {
    WINDOW.with(|w| {
        w.borrow()
            .as_ref()
            .map(|window| window.runtime_hidden)
            .unwrap_or(true)
    })
}

/// Explicitly bring an interactive window to the foreground. Normal Stone
/// startup avoids stealing focus; launchers and demos opt into activation.
#[no_mangle]
pub extern "C" fn stone_activate_window(_window: *mut StoneWindow) {
    if stone_is_window_hidden() {
        return;
    }

    WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().as_mut() {
            if let Some(pending) = window.pending_show.as_mut() {
                pending.activate = true;
            } else {
                activate_window_impl(window);
            }
        }
    });
}

fn activate_window_impl(window: &mut StoneWindow) {
    #[cfg(target_os = "macos")]
    macos_focus::promote_to_regular_once();
    window.window.raise();
    ::log::info!(target: "STONE", "window activated by request");
}

/// GL errors DebugGl has seen since process start (0 in release builds,
/// where the per-call check compiles out). Sims assert this stays 0.
/// u32 so LuaJIT sees a plain number, not a boxed uint64_t cdata.
/// Make every subsequent shader compile/link fail, from inside a running entry.
/// The env spelling (`STONE_SHADER_FORCE_FAIL`) still seeds it.
#[no_mangle]
pub extern "C" fn stone_shader_force_fail(on: bool) {
    stone_gl::set_force_shader_failure(on);
}

#[no_mangle]
pub extern "C" fn stone_gl_error_count() -> u32 {
    stone_gl::GL_ERROR_COUNT.load(std::sync::atomic::Ordering::Relaxed) as u32
}

/// Shader compile + link failures since process start. Unlike GL errors these
/// are counted in release too -- the compile-status check is not a debug-only
/// probe. Bounded runs (`rock frame` / perf / sims) exit nonzero on any.
#[no_mangle]
pub extern "C" fn stone_shader_error_count() -> u32 {
    stone_gl::SHADER_ERROR_COUNT.load(std::sync::atomic::Ordering::Relaxed) as u32
}

/// Begin the GPU timer query for this frame's GL work. No-op if no window /
/// GL context, or if timer queries are unsupported on this platform.
#[no_mangle]
pub extern "C" fn stone_gpu_timer_begin() {
    with_gl(|gl| GPU_TIMER.with(|t| t.borrow_mut().begin(gl)));
}

/// End this frame's GPU timer query.
#[no_mangle]
pub extern "C" fn stone_gpu_timer_end() {
    with_gl(|gl| GPU_TIMER.with(|t| t.borrow_mut().end(gl)));
}

/// Non-blocking read of the oldest ready GPU timer result. Returns elapsed GPU
/// ms (>= 0), `-1` if no result is ready yet, or `-2` if timer queries are
/// unsupported (macOS / no GL context).
#[no_mangle]
pub extern "C" fn stone_gpu_timer_poll_ms() -> f32 {
    with_gl(|gl| GPU_TIMER.with(|t| t.borrow_mut().poll_ms(gl))).unwrap_or(-2.0)
}

/// Whether this process records named GPU scopes (`STONE_GPU_SCOPES=1`). The
/// Rock wrappers read it once so a disabled scope costs no FFI call.
#[no_mangle]
pub extern "C" fn stone_gpu_scopes_requested() -> u8 {
    GPU_TIMER.with(|t| t.borrow().status() != gpu_timer::ScopeStatus::Off) as u8
}

/// Whether the scopes are `glFinish`-bracketed wall time
/// (`STONE_GPU_SCOPES=finish`) rather than GPU timestamps.
#[no_mangle]
pub extern "C" fn stone_gpu_scopes_finish() -> u8 {
    GPU_TIMER.with(|t| t.borrow().status() == gpu_timer::ScopeStatus::Finish) as u8
}

/// Open a named GPU scope (timestamp pair + KHR_debug group) inside the
/// current frame. No-op unless scopes are on and a frame is recording.
///
/// # Safety
/// `name` must be a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn stone_gpu_scope_begin(name: *const c_char) {
    if name.is_null() {
        ::log::error!(target: "STONE", "stone_gpu_scope_begin: null name");
        return;
    }
    let name = std::ffi::CStr::from_ptr(name).to_string_lossy();
    with_gl(|gl| GPU_TIMER.with(|t| t.borrow_mut().scope_begin(gl, &name)));
}

/// Open the scope of one fullscreen pass, named `pass:<shader_name>`.
///
/// # Safety
/// `shader_name` must be a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn stone_gpu_pass_scope_begin(shader_name: *const c_char) {
    if shader_name.is_null() {
        ::log::error!(target: "STONE", "stone_gpu_pass_scope_begin: null shader name");
        return;
    }
    let shader = std::ffi::CStr::from_ptr(shader_name).to_string_lossy();
    with_gl(|gl| GPU_TIMER.with(|t| t.borrow_mut().scope_begin(gl, &format!("pass:{shader}"))));
}

#[no_mangle]
pub extern "C" fn stone_gpu_scope_end() {
    with_gl(|gl| GPU_TIMER.with(|t| t.borrow_mut().scope_end(gl)));
}

/// Label a shader program with its registry name so capture tools show it.
/// Only with `STONE_GPU_SCOPES=1` on a context with `KHR_debug`; labels are
/// capture metadata and cost nothing to skip.
///
/// # Safety
/// `name` must be a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn stone_gpu_label_shader(handle: u64, name: *const c_char) {
    if GPU_TIMER.with(|t| t.borrow().status() == gpu_timer::ScopeStatus::Off) {
        return;
    }
    if name.is_null() {
        ::log::error!(target: "STONE", "stone_gpu_label_shader: null name");
        return;
    }
    let name = std::ffi::CStr::from_ptr(name).to_string_lossy();
    let Some(program) = SHADERS.with(|s| s.borrow().get(&handle).map(|sh| sh.program)) else {
        ::log::error!(target: "STONE", "stone_gpu_label_shader: no program for handle {handle} ({name})");
        return;
    };
    with_gl(|gl| {
        if gl.supports_debug_groups() {
            gl.object_label(glow::PROGRAM, program.0.get(), &name);
        }
    });
}

thread_local! {
    static GPU_SHADER_LABEL: RefCell<Option<std::ffi::CString>> = const { RefCell::new(None) };
}

/// The debug label GL holds for a shader program, or "" when none or when the
/// context has no `KHR_debug`. The oracle for `stone_gpu_label_shader`.
#[no_mangle]
pub extern "C" fn stone_gpu_shader_label(handle: u64) -> *const c_char {
    let label = SHADERS
        .with(|s| s.borrow().get(&handle).map(|sh| sh.program))
        .and_then(|program| {
            with_gl(|gl| unsafe {
                if gl.supports_debug_groups() {
                    gl.get_object_label(glow::PROGRAM, program.0.get())
                } else {
                    String::new()
                }
            })
        })
        .unwrap_or_default();
    GPU_SHADER_LABEL.with(|slot| match std::ffi::CString::new(label) {
        Ok(c) => {
            let ptr = c.as_ptr();
            *slot.borrow_mut() = Some(c);
            ptr
        }
        Err(e) => {
            ::log::error!(target: "STONE", "stone_gpu_shader_label: {e}");
            std::ptr::null()
        }
    })
}

/// Clear aggregated scope samples (the profiler's warmup transition).
#[no_mangle]
pub extern "C" fn stone_gpu_scopes_reset_stats() {
    GPU_TIMER.with(|t| t.borrow_mut().reset_stats());
}

thread_local! {
    static GPU_SCOPES_JSON: RefCell<Option<std::ffi::CString>> = const { RefCell::new(None) };
}

/// The `gpu_scopes` perf-JSON object. The pointer is valid until the next call.
#[no_mangle]
pub extern "C" fn stone_gpu_scopes_json() -> *const c_char {
    let json = GPU_TIMER.with(|t| t.borrow().scopes_json());
    GPU_SCOPES_JSON.with(|slot| match std::ffi::CString::new(json) {
        Ok(c) => {
            let ptr = c.as_ptr();
            *slot.borrow_mut() = Some(c);
            ptr
        }
        Err(e) => {
            ::log::error!(target: "STONE", "stone_gpu_scopes_json: {e}");
            std::ptr::null()
        }
    })
}

thread_local! {
    // When set (perf/sim benchmarking), stone_set_vsync(1) is ignored so a
    // game's per-frame settings-apply (e.g. mag's settings_push pokes the raw
    // FFI, bypassing the engine's engine_set_vsync wrapper) can't re-enable
    // the vblock and pin `rendered` to the refresh rate -- which silently
    // corrupts every render-bench frame-time measurement.
    static VSYNC_FORCED_OFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(target_os = "macos")]
thread_local! {
    // Stone's own vsync on macOS; SDL's interval stays 0 (display_pacer.rs).
    static DISPLAY_PACER: RefCell<Option<display_pacer::DisplayPacer>> = const { RefCell::new(None) };
}

#[cfg(target_os = "macos")]
fn apply_swap_interval(enabled: c_int) {
    if enabled == 0 && DISPLAY_PACER.with(|p| p.borrow().is_none()) {
        return;
    }
    DISPLAY_PACER.with(|p| {
        let mut pacer = p.borrow_mut();
        pacer
            .get_or_insert_with(display_pacer::DisplayPacer::new)
            .set_running(enabled != 0);
    });
}

#[cfg(target_os = "macos")]
fn pace_swap(window: &sdl2::video::Window) {
    DISPLAY_PACER.with(|p| {
        if let Some(pacer) = p.borrow_mut().as_mut() {
            pacer.wait(window.display_index().ok());
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn pace_swap(_window: &sdl2::video::Window) {}

#[cfg(not(target_os = "macos"))]
fn apply_swap_interval(enabled: c_int) {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow().as_ref() {
            let interval = if enabled != 0 {
                sdl2::video::SwapInterval::VSync
            } else {
                sdl2::video::SwapInterval::Immediate
            };
            if let Err(e) = window._video.gl_set_swap_interval(interval) {
                log_once!("VIDEO", "gl_set_swap_interval (enabled={}) failed: {}", enabled, e);
            }
            // The set can fail (iOS has no swap-interval control) or be clamped
            // by the driver. Callers steer real behaviour off this state -- the
            // post-swap glFinish that relocates the vblank stall keys on it --
            // so report the divergence instead of trusting the request.
            let effective = swap_interval_on(window._video.gl_get_swap_interval());
            if effective != (enabled != 0) {
                log_once!(
                    "VIDEO",
                    "vsync requested {} but the driver reports {} -- present pacing is not under our control here",
                    enabled != 0,
                    effective
                );
            }
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn swap_interval_on(interval: sdl2::video::SwapInterval) -> bool {
    !matches!(interval, sdl2::video::SwapInterval::Immediate)
}

/// The vsync state the DRIVER reports, not the one last requested. A
/// driver can refuse or clamp the swap interval, so a caller that assumes a
/// `stone_set_vsync(0)` took will mis-attribute the resulting present stall.
#[no_mangle]
pub extern "C" fn stone_vsync_active() -> bool {
    #[cfg(target_os = "macos")]
    return DISPLAY_PACER.with(|p| p.borrow().as_ref().is_some_and(|pacer| pacer.is_running()));
    #[cfg(not(target_os = "macos"))]
    WINDOW
        .with(|w| {
            w.borrow()
                .as_ref()
                .map(|window| swap_interval_on(window._video.gl_get_swap_interval()))
        })
        .unwrap_or(false)
}

/// Set VSync. Coerced to off while the force-off latch is set (see
/// stone_force_vsync_off) so benchmark runs stay uncapped regardless of
/// game-side vsync requests.
#[no_mangle]
pub extern "C" fn stone_set_vsync(enabled: c_int) {
    let enabled = if VSYNC_FORCED_OFF.with(|f| f.get()) {
        0
    } else {
        enabled
    };
    apply_swap_interval(enabled);
}

/// Latch vsync permanently off for the process (perf/sim mode). While
/// latched, stone_set_vsync(1) is a no-op -- the authoritative benchmark
/// invariant lives here, not in each game's settings code.
#[no_mangle]
pub extern "C" fn stone_force_vsync_off(force: c_int) {
    VSYNC_FORCED_OFF.with(|f| f.set(force != 0));
    if force != 0 {
        apply_swap_interval(0);
    }
}

fn set_fullscreen_impl(window: &mut StoneWindow, mode: c_int) -> bool {
    let fullscreen = match mode {
        1 => sdl2::video::FullscreenType::Desktop,
        2 => sdl2::video::FullscreenType::True,
        _ => sdl2::video::FullscreenType::Off,
    };
    if window.window.fullscreen_state() == fullscreen {
        return false;
    }
    if fullscreen != sdl2::video::FullscreenType::Off
        && window.window_mode.get() == window_placement::WindowMode::Windowed
    {
        let (wx, wy) = window.window.position();
        let (ww, wh) = window.window.size();
        let display = window.window.display_index().ok();
        window
            .windowed_bounds
            .set(window_placement::WindowedBounds {
                pos: (wx, wy),
                size: (ww as i32, wh as i32),
                display,
            });
    }
    if let Err(e) = window.window.set_fullscreen(fullscreen) {
        log_once!("VIDEO", "set_fullscreen (mode={}) failed: {}", mode, e);
        false
    } else {
        let m = match mode {
            1 => window_placement::WindowMode::Fullscreen(1),
            2 => window_placement::WindowMode::Fullscreen(2),
            _ => {
                let b = window.windowed_bounds.get();
                if let Err(e) = window.window.set_size(b.size.0 as u32, b.size.1 as u32) {
                    log_once!("VIDEO", "set_fullscreen restore size failed: {}", e);
                }
                move_window(&mut window.window, b.pos.0, b.pos.1);
                window_placement::WindowMode::Windowed
            }
        };
        window.window_mode.set(m);
        mark_window_change(WC_RESIZED);
        mark_window_change(WC_MOVED);
        true
    }
}

/// Set fullscreen mode at runtime: 0 = windowed, 1 = desktop (borderless)
/// fullscreen, 2 = exclusive fullscreen. Unknown values are treated as windowed.
#[no_mangle]
pub extern "C" fn stone_set_fullscreen(mode: c_int) {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().as_mut() {
            set_fullscreen_impl(window, mode);
        }
    });
}

/// Toggle fullscreen mode between Desktop Fullscreen and Windowed.
/// Preserves and restores windowed bounds. Returns the new mode (1 = Fullscreen, 0 = Windowed).
#[no_mangle]
pub extern "C" fn stone_toggle_fullscreen() -> c_int {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().as_mut() {
            let is_fs = window.window.fullscreen_state() != sdl2::video::FullscreenType::Off;
            let is_max = (window.window.window_flags()
                & sdl2::sys::SDL_WindowFlags::SDL_WINDOW_MAXIMIZED as u32)
                != 0;
            if is_fs || is_max {
                set_fullscreen_impl(window, 0);
                0
            } else {
                set_fullscreen_impl(window, 1);
                1
            }
        } else {
            0
        }
    })
}

/// Delay
#[no_mangle]
pub extern "C" fn stone_delay(ms: u32) {
    std::thread::sleep(std::time::Duration::from_millis(ms as u64));
}

/// Wall time for a perf deadline passed by the launcher before process spawn.
#[no_mangle]
pub extern "C" fn stone_wall_time() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock predates Unix epoch")
        .as_secs_f64()
}

/// Get time
#[no_mangle]
pub extern "C" fn stone_get_time() -> f64 {
    // Use std::time for monotonic time
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    start.elapsed().as_secs_f64()
}

// =============================================================================
// Window Functions
// =============================================================================

#[no_mangle]
pub extern "C" fn stone_get_window_size(width: *mut c_int, height: *mut c_int) {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow().as_ref() {
            let (w, h) = window.window.size();
            unsafe {
                if !width.is_null() {
                    *width = w as c_int;
                }
                if !height.is_null() {
                    *height = h as c_int;
                }
            }
        }
    });
}

#[no_mangle]
pub extern "C" fn stone_get_drawable_size(width: *mut c_int, height: *mut c_int) {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow().as_ref() {
            let (w, h) = window.window.drawable_size();
            unsafe {
                if !width.is_null() {
                    *width = w as c_int;
                }
                if !height.is_null() {
                    *height = h as c_int;
                }
            }
        }
    });
}

/// The machine config's `imgui_scale` for this window's game, or 0 when the
/// config names none (or there is no window, or it is headless).
#[no_mangle]
pub extern "C" fn stone_get_config_imgui_scale() -> c_float {
    WINDOW.with(|w| {
        w.borrow()
            .as_ref()
            .and_then(|window| window.config_imgui_scale)
            .unwrap_or(0.0)
    })
}

#[no_mangle]
pub extern "C" fn stone_get_dpi_scale() -> c_float {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow().as_ref() {
            let (w, _) = window.window.size();
            let (dw, _) = window.window.drawable_size();
            if w > 0 {
                return dw as c_float / w as c_float;
            }
        }
        1.0
    })
}

#[no_mangle]
pub extern "C" fn stone_set_window_title(_window: *mut StoneWindow, title: *const c_char) {
    if title.is_null() {
        return;
    }
    let title_str = unsafe { CStr::from_ptr(title).to_str().unwrap_or("") };
    WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().as_mut() {
            let _ = window.window.set_title(title_str);
        }
    });
}

#[no_mangle]
pub extern "C" fn stone_set_window_position(x: c_int, y: c_int) {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().as_mut() {
            let (fx, fy) = clamp_window_position(&window._video, &window.window, x, y);
            move_window(&mut window.window, fx, fy);
        }
    });
}

#[no_mangle]
pub extern "C" fn stone_get_window_position(x: *mut c_int, y: *mut c_int) {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow().as_ref() {
            let (px, py) = window.window.position();
            unsafe {
                if !x.is_null() {
                    *x = px;
                }
                if !y.is_null() {
                    *y = py;
                }
            }
        }
    });
}

#[no_mangle]
pub extern "C" fn stone_get_fullscreen() -> c_int {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow().as_ref() {
            match window.window.fullscreen_state() {
                sdl2::video::FullscreenType::Desktop => 1,
                sdl2::video::FullscreenType::True => 2,
                sdl2::video::FullscreenType::Off => 0,
            }
        } else {
            0
        }
    })
}

/// Persist the current window geometry for this game + display topology
/// v1 trigger: clean quit. Headless never
/// writes (its geometry is meaningless). Atomic write handled downstream.
fn can_persist_window_geometry(
    runtime_hidden: bool,
    pending_show: bool,
    fullscreen: sdl2::video::FullscreenType,
) -> bool {
    !runtime_hidden && !pending_show && fullscreen == sdl2::video::FullscreenType::Off
}

#[no_mangle]
pub extern "C" fn stone_save_window_geometry(title: *const c_char) {
    if std::env::var_os("STONE_NO_WINDOW_FIT").is_some() {
        // Opt-out also means "don't remember" -- a pinned one-off run
        // shouldn't overwrite the saved geometry for normal launches.
        return;
    }
    let title_str = if title.is_null() {
        "Stone"
    } else {
        unsafe { CStr::from_ptr(title).to_str().unwrap_or("Stone") }
    };
    WINDOW.with(|w| {
        if let Some(window) = w.borrow().as_ref() {
            if !can_persist_window_geometry(
                window.runtime_hidden,
                window.pending_show.is_some(),
                window.window.fullscreen_state(),
            ) {
                return;
            }
            // In borderless/exclusive fullscreen SDL reports the display bounds
            // as the window size/pos -- persisting them would overwrite the
            // remembered *windowed* placement with a full-screen rect, so the
            // next windowed launch restores a display-sized "window". Explicit
            // mode is DisplaySettings' job (mode + monitor); saved geometry only
            // remembers windowed placement.
            let slug = window_placement::game_slug(title_str);
            let displays = gather_display_reports(&window._video);
            let topo = window_placement::topology_signature(&displays);
            let (ww, wh) = window.window.size();
            let (wx, wy) = window.window.position();
            let display = window.window.display_index().ok();
            window_placement::save_geometry(
                &slug,
                &topo,
                wx,
                wy,
                ww as i32,
                wh as i32,
                display,
                window.requested_size,
            );
            ::log::info!(
                target: "STONE",
                "saved window geometry (game '{}' topo {}): pos ({},{}) size {}x{} display {:?}",
                slug, topo, wx, wy, ww, wh, display,
            );
        }
    });
}

/// Take-and-clear the "window was resized" flag (user drag or programmatic
/// set_size). The runner drains this each frame to realloc render canvases when
/// no fixed internal render resolution is set.
#[no_mangle]
pub extern "C" fn stone_take_window_resized() -> bool {
    take_window_change(WC_RESIZED)
}

/// Take-and-clear the "window was moved" flag (drives the debounced geometry
/// write in the runner).
#[no_mangle]
pub extern "C" fn stone_take_window_moved() -> bool {
    take_window_change(WC_MOVED)
}

/// Take-and-clear the "window changed display" flag.
#[no_mangle]
pub extern "C" fn stone_take_window_display_changed() -> bool {
    take_window_change(WC_DISPLAY)
}

/// Resize the window, clamped to the target display's usable area.
/// Fires a SIZE_CHANGED event, so the resize flag is set as a side effect.
#[no_mangle]
pub extern "C" fn stone_set_window_size(width: c_int, height: c_int) {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().as_mut() {
            let headless = std::env::var("STONE_HEADLESS").is_ok();
            let (nw, nh) = if window_fit_enabled(headless) {
                let borders = effective_borders(query_window_borders(&window.window));
                let displays = gather_display_reports(&window._video);
                let (cx, cy) = window.window.position();
                let req = window_report::WindowRequest {
                    size: (width, height),
                    pos: (cx, cy),
                    display: None,
                };
                let p = window_report::resolve_window_placement(req, &displays, borders);
                (p.size.0, p.size.1)
            } else {
                (width, height)
            };
            if let Err(e) = window.window.set_size(nw.max(1) as u32, nh.max(1) as u32) {
                log_once!(
                    "VIDEO",
                    "stone_set_window_size({},{}) failed: {}",
                    nw,
                    nh,
                    e
                );
            }
        }
    });
}

/// A headless window's size: `STONE_WINDOW` when it names one, else the game's.
fn headless_window_size(game: (i32, i32)) -> (i32, i32) {
    window_placement::env_geometry().size.unwrap_or(game)
}

/// Move the window to display `index`, centered on that display's usable area
/// and clamped on-screen. No-op for an unknown index (logged).
#[no_mangle]
pub extern "C" fn stone_set_window_display(index: c_int) {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().as_mut() {
            // Already on the target display: nothing to do. Keeps a redundant
            // re-apply (boot's first display push, reopening settings) from
            // needlessly dropping + restoring fullscreen below.
            if window.window.display_index().ok() == Some(index) {
                return;
            }
            let displays = gather_display_reports(&window._video);
            let Some(target) = displays.iter().find(|d| d.index == index) else {
                log_once!(
                    "VIDEO",
                    "stone_set_window_display({}): no such display",
                    index
                );
                return;
            };
            let (ux, uy, uw, uh) = target.usable;
            let (ww, wh) = window.window.size();
            // Move keeping the window's offset within its CURRENT display, so it
            // lands in the same relative spot on the target monitor. This both
            // makes a live monitor switch feel natural and undoes a WM's
            // post-map relocation to the wrong monitor without re-centering
            // (which would discard a remembered/dragged position).
            let cur = window.window.display_index().unwrap_or(index);
            let cur_origin = displays
                .iter()
                .find(|d| d.index == cur)
                .map(|d| (d.usable.0, d.usable.1))
                .unwrap_or((0, 0));
            let (wx, wy) = window.window.position();
            let off_x = (wx - cur_origin.0).clamp(0, (uw - ww as i32).max(0));
            let off_y = (wy - cur_origin.1).clamp(0, (uh - wh as i32).max(0));
            let borders = effective_borders(query_window_borders(&window.window));
            let req = window_report::WindowRequest {
                size: (ww as i32, wh as i32),
                pos: (ux + off_x, uy + off_y),
                display: Some(index),
            };
            let p = window_report::resolve_window_placement(req, &displays, borders);
            // SDL ignores SetWindowPosition on a fullscreen window, so to move a
            // borderless/exclusive window to another display we drop to windowed,
            // reposition onto the target, then restore the fullscreen mode -- it
            // re-fullscreens on whichever display the window now sits on.
            let fs = window.window.fullscreen_state();
            if fs != sdl2::video::FullscreenType::Off {
                let _ = window
                    .window
                    .set_fullscreen(sdl2::video::FullscreenType::Off);
            }
            move_window(&mut window.window, p.pos.0, p.pos.1);
            if fs != sdl2::video::FullscreenType::Off {
                if let Err(e) = window.window.set_fullscreen(fs) {
                    log_once!(
                        "VIDEO",
                        "stone_set_window_display: restore fullscreen failed: {}",
                        e
                    );
                }
            }
            ::log::info!(
                target: "STONE",
                "window moved to display[{}] (now on display[{}], fullscreen={})",
                index,
                window.window.display_index().unwrap_or(-1),
                fs != sdl2::video::FullscreenType::Off,
            );
        }
    });
}

/// Select the exclusive-fullscreen display mode closest to (w, h) on the
/// window's current display, before `stone_set_fullscreen(2)`.
/// No-op when the display enumerates no modes (logged).
#[no_mangle]
pub extern "C" fn stone_set_display_mode(width: c_int, height: c_int) {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().as_mut() {
            let idx = window.window.display_index().unwrap_or(0);
            let displays = gather_display_reports(&window._video);
            let Some(d) = displays.iter().find(|d| d.index == idx) else {
                return;
            };
            let Some((mw, mh, mr)) = window_report::closest_display_mode(&d.modes, width, height)
            else {
                log_once!(
                    "VIDEO",
                    "stone_set_display_mode: display[{}] enumerates no modes",
                    idx
                );
                return;
            };
            let mode = sdl2::sys::SDL_DisplayMode {
                format: 0,
                w: mw,
                h: mh,
                refresh_rate: mr,
                driverdata: ptr::null_mut(),
            };
            let rc = unsafe { sdl2::sys::SDL_SetWindowDisplayMode(window.window.raw(), &mode) };
            if rc != 0 {
                let err = unsafe { CStr::from_ptr(sdl2::sys::SDL_GetError()) }.to_string_lossy();
                log_once!(
                    "VIDEO",
                    "SDL_SetWindowDisplayMode({}x{}@{}) failed: {}",
                    mw,
                    mh,
                    mr,
                    err
                );
            } else {
                ::log::info!(
                    target: "STONE",
                    "display mode selected {}x{}@{} (target {}x{}) on display[{}]",
                    mw, mh, mr, width, height, idx,
                );
            }
        }
    });
}

#[no_mangle]
pub extern "C" fn stone_get_display_count() -> c_int {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow().as_ref() {
            if let Ok(count) = window._video.num_video_displays() {
                return count as c_int;
            }
        }
        1
    })
}

/// Refresh rate in Hz of the display the window is on, or 0 when it is
/// unknown. Every path to 0 is logged: no window, a failed SDL query, or a
/// mode whose rate SDL reports as 0 because the driver does not know it.
#[no_mangle]
pub extern "C" fn stone_get_window_refresh_rate() -> c_int {
    WINDOW.with(|w| {
        let w = w.borrow();
        let Some(window) = w.as_ref() else {
            log_once!(
                "VIDEO",
                "stone_get_window_refresh_rate: no window; refresh rate unknown"
            );
            return 0;
        };
        let index = match window.window.display_index() {
            Ok(index) => index,
            Err(err) => {
                log_once!(
                    "VIDEO",
                    "SDL_GetWindowDisplayIndex failed: {}; refresh rate unknown",
                    err
                );
                return 0;
            }
        };
        match window._video.current_display_mode(index) {
            Ok(mode) => window_report::known_refresh_rate(mode.refresh_rate).unwrap_or_else(|| {
                log_once!(
                    "VIDEO",
                    "display[{}] reports refresh rate {}; refresh rate unknown",
                    index,
                    mode.refresh_rate,
                );
                0
            }),
            Err(err) => {
                log_once!(
                    "VIDEO",
                    "SDL_GetCurrentDisplayMode({}) failed: {}; refresh rate unknown",
                    index,
                    err
                );
                0
            }
        }
    })
}

#[no_mangle]
pub extern "C" fn stone_get_window_borders(
    top: *mut c_int,
    left: *mut c_int,
    bottom: *mut c_int,
    right: *mut c_int,
) {
    let b = WINDOW.with(|win| {
        win.borrow()
            .as_ref()
            .map(|window| query_window_borders(&window.window))
    });
    let b = b.flatten().unwrap_or(window_report::Borders {
        top: 0,
        left: 0,
        bottom: 0,
        right: 0,
    });
    unsafe {
        if !top.is_null() {
            *top = b.top;
        }
        if !left.is_null() {
            *left = b.left;
        }
        if !bottom.is_null() {
            *bottom = b.bottom;
        }
        if !right.is_null() {
            *right = b.right;
        }
    }
}

/// Query the WM-reported frame extents. Returns zeros AND logs the reason when
/// the WM doesn't support it (SDL_GetWindowBordersSize, SDL 2.0.5+; unsupported
/// on some WMs and on a not-yet-mapped X11 window, where `_NET_FRAME_EXTENTS`
/// is still absent). Never silent on failure.
fn query_window_borders(window: &sdl2::video::Window) -> Option<window_report::Borders> {
    use sdl2::sys;
    let (mut top, mut left, mut bottom, mut right) = (0i32, 0i32, 0i32, 0i32);
    let rc = unsafe {
        sys::SDL_GetWindowBordersSize(window.raw(), &mut top, &mut left, &mut bottom, &mut right)
    };
    if rc != 0 {
        // log_once: a per-frame getter (void's debug UI) polls this; the
        // unsupported-WM reason is a static property, surface it exactly once.
        let err = unsafe { CStr::from_ptr(sys::SDL_GetError()) }
            .to_string_lossy()
            .into_owned();
        log_once!(
            "STONE",
            "SDL_GetWindowBordersSize unsupported on this WM (rc={}, '{}'); borders=0",
            rc,
            err,
        );
        return None;
    }
    Some(window_report::Borders {
        top,
        left,
        bottom,
        right,
    })
}

fn gather_display_reports(video: &sdl2::VideoSubsystem) -> Vec<window_report::DisplayReport> {
    let n = video.num_video_displays().unwrap_or(0);
    let mut out = Vec::with_capacity(n.max(0) as usize);
    for i in 0..n {
        let name = video.display_name(i).unwrap_or_else(|_| "?".to_string());
        let rect_tuple = |r: sdl2::rect::Rect| (r.x, r.y, r.width() as i32, r.height() as i32);
        let bounds = video
            .display_bounds(i)
            .map(rect_tuple)
            .unwrap_or((0, 0, 0, 0));
        let usable = video
            .display_usable_bounds(i)
            .map(rect_tuple)
            .unwrap_or((0, 0, 0, 0));
        let dpi = video.display_dpi(i).unwrap_or((0.0, 0.0, 0.0));
        let current_mode = video
            .current_display_mode(i)
            .ok()
            .map(|m| (m.w, m.h, m.refresh_rate));
        let mode_count = video.num_display_modes(i).unwrap_or(0);
        let mut modes = Vec::with_capacity(mode_count.max(0) as usize);
        for mi in 0..mode_count {
            if let Ok(m) = video.display_mode(i, mi) {
                modes.push((m.w, m.h, m.refresh_rate));
            }
        }
        out.push(window_report::DisplayReport {
            index: i,
            name,
            bounds,
            usable,
            dpi,
            current_mode,
            mode_count,
            modes,
        });
    }
    out
}

const ZERO_BORDERS: window_report::Borders = window_report::Borders {
    top: 0,
    left: 0,
    bottom: 0,
    right: 0,
};

/// Fit clamp is active only on a visible window and never when explicitly
/// opted out. Headless (sim/snapshot) runs
/// must render exactly the requested size on any host, so they never clamp.
fn window_fit_enabled(headless: bool) -> bool {
    !headless && std::env::var_os("STONE_NO_WINDOW_FIT").is_none()
}

/// Borders to feed the fit math. SDL_GetWindowBordersSize is unsupported on
/// Cocoa (returns 0), so on macOS we model the title bar at
/// 28pt when the query yields nothing usable; the menu bar is already excluded
/// via usable_bounds. Other platforms use the
/// real WM-reported extents.
fn effective_borders(queried: Option<window_report::Borders>) -> window_report::Borders {
    let q = queried.unwrap_or(ZERO_BORDERS);
    #[cfg(target_os = "macos")]
    {
        if q == ZERO_BORDERS {
            return window_report::Borders {
                top: 28,
                left: 0,
                bottom: 0,
                right: 0,
            };
        }
    }
    q
}

/// Resolve remembered placement: env override
/// > saved geometry > machine config [game.<slug>] > [default] > game default
///
/// The supplied default is `base`. Reads three sources once; logs the winning provenance
/// when any source overrode the default. Callers gate this on !headless.
fn resolve_window_geometry(
    title: &str,
    base: window_placement::ResolvedGeometry,
    machine: &window_placement::MachineConfig,
    video: &sdl2::VideoSubsystem,
) -> window_placement::ResolvedGeometry {
    use window_placement::Source;
    let slug = window_placement::game_slug(title);
    let displays = gather_display_reports(video);
    let topo = window_placement::topology_signature(&displays);
    let env = window_placement::env_geometry();
    // The remembered size only applies while the game still asks for the same
    // default. A developer changing GameConfig has to beat a stale memory --
    // otherwise the edit does nothing and the only cure is deleting a state
    // file nobody remembers exists.
    let (saved, saved_dropped) = match window_placement::saved_geometry(&slug, &topo) {
        Some((partial, saved_req)) => {
            window_placement::drop_stale_saved_size(partial, saved_req, base.size)
        }
        None => (window_placement::PartialGeometry::default(), false),
    };
    if saved_dropped {
        ::log::info!(
            target: "STONE",
            "saved window size ignored (game '{}' now requests {}x{}); position kept",
            slug, base.size.0, base.size.1,
        );
    }
    let sources = [
        (Source::Env, env),
        (Source::Saved, saved),
        (Source::ConfigGame, machine.game),
        (Source::ConfigDefault, machine.default),
    ];
    let (mut resolved, prov) = window_placement::merge_geometry(base, &sources);

    ::log::info!(
        target: "STONE",
        "window geometry files: saved={} config={}",
        window_placement::geometry_file_path().map_or_else(|| "<unresolved: no HOME/XDG_STATE_HOME>".to_string(), |p| p.display().to_string()),
        window_placement::config_file_path().map_or_else(|| "<unresolved: no HOME/XDG_CONFIG_HOME>".to_string(), |p| p.display().to_string()),
    );

    // A game's explicit monitor (base.display -- e.g. a settings-screen choice)
    // outranks saved geometry for WHICH display the window is on: if the
    // remembered position isn't on that display, center the window there (the
    // remembered size is kept). Env / machine-config displays are manual
    // overrides and still win; a game with no preference (base.display == None)
    // leaves saved geometry in full control.
    if let Some(d) = base.display {
        if matches!(prov.display, Source::Saved | Source::Default) {
            if let Some(disp) = displays.iter().find(|x| x.index == d) {
                let (bx, by, bw, bh) = disp.usable;
                let on_display = resolved.pos.0 >= bx
                    && resolved.pos.0 < bx + bw
                    && resolved.pos.1 >= by
                    && resolved.pos.1 < by + bh;
                if resolved.display != Some(d) || !on_display {
                    resolved.pos = (
                        bx + (bw - resolved.size.0) / 2,
                        by + (bh - resolved.size.1) / 2,
                    );
                    resolved.display = Some(d);
                }
            }
        }
    }

    if prov.size != Source::Default
        || prov.pos != Source::Default
        || prov.display != Source::Default
    {
        ::log::info!(
            target: "STONE",
            "window geometry (game '{}' topo {}): size {}x{} ({:?}) pos ({},{}) ({:?}) display {:?} ({:?})",
            slug, topo,
            resolved.size.0, resolved.size.1, prov.size,
            resolved.pos.0, resolved.pos.1, prov.pos,
            resolved.display, prov.display,
        );
    }
    resolved
}

/// Clamp a requested window position so the whole frame stays on-screen, using
/// the live window's size + borders. Returns the input unchanged when fit is
/// disabled. The runner repositions the window after creation, and saved
/// geometry from a detached monitor has the same shape, so the move path needs
/// the same clamp as creation.
fn clamp_window_position(
    video: &sdl2::VideoSubsystem,
    window: &sdl2::video::Window,
    x: i32,
    y: i32,
) -> (i32, i32) {
    let headless = std::env::var("STONE_HEADLESS").is_ok();
    if !window_fit_enabled(headless) {
        return (x, y);
    }
    let borders = effective_borders(query_window_borders(window));
    let (w, h) = window.size();
    let displays = gather_display_reports(video);
    let req = window_report::WindowRequest {
        size: (w as i32, h as i32),
        pos: (x, y),
        display: None,
    };
    let p = window_report::resolve_window_placement(req, &displays, borders);
    if p.pos != (x, y) {
        ::log::info!(
            target: "STONE",
            "window move clamped: ({},{}) -> ({},{}) (size {}x{} borders t{} l{} b{} r{})",
            x, y, p.pos.0, p.pos.1, w, h, borders.top, borders.left, borders.bottom, borders.right,
        );
    }
    p.pos
}

// =============================================================================
// Logging
// =============================================================================

// Screenshot - read framebuffer and save as PNG
// Helper functions for stats tracking
pub(crate) fn track_draw_call() {
    FRAME_STATS.with(|fs| {
        fs.borrow_mut().draw_calls += 1;
    });
}

pub(crate) fn track_text_draw_call() {
    FRAME_STATS.with(|fs| {
        fs.borrow_mut().text_draw_calls += 1;
    });
}

#[no_mangle]
pub extern "C" fn stone_track_text_draw_call() {
    track_text_draw_call();
}

pub(crate) fn track_canvas_switch() {
    FRAME_STATS.with(|fs| {
        fs.borrow_mut().canvas_switches += 1;
    });
}

pub(crate) fn track_shader_switch() {
    FRAME_STATS.with(|fs| {
        fs.borrow_mut().shader_switches += 1;
    });
}

pub(crate) fn track_stream_upload(vertices: usize, bytes: usize, storage_replaced: bool) {
    FRAME_STATS.with(|fs| {
        let mut stats = fs.borrow_mut();
        stats.stream_upload_calls = stats.stream_upload_calls.saturating_add(1);
        if storage_replaced {
            stats.stream_storage_replacements = stats.stream_storage_replacements.saturating_add(1);
        }
        stats.stream_upload_bytes = stats
            .stream_upload_bytes
            .saturating_add(i32::try_from(bytes).unwrap_or(i32::MAX));
        stats.streamed_vertices = stats
            .streamed_vertices
            .saturating_add(i32::try_from(vertices).unwrap_or(i32::MAX));
    });
}

pub(crate) fn track_retained_upload(vertices: usize, bytes: usize) {
    FRAME_STATS.with(|fs| {
        let mut stats = fs.borrow_mut();
        stats.retained_upload_calls = stats.retained_upload_calls.saturating_add(1);
        stats.retained_upload_bytes = stats
            .retained_upload_bytes
            .saturating_add(i32::try_from(bytes).unwrap_or(i32::MAX));
        stats.retained_uploaded_vertices = stats
            .retained_uploaded_vertices
            .saturating_add(i32::try_from(vertices).unwrap_or(i32::MAX));
    });
}

pub(crate) fn track_instanced_draw() {
    FRAME_STATS.with(|fs| {
        fs.borrow_mut().instanced_draws += 1;
    });
}

fn texture_bytes_to_mib(bytes: u64) -> c_int {
    c_int::try_from(bytes / (1024 * 1024)).unwrap_or_else(|_| {
        crate::log_once!(
            "STONE",
            "live GPU textures exceed the graphics stats MiB range ({bytes} bytes)"
        );
        c_int::MAX
    })
}

#[no_mangle]
pub extern "C" fn stone_get_graphics_stats(stats: *mut GraphicsStats) {
    if stats.is_null() {
        return;
    }

    FRAME_STATS.with(|fs| {
        let frame_stats = fs.borrow();

        // Count active images
        let image_count = IMAGES.with(|imgs| imgs.borrow().len() as c_int);

        // Count active canvases
        let canvas_count = canvas::get_canvas_count() as c_int;

        let texture_memory_mib = texture_bytes_to_mib(live_texture_bytes());

        unsafe {
            (*stats).draw_calls = frame_stats.draw_calls;
            (*stats).text_draw_calls = frame_stats.text_draw_calls;
            (*stats).canvas_switches = frame_stats.canvas_switches;
            (*stats).shader_switches = frame_stats.shader_switches;
            (*stats).stream_upload_calls = frame_stats.stream_upload_calls;
            (*stats).stream_storage_replacements = frame_stats.stream_storage_replacements;
            (*stats).stream_upload_bytes = frame_stats.stream_upload_bytes;
            (*stats).streamed_vertices = frame_stats.streamed_vertices;
            (*stats).retained_upload_calls = frame_stats.retained_upload_calls;
            (*stats).retained_upload_bytes = frame_stats.retained_upload_bytes;
            (*stats).retained_uploaded_vertices = frame_stats.retained_uploaded_vertices;
            (*stats).images = image_count;
            (*stats).canvases = canvas_count;
            (*stats).texture_memory_mib = texture_memory_mib;
            (*stats).instanced_draws = frame_stats.instanced_draws;
        }
    });
}

#[no_mangle]
pub extern "C" fn stone_get_live_texture_bytes() -> u64 {
    // Legacy broad-memory callers mean every tracked image level and canvas
    // attachment. Callers needing one category use the counters below.
    live_texture_bytes()
}

#[no_mangle]
pub extern "C" fn stone_get_live_image_bytes() -> u64 {
    live_image_bytes()
}

#[no_mangle]
pub extern "C" fn stone_get_live_mip_bytes() -> u64 {
    live_mip_bytes()
}

#[no_mangle]
pub extern "C" fn stone_get_live_canvas_bytes() -> u64 {
    canvas::live_canvas_bytes()
}

#[no_mangle]
pub extern "C" fn stone_get_live_image_handle(index: c_int) -> u64 {
    if index < 0 {
        return 0;
    }
    IMAGES.with(|images| {
        let mut entries: Vec<(u64, u32, u32)> = images
            .borrow()
            .iter()
            .map(|(id, image)| (*id, image.width, image.height))
            .collect();
        entries.sort_unstable_by_key(|entry| entry.0);
        entries.get(index as usize).map_or(0, |entry| entry.0)
    })
}

#[no_mangle]
pub extern "C" fn stone_reset_frame_stats() {
    FRAME_STATS.with(|fs| {
        let mut stats = fs.borrow_mut();
        stats.draw_calls = 0;
        stats.text_draw_calls = 0;
        stats.canvas_switches = 0;
        stats.shader_switches = 0;
        stats.stream_upload_calls = 0;
        stats.stream_storage_replacements = 0;
        stats.stream_upload_bytes = 0;
        stats.streamed_vertices = 0;
        stats.retained_upload_calls = 0;
        stats.retained_upload_bytes = 0;
        stats.retained_uploaded_vertices = 0;
        stats.instanced_draws = 0;
    });
    image_texture::reset_image_frame_stats();
    // The frame that just ended is the window in which a block had to be filled.
    crate::uniform_block::report_unfilled_blocks();
    with_gl(|gl| {
        stream_buffer::begin_frame(gl);
        uniform_block::rebind_current_program(gl);
    });
}

// ImageData functions

// Re-export sprite functions that are used internally or externally
pub use mesh::{stone_mesh_create, stone_mesh_draw, stone_mesh_free, stone_mesh_set_instances};
pub use sprite::{stone_draw_mesh, stone_sprite_draw_texture};

// Shader compile/link helpers now live in the shared `stone-gl` crate
// (re-exported above for `crate::compile_shader` / `crate::link_program`).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_interactive_offscreen_video_fallback_rejects() {
        assert!(native_video_driver_error(false, None, "offscreen").is_some());
        assert!(native_video_driver_error(false, None, "dummy").is_some());
        assert!(native_video_driver_error(false, None, "x11").is_none());
        assert!(native_video_driver_error(false, None, "KMSDRM").is_none());
        assert!(native_video_driver_error(true, None, "offscreen").is_none());
        assert!(native_video_driver_error(false, Some("offscreen"), "offscreen").is_none());
    }

    #[test]
    fn graphics_texture_memory_reports_above_two_gib() {
        assert_eq!(texture_bytes_to_mib(3 * 1024 * 1024 * 1024), 3072);
        assert_eq!(texture_bytes_to_mib(1024 * 1024 - 1), 0);
    }

    #[test]
    fn c_header_smoke_exports_have_the_physical_rust_abi() {
        let _: extern "C" fn() -> bool = stone_init;
        let _: extern "C" fn() -> bool = stone_teardown;
        let _: extern "C" fn(
            *const c_char,
            c_int,
            c_int,
            c_int,
            c_int,
            c_int,
            bool,
            c_int,
        ) -> *mut StoneWindow = stone_create_window_ex;
        let _: extern "C" fn(*mut c_int, *mut c_int) = stone_get_window_size;
        let _: extern "C" fn(*mut c_int, *mut c_int) = stone_get_drawable_size;
        let _: extern "C" fn(*mut GraphicsStats) = stone_get_graphics_stats;
        let _: extern "C" fn() = stone_track_text_draw_call;
        let _: extern "C" fn(*mut image_texture::ImageLoadStats) =
            image_texture::stone_get_image_frame_stats;
        let _: extern "C" fn(*mut image_texture::ImageLoadStats) =
            image_texture::stone_get_image_profile_stats;
        let _: extern "C" fn() -> c_float = stone_get_dpi_scale;
        let _: extern "C" fn() -> c_float = stone_get_config_imgui_scale;
        let _: extern "C" fn(c_float, c_float, c_float, c_float) = canvas::stone_clear_canvas;
        let _: extern "C" fn(c_int, c_int) = canvas::stone_clear_canvas_int;
        let _: extern "C" fn(u64, u64) -> bool = canvas::stone_canvas_attach_id_target;
        let _: extern "C" fn() -> u64 = image_texture::stone_get_white_image;
        let _: extern "C" fn(*mut StoneWindow) = stone_swap_window;
        let _: extern "C" fn(u32) = stone_delay;
        let _: extern "C" fn(*const c_char, *const c_char) -> *const c_char =
            filesystem::stone_get_pref_path;
        let _: extern "C" fn() -> bool = imgui::stone_imgui_using_mouse;
        let _: extern "C" fn(*const c_char, c_int) -> bool = imgui::stone_imgui_begin_window;
        let _: extern "C" fn(*const c_char, *const c_char) -> bool = imgui::stone_imgui_begin_combo;
        let _: extern "C" fn() = imgui::stone_imgui_end_combo;
        let _: extern "C" fn() = imgui::stone_imgui_set_item_default_focus;
        let _: extern "C" fn(*const c_char) = imgui::stone_imgui_open_popup;
        let _: extern "C" fn(*const c_char) -> bool = imgui::stone_imgui_begin_popup;
        let _: extern "C" fn() = imgui::stone_imgui_end_popup;
        let _: extern "C" fn(*const c_char, bool) -> bool = imgui::stone_imgui_menu_item;
        let _: extern "C" fn(*const c_char, c_int, StoneString, *mut bool, *mut c_int) =
            imgui::stone_imgui_combo_str;
        let _: extern "C" fn(*const c_char, *mut c_char, usize, c_int) -> bool =
            imgui::stone_imgui_input_text;
        let _: extern "C" fn(*const c_char, *mut c_char, usize, c_float, c_float, c_int) -> bool =
            imgui::stone_imgui_input_text_multiline;
        let _: extern "C" fn(*const c_char, c_int, c_int) -> bool = imgui::stone_imgui_begin_table;
        let _: extern "C" fn() = imgui::stone_imgui_end_table;
        let _: extern "C" fn() = imgui::stone_imgui_table_headers_row;
        let _: extern "C" fn() = imgui::stone_imgui_table_next_row;
        let _: extern "C" fn() -> bool = imgui::stone_imgui_table_next_column;
        let _: extern "C" fn(*const c_char, c_float) = imgui::stone_imgui_table_setup_column_fixed;
        let _: extern "C" fn(*const c_char) = imgui::stone_imgui_table_setup_column_stretch;
        let _: extern "C" fn(bool) = imgui::stone_imgui_begin_disabled;
        let _: extern "C" fn() = imgui::stone_imgui_end_disabled;
        let _: extern "C" fn(c_float) = imgui::stone_imgui_indent;
        let _: extern "C" fn(c_float) = imgui::stone_imgui_unindent;
        let _: extern "C" fn(*const c_char) -> bool = imgui::stone_imgui_collapsing_header_closed;
        let _: extern "C" fn(*const c_char, c_float, c_float) -> bool =
            imgui::stone_imgui_invisible_button;
        let _: extern "C" fn(*const c_char, bool) -> bool = imgui::stone_imgui_selectable;
        let _: extern "C" fn(c_int) -> bool = imgui::stone_imgui_is_item_hovered;
        let _: extern "C" fn() -> bool = imgui::stone_imgui_is_item_active;
        let _: extern "C" fn() -> bool = imgui::stone_imgui_is_item_activated;
        let _: extern "C" fn() -> bool = imgui::stone_imgui_is_item_deactivated;
        let _: extern "C" fn() -> bool = imgui::stone_imgui_is_item_deactivated_after_edit;
        let _: extern "C" fn(*mut c_float, *mut c_float) = imgui::stone_imgui_get_mouse_pos;
        let _: extern "C" fn(*mut c_float, *mut c_float) =
            imgui::stone_imgui_get_content_region_avail;
        let _: extern "C" fn(*mut c_float, *mut c_float) = imgui::stone_imgui_get_window_pos;
        let _: extern "C" fn(*mut c_float, *mut c_float) = imgui::stone_imgui_get_window_size;
        let _: extern "C" fn() -> bool = imgui::stone_imgui_is_any_item_active;
        let _: extern "C" fn(c_float, c_float, c_float, c_float, std::ffi::c_uint) =
            imgui::stone_imgui_window_add_rect_filled;
        let _: extern "C" fn() -> bool = imgui::stone_imgui_is_item_visible;
        let _: extern "C" fn() -> bool = imgui::stone_imgui_is_window_docked;
        let _: extern "C" fn() -> bool = imgui::stone_imgui_wants_mouse;
        let _: extern "C" fn() -> bool = imgui::stone_imgui_wants_text_input;
        let _: extern "C" fn(*mut c_float, *mut c_float) = imgui::stone_imgui_get_cursor_screen_pos;
        let _: extern "C" fn(*mut c_float, *mut c_float) = imgui::stone_imgui_get_item_rect_min;
        let _: extern "C" fn(*mut c_float, *mut c_float) = imgui::stone_imgui_get_item_rect_max;
        let _: extern "C" fn(c_float, c_float) = imgui::stone_imgui_dummy;
        let _: extern "C" fn() = imgui::stone_imgui_end_child;
        let _: extern "C" fn(*const c_char, c_float, c_float, c_float, c_float) =
            imgui::stone_imgui_text_colored;
        let _: extern "C" fn(c_float, c_float) = imgui::stone_imgui_set_cursor_screen_pos;
        let _: extern "C" fn(c_int, std::ffi::c_uint) = imgui::stone_imgui_push_style_color;
        let _: extern "C" fn(c_int) = imgui::stone_imgui_pop_style_color;
        let _: extern "C" fn(c_float) = imgui::stone_imgui_set_next_item_width;
        let _: extern "C" fn(*const c_char, c_float, c_float, c_float, *mut bool, *mut c_float) =
            imgui::stone_imgui_slider_float;
        let _: extern "C" fn(*const c_char, c_int, c_int, c_int, *mut bool, *mut c_int) =
            imgui::stone_imgui_slider_int;
        let _: extern "C" fn(*const c_char, bool, *mut bool, *mut bool) =
            imgui::stone_imgui_checkbox;
        let _: extern "C" fn(
            *const c_char,
            c_float,
            c_float,
            c_float,
            c_float,
            *mut bool,
            *mut c_float,
            *mut c_float,
            *mut c_float,
            *mut c_float,
        ) = imgui::stone_imgui_color_edit4;
        let _: extern "C" fn(c_float) = imgui::stone_imgui_set_scroll_here_y;
        let _: extern "C" fn(*const c_char) = imgui::stone_imgui_set_tooltip;
        let _: extern "C" fn(c_float, c_float, c_float, c_float, std::ffi::c_uint, c_float) =
            imgui::stone_imgui_window_add_line;
        let _: extern "C" fn() -> c_int = input::stone_scancode_count;
        let _: extern "C" fn(c_int) -> *const c_char = input::stone_scancode_token_at;
        let _: extern "C" fn(c_int) -> *const c_char = input::stone_scancode_token_for;
        let _: extern "C" fn(*const c_char) -> c_int = input::stone_scancode_value;
        let _: extern "C" fn(*const c_char) -> *const c_char = input::stone_scancode_label;
        let _: extern "C" fn(*const c_char, *mut usize) -> *mut c_char =
            filesystem::stone_file_read;
        let _: extern "C" fn(*mut c_void) = filesystem::stone_free;
        let _: extern "C" fn(*const c_char, *const c_char, usize) -> bool =
            filesystem::stone_file_append;
        let _: extern "C" fn(*const c_char) -> bool = telemetry::stone_telemetry_clear;
        let _: extern "C" fn(
            *const c_char,
            *const c_char,
            *const c_char,
            *const c_char,
            *const c_char,
            *const c_char,
            *const c_char,
            bool,
        ) -> bool = telemetry::stone_telemetry_configure;
        let _: extern "C" fn(u64) -> u64 = text::stone_get_font_instance_image;
    }

    #[test]
    fn native_key_event_preserves_both_identities_modifiers_and_repeat() {
        let sdl_event = Event::KeyDown {
            timestamp: 1,
            window_id: 2,
            keycode: Some(sdl2::keyboard::Keycode::A),
            scancode: Some(sdl2::keyboard::Scancode::Q),
            keymod: sdl2::keyboard::Mod::LCTRLMOD
                | sdl2::keyboard::Mod::RCTRLMOD
                | sdl2::keyboard::Mod::LSHIFTMOD
                | sdl2::keyboard::Mod::RSHIFTMOD
                | sdl2::keyboard::Mod::LALTMOD
                | sdl2::keyboard::Mod::RALTMOD
                | sdl2::keyboard::Mod::LGUIMOD
                | sdl2::keyboard::Mod::RGUIMOD,
            repeat: true,
        };

        let event = stone_event_from_sdl(&sdl_event).expect("key event must reach Stone");
        assert_eq!(event.event_type, StoneEventType::KeyDown);
        assert_eq!(event.key, sdl2::keyboard::Keycode::A.into_i32());
        assert_eq!(event.scancode, sdl2::keyboard::Scancode::Q as c_int);
        assert_eq!(event.modifiers, 0b1111);
        assert!(event.repeat);
    }

    #[test]
    fn native_text_event_preserves_utf8() {
        let sdl_event = Event::TextInput {
            timestamp: 1,
            window_id: 2,
            text: "M\u{fd}tina".to_string(),
        };

        let event = stone_event_from_sdl(&sdl_event).expect("text input must reach Stone");
        assert_eq!(event.event_type, StoneEventType::TextInput);
        assert_eq!(event.text(), "M\u{fd}tina");
    }

    #[test]
    fn pending_show_geometry_is_not_persisted() {
        assert!(!can_persist_window_geometry(
            false,
            true,
            sdl2::video::FullscreenType::Off,
        ));
        assert!(can_persist_window_geometry(
            false,
            false,
            sdl2::video::FullscreenType::Off,
        ));
        assert!(!can_persist_window_geometry(
            false,
            false,
            sdl2::video::FullscreenType::Desktop,
        ));
        assert!(!can_persist_window_geometry(
            true,
            false,
            sdl2::video::FullscreenType::Off,
        ));
    }

    #[test]
    fn nix_library_path_finds_sdl_gl_and_egl_drivers() {
        let temp = tempfile::tempdir().unwrap();
        let gl_dir = temp.path().join("gl");
        let egl_dir = temp.path().join("egl");
        std::fs::create_dir_all(&gl_dir).unwrap();
        std::fs::create_dir_all(&egl_dir).unwrap();
        std::fs::write(gl_dir.join("libGL.so.1"), []).unwrap();
        std::fs::write(egl_dir.join("libEGL.so.1"), []).unwrap();
        let search = std::env::join_paths([&gl_dir, &egl_dir]).unwrap();

        assert_eq!(
            sdl_graphics_driver_paths(&search),
            (
                Some(gl_dir.join("libGL.so.1")),
                Some(egl_dir.join("libEGL.so.1")),
            )
        );
    }
}
