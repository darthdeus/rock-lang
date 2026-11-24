use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_float, c_int, CStr};

use crate::handles::Class;
use crate::image_texture::{
    create_filtered_image_texture, define_bound_image_texture, insert_image, with_image,
    ImageFormat,
};
use crate::with_gl;

struct ImageData {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl ImageData {
    fn pixel_offset(&self, x: c_int, y: c_int) -> Option<usize> {
        if !(0..self.width as i32).contains(&x) || !(0..self.height as i32).contains(&y) {
            return None;
        }
        Some((y as usize * self.width as usize + x as usize) * 4)
    }
}

thread_local! {
    static IMAGE_DATAS: RefCell<HashMap<u64, ImageData>> = RefCell::new(HashMap::new());
}

fn get<R>(handle: u64, f: impl FnOnce(&ImageData) -> R) -> Option<R> {
    IMAGE_DATAS.with(|ids| ids.borrow().get(&handle).map(f))
}

fn get_mut<R>(handle: u64, f: impl FnOnce(&mut ImageData) -> R) -> Option<R> {
    IMAGE_DATAS.with(|ids| ids.borrow_mut().get_mut(&handle).map(f))
}

/// The UTF-8 path behind a C string, or None for a null, invalid or empty one.
fn c_path<'a>(path: *const c_char) -> Option<&'a str> {
    if path.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(path) }
        .to_str()
        .ok()
        .filter(|path| !path.is_empty())
}

fn store_image_data(image: ImageData) -> u64 {
    let id = crate::handles::mint(Class::ImageData);
    IMAGE_DATAS.with(|ids| {
        ids.borrow_mut().insert(id, image);
    });
    id
}

pub(crate) fn store_owned_rgba(width: usize, height: usize, data: Vec<u8>) -> Option<u64> {
    let width_u32 = u32::try_from(width).ok()?;
    let height_u32 = u32::try_from(height).ok()?;
    if width_u32 == 0 || height_u32 == 0 {
        return None;
    }
    let expected = width.checked_mul(height)?.checked_mul(4)?;
    if data.len() != expected {
        return None;
    }
    Some(store_image_data(ImageData {
        width: width_u32,
        height: height_u32,
        data,
    }))
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_get_image_data_count() -> c_int {
    IMAGE_DATAS.with(|ids| ids.borrow().len() as c_int)
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_create_image_data(width: c_int, height: c_int) -> u64 {
    if width <= 0 || height <= 0 {
        return 0;
    }

    let w = width as usize;
    let h = height as usize;
    let Some(data_size) = w.checked_mul(h).and_then(|pixels| pixels.checked_mul(4)) else {
        return 0;
    };
    store_owned_rgba(w, h, vec![0u8; data_size]).unwrap_or(0)
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_image_data_size(
    handle: u64,
    width: *mut c_int,
    height: *mut c_int,
) -> bool {
    let Some((w, h)) = get(handle, |image| (image.width, image.height)) else {
        return false;
    };
    unsafe {
        if !width.is_null() {
            *width = w as c_int;
        }
        if !height.is_null() {
            *height = h as c_int;
        }
    }
    true
}

/// Live ImageData handles (`native_ledger`).
pub(crate) fn live_image_data_ids() -> Vec<u64> {
    IMAGE_DATAS.with(|ids| ids.borrow().keys().copied().collect())
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_destroy_image_data(handle: u64) {
    IMAGE_DATAS.with(|ids| {
        if ids.borrow_mut().remove(&handle).is_some() {
            crate::handles::retire(Class::ImageData, handle);
        }
    });
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_image_data_set_pixel(
    handle: u64,
    x: c_int,
    y: c_int,
    r: c_float,
    g: c_float,
    b: c_float,
    a: c_float,
) {
    get_mut(handle, |image| {
        if let Some(offset) = image.pixel_offset(x, y) {
            image.data[offset..offset + 4]
                .copy_from_slice(&[r, g, b, a].map(|c| (c * 255.0) as u8));
        }
    });
}

/// `rgba` must hold `pixel_count * 4` bytes.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_image_data_fill_rgba(handle: u64, rgba: *const u8, pixel_count: c_int) {
    if rgba.is_null() || pixel_count <= 0 {
        return;
    }
    get_mut(handle, |image| {
        let n = pixel_count as usize;
        if n * 4 <= image.data.len() {
            unsafe { std::ptr::copy_nonoverlapping(rgba, image.data.as_mut_ptr(), n * 4) };
        }
    });
}

/// rock-ffi stone private c-bool
#[no_mangle]
pub extern "C" fn stone_image_data_read_rgba(
    handle: u64,
    rgba: *mut u8,
    pixel_count: c_int,
) -> bool {
    if rgba.is_null() || pixel_count <= 0 {
        return false;
    }
    get(handle, |image| {
        let n = pixel_count as usize;
        let fits = n * 4 <= image.data.len();
        if fits {
            unsafe { std::ptr::copy_nonoverlapping(image.data.as_ptr(), rgba, n * 4) };
        }
        fits
    })
    .unwrap_or(false)
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_image_data_get_pixel(
    handle: u64,
    x: c_int,
    y: c_int,
    r: *mut c_float,
    g: *mut c_float,
    b: *mut c_float,
    a: *mut c_float,
) {
    let rgba = get(handle, |image| {
        let offset = image.pixel_offset(x, y)?;
        Some([0, 1, 2, 3].map(|channel| image.data[offset + channel]))
    })
    .flatten()
    .unwrap_or([0; 4]);
    for (out, channel) in [r, g, b, a].into_iter().zip(rgba) {
        if !out.is_null() {
            unsafe { *out = channel as f32 / 255.0 };
        }
    }
}

/// Decode a PNG (or any format `image` recognizes) from an asset path into a fresh
/// ImageData handle. Returns 0 on failure. Pairs with stone_image_data_save_png.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_image_data_load_png(path: *const c_char) -> u64 {
    let Some(path_str) = c_path(path) else {
        return 0;
    };

    let Some((data, width, height)) = crate::image_texture::decode_rgba_image(path_str) else {
        crate::log_once!("IMAGE", "stone_image_data_load_png failed for {}", path_str);
        return 0;
    };

    store_image_data(ImageData {
        width,
        height,
        data,
    })
}

/// rock-ffi stone private
#[no_mangle]
pub unsafe extern "C" fn stone_image_data_load_png_bytes(bytes: *const u8, len: c_int) -> u64 {
    if bytes.is_null() || len <= 0 {
        return 0;
    }
    let bytes = std::slice::from_raw_parts(bytes, len as usize);
    let Ok(image) = image::load_from_memory(bytes) else {
        crate::log_once!(
            "IMAGE",
            "stone_image_data_load_png_bytes failed to decode payload"
        );
        return 0;
    };
    let rgba = image.to_rgba8();
    store_owned_rgba(
        rgba.width() as usize,
        rgba.height() as usize,
        rgba.into_raw(),
    )
    .unwrap_or(0)
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_image_data_save_png(handle: u64, path: *const c_char) -> bool {
    let Some(path_str) = c_path(path) else {
        return false;
    };
    let saved = get(handle, |img_data| {
        match image::save_buffer(
            path_str,
            &img_data.data,
            img_data.width,
            img_data.height,
            image::ColorType::Rgba8,
        ) {
            Ok(()) => true,
            Err(e) => {
                crate::log_once!(
                    "IMAGE",
                    "stone_image_data_save_png failed for {}: {}",
                    path_str,
                    e
                );
                false
            }
        }
    });
    saved.unwrap_or_else(|| {
        crate::log_once!(
            "IMAGE",
            "stone_image_data_save_png: invalid handle {}",
            handle
        );
        false
    })
}

/// In-place upload from an ImageData buffer into an existing GL texture.
/// Both must already match in size — caller is responsible. Used by the
/// pixel editor for live brush feedback without allocating a new texture
/// per stroke.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_image_update_from_data(image_handle: u64, data_handle: u64) -> bool {
    let updated = get(data_handle, |img_data| {
        let result = with_image(image_handle, |img| {
            if img.width != img_data.width || img.height != img_data.height {
                crate::log_once!(
                    "IMAGE",
                    "stone_image_update_from_data: size mismatch image={}x{} data={}x{}",
                    img.width,
                    img.height,
                    img_data.width,
                    img_data.height
                );
                return None;
            }

            Some((img.width, img.height, img.format))
        });
        let (width, height, format) = match result {
            Some(Some(image)) => image,
            Some(None) => return false,
            None => {
                crate::log_once!(
                    "IMAGE",
                    "stone_image_update_from_data: invalid image handle {}",
                    image_handle
                );
                return false;
            }
        };
        let Some(pixels) = format.pack_rgba8(&img_data.data) else {
            crate::log_once!(
                "IMAGE",
                "stone_image_update_from_data: invalid RGBA byte length for {:?}",
                format
            );
            return false;
        };

        let Some((texture, fresh)) =
            crate::image_write_target(image_handle, width, height, &pixels)
        else {
            return false;
        };
        if fresh {
            crate::image_texture::refresh_image_mipmaps(image_handle, texture);
            return true;
        }

        let updated = with_gl(|gl| unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            crate::image_texture::with_rg_unpack_alignment(gl, format, || {
                gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    format.gl_external(),
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(&pixels)),
                );
            });
        })
        .is_some();
        if updated {
            crate::image_texture::refresh_image_mipmaps(image_handle, texture);
        }
        updated
    });
    updated.unwrap_or_else(|| {
        crate::log_once!(
            "IMAGE",
            "stone_image_update_from_data: invalid data handle {}",
            data_handle
        );
        false
    })
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_new_image_from_data(data_handle: u64, filter_mode: i32) -> u64 {
    let gl_filter = match crate::gl_state::gl_filter_for(filter_mode) {
        Some(f) => f,
        None => {
            crate::log_once!(
                "IMAGE",
                "stone_new_image_from_data: unknown filter_mode {}",
                filter_mode
            );
            return 0;
        }
    };
    let image = get(data_handle, |img_data| {
        let size = (img_data.width, img_data.height);
        let texture = with_gl(|gl| unsafe {
            let texture = match create_filtered_image_texture(gl, filter_mode, gl_filter) {
                Ok(t) => t,
                Err(e) => {
                    crate::log_once!(
                        "IMAGE",
                        "stone_new_image_from_data: gl.create_texture() failed for handle {}: {}",
                        data_handle,
                        e
                    );
                    return None;
                }
            };
            define_bound_image_texture(gl, size, ImageFormat::Rgba8, &img_data.data);
            if filter_mode == 2 {
                gl.generate_mipmap(glow::TEXTURE_2D);
            }

            Some(texture)
        });

        match texture {
            Some(Some(tex)) => {
                let id = crate::handles::mint(Class::Image);
                insert_image(id, tex, size, ImageFormat::Rgba8, filter_mode == 2);
                id
            }
            _ => 0,
        }
    });
    image.unwrap_or_else(|| {
        crate::log_once!(
            "IMAGE",
            "stone_new_image_from_data: invalid handle {}",
            data_handle
        );
        0
    })
}

/// PNG bytes of `width` x `height` RGBA pixels, for tests that need an
/// encoded image without a checked-in file. `pixels` repeats to fill the image.
#[cfg(test)]
pub(crate) fn encode_test_png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    let rgba: Vec<u8> = pixels
        .iter()
        .copied()
        .cycle()
        .take((width * height * 4) as usize)
        .collect();
    let image = image::RgbaImage::from_raw(width, height, rgba).expect("RGBA test image");
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .expect("encode test PNG");
    encoded.into_inner()
}

/// Fill a clipped rectangle without one FFI call per pixel.
/// rock-ffi stone-native
#[no_mangle]
pub extern "C" fn stone_image_data_fill_rect_rgba8(
    handle: u64,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    r: i64,
    g: i64,
    b: i64,
    a: i64,
) -> bool {
    if width < 0 || height < 0 || [r, g, b, a].iter().any(|v| !(0..=255).contains(v)) {
        return false;
    }
    get_mut(handle, |image| {
        let x0 = (x as i64).clamp(0, image.width as i64) as usize;
        let x1 = (x as i64 + width as i64).clamp(0, image.width as i64) as usize;
        let y0 = (y as i64).clamp(0, image.height as i64) as usize;
        let y1 = (y as i64 + height as i64).clamp(0, image.height as i64) as usize;
        let rgba = [r as u8, g as u8, b as u8, a as u8];
        for row in y0..y1 {
            for pixel in image.data
                [(row * image.width as usize + x0) * 4..(row * image.width as usize + x1) * 4]
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
            {
                pixel.copy_from_slice(&rgba);
            }
        }
        true
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents(handle: u64) -> Option<(u32, u32, Vec<u8>)> {
        get(handle, |image| {
            (image.width, image.height, image.data.clone())
        })
    }

    #[test]
    fn rgba_pixels_enter_and_leave_the_image_data_registry() {
        let before = stone_get_image_data_count();
        assert_eq!(store_owned_rgba(0, 1, vec![]), None);
        assert_eq!(store_owned_rgba(2, 2, vec![0; 15]), None);
        assert_eq!(
            stone_get_image_data_count(),
            before,
            "an invalid shape registers nothing"
        );

        let pixels = vec![255, 0, 0, 255, 0, 128, 255, 64];
        let owned = store_owned_rgba(2, 1, pixels.clone()).expect("valid RGBA image");
        let encoded = encode_test_png(2, 1, &pixels);
        let decoded =
            unsafe { stone_image_data_load_png_bytes(encoded.as_ptr(), encoded.len() as c_int) };
        assert_ne!(decoded, 0, "memory PNG decode returns an ImageData handle");
        assert_eq!(stone_get_image_data_count(), before + 2);
        for handle in [owned, decoded] {
            assert_eq!(contents(handle), Some((2, 1, pixels.clone())));
            stone_destroy_image_data(handle);
        }
        assert_eq!(stone_get_image_data_count(), before);
    }

    #[test]
    fn png_load_reads_embedded_assets() {
        let path = "embedded-only/image-data-test.png";
        let c_path = std::ffi::CString::new(path).unwrap();
        let bytes = encode_test_png(32, 32, &[255, 128, 0, 255]);
        crate::asset_registry::stone_register_asset(c_path.as_ptr(), bytes.as_ptr(), bytes.len());

        let handle = stone_image_data_load_png(c_path.as_ptr());
        assert_ne!(handle, 0, "PNG load must use the embedded asset registry");
        assert_eq!(
            contents(handle),
            Some((32, 32, [255, 128, 0, 255].repeat(32 * 32)))
        );
        stone_destroy_image_data(handle);
    }
}
