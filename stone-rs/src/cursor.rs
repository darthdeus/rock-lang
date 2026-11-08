use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, CStr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use sdl2::mouse::{Cursor, SystemCursor};
use sdl2::pixels::PixelFormatEnum;
use sdl2::surface::Surface;

use crate::handles::Class;
use crate::image_data::with_image_data;

/// Tracks the desired cursor visibility set by the game via stone_show_cursor.
/// ImGui's NewFrame resets SDL cursor state every frame, so we re-apply after it.
static CURSOR_VISIBLE: AtomicBool = AtomicBool::new(true);

thread_local! {
    static CURSORS: RefCell<HashMap<u64, StoredCursor>> = RefCell::new(HashMap::new());
}

struct StoredCursor {
    cursor: Cursor,
    raw: usize,
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
    let raw = unsafe { sdl2::sys::SDL_GetCursor() as usize };
    set_desired_cursor(id);
    CURSORS.with(|c| {
        c.borrow_mut().insert(id, StoredCursor { cursor, raw });
    });
    id
}

/// Show or hide the OS cursor.
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
        let found = CURSORS.with(|c| {
            c.borrow()
                .get(&desired)
                .map(|stored| stored.cursor.set())
                .is_some()
        });
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

/// Returns whether the OS cursor is currently visible.
#[no_mangle]
pub extern "C" fn stone_is_cursor_visible() -> bool {
    unsafe { sdl2::sys::SDL_ShowCursor(-1) == 1 }
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

/// Create a cursor from an ImageData handle. Returns cursor_id (0 = error).
/// hot_x/hot_y is the click point relative to the top-left of the image.
#[no_mangle]
pub extern "C" fn stone_create_cursor_from_image_data(
    image_data_handle: u64,
    hot_x: c_int,
    hot_y: c_int,
) -> u64 {
    with_image_data(image_data_handle, |width, height, data| {
        create_cursor_from_rgba(data, width, height, hot_x, hot_y)
    })
    .unwrap_or(0)
}

/// Set a system cursor (arrow, crosshair, hand, etc.).
/// Returns cursor_id (0 = error).
///
/// Types: 0=Arrow, 1=IBeam, 2=Wait, 3=Crosshair, 4=WaitArrow,
///        5=SizeNWSE, 6=SizeNESW, 7=SizeWE, 8=SizeNS, 9=SizeAll,
///        10=No, 11=Hand
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

/// Activate a previously created cursor by id.
#[no_mangle]
pub extern "C" fn stone_set_cursor(cursor_id: u64) {
    CURSORS.with(|c| {
        if let Some(stored) = c.borrow().get(&cursor_id) {
            stored.cursor.set();
            set_desired_cursor(cursor_id);
        } else {
            ::log::warn!(target: "CURSOR", "stone_set_cursor: invalid cursor id {}", cursor_id);
        }
    });
}

/// Reset to the default OS cursor.
#[no_mangle]
pub extern "C" fn stone_reset_cursor() {
    set_default_cursor();
    set_desired_cursor(0);
}

/// Which registered cursor SDL is actually displaying, 0 for the default or
/// an unregistered cursor installed by another subsystem.
#[no_mangle]
pub extern "C" fn stone_active_cursor() -> u64 {
    let raw = unsafe { sdl2::sys::SDL_GetCursor() as usize };
    CURSORS.with(|c| {
        c.borrow()
            .iter()
            .find_map(|(id, stored)| (stored.raw == raw).then_some(*id))
            .unwrap_or(0)
    })
}

/// Live custom-cursor ids (`native_ledger`).
pub(crate) fn live_cursor_ids() -> Vec<u64> {
    CURSORS.with(|c| c.borrow().keys().copied().collect())
}

/// Free a cursor, resetting to the default first when SDL is still pointing at
/// it. The doc comment claimed that reset for a long time and the code did not
/// do it; dropping the `Cursor` calls `SDL_FreeCursor` on a cursor SDL still
/// holds. `store_cursor` ACTIVATES what it creates, so every freshly created
/// cursor is in exactly that state.
#[no_mangle]
pub extern "C" fn stone_free_cursor(cursor_id: u64) {
    if DESIRED_CURSOR.load(Ordering::SeqCst) == cursor_id {
        stone_reset_cursor();
    }
    CURSORS.with(|c| {
        // Drop the Cursor, which calls SDL_FreeCursor
        if c.borrow_mut().remove(&cursor_id).is_some() {
            crate::handles::retire(Class::Cursor, cursor_id);
        }
    });
}

/// Free every live cursor and return the ambient cursor state to the process
/// default. Called where the SDL context is about to be dropped
/// (`stone_teardown`), while SDL is still up.
///
/// An `SDL_Cursor` has to be freed before `SDL_Quit`, and the ordering belongs
/// to whoever owns SDL, so the free runs once, at window destruction.
pub(crate) fn free_all_cursors() {
    // `stone_free_cursor` resets the pointer when it frees the active cursor
    // and retires the handle, so the ledger and SDL stay in step.
    for id in live_cursor_ids() {
        stone_free_cursor(id);
    }

    // The mirrors of the two ambient values, returned to what this module
    // starts a process with. SDL's own cursor state dies with the context
    // being dropped, so only the mirrors are written here -- reaching for
    // SDL_ShowCursor would be driving a subsystem on its way out.
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
