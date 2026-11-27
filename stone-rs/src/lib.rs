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
#[derive(Clone, Copy)]
enum LuaAllocCounter {
    GrossRequested,
    PositiveGrowth,
    ExplicitFreed,
    ReallocShrunk,
    Calls,
    AllocationCalls,
    ReallocationCalls,
    FreeCalls,
}

static LUA_ALLOC_COUNTERS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

impl LuaAllocCounter {
    fn add(self, value: u64) {
        LUA_ALLOC_COUNTERS[self as usize].fetch_add(value, Ordering::Relaxed);
    }

    fn get(self) -> u64 {
        LUA_ALLOC_COUNTERS[self as usize].load(Ordering::Relaxed)
    }
}

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
        LuaAllocCounter::Calls.add(1);
        if nsize == 0 {
            if !ptr.is_null() {
                // Only nsize == 0 is an unambiguous free; a realloc may shrink in place.
                LuaAllocCounter::FreeCalls.add(1);
                LuaAllocCounter::ExplicitFreed.add(osize as u64);
            }
        } else {
            LuaAllocCounter::GrossRequested.add(nsize as u64);
            if ptr.is_null() {
                LuaAllocCounter::AllocationCalls.add(1);
                LuaAllocCounter::PositiveGrowth.add(nsize as u64);
            } else {
                LuaAllocCounter::ReallocationCalls.add(1);
                if nsize >= osize {
                    LuaAllocCounter::PositiveGrowth.add((nsize - osize) as u64);
                } else {
                    LuaAllocCounter::ReallocShrunk.add((osize - nsize) as u64);
                }
            }
        }
    }
    // Allocation callbacks cannot unwind or call back into Lua.
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
    for counter in LUA_ALLOC_COUNTERS
        .iter()
        .chain(LUA_ALLOC_ORIGIN_BYTES.iter())
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
    LuaAllocCounter::GrossRequested.get() as c_double / 1024.0
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_positive_growth_kb() -> c_double {
    LuaAllocCounter::PositiveGrowth.get() as c_double / 1024.0
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_explicit_free_kb() -> c_double {
    LuaAllocCounter::ExplicitFreed.get() as c_double / 1024.0
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_reallocation_shrink_kb() -> c_double {
    LuaAllocCounter::ReallocShrunk.get() as c_double / 1024.0
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_call_count() -> u64 {
    LuaAllocCounter::Calls.get()
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_allocation_call_count() -> u64 {
    LuaAllocCounter::AllocationCalls.get()
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_reallocation_call_count() -> u64 {
    LuaAllocCounter::ReallocationCalls.get()
}

#[no_mangle]
pub extern "C" fn stone_lua_alloc_free_call_count() -> u64 {
    LuaAllocCounter::FreeCalls.get()
}

use sdl2::event::Event;
use sdl2::keyboard::Mod;
use sdl2::video::GLProfile;

pub mod log;
#[doc(hidden)]
pub use rock_log as rock_log_reexport;
mod actor_query;
pub mod asset_registry;
mod asset_watcher;
mod audio;
mod cursor;
mod ffi_array;
mod file_jobs;
mod filesystem;
mod frame_stream;
mod image_data;
pub mod image_decode;
mod image_publication;
mod image_texture;
mod imgui;
mod input;
mod jobs;
mod light_table;
mod meadow;
mod native_ffi {
    include!(concat!(env!("OUT_DIR"), "/stone-native/ffi.rs"));
}
mod engine_ffi {
    include!(concat!(env!("OUT_DIR"), "/stone/ffi.rs"));
}
use engine_ffi::STONE_TEXT_INPUT_BYTES;
pub use engine_ffi::{GraphicsStats, StoneEvent};
mod rendering_ffi {
    include!(concat!(env!("OUT_DIR"), "/stone-rendering/ffi.rs"));
}

mod relight_jobs;
mod screenshot;
#[cfg(test)]
mod test_fonts;
#[cfg(test)]
mod test_support;
pub(crate) use image_texture::{
    image_write_target, live_image_bytes, live_mip_bytes, live_texture_bytes, stone_unload_image,
    IMAGES, WHITE_IMAGE,
};
mod canvas;
pub use canvas::stone_get_live_canvas_bytes;
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
pub mod native_renderer;
mod pixel_readback;
mod shader;
mod sprite;
mod stream_buffer;
mod system_report;
mod text;
pub use text::stone_track_text_draw_call;
mod ui_submission_probe;
mod uniform_block;
pub mod window_placement;
pub mod window_report;
#[cfg(target_os = "macos")]
mod macos_focus {
    use objc2::runtime::AnyObject;
    use objc2::MainThreadMarker;
    use objc2::{class, msg_send};
    use std::cell::Cell;

    const POLICY_ACCESSORY: i64 = 1;
    const POLICY_REGULAR: i64 = 0;

    #[link(name = "AppKit", kind = "framework")]
    extern "C" {}

    thread_local! {
        /// PID of the app frontmost before SDL init. Teardown consumes it only
        /// when Stone still owns frontmost status. 0 means "nothing saved".
        static PREVIOUS_FRONTMOST_PID: Cell<i32> = const { Cell::new(0) };

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
                // Returns BOOL, not void; a mismatched type panics under objc2's type check.
                let _: bool = msg_send![app, setActivationPolicy: POLICY_ACCESSORY];
            }
        }
    }

    /// Explicit activation updates the macOS Cmd-Tab MRU after promotion.
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

/// SDL may translate through a window-manager frame destroyed during teardown.
/// Its X11 safety net delegates to a fatal handler, so retain and log errors
/// here; window moves retry BadWindow and BadDrawable.
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

    fn x11_window(
        window: &sdl2::video::Window,
    ) -> Option<(*mut x11::xlib::Display, x11::xlib::Window)> {
        use sdl2::sys;

        let mut wm_info: sys::SDL_SysWMinfo = unsafe { std::mem::zeroed() };
        unsafe { sys::SDL_GetVersion(&mut wm_info.version) };
        if unsafe { sys::SDL_GetWindowWMInfo(window.raw(), &mut wm_info) }
            == sys::SDL_bool::SDL_FALSE
            || wm_info.subsystem != sys::SDL_SYSWM_TYPE::SDL_SYSWM_X11
        {
            return None;
        }
        let xinfo = unsafe { wm_info.info.x11 };
        let display = xinfo.display as *mut x11::xlib::Display;
        if display.is_null() {
            None
        } else {
            Some((display, xinfo.window))
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
            crate::log_once!("STONE", "X error tolerated (the window manager replaced the frame under the request): {error}");
        } else {
            ::log::error!(target: "STONE", "X error surfaced (not fatal): {error}");
        }
        0
    }

    /// Install stone's X error handler for this window's display. Called once
    /// per display, so a re-initialized video subsystem gets it again.
    pub fn install_error_handler(window: &sdl2::video::Window) -> bool {
        static HANDLED_DISPLAY: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
        let Some((display, _)) = x11_window(window) else {
            return false;
        };
        if HANDLED_DISPLAY.load(Ordering::SeqCst) == display as *mut std::ffi::c_void {
            return false;
        }
        unsafe { x11::xlib::XSetErrorHandler(Some(handle)) };
        HANDLED_DISPLAY.store(display as *mut std::ffi::c_void, Ordering::SeqCst);
        true
    }

    /// Whether `_NET_WM_STATE` on the window lists `_NET_WM_STATE_FULLSCREEN`.
    /// None when the window is not on X11 or the state cannot be read.
    pub(super) fn net_wm_fullscreen(window: &sdl2::video::Window) -> Option<bool> {
        use x11::xlib;

        let (display, xwindow) = x11_window(window)?;
        unsafe {
            let state = xlib::XInternAtom(display, c"_NET_WM_STATE".as_ptr(), 0);
            let fullscreen = xlib::XInternAtom(display, c"_NET_WM_STATE_FULLSCREEN".as_ptr(), 0);
            let (mut actual_type, mut actual_format) = (0, 0);
            let (mut items, mut bytes_after) = (0, 0);
            let mut prop: *mut u8 = std::ptr::null_mut();
            let status = xlib::XGetWindowProperty(
                display,
                xwindow,
                state,
                0,
                1024,
                0,
                xlib::XA_ATOM,
                &mut actual_type,
                &mut actual_format,
                &mut items,
                &mut bytes_after,
                &mut prop,
            );
            let listed = if status != xlib::Success as c_int || bytes_after != 0 {
                crate::log_once!(
                    "STONE",
                    "cannot read the complete WM fullscreen state; keeping SDL state"
                );
                None
            } else if actual_type == 0 {
                Some(false)
            } else if actual_type == xlib::XA_ATOM && actual_format == 32 && !prop.is_null() {
                // Format-32 property data is an array of C long per Xlib ABI.
                let atoms =
                    std::slice::from_raw_parts(prop as *const std::ffi::c_ulong, items as usize);
                Some(atoms.contains(&fullscreen))
            } else {
                crate::log_once!(
                    "STONE",
                    "invalid WM fullscreen state property; keeping SDL state"
                );
                None
            };
            if !prop.is_null() {
                xlib::XFree(prop.cast());
            }
            listed
        }
    }

    /// The window's position on the root window and its size, read from the X
    /// server rather than SDL's cache. None when the window is not on X11.
    pub(super) fn root_geometry(window: &sdl2::video::Window) -> Option<((i32, i32), (u32, u32))> {
        use x11::xlib;

        let (display, xwindow) = x11_window(window)?;
        unsafe {
            let mut attrs: xlib::XWindowAttributes = std::mem::zeroed();
            if xlib::XGetWindowAttributes(display, xwindow, &mut attrs) == 0 {
                return None;
            }
            let (mut x, mut y) = (0, 0);
            let mut child: xlib::Window = 0;
            if xlib::XTranslateCoordinates(
                display, xwindow, attrs.root, 0, 0, &mut x, &mut y, &mut child,
            ) == 0
            {
                return None;
            }
            Some((
                (x, y),
                (attrs.width.max(1) as u32, attrs.height.max(1) as u32),
            ))
        }
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

    /// Set EWMH user-time zero before mapping to suppress initial focus.
    /// SDL supplies a real timestamp on subsequent input.
    pub fn set_user_time_zero(window: &sdl2::video::Window) {
        use sdl2::sys;

        // The raw SDL query returns failure; the wrapper panics across FFI.
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

    /// Set USPosition before mapping so the WM honors the requested display.
    /// Preserve SDL size hints.
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
#[cfg(target_os = "linux")]
const MOVE_ATTEMPTS: u32 = 3;

/// SDL may translate through an X11 frame being torn down; retry that error.
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

struct PendingShow {
    pos: (i32, i32),
    map_requested: bool,
    activate: bool,
}

pub struct StoneWindow {
    _sdl_context: sdl2::Sdl,
    _video: sdl2::VideoSubsystem,
    pub(crate) window: sdl2::video::Window,
    _gl_context: sdl2::video::GLContext,
    gl: Rc<DebugGl>,
    event_pump: RefCell<sdl2::EventPump>,
    controller_manager: RefCell<input::SdlControllerManager>,
    // X11 mapping is deferred until the frame loop can reassert the position.
    pending_show: Option<PendingShow>,
    runtime_hidden: bool,
    title: String,
    window_mode: Cell<window_placement::WindowMode>,
    #[cfg(target_os = "linux")]
    wm_fullscreen_confirmed: Cell<bool>,
    windowed_bounds: Cell<window_placement::WindowedBounds>,
    saved_state_on_hide: RefCell<Option<window_placement::HiddenWindowState>>,
    initial_hidden_boot: Cell<bool>,
    requested_size: (i32, i32),
    requested_pos: (i32, i32),
    requested_display: Option<i32>,
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
        }
        | Event::KeyUp {
            keycode,
            scancode,
            keymod,
            repeat,
            ..
        } => {
            translated.event_type = if matches!(event, Event::KeyDown { .. }) {
                StoneEventType::KeyDown
            } else {
                StoneEventType::KeyUp
            };
            translated.key = keycode.map(|value| value.into_i32()).unwrap_or(0);
            translated.scancode = scancode.map(|value| value as c_int).unwrap_or(0);
            translated.modifiers = normalized_key_modifiers(*keymod);
            translated.repeat = *repeat;
        }
        Event::MouseButtonDown {
            mouse_btn, x, y, ..
        }
        | Event::MouseButtonUp {
            mouse_btn, x, y, ..
        } => {
            translated.event_type = if matches!(event, Event::MouseButtonDown { .. }) {
                StoneEventType::MouseButtonDown
            } else {
                StoneEventType::MouseButtonUp
            };
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

// We use thread-local storage for the window since SDL2 isn't thread-safe anyway
thread_local! {
    pub(crate) static WINDOW: RefCell<Option<Box<StoneWindow>>> = const { RefCell::new(None) };
    static GPU_HOST_LIFETIME: Cell<GpuHostLifetime> = const { Cell::new(GpuHostLifetime::Unclaimed) };
    pub(crate) static SHADERS: RefCell<HashMap<u64, shader::Shader>> = RefCell::new(HashMap::new());
    pub(crate) static CURRENT_SHADER: RefCell<Option<glow::Program>> = const { RefCell::new(None) };
    static FRAME_STATS: RefCell<GraphicsStats> = RefCell::new(GraphicsStats::default());
    static GPU_TIMER: RefCell<gpu_timer::GpuTimer> = RefCell::new(gpu_timer::GpuTimer::new());
    // Borders reported at window creation, re-checked once after the first
    // present (X11 _NET_FRAME_EXTENTS can be 0 until the window is mapped).
    // Set only on a visible window, so headless runs stay quiet.
    static INITIAL_BORDERS: std::cell::Cell<Option<window_report::Borders>> = const { std::cell::Cell::new(None) };
    static POST_FRAME_BORDERS_LOGGED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
        handles::set_context_live(false);
    });
}

/// Borrow the process GPU context, creating a hidden host only on first use.
/// A lost host cannot be recreated: its callers retain context-owned resources.
/// rock-ffi stone private
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
// Deferred: the loop ends after the current frame, and stone_teardown runs after it exits.
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn with_window<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&StoneWindow) -> R,
{
    WINDOW.with(|w| w.borrow().as_ref().map(|window| f(window)))
}

pub(crate) fn with_window_mut<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut StoneWindow) -> R,
{
    WINDOW.with(|w| w.borrow_mut().as_mut().map(|window| f(window)))
}

static OFF_THREAD_GL_CALLS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
fn off_thread_gl_calls() -> u64 {
    OFF_THREAD_GL_CALLS.load(Ordering::Relaxed)
}

pub(crate) fn with_gl<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&DebugGl) -> R,
{
    let result = with_window(|w| f(&w.gl));
    if result.is_none() && handles::context_live() {
        OFF_THREAD_GL_CALLS.fetch_add(1, Ordering::Relaxed);
        crate::log_once!(
            "STONE",
            "a Stone rendering call ran off the render thread and was ignored; every rendering entry is render-thread only"
        );
    }
    result
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_init() -> bool {
    rock_log::init();
    QUIT_REQUESTED.store(false, Ordering::SeqCst);
    INITIALIZED.store(true, Ordering::SeqCst);
    true
}

/// Request quit after the current frame, keeping its window and GL context alive.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_quit() {
    QUIT_REQUESTED.store(true, Ordering::SeqCst);
}

/// Programmatic quit does not produce an SDL Quit event.
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_quit_requested() -> bool {
    QUIT_REQUESTED.load(Ordering::SeqCst)
}

/// Release context-owned resources while the context is current. Call after the
/// main loop, never mid-frame. Safe if already torn down or never initialized.
#[no_mangle]
pub extern "C" fn stone_teardown() -> bool {
    crate::relight_jobs::shutdown();
    input::test_virtual_pad_cleanup();
    // ImGui's OpenGL3 backend shutdown needs the live GL context. Idempotent:
    // early-returns if ImGui was never initialized.
    crate::imgui::stone_imgui_shutdown();
    crate::pixel_readback::release_all();
    // An SDL_Cursor must be freed before SDL_Quit, and dropping WINDOW drops
    // the sdl2::Sdl context.
    crate::cursor::free_all_cursors();
    text::shutdown();
    sprite::shutdown();
    with_gl(stream_buffer::release_all);
    shader::destroy_all();
    canvas::destroy_all();
    canvas::reset_current_canvas_handle();
    native_renderer::reset_on_teardown();
    image_texture::unload_all();
    gl_state::forget_samplers();
    mesh::clear_all();
    GPU_TIMER.with(|timer| {
        *timer.borrow_mut() = gpu_timer::GpuTimer::new();
    });
    release_native_window();
    INITIALIZED.store(false, Ordering::SeqCst);
    QUIT_REQUESTED.store(false, Ordering::SeqCst);

    #[cfg(target_os = "macos")]
    macos_focus::restore_previous_frontmost();
    true
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_is_initialized() -> bool {
    INITIALIZED.load(Ordering::SeqCst)
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
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_create_window_ex(
    title: *const c_char,
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
    display: c_int,
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

    let requested = window_placement::ResolvedGeometry {
        size: (width, height),
        pos: (x, y),
        display: (display >= 0).then_some(display),
    };
    let Some(window) = create_window(title_str, requested, resizable, fullscreen) else {
        return ptr::null_mut();
    };
    handles::bump_context_generation();
    handles::set_context_live(true);
    canvas::reset_current_canvas_handle();
    WINDOW.with(|slot| slot.borrow_mut().insert(Box::new(window)).as_mut() as *mut StoneWindow)
}

fn create_window(
    title: &str,
    requested: window_placement::ResolvedGeometry,
    resizable: bool,
    fullscreen: c_int,
) -> Option<StoneWindow> {
    // SDL focus suppression differs by platform; X11 also needs user-time zero.
    let prevent_focus_steal = std::env::var("STONE_FOCUS_ON_SHOW").is_err();
    if prevent_focus_steal {
        sdl2::hint::set("SDL_MAC_BACKGROUND_APP", "1");
        sdl2::hint::set("SDL_WINDOW_NO_ACTIVATION_WHEN_SHOWN", "1");

        // Capture before SDL can change the frontmost app.
        #[cfg(target_os = "macos")]
        macos_focus::capture_previous_frontmost();
    }

    sdl2::hint::set("SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS", "1");

    // SDL leaves exclusive fullscreen to minimize on focus loss. A tiling WM
    // such as i3 ignores that minimize and never restores fullscreen.
    sdl2::hint::set("SDL_VIDEO_MINIMIZE_ON_FOCUS_LOSS", "0");

    #[cfg(target_os = "linux")]
    configure_sdl_graphics_drivers();

    // SDL reads WM_CLASS from the environment at video init for X11 WM rules.
    if std::env::var_os("SDL_VIDEO_X11_WMCLASS").is_none() {
        std::env::set_var("SDL_VIDEO_X11_WMCLASS", "stone");
    }

    let t_boot = std::time::Instant::now();
    let sdl_context = sdl2::init()
        .inspect_err(|e| ::log::error!(target: "STONE", "failed to init sdl: {e}"))
        .ok()?;

    let t_sdl_init = t_boot.elapsed();
    let video = sdl_context
        .video()
        .inspect_err(|e| ::log::error!(target: "STONE", "failed to init video: {e}"))
        .ok()?;

    #[cfg(target_os = "linux")]
    {
        let requested = std::env::var("SDL_VIDEODRIVER").ok();
        if let Some(message) = native_video_driver_error(
            std::env::var_os("STONE_HEADLESS").is_some(),
            requested.as_deref(),
            video.current_video_driver(),
        ) {
            ::log::error!(target: "STONE", "{message}");
            return None;
        }
    }

    let t_video = t_boot.elapsed();
    #[cfg(target_os = "macos")]
    if prevent_focus_steal {
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

    let headless = std::env::var("STONE_HEADLESS").is_ok();
    use sdl2::video::FullscreenType;
    let boot_mode = if headless {
        FullscreenType::Off
    } else {
        fullscreen_type(fullscreen)
    };
    let born_fullscreen = boot_mode != FullscreenType::Off;
    let fit_enabled = !born_fullscreen && window_fit_enabled(headless);

    // A tiling WM can ignore create-time position; reassert it after mapping.
    let defer_windowed_show = cfg!(target_os = "linux")
        && !headless
        && !born_fullscreen
        && std::env::var_os("STONE_IMMEDIATE_SHOW").is_none();

    let machine = if headless {
        window_placement::MachineConfig::default()
    } else {
        window_placement::machine_config(&window_placement::game_slug(title))
    };
    let geometry = boot_geometry(
        &video,
        title,
        requested,
        &machine,
        headless,
        born_fullscreen,
    );
    let fitted = if fit_enabled {
        fit_before_create(&video, geometry)
    } else {
        geometry
    };
    let (build_x, build_y) = fitted.pos;
    let mut window_builder = video.window(title, fitted.size.0 as u32, fitted.size.1 as u32);
    window_builder.opengl();

    if resizable {
        window_builder.resizable();
    }

    // On X11, stamp user time before the initial map.
    let defer_show_for_focus = cfg!(target_os = "linux") && prevent_focus_steal && !headless;
    if headless || born_fullscreen || defer_windowed_show || defer_show_for_focus {
        window_builder.hidden();
    }
    // Headless snapshots use logical pixels so display scale cannot change them.
    if !headless {
        window_builder.allow_highdpi();
    }

    if geometry.pos == (SDL_WINDOWPOS_CENTERED, SDL_WINDOWPOS_CENTERED) {
        window_builder.position_centered();
    } else {
        window_builder.position(build_x, build_y);
    }

    let t_pre_window = t_boot.elapsed();
    let mut window = window_builder
        .build()
        .inspect_err(|e| ::log::error!(target: "STONE", "failed to create window: {e}"))
        .ok()?;

    #[cfg(target_os = "linux")]
    if x11_wm::install_error_handler(&window) {
        ::log::debug!(target: "STONE", "X11 error handler installed: X errors are logged and never fatal");
    }

    // SDL fullscreen selects the current display, so map at the target first.
    if born_fullscreen || (defer_show_for_focus && !defer_windowed_show) {
        #[cfg(target_os = "linux")]
        if defer_show_for_focus {
            x11_focus::set_user_time_zero(&window);
        }
        window.show();
    }
    if born_fullscreen {
        move_window(&mut window, build_x, build_y);
        if let Err(e) = window.set_fullscreen(boot_mode) {
            ::log::error!(target: "STONE", "boot fullscreen (mode={fullscreen}) failed: {e}");
        }
    }

    let t_window = t_boot.elapsed();
    let gl_context = window
        .gl_create_context()
        .inspect_err(|e| ::log::error!(target: "STONE", "failed to create GL context {:?} {}.{}: {e}", gl_request.profile, gl_request.major, gl_request.minor))
        .ok()?;

    let t_gl_context = t_boot.elapsed();

    let gl = unsafe {
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
    log_boot_timing(
        t_boot,
        [t_sdl_init, t_video, t_pre_window, t_window, t_gl_context],
    );

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
            return None;
        }
        ::log::info!(target: "STONE", "headless window draws into an owned {dw}x{dh} screen framebuffer");
    }

    if let Err(e) = video.gl_set_swap_interval(sdl2::video::SwapInterval::Immediate) {
        log_once!(
            "VIDEO",
            "gl_set_swap_interval(Immediate) at init failed: {}",
            e
        );
    }

    if fit_enabled {
        fit_after_create(&video, &mut window, geometry.display);
    }

    let (bounds, drawable) = report_created_window(&video, &window, requested, headless);
    unsafe { debug_gl.viewport(0, 0, drawable.0, drawable.1) };

    let event_pump = sdl_context
        .event_pump()
        .inspect_err(|e| ::log::error!(target: "STONE", "failed to create event pump: {e}"))
        .ok()?;
    let game_controller = sdl_context.game_controller()
        .inspect_err(|e| ::log::error!(target: "STONE", "failed to initialize SDL game-controller subsystem: {e}"))
        .ok()?;
    let controller_manager = input::SdlControllerManager::new(game_controller);

    let sdl_window_raw = window.raw() as *mut c_void;
    let gl_context_raw = unsafe { gl_context.raw() };
    let pending_show = defer_windowed_show.then(|| PendingShow {
        pos: window.position(),
        map_requested: false,
        activate: false,
    });
    let initial_mode = if born_fullscreen {
        window_placement::WindowMode::Fullscreen(fullscreen)
    } else {
        window_placement::WindowMode::Windowed
    };

    Some(StoneWindow {
        _sdl_context: sdl_context,
        _video: video,
        window,
        _gl_context: gl_context,
        gl: Rc::new(debug_gl),
        pending_show,
        runtime_hidden: headless,
        title: title.to_string(),
        window_mode: Cell::new(initial_mode),
        #[cfg(target_os = "linux")]
        wm_fullscreen_confirmed: Cell::new(false),
        windowed_bounds: Cell::new(bounds),
        saved_state_on_hide: RefCell::new(None),
        initial_hidden_boot: Cell::new(headless),
        requested_size: requested.size,
        requested_pos: requested.pos,
        requested_display: requested.display,
        config_imgui_scale: machine.imgui_scale,
        event_pump: RefCell::new(event_pump),
        controller_manager: RefCell::new(controller_manager),
        sdl_window_raw,
        gl_context_raw,
    })
}

fn boot_geometry(
    video: &sdl2::VideoSubsystem,
    title: &str,
    mut base: window_placement::ResolvedGeometry,
    machine: &window_placement::MachineConfig,
    headless: bool,
    fullscreen: bool,
) -> window_placement::ResolvedGeometry {
    if headless {
        base.size = headless_window_size(base.size);
    } else if fullscreen {
        let displays = gather_display_reports(video);
        if let Some(display) = displays
            .iter()
            .find(|d| Some(d.index) == base.display)
            .or_else(|| displays.first())
        {
            base.pos = (display.usable.0, display.usable.1);
        }
    } else {
        return resolve_window_geometry(title, base, machine, video);
    }
    base
}

fn fit_window(
    video: &sdl2::VideoSubsystem,
    request: window_report::WindowRequest,
    borders: window_report::Borders,
) -> window_report::WindowPlacement {
    window_report::resolve_window_placement(request, &gather_display_reports(video), borders)
}

fn fit_before_create(
    video: &sdl2::VideoSubsystem,
    geometry: window_placement::ResolvedGeometry,
) -> window_placement::ResolvedGeometry {
    let centered = geometry.pos == (SDL_WINDOWPOS_CENTERED, SDL_WINDOWPOS_CENTERED);
    let fitted = fit_window(
        video,
        window_report::WindowRequest {
            size: geometry.size,
            pos: if centered { (0, 0) } else { geometry.pos },
            display: geometry.display,
        },
        ZERO_BORDERS,
    );
    if fitted.size != geometry.size {
        ::log::info!(
            target: "STONE",
            "window fit (pre-clamp): size {}x{} -> {}x{}",
            geometry.size.0, geometry.size.1, fitted.size.0, fitted.size.1,
        );
    }
    window_placement::ResolvedGeometry {
        size: fitted.size,
        pos: if centered { geometry.pos } else { fitted.pos },
        ..geometry
    }
}

// Once SDL has created the window, fit its actual geometry including frame borders.
fn fit_after_create(
    video: &sdl2::VideoSubsystem,
    window: &mut sdl2::video::Window,
    display: Option<i32>,
) {
    let borders = effective_borders(query_window_borders(window));
    let (cur_w, cur_h) = window.size();
    let (cur_x, cur_y) = window.position();
    let request = window_report::WindowRequest {
        size: (cur_w as i32, cur_h as i32),
        pos: (cur_x, cur_y),
        display,
    };
    let fitted = fit_window(video, request, borders);
    let need_resize = fitted.size != request.size;
    let need_move = fitted.pos != request.pos;
    if need_resize {
        if let Err(e) = window.set_size(fitted.size.0 as u32, fitted.size.1 as u32) {
            log_once!("STONE", "post-create fit set_size failed: {}", e);
        }
    }
    if need_move {
        move_window(window, fitted.pos.0, fitted.pos.1);
    }
    if need_resize || need_move {
        ::log::info!(
            target: "STONE",
            "window fit (post-create): size {}x{} pos ({},{}) -> size {}x{} pos ({},{}) (borders t{} l{} b{} r{})",
            cur_w, cur_h, cur_x, cur_y,
            fitted.size.0, fitted.size.1, fitted.pos.0, fitted.pos.1,
            borders.top, borders.left, borders.bottom, borders.right,
        );
    }
}

fn log_boot_timing(
    start: std::time::Instant,
    [sdl_init, video, pre_window, window, gl_context]: [std::time::Duration; 5],
) {
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    ::log::debug!(target: "STONE",
        "boot sdl_init={:.0}ms video={:.0}ms attrs={:.0}ms window_build={:.0}ms gl_context={:.0}ms glow_load={:.0}ms",
        ms(sdl_init), ms(video - sdl_init), ms(pre_window - video),
        ms(window - pre_window), ms(gl_context - window), ms(start.elapsed() - gl_context));
}

fn report_created_window(
    video: &sdl2::VideoSubsystem,
    window: &sdl2::video::Window,
    requested: window_placement::ResolvedGeometry,
    headless: bool,
) -> (window_placement::WindowedBounds, (i32, i32)) {
    let (drawable_w, drawable_h) = window.drawable_size();
    let bounds = current_bounds(window);
    let (win_w, win_h) = bounds.size;
    let (win_x, win_y) = bounds.pos;
    let display_index = bounds.display.unwrap_or(-1);
    let drawable = (drawable_w as i32, drawable_h as i32);
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

    if !headless {
        let borders = query_window_borders(window).unwrap_or(ZERO_BORDERS);
        INITIAL_BORDERS.with(|c| c.set(Some(borders)));
        POST_FRAME_BORDERS_LOGGED.with(|c| c.set(false));
        let report = window_report::WindowReport {
            displays: gather_display_reports(video),
            requested_size: requested.size,
            requested_pos: requested.pos,
            actual_size: bounds.size,
            actual_pos: bounds.pos,
            landed_display: display_index,
            drawable_size: drawable,
            dpi_scale: scale,
            borders,
        };
        ::log::info!(target: "STONE", "{}", report.render_human());
    }
    (
        window_placement::WindowedBounds {
            display: bounds.display.filter(|&index| index >= 0),
            ..bounds
        },
        drawable,
    )
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_poll_event(event: *mut StoneEvent) -> bool {
    if event.is_null() {
        return false;
    }
    let polled = with_window_mut(poll_window_event).flatten();
    unsafe { *event = polled.unwrap_or_default() };
    polled.is_some()
}

fn is_maximized(window: &sdl2::video::Window) -> bool {
    window.window_flags() & sdl2::sys::SDL_WindowFlags::SDL_WINDOW_MAXIMIZED as u32 != 0
}

/// Stone's fullscreen modes: 0 windowed, 1 desktop, 2 exclusive.
fn fullscreen_type(mode: c_int) -> sdl2::video::FullscreenType {
    match mode {
        1 => sdl2::video::FullscreenType::Desktop,
        2 => sdl2::video::FullscreenType::True,
        _ => sdl2::video::FullscreenType::Off,
    }
}

fn fullscreen_mode(fullscreen: sdl2::video::FullscreenType) -> c_int {
    match fullscreen {
        sdl2::video::FullscreenType::Off => 0,
        sdl2::video::FullscreenType::Desktop => 1,
        sdl2::video::FullscreenType::True => 2,
    }
}

fn current_bounds(window: &sdl2::video::Window) -> window_placement::WindowedBounds {
    let (w, h) = window.size();
    window_placement::WindowedBounds {
        pos: window.position(),
        size: (w as i32, h as i32),
        display: window.display_index().ok(),
    }
}

/// Write an out-parameter the caller may pass as NULL.
fn write_out<T>(out: *mut T, value: T) {
    if !out.is_null() {
        unsafe { *out = value };
    }
}

/// Alt+Enter is the engine's fullscreen toggle and never reaches the game:
/// Some(true) for the press, Some(false) for its repeats and release.
fn alt_enter(event: &Event) -> Option<bool> {
    use sdl2::keyboard::Keycode;
    match *event {
        Event::KeyDown {
            keycode: Some(Keycode::Return | Keycode::KpEnter),
            keymod,
            repeat,
            ..
        } if keymod.intersects(Mod::LALTMOD | Mod::RALTMOD) => Some(!repeat),
        Event::KeyUp {
            keycode: Some(Keycode::Return | Keycode::KpEnter),
            keymod,
            ..
        } if keymod.intersects(Mod::LALTMOD | Mod::RALTMOD) => Some(false),
        _ => None,
    }
}

/// The next event for the game, or a blank event for one the engine consumed;
/// None once the queue is empty.
fn poll_window_event(window: &mut StoneWindow) -> Option<StoneEvent> {
    if let Some(translated) = window
        .controller_manager
        .borrow_mut()
        .manager
        .pop_stone_event()
    {
        return Some(translated);
    }
    // Raw SDL_PollEvent, so ImGui's SDL2 backend sees each event (its NewFrame
    // only polls the mouse position, so clicks must arrive as events).
    unsafe { sdl2::sys::SDL_JoystickUpdate() };
    let mut raw = std::mem::MaybeUninit::<sdl2::sys::SDL_Event>::uninit();
    if unsafe { sdl2::sys::SDL_PollEvent(raw.as_mut_ptr()) } == 0 {
        return None;
    }
    let raw = unsafe { raw.assume_init() };
    let sdl_event = Event::from_ll(raw);

    #[cfg(target_os = "linux")]
    if let sdl2::event::Event::Window { window_id, .. } = sdl_event {
        if window_id == window.window.id() {
            follow_wm_fullscreen_exit(window);
        }
    }

    if let Some(toggle) = alt_enter(&sdl_event) {
        if toggle {
            let windowed = window.window.fullscreen_state() == sdl2::video::FullscreenType::Off
                && !is_maximized(&window.window);
            set_fullscreen_impl(window, windowed as c_int);
        }
        return Some(StoneEvent::default());
    }

    let _ = crate::imgui::process_event(&raw as *const _ as *mut std::ffi::c_void);
    let controller_event = {
        let mut manager = window.controller_manager.borrow_mut();
        manager.handle_sdl_event(&sdl_event);
        manager.manager.pop_stone_event()
    };
    if controller_event.is_some() {
        return controller_event;
    }
    if let Some(translated) = stone_event_from_sdl(&sdl_event) {
        return Some(translated);
    }
    if let Event::Window { win_event, .. } = sdl_event {
        track_window_event(window, win_event);
    }
    Some(StoneEvent::default())
}

fn track_window_event(window: &mut StoneWindow, win_event: sdl2::event::WindowEvent) {
    use sdl2::event::WindowEvent as WE;
    match win_event {
        #[cfg(target_os = "macos")]
        WE::FocusGained => macos_focus::promote_to_regular_once(),
        WE::Resized(..) | WE::SizeChanged(..) | WE::Moved(..) => {
            let moved = matches!(win_event, WE::Moved(..));
            mark_window_change(if moved { WC_MOVED } else { WC_RESIZED });
            if window.window_mode.get() == window_placement::WindowMode::Windowed
                && window.window.fullscreen_state() == sdl2::video::FullscreenType::Off
                && !is_maximized(&window.window)
            {
                let mut bounds = window.windowed_bounds.get();
                if moved {
                    bounds.pos = window.window.position();
                    bounds.display = window.window.display_index().ok();
                } else {
                    let (w, h) = window.window.size();
                    bounds.size = (w as i32, h as i32);
                }
                window.windowed_bounds.set(bounds);
            }
        }
        WE::DisplayChanged(..) => mark_window_change(WC_DISPLAY),
        _ => {}
    }
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_start_text_input() {
    unsafe { sdl2::sys::SDL_StartTextInput() };
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_stop_text_input() {
    unsafe { sdl2::sys::SDL_StopTextInput() };
}

/// rock-ffi stone rename:_window=window
#[no_mangle]
pub extern "C" fn stone_swap_window(_window: *mut StoneWindow) {
    GL_SWAP_COUNT.fetch_add(1, Ordering::SeqCst);
    with_window(|window| {
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
    });
}

#[no_mangle]
pub extern "C" fn stone_gl_swap_count() -> u64 {
    GL_SWAP_COUNT.load(Ordering::SeqCst)
}

/// Advance a deferred map after each presented frame. X11 map requests remain
/// pending while another window covers the workspace fullscreen; once viewable,
/// synchronize SDL state and reassert the saved position.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_show_window_if_deferred() {
    with_window_mut(|window| {
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
    });
}

#[no_mangle]
pub extern "C" fn stone_hide_window() {
    with_window_mut(|window| {
        if window.pending_show.is_some() {
            window.pending_show = None;
        }
        if !window.runtime_hidden {
            let current_mode = match fullscreen_mode(window.window.fullscreen_state()) {
                0 if is_maximized(&window.window) => window_placement::WindowMode::Maximized,
                0 => window_placement::WindowMode::Windowed,
                mode => window_placement::WindowMode::Fullscreen(mode),
            };
            window.window_mode.set(current_mode);
            let current = current_bounds(&window.window);
            if current_mode == window_placement::WindowMode::Windowed {
                window.windowed_bounds.set(current);
            }
            *window.saved_state_on_hide.borrow_mut() = Some(window_placement::HiddenWindowState {
                mode: current_mode,
                pos: current.pos,
                size: current.size,
                display: current.display,
                windowed_bounds: window.windowed_bounds.get(),
            });
        }
        window.window.hide();
        window.runtime_hidden = true;
        ::log::info!(target: "STONE", "window hidden by request");
    });
}

/// Show the window without changing scene state or forcing input focus.
#[no_mangle]
pub extern "C" fn stone_show_window() {
    show_window_impl(None);
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_show_window_mode(mode: c_int) {
    if matches!(mode, 0 | 1) {
        show_window_impl(Some(mode));
    }
}

fn show_window_impl(requested_mode: Option<c_int>) {
    // Fullscreen transitions consult SDL's cached mouse focus, including queued leave events.
    unsafe { sdl2::sys::SDL_PumpEvents() };
    with_window_mut(|window| {
        if !window.runtime_hidden {
            if let Some(mode) = requested_mode {
                set_fullscreen_impl(window, mode);
            }
            return;
        }
        let displays = gather_display_reports(&window._video);
        let borders = effective_borders(query_window_borders(&window.window));

        let (mut mode, mut target_pos, mut target_size, mut target_display) = if window
            .initial_hidden_boot
            .get()
        {
            window.initial_hidden_boot.set(false);
            let base = window_placement::ResolvedGeometry {
                size: window.requested_size,
                pos: window.requested_pos,
                display: window.requested_display,
            };
            let machine =
                window_placement::machine_config(&window_placement::game_slug(&window.title));
            let resolved = resolve_window_geometry(&window.title, base, &machine, &window._video);
            let req = window_report::WindowRequest {
                size: resolved.size,
                pos: resolved.pos,
                display: resolved.display,
            };
            let mut p = window_report::resolve_window_placement(req, &displays, borders);
            if resolved.pos.0 == SDL_WINDOWPOS_CENTERED && resolved.pos.1 == SDL_WINDOWPOS_CENTERED
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
                if let Err(e) = request_sdl_fullscreen(window, fullscreen) {
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
                let _ = request_sdl_fullscreen(window, sdl2::video::FullscreenType::Off);
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
    });
    stone_show_window_if_deferred();
}

/// Live runtime visibility of the native window. A headless-booted game
/// reports hidden; hide/show at runtime flips it.
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_is_window_hidden() -> bool {
    with_window(|window| window.runtime_hidden).unwrap_or(true)
}

/// Explicitly bring an interactive window to the foreground. Normal Stone
/// startup avoids stealing focus; launchers and demos opt into activation.
/// rock-ffi stone rename:_window=window
#[no_mangle]
pub extern "C" fn stone_activate_window(_window: *mut StoneWindow) {
    if stone_is_window_hidden() {
        return;
    }

    with_window_mut(|window| {
        if let Some(pending) = window.pending_show.as_mut() {
            pending.activate = true;
        } else {
            activate_window_impl(window);
        }
    });
}

fn activate_window_impl(window: &mut StoneWindow) {
    #[cfg(target_os = "macos")]
    macos_focus::promote_to_regular_once();
    window.window.raise();
    ::log::info!(target: "STONE", "window activated by request");
}

/// GL errors since process start, or 0 in release builds.
/// u32 avoids boxed uint64_t cdata in LuaJIT.
/// After swap, pull the driver's vblank wait out of CPU timing spans.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_gl_finish() {
    with_gl(|gl| unsafe { gl.finish() });
}

/// Flush submits without waiting; finish also waits for GPU or display work.
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_gl_flush() {
    with_gl(|gl| unsafe { gl.flush() });
}

/// GL errors since process start: buffer allocations in every build, other calls in debug.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_gl_error_count() -> u32 {
    stone_gl::GL_ERROR_COUNT.load(std::sync::atomic::Ordering::Relaxed) as u32
}

/// Shader compile and link failures since process start, including release builds.
/// Shader compile/link failures. Counted in release too -- a failed compile sets
/// no GL error, so this is the only machine-readable signal.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_shader_error_count() -> u32 {
    stone_gl::SHADER_ERROR_COUNT.load(std::sync::atomic::Ordering::Relaxed) as u32
}

/// Begin the GPU timer query for this frame's GL work. No-op if no window /
/// GL context, or if timer queries are unsupported on this platform.
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_gpu_timer_begin() {
    with_gl(|gl| GPU_TIMER.with(|t| t.borrow_mut().begin(gl)));
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_gpu_timer_end() {
    with_gl(|gl| GPU_TIMER.with(|t| t.borrow_mut().end(gl)));
}

/// Non-blocking read of the oldest ready GPU timer result. Returns elapsed GPU
/// ms (>= 0), `-1` if no result is ready yet, or `-2` if timer queries are
/// unsupported (macOS / no GL context).
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_gpu_timer_poll_ms() -> f32 {
    with_gl(|gl| GPU_TIMER.with(|t| t.borrow_mut().poll_ms(gl))).unwrap_or(-2.0)
}

/// Open a timed pass: one GPU timestamp pair around the GL work up to
/// `stone_gpu_timed_pass_end`, available in normal play. No-op when timer
/// queries are unsupported, another timed pass is open, or too many results
/// are unread.
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_gpu_timed_pass_begin() {
    with_gl(|gl| GPU_TIMER.with(|t| t.borrow_mut().timed_begin(gl)));
}

/// Close the open timed pass and queue it for readback under `tag`.
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_gpu_timed_pass_end(tag: u32) {
    with_gl(|gl| GPU_TIMER.with(|t| t.borrow_mut().timed_end(gl, tag)));
}

/// Non-blocking read of the oldest timed pass, in submission order. Returns
/// its GPU ms (>= 0), `-1` if none is ready, or `-2` if passes cannot be
/// timed (macOS / no GL context / `STONE_GPU_SCOPES=finish`).
/// GPU ms of the oldest finished timed pass; -1 when none is ready, -2 when this
/// context cannot time passes. Results arrive a few frames after the pass ran.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_gpu_timed_pass_poll_ms() -> f32 {
    with_gl(|gl| GPU_TIMER.with(|t| t.borrow_mut().timed_poll_ms(gl))).unwrap_or(-2.0)
}

/// The tag of the pass `stone_gpu_timed_pass_poll_ms` last returned.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_gpu_timed_pass_tag() -> u32 {
    GPU_TIMER.with(|t| t.borrow().timed_tag())
}

/// Whether this process records named GPU scopes (`STONE_GPU_SCOPES=1`). The
/// Rock wrappers read it once so a disabled scope costs no FFI call.
/// rock-ffi stone private byte-bool
#[no_mangle]
pub extern "C" fn stone_gpu_scopes_requested() -> u8 {
    GPU_TIMER.with(|t| t.borrow().status() != gpu_timer::ScopeStatus::Off) as u8
}

/// Whether the scopes are `glFinish`-bracketed wall time
/// (`STONE_GPU_SCOPES=finish`) rather than GPU timestamps.
/// rock-ffi stone byte-bool
#[no_mangle]
pub extern "C" fn stone_gpu_scopes_finish() -> u8 {
    GPU_TIMER.with(|t| t.borrow().status() == gpu_timer::ScopeStatus::Finish) as u8
}

/// Open a named GPU scope (timestamp pair + KHR_debug group) inside the
/// current frame. No-op unless scopes are on and a frame is recording.
///
/// # Safety
/// `name` must be a valid NUL-terminated string.
/// rock-ffi stone private
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
/// rock-ffi stone private
#[no_mangle]
pub unsafe extern "C" fn stone_gpu_pass_scope_begin(shader_name: *const c_char) {
    if shader_name.is_null() {
        ::log::error!(target: "STONE", "stone_gpu_pass_scope_begin: null shader name");
        return;
    }
    let shader = std::ffi::CStr::from_ptr(shader_name).to_string_lossy();
    with_gl(|gl| GPU_TIMER.with(|t| t.borrow_mut().scope_begin(gl, &format!("pass:{shader}"))));
}

/// rock-ffi stone private
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
/// rock-ffi stone private
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

/// Clear aggregated scope samples (the profiler's warmup transition).
/// rock-ffi stone
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
    // game's per-frame settings-apply (e.g. a settings push that pokes the raw
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
    with_window(|window| {
        let interval = if enabled != 0 {
            sdl2::video::SwapInterval::VSync
        } else {
            sdl2::video::SwapInterval::Immediate
        };
        if let Err(e) = window._video.gl_set_swap_interval(interval) {
            log_once!(
                "VIDEO",
                "gl_set_swap_interval (enabled={}) failed: {}",
                enabled,
                e
            );
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
    });
}

#[cfg(not(target_os = "macos"))]
fn swap_interval_on(interval: sdl2::video::SwapInterval) -> bool {
    !matches!(interval, sdl2::video::SwapInterval::Immediate)
}

/// The vsync state the DRIVER reports, not the one last requested. A
/// driver can refuse or clamp the swap interval, so a caller that assumes a
/// `stone_set_vsync(0)` took will mis-attribute the resulting present stall.
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_vsync_active() -> bool {
    #[cfg(target_os = "macos")]
    return DISPLAY_PACER.with(|p| p.borrow().as_ref().is_some_and(|pacer| pacer.is_running()));
    #[cfg(not(target_os = "macos"))]
    return with_window(|window| swap_interval_on(window._video.gl_get_swap_interval()))
        .unwrap_or(false);
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_set_vsync(enabled: c_int) {
    let enabled = if VSYNC_FORCED_OFF.with(|f| f.get()) {
        0
    } else {
        enabled
    };
    apply_swap_interval(enabled);
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_force_vsync_off(force: c_int) {
    VSYNC_FORCED_OFF.with(|f| f.set(force != 0));
    if force != 0 {
        apply_swap_interval(0);
    }
}

/// Runtime fullscreen requests go through here. A new request is unconfirmed
/// until the WM lists it (see `follow_wm_fullscreen_exit`).
fn request_sdl_fullscreen(
    window: &mut StoneWindow,
    mode: sdl2::video::FullscreenType,
) -> Result<(), String> {
    window.window.set_fullscreen(mode)?;
    #[cfg(target_os = "linux")]
    window.wm_fullscreen_confirmed.set(false);
    Ok(())
}

/// Follow a WM's fullscreen exit after it has confirmed an SDL fullscreen
/// request. SDL2 on X11 ignores a `_NET_WM_STATE` change that drops fullscreen,
/// so SDL can keep reporting the display origin and old geometry. On exit,
/// keep the WM's current size and position as the windowed bounds.
#[cfg(target_os = "linux")]
fn follow_wm_fullscreen_exit(window: &mut StoneWindow) {
    if window.runtime_hidden || window.window.fullscreen_state() == sdl2::video::FullscreenType::Off
    {
        return;
    }
    match x11_wm::net_wm_fullscreen(&window.window) {
        None => return,
        Some(true) => {
            window.wm_fullscreen_confirmed.set(true);
            return;
        }
        Some(false) if !window.wm_fullscreen_confirmed.get() => return,
        Some(false) => {}
    }
    let Some(((x, y), (w, h))) = x11_wm::root_geometry(&window.window) else {
        crate::log_once!("STONE", "the WM took the window out of fullscreen but its X geometry is unreadable; SDL keeps its fullscreen state");
        return;
    };
    // SDL records a position or size set while fullscreen as the windowed
    // geometry it restores on leaving fullscreen.
    move_window(&mut window.window, x, y);
    if let Err(e) = window.window.set_size(w, h) {
        ::log::error!(target: "STONE", "keeping the WM's size after it left fullscreen failed: {}", e);
        return;
    }
    if let Err(e) = request_sdl_fullscreen(window, sdl2::video::FullscreenType::Off) {
        ::log::error!(target: "STONE", "clearing fullscreen after the WM left it failed: {}", e);
        return;
    }
    let pos = window.window.position();
    window
        .windowed_bounds
        .set(window_placement::WindowedBounds {
            pos,
            size: (w as i32, h as i32),
            display: window.window.display_index().ok(),
        });
    window
        .window_mode
        .set(window_placement::WindowMode::Windowed);
    mark_window_change(WC_RESIZED);
    mark_window_change(WC_MOVED);
    ::log::info!(target: "STONE", "the WM took the window out of fullscreen: windowed at ({},{}) size {}x{}", pos.0, pos.1, w, h);
}

fn display_change_allowed(window: &StoneWindow, request: &str) -> bool {
    if window.runtime_hidden {
        ::log::debug!(target: "VIDEO", "{request} ignored while the window is hidden");
        return false;
    }
    true
}

fn set_fullscreen_impl(window: &mut StoneWindow, mode: c_int) -> bool {
    let fullscreen = fullscreen_type(mode);
    if window.window.fullscreen_state() == fullscreen {
        return false;
    }
    if fullscreen != sdl2::video::FullscreenType::Off
        && window.window_mode.get() == window_placement::WindowMode::Windowed
    {
        window.windowed_bounds.set(current_bounds(&window.window));
    }
    if let Err(e) = request_sdl_fullscreen(window, fullscreen) {
        log_once!("VIDEO", "set_fullscreen (mode={}) failed: {}", mode, e);
        return false;
    }
    let new_mode = if fullscreen == sdl2::video::FullscreenType::Off {
        let b = window.windowed_bounds.get();
        if let Err(e) = window.window.set_size(b.size.0 as u32, b.size.1 as u32) {
            log_once!("VIDEO", "set_fullscreen restore size failed: {}", e);
        }
        move_window(&mut window.window, b.pos.0, b.pos.1);
        window_placement::WindowMode::Windowed
    } else {
        window_placement::WindowMode::Fullscreen(mode)
    };
    window.window_mode.set(new_mode);
    mark_window_change(WC_RESIZED);
    mark_window_change(WC_MOVED);
    true
}

/// Set fullscreen mode at runtime: 0 = windowed, 1 = desktop (borderless)
/// fullscreen, 2 = exclusive fullscreen. Unknown values are treated as windowed.
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_set_fullscreen(mode: c_int) {
    with_window_mut(|window| {
        if !display_change_allowed(window, "set_fullscreen") {
            return;
        }
        set_fullscreen_impl(window, mode);
    });
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_delay(ms: u32) {
    std::thread::sleep(std::time::Duration::from_millis(ms as u64));
}

/// Wall time for a perf deadline passed by the launcher before process spawn.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_wall_time() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock predates Unix epoch")
        .as_secs_f64()
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_get_time() -> f64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    start.elapsed().as_secs_f64()
}

/// rock-ffi stone private rename:width=w rename:height=h
#[no_mangle]
pub extern "C" fn stone_get_window_size(width: *mut c_int, height: *mut c_int) {
    with_window(|window| {
        let (w, h) = window.window.size();
        write_out(width, w as c_int);
        write_out(height, h as c_int);
    });
}

/// rock-ffi stone private rename:width=w rename:height=h
#[no_mangle]
pub extern "C" fn stone_get_drawable_size(width: *mut c_int, height: *mut c_int) {
    with_window(|window| {
        let (w, h) = window.window.drawable_size();
        write_out(width, w as c_int);
        write_out(height, h as c_int);
    });
}

/// The machine config's `imgui_scale` for this window's game, or 0 when the
/// config names none (or there is no window, or it is headless).
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_get_config_imgui_scale() -> c_float {
    with_window(|window| window.config_imgui_scale)
        .flatten()
        .unwrap_or(0.0)
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_get_dpi_scale() -> c_float {
    with_window(|window| {
        let (w, _) = window.window.size();
        let (dw, _) = window.window.drawable_size();
        if w > 0 {
            dw as c_float / w as c_float
        } else {
            1.0
        }
    })
    .unwrap_or(1.0)
}

/// rock-ffi stone rename:_window=window
#[no_mangle]
pub extern "C" fn stone_set_window_title(_window: *mut StoneWindow, title: *const c_char) {
    if title.is_null() {
        return;
    }
    let title_str = unsafe { CStr::from_ptr(title).to_str().unwrap_or("") };
    with_window_mut(|window| {
        let _ = window.window.set_title(title_str);
    });
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_get_window_position(x: *mut c_int, y: *mut c_int) {
    with_window(|window| {
        let (px, py) = window.window.position();
        write_out(x, px);
        write_out(y, py);
    });
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_get_fullscreen() -> c_int {
    with_window(|window| fullscreen_mode(window.window.fullscreen_state())).unwrap_or(0)
}

fn can_persist_window_geometry(
    runtime_hidden: bool,
    pending_show: bool,
    fullscreen: sdl2::video::FullscreenType,
) -> bool {
    !runtime_hidden && !pending_show && fullscreen == sdl2::video::FullscreenType::Off
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_save_window_geometry(title: *const c_char) {
    if std::env::var_os("STONE_NO_WINDOW_FIT").is_some() {
        return;
    }
    let title_str = if title.is_null() {
        "Stone"
    } else {
        unsafe { CStr::from_ptr(title).to_str().unwrap_or("Stone") }
    };
    with_window(|window| {
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
        let geometry = window_placement::ResolvedGeometry {
            size: (ww as i32, wh as i32),
            pos: (wx, wy),
            display,
        };
        if window_placement::save_geometry(&slug, &topo, geometry, window.requested_size) {
            ::log::info!(
                target: "STONE",
                "saved window geometry (game '{}' topo {}): pos ({},{}) size {}x{} display {:?}",
                slug, topo, wx, wy, ww, wh, display,
            );
        }
    });
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_take_window_resized() -> bool {
    take_window_change(WC_RESIZED)
}

/// Take-and-clear the "window was moved" flag (drives the debounced geometry
/// write in the runner).
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_take_window_moved() -> bool {
    take_window_change(WC_MOVED)
}

/// Take-and-clear the "window changed display" flag.
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_take_window_display_changed() -> bool {
    take_window_change(WC_DISPLAY)
}

/// Resize the window, clamped to the target display's usable area.
/// Fires a SIZE_CHANGED event, so the resize flag is set as a side effect.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_set_window_size(width: c_int, height: c_int) {
    with_window_mut(|window| {
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
    });
}

/// A headless window's size: `STONE_WINDOW` when it names one, else the game's.
fn headless_window_size(game: (i32, i32)) -> (i32, i32) {
    window_placement::env_geometry().size.unwrap_or(game)
}

/// Move the window to display `index`, centered on that display's usable area
/// and clamped on-screen. No-op for an unknown index (logged).
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_set_window_display(index: c_int) {
    with_window_mut(|window| {
        if !display_change_allowed(window, "set_window_display") {
            return;
        }
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
            let _ = request_sdl_fullscreen(window, sdl2::video::FullscreenType::Off);
        }
        move_window(&mut window.window, p.pos.0, p.pos.1);
        if fs != sdl2::video::FullscreenType::Off {
            if let Err(e) = request_sdl_fullscreen(window, fs) {
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
    });
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_set_display_mode(width: c_int, height: c_int) {
    with_window_mut(|window| {
        if !display_change_allowed(window, "set_display_mode") {
            return;
        }
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
    });
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_get_display_count() -> c_int {
    with_window(|window| {
        window
            ._video
            .num_video_displays()
            .ok()
            .map(|count| count as c_int)
    })
    .flatten()
    .unwrap_or(1)
}

/// Refresh rate in Hz, or 0 when unknown.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_get_window_refresh_rate() -> c_int {
    with_window(|window| {
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
    .unwrap_or_else(|| {
        log_once!(
            "VIDEO",
            "stone_get_window_refresh_rate: no window; refresh rate unknown"
        );
        0
    })
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_get_window_borders(
    top: *mut c_int,
    left: *mut c_int,
    bottom: *mut c_int,
    right: *mut c_int,
) {
    let b = with_window(|window| query_window_borders(&window.window))
        .flatten()
        .unwrap_or(ZERO_BORDERS);
    write_out(top, b.top);
    write_out(left, b.left);
    write_out(bottom, b.bottom);
    write_out(right, b.right);
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
        window_placement::geometry_file_path().map_or_else(|| "<unresolved user state directory>".to_string(), |p| p.display().to_string()),
        window_placement::config_file_path().map_or_else(|| "<unresolved user config directory>".to_string(), |p| p.display().to_string()),
    );

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

/// rock-ffi stone return-out
#[no_mangle]
pub extern "C" fn stone_get_graphics_stats(stats: *mut GraphicsStats) {
    if stats.is_null() {
        return;
    }

    let mut frame = FRAME_STATS.with(|fs| *fs.borrow());
    frame.images = IMAGES.with(|imgs| imgs.borrow().len() as c_int);
    frame.canvases = canvas::get_canvas_count() as c_int;
    frame.texture_memory_mib = texture_bytes_to_mib(live_texture_bytes());
    unsafe { *stats = frame };
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_get_live_texture_bytes() -> u64 {
    live_texture_bytes()
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_get_live_image_bytes() -> u64 {
    live_image_bytes()
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_get_live_mip_bytes() -> u64 {
    live_mip_bytes()
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_reset_frame_stats() {
    FRAME_STATS.with(|fs| *fs.borrow_mut() = GraphicsStats::default());
    image_texture::reset_image_frame_stats();
    crate::uniform_block::report_unfilled_blocks();
    native_renderer::begin_frame();
    with_gl(|gl| {
        stream_buffer::begin_frame(gl);
        uniform_block::rebind_current_program(gl);
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn gl_calls_off_the_render_thread_are_counted_while_a_context_is_live() {
        let before = off_thread_gl_calls();
        assert!(with_gl(|_| ()).is_none());
        assert_eq!(off_thread_gl_calls(), before);
        handles::set_context_live(true);
        let (active, target, size, after_queries, after_render) = std::thread::spawn(|| {
            let active = native_renderer::stone_native_frame_active();
            let target = native_renderer::stone_native_frame_target();
            let size = native_renderer::stone_native_frame_size(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            let after_queries = off_thread_gl_calls();
            assert!(with_gl(|_| ()).is_none());
            (active, target, size, after_queries, off_thread_gl_calls())
        })
        .join()
        .unwrap();
        handles::set_context_live(false);
        assert!(!active && target == 0 && !size);
        assert_eq!(after_queries, before);
        assert_eq!(after_render, before + 1);
    }

    use super::*;

    #[test]
    fn lua_allocator_counts_growth_shrink_and_free_separately() {
        unsafe extern "C" fn allocator(
            ud: *mut c_void,
            ptr: *mut c_void,
            _osize: usize,
            nsize: usize,
        ) -> *mut c_void {
            let calls = &*(ud as *const Cell<usize>);
            calls.set(calls.get() + 1);
            if nsize == 0 {
                ptr::null_mut()
            } else {
                ptr
            }
        }

        let delegated_calls = Cell::new(0_usize);
        let state = LuaAllocatorState {
            state: ptr::null_mut(),
            allocf: allocator,
            ud: (&delegated_calls as *const Cell<usize>).cast_mut().cast(),
            category: None,
        };
        let ud = (&state as *const LuaAllocatorState).cast_mut().cast();
        let allocation = ptr::dangling_mut::<u8>().cast();
        LUA_COUNTER_ACTIVE.store(true, Ordering::Relaxed);
        unsafe {
            stone_lua_counting_alloc(ud, ptr::null_mut(), 5, 4096);
            assert_eq!(
                stone_lua_counting_alloc(ud, allocation, 4096, 8192),
                allocation
            );
            stone_lua_counting_alloc(ud, allocation, 8192, 2048);
            assert!(stone_lua_counting_alloc(ud, allocation, 2048, 0).is_null());
            stone_lua_counting_alloc(ud, ptr::null_mut(), 0, 0);
        }
        LUA_COUNTER_ACTIVE.store(false, Ordering::Relaxed);
        unsafe { stone_lua_counting_alloc(ud, allocation, 1024, 4096) };

        assert_eq!(delegated_calls.get(), 6);
        assert_eq!(stone_lua_alloc_gross_request_kb(), 14.0);
        assert_eq!(stone_lua_alloc_positive_growth_kb(), 8.0);
        assert_eq!(stone_lua_alloc_explicit_free_kb(), 2.0);
        assert_eq!(stone_lua_alloc_reallocation_shrink_kb(), 6.0);
        assert_eq!(stone_lua_alloc_call_count(), 5);
        assert_eq!(stone_lua_alloc_allocation_call_count(), 1);
        assert_eq!(stone_lua_alloc_reallocation_call_count(), 2);
        assert_eq!(stone_lua_alloc_free_call_count(), 1);
    }

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
    fn alt_enter_toggles_once_and_swallows_repeats_and_release() {
        use sdl2::keyboard::{Keycode, Mod};
        let key = |down: bool, keycode, keymod, repeat| {
            let (timestamp, window_id, scancode) = (0, 1, None);
            let event = if down {
                Event::KeyDown {
                    timestamp,
                    window_id,
                    keycode: Some(keycode),
                    scancode,
                    keymod,
                    repeat,
                }
            } else {
                Event::KeyUp {
                    timestamp,
                    window_id,
                    keycode: Some(keycode),
                    scancode,
                    keymod,
                    repeat,
                }
            };
            alt_enter(&event)
        };
        assert_eq!(key(true, Keycode::Return, Mod::LALTMOD, false), Some(true));
        assert_eq!(key(true, Keycode::KpEnter, Mod::RALTMOD, false), Some(true));
        assert_eq!(key(true, Keycode::Return, Mod::LALTMOD, true), Some(false));
        assert_eq!(
            key(false, Keycode::Return, Mod::RALTMOD, false),
            Some(false)
        );
        assert_eq!(key(true, Keycode::Return, Mod::LCTRLMOD, false), None);
        assert_eq!(key(false, Keycode::Return, Mod::NOMOD, false), None);
        assert_eq!(key(true, Keycode::A, Mod::LALTMOD, false), None);
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
