use std::ffi::{c_char, c_int, CStr};
use std::ptr;

use stone_capture::RowOrigin;

use crate::with_gl;

#[no_mangle]
pub extern "C" fn stone_save_screenshot(
    path: *const c_char,
    w: c_int,
    h: c_int,
    max_size: c_int,
) -> bool {
    stone_save_screenshot_sized(path, w, h, max_size, ptr::null_mut(), ptr::null_mut())
}

// Same capture as stone_save_screenshot, plus the ACTUAL saved PNG
// dimensions (which differ from w/h when max_size triggers a resize) via
// out-pointers. A separate exported symbol rather than widening
// stone_save_screenshot in place: that binding is called directly, for its
// plain bool, by many existing sims and callers -- changing its signature
// would break all of them for a dimension only one caller needs.
#[no_mangle]
pub extern "C" fn stone_save_screenshot_sized(
    path: *const c_char,
    w: c_int,
    h: c_int,
    max_size: c_int,
    out_width: *mut c_int,
    out_height: *mut c_int,
) -> bool {
    if path.is_null() || w <= 0 || h <= 0 {
        return false;
    }

    let path_str = unsafe { CStr::from_ptr(path).to_str().unwrap_or("") };
    if path_str.is_empty() {
        return false;
    }

    let width = w as usize;
    let height = h as usize;

    // Read pixels from framebuffer
    let pixels = with_gl(|gl| unsafe {
        let mut pixels = vec![0u8; width * height * 4];
        gl.read_pixels(
            0,
            0,
            w,
            h,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(pixels.as_mut_slice())),
        );
        pixels
    });

    let pixels = match pixels {
        Some(p) => p,
        None => return false,
    };

    match stone_capture::save_rgba_png(
        path_str,
        w as u32,
        h as u32,
        RowOrigin::BottomUp,
        pixels,
        max_size,
    ) {
        Ok((final_width, final_height)) => {
            unsafe {
                if !out_width.is_null() {
                    *out_width = final_width as c_int;
                }
                if !out_height.is_null() {
                    *out_height = final_height as c_int;
                }
            }
            true
        }
        Err(e) => {
            ::log::error!(target: "SCREENSHOT", "{e}");
            false
        }
    }
}
