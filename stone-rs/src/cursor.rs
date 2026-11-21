use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, CStr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use sdl2::mouse::{Cursor, SystemCursor};
use sdl2::pixels::PixelFormatEnum;
use sdl2::surface::Surface;

use crate::handles::Class;

/// Tracks the desired cursor visibility set by the game via stone_show_cursor.
/// ImGui's NewFrame resets SDL cursor state every frame, so we re-apply after it.
static CURSOR_VISIBLE: AtomicBool = AtomicBool::new(true);

thread_local! {
    static CURSORS: RefCell<HashMap<u64, Cursor>> = RefCell::new(HashMap::new());
}

/// Which cursor id the game wants active. ImGui may temporarily replace SDL's
/// actual cursor during its NewFrame.
static DESIRED_CURSOR: AtomicU64 = AtomicU64::new(0);

fn set_desired_cursor(id: u64) {
    DESIRED_CURSOR.store(id, Ordering::SeqCst);
}

fn set_default_cursor() {
    unsafe {
        let default = sdl2::sys::SDL_GetDefaultCursor();
        if !default.is_null() {
            sdl2::sys::SDL_SetCursor(default);
        }
    }
}

fn store_cursor(cursor: Cursor) -> u64 {
    let id = crate::handles::mint(Class::Cursor);
    cursor.set();
    set_desired_cursor(id);
    CURSORS.with(|c| {
        c.borrow_mut().insert(id, cursor);
    });
    id
}

#[no_mangle]
pub extern "C" fn stone_show_cursor(show: bool) {
    CURSOR_VISIBLE.store(show, Ordering::SeqCst);
    unsafe {
        sdl2::sys::SDL_ShowCursor(show as i32);
    }
}

/// Re-apply the desired cursor after ImGui's NewFrame, whose SDL backend may
/// replace both its identity and visibility.
pub fn reapply_cursor_state() {
    let desired = DESIRED_CURSOR.load(Ordering::SeqCst);
    if desired == 0 {
        set_default_cursor();
    } else {
        let found = CURSORS.with(|c| c.borrow().get(&desired).map(Cursor::set).is_some());
        if !found {
            ::log::error!(target: "CURSOR",
                "reapply_cursor_state: desired cursor {} is no longer live", desired);
            set_default_cursor();
            set_desired_cursor(0);
        }
    }

    let visible = CURSOR_VISIBLE.load(Ordering::SeqCst);
    unsafe {
        sdl2::sys::SDL_ShowCursor(visible as i32);
    }
}

/// Create a cursor from an asset path or image file. Returns cursor_id (0 = error).
/// hot_x/hot_y is the click point relative to the top-left of the image.
#[no_mangle]
pub extern "C" fn stone_create_cursor(path: *const c_char, hot_x: c_int, hot_y: c_int) -> u64 {
    if path.is_null() {
        return 0;
    }
    let path_str = unsafe { CStr::from_ptr(path).to_str().unwrap_or("") };

    let Some((data, width, height)) = decode_cursor_image(path_str) else {
        ::log::error!(target: "CURSOR", "stone_create_cursor: failed to load {}", path_str);
        return 0;
    };

    create_cursor_from_rgba(&data, width, height, hot_x, hot_y)
}

fn decode_cursor_image(path_str: &str) -> Option<(Vec<u8>, u32, u32)> {
    crate::image_texture::decode_rgba_image(path_str)
}

#[no_mangle]
pub extern "C" fn stone_set_system_cursor(cursor_type: c_int) -> u64 {
    let system = match cursor_type {
        0 => SystemCursor::Arrow,
        1 => SystemCursor::IBeam,
        2 => SystemCursor::Wait,
        3 => SystemCursor::Crosshair,
        4 => SystemCursor::WaitArrow,
        5 => SystemCursor::SizeNWSE,
        6 => SystemCursor::SizeNESW,
        7 => SystemCursor::SizeWE,
        8 => SystemCursor::SizeNS,
        9 => SystemCursor::SizeAll,
        10 => SystemCursor::No,
        11 => SystemCursor::Hand,
        _ => {
            ::log::warn!(target: "CURSOR", "stone_set_system_cursor: invalid cursor type {}", cursor_type);
            return 0;
        }
    };

    match Cursor::from_system(system) {
        Ok(cursor) => store_cursor(cursor),
        Err(e) => {
            ::log::error!(target: "CURSOR", "stone_set_system_cursor: failed: {}", e);
            0
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_set_cursor(cursor_id: u64) {
    CURSORS.with(|c| {
        if let Some(cursor) = c.borrow().get(&cursor_id) {
            cursor.set();
            set_desired_cursor(cursor_id);
        } else {
            ::log::warn!(target: "CURSOR", "stone_set_cursor: invalid cursor id {}", cursor_id);
        }
    });
}

#[no_mangle]
pub extern "C" fn stone_reset_cursor() {
    set_default_cursor();
    set_desired_cursor(0);
}

/// Live custom-cursor ids (`native_ledger`).
pub(crate) fn live_cursor_ids() -> Vec<u64> {
    CURSORS.with(|c| c.borrow().keys().copied().collect())
}

/// Reset the active cursor before freeing it: SDL may still hold its pointer.
#[no_mangle]
pub extern "C" fn stone_free_cursor(cursor_id: u64) {
    if DESIRED_CURSOR.load(Ordering::SeqCst) == cursor_id {
        stone_reset_cursor();
    }
    CURSORS.with(|c| {
        if c.borrow_mut().remove(&cursor_id).is_some() {
            crate::handles::retire(Class::Cursor, cursor_id);
        }
    });
}

/// Free cursors and restore defaults before SDL_Quit.
pub(crate) fn free_all_cursors() {
    // `stone_free_cursor` resets the pointer when it frees the active cursor
    // and retires the handle, so the ledger and SDL stay in step.
    for id in live_cursor_ids() {
        stone_free_cursor(id);
    }

    // Only reset mirrors; SDL is being torn down.
    CURSOR_VISIBLE.store(true, Ordering::SeqCst);
    set_desired_cursor(0);
}

fn create_cursor_from_rgba(
    data: &[u8],
    width: u32,
    height: u32,
    hot_x: c_int,
    hot_y: c_int,
) -> u64 {
    let pitch = width * 4;
    let mut pixels = data.to_vec();

    let surface = match Surface::from_data(
        &mut pixels,
        width,
        height,
        pitch,
        PixelFormatEnum::RGBA32,
    ) {
        Ok(s) => s,
        Err(e) => {
            ::log::error!(target: "CURSOR", "stone_create_cursor: failed to create surface: {}", e);
            return 0;
        }
    };

    match Cursor::from_surface(surface, hot_x, hot_y) {
        Ok(cursor) => store_cursor(cursor),
        Err(e) => {
            ::log::error!(target: "CURSOR", "stone_create_cursor: failed to create cursor: {}", e);
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn cursor_image_decode_reads_embedded_assets() {
        let path = "embedded-only/cursor-test.png";
        let c_path = CString::new(path).unwrap();
        let bytes = crate::image_data::encode_test_png(32, 32, &[255, 255, 255, 255]);
        crate::asset_registry::stone_register_asset(c_path.as_ptr(), bytes.as_ptr(), bytes.len());

        let (rgba, width, height) =
            decode_cursor_image(path).expect("cursor decode must use the embedded asset registry");

        assert_eq!((width, height), (32, 32));
        assert_eq!(rgba.len(), (width * height * 4) as usize);
    }
}
