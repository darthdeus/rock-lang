//! The `stone_imgui_*` ABI.

use std::ffi::{c_char, c_float, c_int, c_void};

use crate::StoneWindow;
use {
    crate::{cursor::reapply_cursor_state, with_window},
    std::{
        ptr,
        sync::atomic::{AtomicBool, AtomicI32, Ordering},
    },
    stone_imgui_abi::ImDrawData,
};

#[repr(C)]
pub struct ImGuiContext {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct ImFontAtlas {
    _opaque: [u8; 0],
}

#[repr(C)]
struct ImGuiIoCapturePrefix {
    _config_flags: i32,
    backend_flags: i32,
    _display_size: [f32; 2],
    _display_framebuffer_scale: [f32; 2],
    _delta_time: f32,
    _ini_saving_rate: f32,
    ini_filename: *const c_char,
    _log_filename: *const c_char,
    _user_data: *mut c_void,
    _fonts: *mut ImFontAtlas,
    _font_default: *mut c_void,
    _font_allow_user_scaling: bool,
    _config_nav_swap_gamepad_buttons: bool,
    _config_nav_move_set_mouse_pos: bool,
    _config_nav_capture_keyboard: bool,
    _config_nav_escape_clear_focus_item: bool,
    _config_nav_escape_clear_focus_window: bool,
    _config_nav_cursor_visible_auto: bool,
    _config_nav_cursor_visible_always: bool,
    _config_docking_no_split: bool,
    _config_docking_with_shift: bool,
    _config_docking_always_tab_bar: bool,
    _config_docking_transparent_payload: bool,
    _config_viewports_no_auto_merge: bool,
    _config_viewports_no_task_bar_icon: bool,
    _config_viewports_no_decoration: bool,
    _config_viewports_no_default_parent: bool,
    _config_viewport_platform_focus_sets_imgui_focus: bool,
    _config_dpi_scale_fonts: bool,
    _config_dpi_scale_viewports: bool,
    _mouse_draw_cursor: bool,
    _config_macosx_behaviors: bool,
    _config_input_trickle_event_queue: bool,
    _config_input_text_cursor_blink: bool,
    _config_input_text_enter_keep_active: bool,
    _config_drag_click_to_input_text: bool,
    _config_windows_resize_from_edges: bool,
    _config_windows_move_from_title_bar_only: bool,
    _config_windows_copy_contents_with_ctrl_c: bool,
    _config_scrollbar_scroll_by_page: bool,
    _config_memory_compact_timer: f32,
    _mouse_double_click_time: f32,
    _mouse_double_click_max_dist: f32,
    _mouse_drag_threshold: f32,
    _key_repeat_delay: f32,
    _key_repeat_rate: f32,
    _config_error_recovery: bool,
    _config_error_recovery_enable_assert: bool,
    _config_error_recovery_enable_debug_log: bool,
    _config_error_recovery_enable_tooltip: bool,
    _config_debug_is_debugger_present: bool,
    _config_debug_highlight_id_conflicts: bool,
    _config_debug_highlight_id_conflicts_show_item_picker: bool,
    _config_debug_begin_return_value_once: bool,
    _config_debug_begin_return_value_loop: bool,
    _config_debug_ignore_focus_loss: bool,
    _config_debug_ini_settings: bool,
    _backend_platform_name: *const c_char,
    _backend_renderer_name: *const c_char,
    _backend_platform_user_data: *mut c_void,
    _backend_renderer_user_data: *mut c_void,
    _backend_language_user_data: *mut c_void,
    _want_capture_mouse: bool,
    _want_capture_keyboard: bool,
    want_text_input: bool,
}

#[link(name = "cimgui_complete")]
extern "C" {
    fn igCreateContext(shared_font_atlas: *mut ImFontAtlas) -> *mut ImGuiContext;
    fn igDestroyContext(ctx: *mut ImGuiContext);
    fn igGetIO_Nil() -> *mut c_void;
    fn ImGuiIO_AddMousePosEvent(io: *mut c_void, x: c_float, y: c_float);
    fn igNewFrame();
    fn igRender();
    fn igGetDrawData() -> *mut ImDrawData;

    // SDL2 backend
    fn cImGui_ImplSDL2_InitForOpenGL(window: *mut c_void, gl_context: *mut c_void) -> bool;
    fn cImGui_ImplSDL2_Shutdown();
    fn cImGui_ImplSDL2_NewFrame();
    fn cImGui_ImplSDL2_ProcessEvent(event: *const c_void) -> bool;

    // OpenGL3 backend
    fn cImGui_ImplOpenGL3_Init(glsl_version: *const c_char) -> bool;
    fn cImGui_ImplOpenGL3_Shutdown();
    fn cImGui_ImplOpenGL3_NewFrame();
    fn cImGui_ImplOpenGL3_RenderDrawData(draw_data: *mut ImDrawData);
}

static IMGUI_INITIALIZED: AtomicBool = AtomicBool::new(false);
static IMGUI_PLATFORM_INITIALIZED: AtomicBool = AtomicBool::new(false);
static IMGUI_GL_BACKEND_INITIALIZED: AtomicBool = AtomicBool::new(false);

static LAST_VTX_COUNT: AtomicI32 = AtomicI32::new(-1);

/// rock-ffi stone-rendering rename:_window=window
#[no_mangle]
pub extern "C" fn stone_imgui_init(_window: *mut StoneWindow) -> bool {
    IMGUI_INITIALIZED.store(false, Ordering::SeqCst);
    IMGUI_PLATFORM_INITIALIZED.store(false, Ordering::SeqCst);
    IMGUI_GL_BACKEND_INITIALIZED.store(false, Ordering::SeqCst);
    LAST_VTX_COUNT.store(-1, Ordering::SeqCst);
    let result = with_window(|window: &StoneWindow| {
        unsafe {
            let ctx = igCreateContext(ptr::null_mut());
            if ctx.is_null() {
                ::log::error!(target: "IMGUI", "stone_imgui_init: failed to create ImGui context");
                return false;
            }

            let io = igGetIO_Nil().cast::<ImGuiIoCapturePrefix>();
            if io.is_null() {
                ::log::error!(target: "IMGUI", "stone_imgui_init: cimgui returned no ImGuiIO");
                igDestroyContext(ptr::null_mut());
                return false;
            }
            if std::env::var_os("STONE_HEADLESS").is_some() {
                // Parallel headless runs share a cwd; never load or save its editor layout.
                (*io).ini_filename = ptr::null();
            }

            if !cImGui_ImplSDL2_InitForOpenGL(window.sdl_window_raw, window.gl_context_raw) {
                ::log::error!(target: "IMGUI", "stone_imgui_init: failed to init SDL2 backend");
                igDestroyContext(ptr::null_mut());
                return false;
            }
            IMGUI_PLATFORM_INITIALIZED.store(true, Ordering::SeqCst);

            if !cImGui_ImplOpenGL3_Init(c"#version 330".as_ptr()) {
                ::log::error!(target: "IMGUI", "stone_imgui_init: failed to init OpenGL3 backend");
                cImGui_ImplSDL2_Shutdown();
                IMGUI_PLATFORM_INITIALIZED.store(false, Ordering::SeqCst);
                igDestroyContext(ptr::null_mut());
                return false;
            }
            IMGUI_GL_BACKEND_INITIALIZED.store(true, Ordering::SeqCst);

            IMGUI_INITIALIZED.store(true, Ordering::SeqCst);
            true
        }
    });

    result.unwrap_or_else(|| {
        ::log::error!(target: "IMGUI", "stone_imgui_init: no window available");
        false
    })
}

#[no_mangle]
pub extern "C" fn stone_imgui_shutdown() {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }

    unsafe {
        if IMGUI_GL_BACKEND_INITIALIZED.load(Ordering::SeqCst) {
            cImGui_ImplOpenGL3_Shutdown();
        }
        if IMGUI_PLATFORM_INITIALIZED.load(Ordering::SeqCst) {
            cImGui_ImplSDL2_Shutdown();
        }
        igDestroyContext(ptr::null_mut());
    }

    IMGUI_INITIALIZED.store(false, Ordering::SeqCst);
    IMGUI_PLATFORM_INITIALIZED.store(false, Ordering::SeqCst);
    IMGUI_GL_BACKEND_INITIALIZED.store(false, Ordering::SeqCst);
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_imgui_new_frame() {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }

    unsafe {
        if IMGUI_GL_BACKEND_INITIALIZED.load(Ordering::SeqCst) {
            cImGui_ImplOpenGL3_NewFrame();
        }
        cImGui_ImplSDL2_NewFrame();
        if let Some(window) = with_window(|window| window.sdl_window_raw) {
            feed_window_relative_pointer(window.cast());
        }
        reapply_cursor_state();
        igNewFrame();
    }
}

/// Give ImGui the pointer SDL tracks relative to the window.
///
/// While the app has keyboard focus and no button is held, the SDL2 backend's
/// NewFrame rebuilds the pointer as the global pointer minus
/// SDL_GetWindowPosition. Use SDL's window-relative motion state directly,
/// preserving hover while the WM changes the window geometry or fullscreen
/// state. Outside the window, feed "no pointer" instead.
unsafe fn feed_window_relative_pointer(window: *mut sdl2::sys::SDL_Window) {
    use sdl2::sys::{SDL_GetKeyboardFocus, SDL_GetMouseFocus, SDL_GetMouseState};

    if SDL_GetKeyboardFocus() != window {
        return;
    }
    let (mut x, mut y) = (0, 0);
    if SDL_GetMouseState(&mut x, &mut y) != 0 {
        return;
    }
    let io = igGetIO_Nil();
    if SDL_GetMouseFocus() == window {
        ImGuiIO_AddMousePosEvent(io, x as c_float, y as c_float);
    } else {
        ImGuiIO_AddMousePosEvent(io, -c_float::MAX, -c_float::MAX);
    }
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_imgui_render() {
    let draw_data = stone_imgui_prepare_render();
    if !draw_data.is_null() && IMGUI_GL_BACKEND_INITIALIZED.load(Ordering::SeqCst) {
        unsafe {
            cImGui_ImplOpenGL3_RenderDrawData(draw_data.cast());
            crate::ui_submission_probe::imgui_consumed(draw_data.cast());
        }
    }
}

/// Finish the ImGui frame and return its stable draw-data pointer to the
/// selected renderer. The pointer remains valid until the next NewFrame.
/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_imgui_prepare_render() -> *mut c_void {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return ptr::null_mut();
    }

    unsafe {
        igRender();
        let draw_data = igGetDrawData();
        if draw_data.is_null() {
            return ptr::null_mut();
        }
        record_draw_data(draw_data);
        draw_data.cast()
    }
}

unsafe fn record_draw_data(draw_data: *mut ImDrawData) {
    if crate::ui_submission_probe::enabled() {
        unsafe {
            crate::ui_submission_probe::imgui_prepared(draw_data);
        }
    }
    LAST_VTX_COUNT.store((*draw_data).total_vtx_count, Ordering::SeqCst);
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_imgui_last_vtx_count() -> c_int {
    LAST_VTX_COUNT.load(Ordering::SeqCst)
}

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_imgui_initialized() -> bool {
    IMGUI_INITIALIZED.load(Ordering::SeqCst)
}

pub(crate) fn process_event(sdl_event: *mut c_void) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || sdl_event.is_null() {
        return false;
    }

    unsafe { cImGui_ImplSDL2_ProcessEvent(sdl_event) }
}
#[cfg(test)]
mod tests {
    #[test]
    fn imgui_io_capture_prefix_matches_cimgui_want_text_input_offset() {
        assert_eq!(
            std::mem::offset_of!(super::ImGuiIoCapturePrefix, _want_capture_mouse),
            184
        );
        assert_eq!(
            std::mem::offset_of!(super::ImGuiIoCapturePrefix, want_text_input),
            186
        );
    }
}
