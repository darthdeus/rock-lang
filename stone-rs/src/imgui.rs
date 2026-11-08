//! The `stone_imgui_*` ABI.

use std::ffi::{c_char, c_float, c_int, c_uint, c_void};

use crate::{StoneString, StoneWindow};
use {
    crate::{cursor::reapply_cursor_state, with_window},
    std::{
        ffi::CStr,
        ptr,
        sync::atomic::{AtomicBool, AtomicI32, Ordering},
    },
    stone_imgui_abi::{ImDrawData, ImTextureRef, ImVec2, ImVec4},
};

// =============================================================================
// cimgui FFI declarations
// =============================================================================

#[repr(C)]
pub struct ImGuiContext {
    _opaque: [u8; 0],
}

#[repr(C)]
struct ImGuiInputTextCallbackData {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct ImFontAtlas {
    _opaque: [u8; 0],
}

#[repr(C)]
struct ImDrawList {
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

#[link(name = "cimgui_complete")]
extern "C" {
    fn igCreateContext(shared_font_atlas: *mut ImFontAtlas) -> *mut ImGuiContext;
    fn igDestroyContext(ctx: *mut ImGuiContext);
    fn igGetIO_Nil() -> *mut c_void;
    fn ImGuiIO_AddKeyEvent(io: *mut c_void, key: c_int, down: bool);
    fn ImGuiIO_AddMousePosEvent(io: *mut c_void, x: c_float, y: c_float);
    fn igNewFrame();
    fn igRender();
    fn igGetDrawData() -> *mut ImDrawData;
    fn igIsKeyDown_Nil(key: c_int) -> bool;
    fn igIsWindowHovered(flags: c_int) -> bool;
    fn igIsAnyItemHovered() -> bool;
    fn igIsRectVisible_Nil(size: ImVec2) -> bool;
    fn igIsRectVisible_Vec2(min: ImVec2, max: ImVec2) -> bool;
    fn igSetNextWindowPos(pos: ImVec2, cond: c_int, pivot: ImVec2);
    fn igSetWindowFocus_Str(name: *const c_char);
    fn igInputFloat(
        label: *const c_char,
        value: *mut c_float,
        step: c_float,
        step_fast: c_float,
        format: *const c_char,
        flags: c_int,
    ) -> bool;
    fn ImDrawList_AddQuadFilled(
        list: *mut ImDrawList,
        p1: ImVec2,
        p2: ImVec2,
        p3: ImVec2,
        p4: ImVec2,
        col: c_uint,
    );
    fn ImDrawList_AddImageQuad(
        list: *mut ImDrawList,
        texture: ImTextureRef,
        p1: ImVec2,
        p2: ImVec2,
        p3: ImVec2,
        p4: ImVec2,
        uv1: ImVec2,
        uv2: ImVec2,
        uv3: ImVec2,
        uv4: ImVec2,
        col: c_uint,
    );
    fn igIsItemVisible() -> bool;
    fn igIsAnyItemActive() -> bool;
    fn igIsItemDeactivatedAfterEdit() -> bool;
    fn igIsWindowDocked() -> bool;
    fn igBegin(name: *const c_char, p_open: *mut bool, flags: c_int) -> bool;
    fn igButton(label: *const c_char, size: ImVec2) -> bool;
    fn igBeginCombo(label: *const c_char, preview_value: *const c_char, flags: c_int) -> bool;
    fn igEndCombo();
    fn igSetItemDefaultFocus();
    fn igOpenPopup_Str(str_id: *const c_char, popup_flags: c_int);
    fn igBeginPopup(str_id: *const c_char, flags: c_int) -> bool;
    fn igEndPopup();
    fn igMenuItem_Bool(
        label: *const c_char,
        shortcut: *const c_char,
        selected: bool,
        enabled: bool,
    ) -> bool;
    fn igBeginTable(
        str_id: *const c_char,
        columns: c_int,
        flags: c_int,
        outer_size: ImVec2,
        inner_width: c_float,
    ) -> bool;
    fn igEndTable();
    fn igTableHeadersRow();
    fn igTableNextRow(row_flags: c_int, min_row_height: c_float);
    fn igTableNextColumn() -> bool;
    fn igTableSetupColumn(
        label: *const c_char,
        flags: c_int,
        init_width_or_weight: c_float,
        user_id: c_uint,
    );
    fn igCombo_Str(
        label: *const c_char,
        current_item: *mut c_int,
        items_separated_by_zeros: *const c_char,
        popup_max_height_in_items: c_int,
    ) -> bool;
    fn igInputText(
        label: *const c_char,
        buf: *mut c_char,
        buf_size: usize,
        flags: c_int,
        callback: Option<unsafe extern "C" fn(*mut ImGuiInputTextCallbackData) -> c_int>,
        user_data: *mut c_void,
    ) -> bool;
    fn igInputTextMultiline(
        label: *const c_char,
        buf: *mut c_char,
        buf_size: usize,
        size: ImVec2,
        flags: c_int,
        callback: Option<unsafe extern "C" fn(*mut ImGuiInputTextCallbackData) -> c_int>,
        user_data: *mut c_void,
    ) -> bool;
    fn igBeginDisabled(disabled: bool);
    fn igEndDisabled();
    fn igIndent(indent_w: c_float);
    fn igUnindent(indent_w: c_float);
    fn igCollapsingHeader_TreeNodeFlags(label: *const c_char, flags: c_int) -> bool;
    fn igInvisibleButton(label: *const c_char, size: ImVec2, flags: c_int) -> bool;
    fn igSelectable_Bool(label: *const c_char, selected: bool, flags: c_int, size: ImVec2) -> bool;
    fn igSameLine(offset_from_start_x: f32, spacing: f32);
    fn igGetMousePos(p_out: *mut ImVec2);
    fn igGetContentRegionAvail(p_out: *mut ImVec2);
    fn igGetWindowPos(p_out: *mut ImVec2);
    fn igGetWindowSize(p_out: *mut ImVec2);
    fn igImageWithBg(
        tex_ref: ImTextureRef,
        image_size: ImVec2,
        uv0: ImVec2,
        uv1: ImVec2,
        bg_col: ImVec4,
        tint_col: ImVec4,
    );
    fn igImageButton(
        str_id: *const c_char,
        tex_ref: ImTextureRef,
        image_size: ImVec2,
        uv0: ImVec2,
        uv1: ImVec2,
        bg_col: ImVec4,
        tint_col: ImVec4,
    ) -> bool;
    fn igGetCursorScreenPos(p_out: *mut ImVec2);
    fn igGetItemRectMin(p_out: *mut ImVec2);
    fn igGetItemRectMax(p_out: *mut ImVec2);
    fn igSetTooltip(fmt: *const c_char, ...);
    fn igDummy(size: ImVec2);
    fn igSetCursorScreenPos(pos: ImVec2);
    fn igPushStyleColor_U32(idx: c_int, col: c_uint);
    fn igPopStyleColor(count: c_int);
    fn igSetNextItemWidth(item_width: c_float);
    fn igSliderFloat(
        label: *const c_char,
        v: *mut c_float,
        v_min: c_float,
        v_max: c_float,
        format: *const c_char,
        flags: c_int,
    ) -> bool;
    fn igSliderInt(
        label: *const c_char,
        v: *mut c_int,
        v_min: c_int,
        v_max: c_int,
        format: *const c_char,
        flags: c_int,
    ) -> bool;
    fn igCheckbox(label: *const c_char, v: *mut bool) -> bool;
    fn igColorEdit4(label: *const c_char, col: *mut c_float, flags: c_int) -> bool;
    fn igSetScrollHereY(center_y_ratio: c_float);
    fn igGetWindowDrawList() -> *mut ImDrawList;
    fn ImDrawList_AddLine(
        self_: *mut ImDrawList,
        p1: ImVec2,
        p2: ImVec2,
        col: c_uint,
        thickness: c_float,
    );
    fn ImDrawList_AddRectFilled(
        self_: *mut ImDrawList,
        p_min: ImVec2,
        p_max: ImVec2,
        col: c_uint,
        rounding: c_float,
        flags: c_int,
    );
    fn igEndChild();
    fn igEnd();
    fn igTextUnformatted(text: *const c_char, text_end: *const c_char);
    fn igTextColored(col: ImVec4, fmt: *const c_char, ...);
    fn igSeparator();
    fn igIsItemHovered(flags: c_int) -> bool;
    fn igIsItemActive() -> bool;
    fn igIsItemActivated() -> bool;
    fn igIsItemDeactivated() -> bool;
    fn igShowDemoWindow(p_open: *mut bool);

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
static LAST_FRAME_RENDERED: AtomicBool = AtomicBool::new(false);
static LAST_CMD_LISTS_COUNT: AtomicI32 = AtomicI32::new(0);
static LAST_IDX_COUNT: AtomicI32 = AtomicI32::new(0);

/// Vertices in the last frame `stone_imgui_render` submitted; -1 before the
/// first one. Read from Rock through `stone_imgui_last_vtx_count` -- see the
/// note there for why Lua cannot read the draw data itself.
static LAST_VTX_COUNT: AtomicI32 = AtomicI32::new(-1);
const IMGUI_HOVERED_FLAGS_ANY_WINDOW: c_int = 1 << 2;

#[no_mangle]
pub extern "C" fn stone_imgui_init(_window: *mut StoneWindow) -> bool {
    IMGUI_INITIALIZED.store(false, Ordering::SeqCst);
    IMGUI_PLATFORM_INITIALIZED.store(false, Ordering::SeqCst);
    IMGUI_GL_BACKEND_INITIALIZED.store(false, Ordering::SeqCst);
    LAST_FRAME_RENDERED.store(false, Ordering::SeqCst);
    LAST_CMD_LISTS_COUNT.store(0, Ordering::SeqCst);
    LAST_IDX_COUNT.store(0, Ordering::SeqCst);
    LAST_VTX_COUNT.store(-1, Ordering::SeqCst);
    let result = with_window(|window: &StoneWindow| {
        unsafe {
            // Create ImGui context
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
/// SDL_GetWindowPosition. SDL reports a window it flags fullscreen at its
/// display's origin, and SDL2 on X11 keeps that flag when the WM takes the
/// window out of fullscreen (it ignores a _NET_WM_STATE that drops it). The
/// window can then be resized and moved while every ImGui hit lands off by
/// its real origin (card #878). Under the same condition this replaces that
/// pointer with the window-relative one SDL takes from motion events, and
/// with "no pointer" while the pointer is outside the window.
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

#[no_mangle]
pub extern "C" fn stone_imgui_using_mouse() -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return false;
    }
    unsafe { igIsWindowHovered(IMGUI_HOVERED_FLAGS_ANY_WINDOW) || igIsAnyItemHovered() }
}

#[no_mangle]
pub extern "C" fn stone_imgui_wants_mouse() -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return false;
    }
    unsafe {
        let io = igGetIO_Nil().cast::<ImGuiIoCapturePrefix>();
        !io.is_null() && (*io)._want_capture_mouse
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_wants_text_input() -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return false;
    }
    unsafe {
        let io = igGetIO_Nil().cast::<ImGuiIoCapturePrefix>();
        !io.is_null() && (*io).want_text_input
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_begin_window(title: *const c_char, flags: c_int) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || title.is_null() {
        return false;
    }
    unsafe { igBegin(title, ptr::null_mut(), flags) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_button(label: *const c_char) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || label.is_null() {
        return false;
    }
    unsafe { igButton(label, ImVec2 { x: 0.0, y: 0.0 }) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_begin_combo(label: *const c_char, preview: *const c_char) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || label.is_null() || preview.is_null() {
        return false;
    }
    unsafe { igBeginCombo(label, preview, 0) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_end_combo() {
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igEndCombo() }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_set_item_default_focus() {
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igSetItemDefaultFocus() }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_open_popup(id: *const c_char) {
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) && !id.is_null() {
        unsafe { igOpenPopup_Str(id, 0) }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_begin_popup(id: *const c_char) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || id.is_null() {
        return false;
    }
    unsafe { igBeginPopup(id, 0) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_end_popup() {
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igEndPopup() }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_menu_item(label: *const c_char, enabled: bool) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || label.is_null() {
        return false;
    }
    unsafe { igMenuItem_Bool(label, ptr::null(), false, enabled) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_combo_str(
    label: *const c_char,
    current_item: c_int,
    items: StoneString,
    changed: *mut bool,
    out_current_item: *mut c_int,
) {
    unsafe {
        if !changed.is_null() {
            *changed = false;
        }
        if !out_current_item.is_null() {
            *out_current_item = current_item;
        }
    }
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst)
        || label.is_null()
        || (items.len != 0 && items.data.is_null())
    {
        return;
    }
    let bytes = if items.len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(items.data as *const u8, items.len) }
    };
    let Some(capacity) = bytes.len().checked_add(1) else {
        return;
    };
    let mut nul_terminated = Vec::with_capacity(capacity);
    nul_terminated.extend_from_slice(bytes);
    nul_terminated.push(0);
    let mut index = current_item;
    let was_changed = unsafe {
        igCombo_Str(
            label,
            &mut index,
            nul_terminated.as_ptr() as *const c_char,
            -1,
        )
    };
    unsafe {
        if !changed.is_null() {
            *changed = was_changed;
        }
        if !out_current_item.is_null() {
            *out_current_item = index;
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_input_text(
    label: *const c_char,
    buffer: *mut c_char,
    buffer_size: usize,
    flags: c_int,
) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || label.is_null() || buffer.is_null() {
        return false;
    }
    unsafe { igInputText(label, buffer, buffer_size, flags, None, ptr::null_mut()) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_input_text_multiline(
    label: *const c_char,
    buffer: *mut c_char,
    buffer_size: usize,
    width: c_float,
    height: c_float,
    flags: c_int,
) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || label.is_null() || buffer.is_null() {
        return false;
    }
    unsafe {
        igInputTextMultiline(
            label,
            buffer,
            buffer_size,
            ImVec2 {
                x: width,
                y: height,
            },
            flags,
            None,
            ptr::null_mut(),
        )
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_begin_table(id: *const c_char, columns: c_int, flags: c_int) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || id.is_null() {
        return false;
    }
    unsafe { igBeginTable(id, columns, flags, ImVec2 { x: 0.0, y: 0.0 }, 0.0) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_end_table() {
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igEndTable() }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_table_headers_row() {
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igTableHeadersRow() }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_table_next_row() {
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igTableNextRow(0, 0.0) }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_table_next_column() -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return false;
    }
    unsafe { igTableNextColumn() }
}

#[no_mangle]
pub extern "C" fn stone_imgui_table_setup_column_fixed(label: *const c_char, width: c_float) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || label.is_null() {
        return;
    }
    unsafe { igTableSetupColumn(label, 16, width, 0) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_table_setup_column_stretch(label: *const c_char) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || label.is_null() {
        return;
    }
    unsafe { igTableSetupColumn(label, 8, 0.0, 0) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_begin_disabled(disabled: bool) {
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igBeginDisabled(disabled) }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_end_disabled() {
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igEndDisabled() }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_indent(amount: c_float) {
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igIndent(amount) }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_unindent(amount: c_float) {
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igUnindent(amount) }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_collapsing_header_closed(label: *const c_char) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || label.is_null() {
        return false;
    }
    unsafe { igCollapsingHeader_TreeNodeFlags(label, 0) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_invisible_button(
    label: *const c_char,
    size_x: c_float,
    size_y: c_float,
) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || label.is_null() {
        return false;
    }
    unsafe {
        igInvisibleButton(
            label,
            ImVec2 {
                x: size_x,
                y: size_y,
            },
            0,
        )
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_selectable(label: *const c_char, selected: bool) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || label.is_null() {
        return false;
    }
    unsafe { igSelectable_Bool(label, selected, 0, ImVec2 { x: 0.0, y: 0.0 }) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_is_item_active() -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return false;
    }
    unsafe { igIsItemActive() }
}

#[no_mangle]
pub extern "C" fn stone_imgui_is_item_activated() -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return false;
    }
    unsafe { igIsItemActivated() }
}

#[no_mangle]
pub extern "C" fn stone_imgui_is_item_deactivated() -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return false;
    }
    unsafe { igIsItemDeactivated() }
}

#[no_mangle]
pub extern "C" fn stone_imgui_is_item_deactivated_after_edit() -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return false;
    }
    unsafe { igIsItemDeactivatedAfterEdit() }
}

#[no_mangle]
pub extern "C" fn stone_imgui_get_mouse_pos(x: *mut c_float, y: *mut c_float) {
    let mut pos = ImVec2::default();
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igGetMousePos(&mut pos) };
    }
    unsafe {
        if !x.is_null() {
            *x = pos.x;
        }
        if !y.is_null() {
            *y = pos.y;
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_get_content_region_avail(x: *mut c_float, y: *mut c_float) {
    let mut pos = ImVec2::default();
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igGetContentRegionAvail(&mut pos) };
    }
    unsafe {
        if !x.is_null() {
            *x = pos.x;
        }
        if !y.is_null() {
            *y = pos.y;
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_get_window_pos(x: *mut c_float, y: *mut c_float) {
    let mut pos = ImVec2::default();
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igGetWindowPos(&mut pos) };
    }
    unsafe {
        if !x.is_null() {
            *x = pos.x;
        }
        if !y.is_null() {
            *y = pos.y;
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_get_window_size(x: *mut c_float, y: *mut c_float) {
    let mut pos = ImVec2::default();
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igGetWindowSize(&mut pos) };
    }
    unsafe {
        if !x.is_null() {
            *x = pos.x;
        }
        if !y.is_null() {
            *y = pos.y;
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_is_any_item_active() -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return false;
    }
    unsafe { igIsAnyItemActive() }
}

#[no_mangle]
pub extern "C" fn stone_imgui_image_raw(
    texture: u64,
    size_x: c_float,
    size_y: c_float,
    uv1_x: c_float,
    uv1_y: c_float,
    uv2_x: c_float,
    uv2_y: c_float,
    tint_r: c_float,
    tint_g: c_float,
    tint_b: c_float,
    tint_a: c_float,
    border_r: c_float,
    border_g: c_float,
    border_b: c_float,
    border_a: c_float,
) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe {
        igImageWithBg(
            ImTextureRef {
                tex_data: ptr::null_mut(),
                tex_id: texture,
            },
            ImVec2 {
                x: size_x,
                y: size_y,
            },
            ImVec2 { x: uv1_x, y: uv1_y },
            ImVec2 { x: uv2_x, y: uv2_y },
            ImVec4 {
                x: border_r,
                y: border_g,
                z: border_b,
                w: border_a,
            },
            ImVec4 {
                x: tint_r,
                y: tint_g,
                z: tint_b,
                w: tint_a,
            },
        )
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_image_button_raw(
    id: *const c_char,
    texture: u64,
    size_x: c_float,
    size_y: c_float,
    uv1_x: c_float,
    uv1_y: c_float,
    uv2_x: c_float,
    uv2_y: c_float,
    bg_r: c_float,
    bg_g: c_float,
    bg_b: c_float,
    bg_a: c_float,
    tint_r: c_float,
    tint_g: c_float,
    tint_b: c_float,
    tint_a: c_float,
) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || id.is_null() {
        return false;
    }
    unsafe {
        igImageButton(
            id,
            ImTextureRef {
                tex_data: ptr::null_mut(),
                tex_id: texture,
            },
            ImVec2 {
                x: size_x,
                y: size_y,
            },
            ImVec2 { x: uv1_x, y: uv1_y },
            ImVec2 { x: uv2_x, y: uv2_y },
            ImVec4 {
                x: bg_r,
                y: bg_g,
                z: bg_b,
                w: bg_a,
            },
            ImVec4 {
                x: tint_r,
                y: tint_g,
                z: tint_b,
                w: tint_a,
            },
        )
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_is_item_visible() -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return false;
    }
    unsafe { igIsItemVisible() }
}

#[no_mangle]
pub extern "C" fn stone_imgui_is_window_docked() -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return false;
    }
    unsafe { igIsWindowDocked() }
}

#[no_mangle]
pub extern "C" fn stone_imgui_same_line() {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe { igSameLine(0.0, -1.0) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_get_cursor_screen_pos(x: *mut c_float, y: *mut c_float) {
    let mut pos = ImVec2::default();
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igGetCursorScreenPos(&mut pos) };
    }
    unsafe {
        if !x.is_null() {
            *x = pos.x;
        }
        if !y.is_null() {
            *y = pos.y;
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_get_item_rect_min(x: *mut c_float, y: *mut c_float) {
    let mut pos = ImVec2::default();
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igGetItemRectMin(&mut pos) };
    }
    unsafe {
        if !x.is_null() {
            *x = pos.x;
        }
        if !y.is_null() {
            *y = pos.y;
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_get_item_rect_max(x: *mut c_float, y: *mut c_float) {
    let mut pos = ImVec2::default();
    if IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        unsafe { igGetItemRectMax(&mut pos) };
    }
    unsafe {
        if !x.is_null() {
            *x = pos.x;
        }
        if !y.is_null() {
            *y = pos.y;
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_dummy(size_x: c_float, size_y: c_float) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe {
        igDummy(ImVec2 {
            x: size_x,
            y: size_y,
        })
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_set_cursor_screen_pos(pos_x: c_float, pos_y: c_float) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe { igSetCursorScreenPos(ImVec2 { x: pos_x, y: pos_y }) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_push_style_color(idx: c_int, col_abgr: c_uint) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe { igPushStyleColor_U32(idx, col_abgr) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_pop_style_color(count: c_int) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe { igPopStyleColor(count) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_set_next_item_width(width: c_float) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe { igSetNextItemWidth(width) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_slider_float(
    label: *const c_char,
    value: c_float,
    min: c_float,
    max: c_float,
    changed: *mut bool,
    out_value: *mut c_float,
) {
    let mut next_value = value;
    let did_change = if IMGUI_INITIALIZED.load(Ordering::SeqCst) && !label.is_null() {
        unsafe { igSliderFloat(label, &mut next_value, min, max, ptr::null(), 0) }
    } else {
        false
    };
    unsafe {
        if !changed.is_null() {
            *changed = did_change;
        }
        if !out_value.is_null() {
            *out_value = next_value;
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_slider_int(
    label: *const c_char,
    value: c_int,
    min: c_int,
    max: c_int,
    changed: *mut bool,
    out_value: *mut c_int,
) {
    let mut next_value = value;
    let did_change = if IMGUI_INITIALIZED.load(Ordering::SeqCst) && !label.is_null() {
        unsafe { igSliderInt(label, &mut next_value, min, max, ptr::null(), 0) }
    } else {
        false
    };
    unsafe {
        if !changed.is_null() {
            *changed = did_change;
        }
        if !out_value.is_null() {
            *out_value = next_value;
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_checkbox(
    label: *const c_char,
    value: bool,
    changed: *mut bool,
    out_value: *mut bool,
) {
    let mut next_value = value;
    let did_change = if IMGUI_INITIALIZED.load(Ordering::SeqCst) && !label.is_null() {
        unsafe { igCheckbox(label, &mut next_value) }
    } else {
        false
    };
    unsafe {
        if !changed.is_null() {
            *changed = did_change;
        }
        if !out_value.is_null() {
            *out_value = next_value;
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_color_edit4(
    label: *const c_char,
    r: c_float,
    g: c_float,
    b: c_float,
    a: c_float,
    changed: *mut bool,
    out_r: *mut c_float,
    out_g: *mut c_float,
    out_b: *mut c_float,
    out_a: *mut c_float,
) {
    let mut values = [r, g, b, a];
    let did_change = if IMGUI_INITIALIZED.load(Ordering::SeqCst) && !label.is_null() {
        unsafe { igColorEdit4(label, values.as_mut_ptr(), 0) }
    } else {
        false
    };
    unsafe {
        if !changed.is_null() {
            *changed = did_change;
        }
        if !out_r.is_null() {
            *out_r = values[0];
        }
        if !out_g.is_null() {
            *out_g = values[1];
        }
        if !out_b.is_null() {
            *out_b = values[2];
        }
        if !out_a.is_null() {
            *out_a = values[3];
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_set_scroll_here_y(center_y_ratio: c_float) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe { igSetScrollHereY(center_y_ratio) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_window_add_line(
    p1_x: c_float,
    p1_y: c_float,
    p2_x: c_float,
    p2_y: c_float,
    col_abgr: c_uint,
    thickness: c_float,
) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    let draw_list = unsafe { igGetWindowDrawList() };
    if draw_list.is_null() {
        return;
    }
    unsafe {
        ImDrawList_AddLine(
            draw_list,
            ImVec2 { x: p1_x, y: p1_y },
            ImVec2 { x: p2_x, y: p2_y },
            col_abgr,
            thickness,
        )
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_window_add_rect_filled(
    min_x: c_float,
    min_y: c_float,
    max_x: c_float,
    max_y: c_float,
    col_abgr: c_uint,
) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    let draw_list = unsafe { igGetWindowDrawList() };
    if draw_list.is_null() {
        return;
    }
    unsafe {
        ImDrawList_AddRectFilled(
            draw_list,
            ImVec2 { x: min_x, y: min_y },
            ImVec2 { x: max_x, y: max_y },
            col_abgr,
            0.0,
            0,
        )
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_end_child() {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe { igEndChild() }
}

#[no_mangle]
pub extern "C" fn stone_imgui_end() {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe { igEnd() }
}

#[no_mangle]
pub extern "C" fn stone_imgui_text(text: *const c_char) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || text.is_null() {
        return;
    }
    unsafe { igTextUnformatted(text, ptr::null()) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_set_tooltip(text: *const c_char) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || text.is_null() {
        return;
    }
    unsafe { igSetTooltip(text) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_text_colored(
    text: *const c_char,
    r: c_float,
    g: c_float,
    b: c_float,
    a: c_float,
) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || text.is_null() {
        return;
    }
    unsafe {
        igTextColored(
            ImVec4 {
                x: r,
                y: g,
                z: b,
                w: a,
            },
            c"%s".as_ptr(),
            text,
        )
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_separator() {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe { igSeparator() }
}

#[no_mangle]
pub extern "C" fn stone_imgui_is_item_hovered(flags: c_int) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return false;
    }
    unsafe { igIsItemHovered(flags) }
}

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
    LAST_FRAME_RENDERED.store(true, Ordering::SeqCst);
    LAST_CMD_LISTS_COUNT.store((*draw_data).cmd_lists_count, Ordering::SeqCst);
    LAST_IDX_COUNT.store((*draw_data).total_idx_count, Ordering::SeqCst);
    LAST_VTX_COUNT.store((*draw_data).total_vtx_count, Ordering::SeqCst);
}

/// Vertices in the last rendered imgui frame, or -1 before the first render.
///
/// This exists because Lua cannot read it: `igGetDrawData()` returns NULL
/// outside Render()..NewFrame(), and every Rock-visible frame phase sits
/// between NewFrame and Render. A gate asserting imgui actually produced
/// geometry -- rather than that its API merely answered -- needs this.
#[no_mangle]
pub extern "C" fn stone_imgui_last_vtx_count() -> c_int {
    LAST_VTX_COUNT.load(Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn stone_imgui_initialized() -> bool {
    IMGUI_INITIALIZED.load(Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn stone_imgui_platform_initialized() -> bool {
    IMGUI_PLATFORM_INITIALIZED.load(Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn stone_imgui_gl_backend_initialized() -> bool {
    IMGUI_GL_BACKEND_INITIALIZED.load(Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn stone_imgui_last_frame_rendered() -> bool {
    LAST_FRAME_RENDERED.load(Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn stone_imgui_last_cmd_lists_count() -> c_int {
    LAST_CMD_LISTS_COUNT.load(Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn stone_imgui_last_idx_count() -> c_int {
    LAST_IDX_COUNT.load(Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn stone_imgui_process_event(sdl_event: *mut c_void) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || sdl_event.is_null() {
        return false;
    }

    unsafe { cImGui_ImplSDL2_ProcessEvent(sdl_event) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_show_demo() {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }

    unsafe {
        igShowDemoWindow(ptr::null_mut());
    }
}

const IMGUI_KEY_NAMED_BEGIN: c_int = 512;
const IMGUI_KEY_NAMED_END: c_int = 667;
const IMGUI_KEY_LEFT_CTRL: c_int = 527;
const IMGUI_KEY_RIGHT_CTRL: c_int = 531;
const IMGUI_KEY_RESERVED_CTRL: c_int = 663;

fn imgui_key_id(name: &str) -> Option<c_int> {
    let named = match name {
        "tab" => Some(512),
        "left" => Some(513),
        "right" => Some(514),
        "up" => Some(515),
        "down" => Some(516),
        "delete" => Some(522),
        "backspace" => Some(523),
        "space" => Some(524),
        "return" | "enter" => Some(525),
        "escape" => Some(526),
        "lctrl" => Some(IMGUI_KEY_LEFT_CTRL),
        "lshift" => Some(528),
        "lalt" => Some(529),
        "rctrl" => Some(IMGUI_KEY_RIGHT_CTRL),
        "rshift" => Some(532),
        "ralt" => Some(533),
        "[" => Some(603),
        "\\" => Some(604),
        "]" => Some(605),
        ";" => Some(601),
        "'" => Some(596),
        "," => Some(597),
        "-" => Some(598),
        "." => Some(599),
        "/" => Some(600),
        "=" => Some(602),
        "`" => Some(606),
        _ => None,
    };
    if named.is_some() {
        return named;
    }

    if let Some(digits) = name.strip_prefix('f').or_else(|| name.strip_prefix('F')) {
        if !digits.is_empty()
            && !digits.starts_with('0')
            && digits.bytes().all(|byte| byte.is_ascii_digit())
        {
            if let Ok(number) = digits.parse::<c_int>() {
                if (1..=12).contains(&number) {
                    return Some(571 + number);
                }
            }
        }
    }

    if name.len() == 1 {
        let byte = name.as_bytes()[0];
        if byte.is_ascii_digit() {
            return Some(536 + (byte - b'0') as c_int);
        }
        if byte.is_ascii_alphabetic() {
            return Some(546 + (byte.to_ascii_uppercase() - b'A') as c_int);
        }
    }

    None
}

fn is_valid_imgui_key_id(id: c_int) -> bool {
    (IMGUI_KEY_NAMED_BEGIN..IMGUI_KEY_NAMED_END).contains(&id)
}

unsafe fn current_imgui_io() -> Option<*mut c_void> {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return None;
    }

    let io = igGetIO_Nil();
    if io.is_null() {
        ::log::error!(target: "IMGUI", "ImGui is initialized but cimgui returned no ImGuiIO");
        None
    } else {
        Some(io)
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_is_key_down(id: c_int) -> bool {
    if !is_valid_imgui_key_id(id) {
        ::log::error!(target: "IMGUI", "stone_imgui_is_key_down: invalid ImGui key id {id}");
        return false;
    }

    unsafe {
        if current_imgui_io().is_none() {
            return false;
        }
        igIsKeyDown_Nil(id)
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_ctrl_down() -> bool {
    unsafe {
        if current_imgui_io().is_none() {
            return false;
        }
        igIsKeyDown_Nil(IMGUI_KEY_RESERVED_CTRL)
            || igIsKeyDown_Nil(IMGUI_KEY_LEFT_CTRL)
            || igIsKeyDown_Nil(IMGUI_KEY_RIGHT_CTRL)
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_key_id(name: *const c_char) -> c_int {
    if name.is_null() {
        ::log::error!(target: "IMGUI", "stone_imgui_key_id: null key name");
        return -1;
    }

    let name = unsafe { CStr::from_ptr(name) };
    match name.to_str() {
        Ok(name) => imgui_key_id(name).unwrap_or(-1),
        Err(error) => {
            ::log::error!(target: "IMGUI", "stone_imgui_key_id: key name is not UTF-8: {error}");
            -1
        }
    }
}

unsafe fn add_imgui_key_event(id: c_int, down: bool, operation: &str) {
    if !is_valid_imgui_key_id(id) {
        ::log::error!(target: "IMGUI", "{operation}: invalid ImGui key id {id}");
        return;
    }

    let Some(io) = current_imgui_io() else {
        return;
    };
    ImGuiIO_AddKeyEvent(io, id, down);

    if id == IMGUI_KEY_LEFT_CTRL || id == IMGUI_KEY_RIGHT_CTRL {
        ImGuiIO_AddKeyEvent(io, IMGUI_KEY_RESERVED_CTRL, down);
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_inject_key(id: c_int) {
    unsafe { add_imgui_key_event(id, true, "stone_imgui_inject_key") }
}

#[no_mangle]
pub extern "C" fn stone_imgui_release_key(id: c_int) {
    unsafe { add_imgui_key_event(id, false, "stone_imgui_release_key") }
}

#[no_mangle]
pub extern "C" fn stone_imgui_is_rect_visible(x: c_float, y: c_float) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return false;
    }
    unsafe { igIsRectVisible_Nil(ImVec2 { x, y }) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_is_item_fully_visible() -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return false;
    }
    unsafe {
        let mut min = ImVec2 { x: 0.0, y: 0.0 };
        let mut max = ImVec2 { x: 0.0, y: 0.0 };
        igGetItemRectMin(&mut min);
        igGetItemRectMax(&mut max);
        igIsRectVisible_Vec2(
            min,
            ImVec2 {
                x: min.x + 1.0,
                y: min.y + 1.0,
            },
        ) && igIsRectVisible_Vec2(
            ImVec2 {
                x: max.x - 1.0,
                y: max.y - 1.0,
            },
            max,
        )
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_set_next_window_pos(x: c_float, y: c_float) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe { igSetNextWindowPos(ImVec2 { x, y }, 0, ImVec2 { x: 0.0, y: 0.0 }) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_set_window_focus(name: *const c_char) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || name.is_null() {
        return;
    }
    unsafe { igSetWindowFocus_Str(name) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_button_sized(label: *const c_char, x: c_float, y: c_float) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || label.is_null() {
        return false;
    }
    unsafe { igButton(label, ImVec2 { x, y }) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_collapsing_header(label: *const c_char) -> bool {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) || label.is_null() {
        return false;
    }
    unsafe { igCollapsingHeader_TreeNodeFlags(label, 32) }
}

#[no_mangle]
pub extern "C" fn stone_imgui_input_float(
    label: *const c_char,
    value: c_float,
    step: c_float,
    step_fast: c_float,
    format: *const c_char,
    changed: *mut bool,
    result: *mut c_float,
) {
    let mut value = value;
    let did_change =
        if IMGUI_INITIALIZED.load(Ordering::SeqCst) && !label.is_null() && !format.is_null() {
            unsafe { igInputFloat(label, &mut value, step, step_fast, format, 0) }
        } else {
            false
        };
    unsafe {
        if !changed.is_null() {
            *changed = did_change;
        }
        if !result.is_null() {
            *result = value;
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_window_add_quad_filled(
    p1_x: c_float,
    p1_y: c_float,
    p2_x: c_float,
    p2_y: c_float,
    p3_x: c_float,
    p3_y: c_float,
    p4_x: c_float,
    p4_y: c_float,
    col: c_uint,
) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    let list = unsafe { igGetWindowDrawList() };
    if list.is_null() {
        return;
    }
    unsafe {
        ImDrawList_AddQuadFilled(
            list,
            ImVec2 { x: p1_x, y: p1_y },
            ImVec2 { x: p2_x, y: p2_y },
            ImVec2 { x: p3_x, y: p3_y },
            ImVec2 { x: p4_x, y: p4_y },
            col,
        )
    }
}

#[no_mangle]
pub extern "C" fn stone_imgui_window_add_image_quad(
    texture: u64,
    p1_x: c_float,
    p1_y: c_float,
    p2_x: c_float,
    p2_y: c_float,
    p3_x: c_float,
    p3_y: c_float,
    p4_x: c_float,
    p4_y: c_float,
    uv1_x: c_float,
    uv1_y: c_float,
    uv2_x: c_float,
    uv2_y: c_float,
    uv3_x: c_float,
    uv3_y: c_float,
    uv4_x: c_float,
    uv4_y: c_float,
    col: c_uint,
) {
    if !IMGUI_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    let list = unsafe { igGetWindowDrawList() };
    if list.is_null() {
        return;
    }
    unsafe {
        ImDrawList_AddImageQuad(
            list,
            ImTextureRef {
                tex_data: ptr::null_mut(),
                tex_id: texture,
            },
            ImVec2 { x: p1_x, y: p1_y },
            ImVec2 { x: p2_x, y: p2_y },
            ImVec2 { x: p3_x, y: p3_y },
            ImVec2 { x: p4_x, y: p4_y },
            ImVec2 { x: uv1_x, y: uv1_y },
            ImVec2 { x: uv2_x, y: uv2_y },
            ImVec2 { x: uv3_x, y: uv3_y },
            ImVec2 { x: uv4_x, y: uv4_y },
            col,
        )
    }
}
