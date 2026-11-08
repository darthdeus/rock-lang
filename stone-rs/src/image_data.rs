use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_float, c_int, CStr};

use crate::handles::Class;
use crate::{with_gl, LoadedImage, IMAGES};

struct ImageData {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

thread_local! {
    static IMAGE_DATAS: RefCell<HashMap<u64, ImageData>> = RefCell::new(HashMap::new());
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

#[no_mangle]
pub extern "C" fn stone_get_image_data_count() -> c_int {
    IMAGE_DATAS.with(|ids| ids.borrow().len() as c_int)
}

/// Access an ImageData's raw pixels by handle. Returns None if handle is invalid.
pub(crate) fn with_image_data<F, R>(handle: u64, f: F) -> Option<R>
where
    F: FnOnce(u32, u32, &[u8]) -> R,
{
    IMAGE_DATAS.with(|ids| {
        let ids_borrow = ids.borrow();
        let img_data = ids_borrow.get(&handle)?;
        Some(f(img_data.width, img_data.height, &img_data.data))
    })
}

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

#[no_mangle]
pub extern "C" fn stone_image_data_size(
    handle: u64,
    width: *mut c_int,
    height: *mut c_int,
) -> bool {
    let Some((w, h)) =
        IMAGE_DATAS.with(|ids| ids.borrow().get(&handle).map(|img| (img.width, img.height)))
    else {
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

/// Allocate a new ImageData with the same dimensions and a byte-for-byte
/// copy of `src`'s pixel buffer. Returns 0 on invalid handle. Used by
/// the pixel editor for undo snapshots and the future copy/paste pipeline.
#[no_mangle]
pub extern "C" fn stone_image_data_clone(src: u64) -> u64 {
    let snapshot = IMAGE_DATAS.with(|ids| {
        ids.borrow().get(&src).map(|img| ImageData {
            width: img.width,
            height: img.height,
            data: img.data.clone(),
        })
    });
    snapshot.map(store_image_data).unwrap_or(0)
}

/// Overwrite `dst`'s pixel buffer with a copy of `src`'s, in-place. The
/// two handles must refer to ImageDatas of identical dimensions. Returns
/// false on dim mismatch or invalid handle. Pairs with
/// `stone_image_data_clone` for undo: clone before stroke, copy back on
/// undo, no handle churn.
#[no_mangle]
pub extern "C" fn stone_image_data_copy_into(dst: u64, src: u64) -> bool {
    IMAGE_DATAS.with(|ids| {
        let ids = ids.borrow();
        let src_img = match ids.get(&src) {
            Some(img) => img,
            None => return false,
        };
        let dst_img = match ids.get(&dst) {
            Some(img) => img,
            None => return false,
        };
        if src_img.width != dst_img.width || src_img.height != dst_img.height {
            return false;
        }
        let bytes = src_img.data.clone();
        drop(ids);
        IMAGE_DATAS.with(|ids| {
            if let Some(d) = ids.borrow_mut().get_mut(&dst) {
                d.data.copy_from_slice(&bytes);
            }
        });
        true
    })
}

/// Copy the `w` by `h` rectangle of `src` at (`sx`, `sy`) into `dst` at
/// (`dx`, `dy`), multiplying each channel by its factor (all 1.0 copies the
/// bytes exactly). `src` and `dst` may not be the same handle. Returns false
/// for an unknown handle, an aliased pair or a rectangle outside either image.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn stone_image_data_blit(
    dst: u64,
    src: u64,
    sx: c_int,
    sy: c_int,
    w: c_int,
    h: c_int,
    dx: c_int,
    dy: c_int,
    r: c_float,
    g: c_float,
    b: c_float,
    a: c_float,
) -> bool {
    if dst == src || sx < 0 || sy < 0 || dx < 0 || dy < 0 || w < 0 || h < 0 {
        return false;
    }
    let (sx, sy, w, h, dx, dy) = (
        sx as usize,
        sy as usize,
        w as usize,
        h as usize,
        dx as usize,
        dy as usize,
    );
    IMAGE_DATAS.with(|ids| {
        let mut ids = ids.borrow_mut();
        let Some(source) = ids.get(&src) else {
            return false;
        };
        let (src_w, src_h) = (source.width as usize, source.height as usize);
        if sx + w > src_w || sy + h > src_h {
            return false;
        }
        let rows: Vec<u8> = (0..h)
            .flat_map(|y| {
                let start = ((sy + y) * src_w + sx) * 4;
                source.data[start..start + w * 4].iter().copied()
            })
            .collect();
        let Some(target) = ids.get_mut(&dst) else {
            return false;
        };
        let (dst_w, dst_h) = (target.width as usize, target.height as usize);
        if dx + w > dst_w || dy + h > dst_h {
            return false;
        }
        let factors = [r, g, b, a];
        let exact = factors.iter().all(|f| *f == 1.0);
        for y in 0..h {
            let row = &rows[y * w * 4..(y + 1) * w * 4];
            let start = ((dy + y) * dst_w + dx) * 4;
            let out = &mut target.data[start..start + w * 4];
            if exact {
                out.copy_from_slice(row);
            } else {
                for (i, (o, s)) in out.iter_mut().zip(row).enumerate() {
                    *o = (*s as f32 * factors[i % 4]).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
        true
    })
}

/// Live ImageData handles (`native_ledger`).
pub(crate) fn live_image_data_ids() -> Vec<u64> {
    IMAGE_DATAS.with(|ids| ids.borrow().keys().copied().collect())
}

#[no_mangle]
pub extern "C" fn stone_destroy_image_data(handle: u64) {
    IMAGE_DATAS.with(|ids| {
        if ids.borrow_mut().remove(&handle).is_some() {
            crate::handles::retire(Class::ImageData, handle);
        }
    });
}

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
    IMAGE_DATAS.with(|ids| {
        if let Some(img_data) = ids.borrow_mut().get_mut(&handle) {
            if x < 0 || x >= img_data.width as i32 || y < 0 || y >= img_data.height as i32 {
                return;
            }

            let idx = ((y as usize) * (img_data.width as usize) + (x as usize)) * 4;
            img_data.data[idx] = (r * 255.0) as u8;
            img_data.data[idx + 1] = (g * 255.0) as u8;
            img_data.data[idx + 2] = (b * 255.0) as u8;
            img_data.data[idx + 3] = (a * 255.0) as u8;
        }
    });
}

/// Bulk RGBA upload: copies `pixel_count * 4` bytes from `rgba` into the
/// image data buffer starting at pixel 0. No partial writes — if the
/// pixel_count exceeds image capacity, the call is a no-op.
#[no_mangle]
pub extern "C" fn stone_image_data_fill_rgba(handle: u64, rgba: *const u8, pixel_count: c_int) {
    if rgba.is_null() || pixel_count <= 0 {
        return;
    }
    IMAGE_DATAS.with(|ids| {
        if let Some(img_data) = ids.borrow_mut().get_mut(&handle) {
            let capacity = (img_data.width as usize) * (img_data.height as usize);
            let n = pixel_count as usize;
            if n > capacity {
                return;
            }
            let bytes = n * 4;
            unsafe {
                std::ptr::copy_nonoverlapping(rgba, img_data.data.as_mut_ptr(), bytes);
            }
        }
    });
}

/// Bulk RGBA read, the inverse of `stone_image_data_fill_rgba`: copies
/// `pixel_count * 4` bytes from pixel 0 into `rgba`. Returns false and writes
/// nothing for an unknown handle or a count beyond the image's capacity.
#[no_mangle]
pub extern "C" fn stone_image_data_read_rgba(
    handle: u64,
    rgba: *mut u8,
    pixel_count: c_int,
) -> bool {
    if rgba.is_null() || pixel_count <= 0 {
        return false;
    }
    IMAGE_DATAS.with(|ids| {
        let ids = ids.borrow();
        let Some(img_data) = ids.get(&handle) else {
            return false;
        };
        let n = pixel_count as usize;
        if n > (img_data.width as usize) * (img_data.height as usize) {
            return false;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(img_data.data.as_ptr(), rgba, n * 4);
        }
        true
    })
}

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
    IMAGE_DATAS.with(|ids| {
        if let Some(img_data) = ids.borrow().get(&handle) {
            if x < 0 || x >= img_data.width as i32 || y < 0 || y >= img_data.height as i32 {
                // Out of bounds - set to zero
                unsafe {
                    if !r.is_null() {
                        *r = 0.0;
                    }
                    if !g.is_null() {
                        *g = 0.0;
                    }
                    if !b.is_null() {
                        *b = 0.0;
                    }
                    if !a.is_null() {
                        *a = 0.0;
                    }
                }
                return;
            }

            let idx = ((y as usize) * (img_data.width as usize) + (x as usize)) * 4;
            unsafe {
                if !r.is_null() {
                    *r = img_data.data[idx] as f32 / 255.0;
                }
                if !g.is_null() {
                    *g = img_data.data[idx + 1] as f32 / 255.0;
                }
                if !b.is_null() {
                    *b = img_data.data[idx + 2] as f32 / 255.0;
                }
                if !a.is_null() {
                    *a = img_data.data[idx + 3] as f32 / 255.0;
                }
            }
        } else {
            // Not found - set to zero
            unsafe {
                if !r.is_null() {
                    *r = 0.0;
                }
                if !g.is_null() {
                    *g = 0.0;
                }
                if !b.is_null() {
                    *b = 0.0;
                }
                if !a.is_null() {
                    *a = 0.0;
                }
            }
        }
    });
}

/// Decode a PNG (or any format `image` recognizes) from an asset path into a fresh
/// ImageData handle. Returns 0 on failure. Pairs with stone_image_data_save_png.
#[no_mangle]
pub extern "C" fn stone_image_data_load_png(path: *const c_char) -> u64 {
    if path.is_null() {
        return 0;
    }
    let path_str = unsafe { CStr::from_ptr(path).to_str().unwrap_or("") };
    if path_str.is_empty() {
        return 0;
    }

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

/// Decode PNG bytes without making the caller materialize a persistent file.
/// The returned ImageData owns the decoded RGBA8 buffer until its handle is
/// destroyed.
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

/// Encode an ImageData handle as a PNG and write to `path`. Format is
/// inferred from the extension; PNG is the only one currently exercised
/// by the editor. Returns true on success.
#[no_mangle]
pub extern "C" fn stone_image_data_save_png(handle: u64, path: *const c_char) -> bool {
    if path.is_null() {
        return false;
    }
    let path_str = unsafe { CStr::from_ptr(path).to_str().unwrap_or("") };
    if path_str.is_empty() {
        return false;
    }

    IMAGE_DATAS.with(|ids| {
        let ids_borrow = ids.borrow();
        let img_data = match ids_borrow.get(&handle) {
            Some(d) => d,
            None => {
                crate::log_once!(
                    "IMAGE",
                    "stone_image_data_save_png: invalid handle {}",
                    handle
                );
                return false;
            }
        };

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
    })
}

/// In-place upload from an ImageData buffer into an existing GL texture.
/// Both must already match in size — caller is responsible. Used by the
/// pixel editor for live brush feedback without allocating a new texture
/// per stroke.
#[no_mangle]
pub extern "C" fn stone_image_update_from_data(image_handle: u64, data_handle: u64) -> bool {
    IMAGE_DATAS.with(|ids| {
        let ids_borrow = ids.borrow();
        let img_data = match ids_borrow.get(&data_handle) {
            Some(d) => d,
            None => {
                crate::log_once!(
                    "IMAGE",
                    "stone_image_update_from_data: invalid data handle {}",
                    data_handle
                );
                return false;
            }
        };

        let result = IMAGES.with(|images| {
            let images_borrow = images.borrow();
            let img = match images_borrow.get(&image_handle) {
                Some(i) => i,
                None => {
                    crate::log_once!(
                        "IMAGE",
                        "stone_image_update_from_data: invalid image handle {}",
                        image_handle
                    );
                    return None;
                }
            };

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
        let Some((width, height, format)) = result else {
            return false;
        };
        let Some(pixels) = format.pack_rgba8(&img_data.data) else {
            crate::log_once!(
                "IMAGE",
                "stone_image_update_from_data: invalid RGBA byte length for {:?}",
                format
            );
            return false;
        };

        // Copy-on-write: the first write to a BOOT-owned image is diverted onto
        // a replacement texture so the boundary can hand boot's own object back.
        // A refusal is a refusal -- boot's texture is never the fallback.
        let Some((texture, fresh)) =
            crate::image_write_target(image_handle, width, height, &pixels)
        else {
            return false;
        };
        if fresh {
            crate::image_texture::refresh_image_mipmaps(image_handle, texture);
            // The replacement was created carrying exactly these pixels.
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
    })
}

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
    IMAGE_DATAS.with(|ids| {
        let ids_borrow = ids.borrow();
        let img_data = match ids_borrow.get(&data_handle) {
            Some(d) => d,
            None => {
                crate::log_once!(
                    "IMAGE",
                    "stone_new_image_from_data: invalid handle {}",
                    data_handle
                );
                return 0;
            }
        };

        // Create OpenGL texture
        let texture = with_gl(|gl| unsafe {
            let texture = match gl.create_texture() {
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
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));

            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                crate::gl_state::gl_min_filter_for(filter_mode).unwrap() as i32,
            );
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, gl_filter as i32);
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );

            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                img_data.width as i32,
                img_data.height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&img_data.data)),
            );
            if filter_mode == 2 {
                gl.generate_mipmap(glow::TEXTURE_2D);
            }

            Some(texture)
        });

        match texture {
            Some(Some(tex)) => {
                let id = crate::handles::mint(Class::Image);
                IMAGES.with(|images| {
                    images.borrow_mut().insert(
                        id,
                        LoadedImage {
                            texture: tex,
                            width: img_data.width,
                            height: img_data.height,
                            repeat: false,
                            format: crate::image_texture::ImageFormat::Rgba8,
                            stub: false,
                        },
                    );
                });
                if filter_mode == 2 {
                    crate::image_texture::register_mipmapped_image(id);
                }
                id
            }
            _ => 0,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_rgba_pixels_enter_and_leave_the_image_data_registry() {
        let before = stone_get_image_data_count();
        let pixels = vec![255, 0, 0, 255, 0, 128, 255, 64];
        let handle = store_owned_rgba(2, 1, pixels.clone()).expect("valid RGBA image");

        assert_eq!(stone_get_image_data_count(), before + 1);
        assert_eq!(
            with_image_data(handle, |width, height, bytes| (
                width,
                height,
                bytes.to_vec()
            )),
            Some((2, 1, pixels)),
        );

        stone_destroy_image_data(handle);
        assert_eq!(stone_get_image_data_count(), before);
    }

    #[test]
    fn blit_copies_and_scales_a_rectangle_and_refuses_bad_ones() {
        let src = store_owned_rgba(2, 1, vec![10, 20, 30, 40, 200, 100, 50, 255]).unwrap();
        let dst = stone_create_image_data(3, 2);
        assert!(stone_image_data_blit(
            dst, src, 0, 0, 2, 1, 1, 1, 1.0, 1.0, 1.0, 1.0
        ));
        assert!(stone_image_data_blit(
            dst, src, 1, 0, 1, 1, 0, 0, 0.5, 1.0, 2.0, 1.0
        ));
        let bytes = with_image_data(dst, |_, _, bytes| bytes.to_vec()).unwrap();
        assert_eq!(&bytes[0..4], &[100, 100, 100, 255]);
        assert_eq!(&bytes[16..24], &[10, 20, 30, 40, 200, 100, 50, 255]);
        assert!(!stone_image_data_blit(
            dst, src, 1, 0, 2, 1, 0, 0, 1.0, 1.0, 1.0, 1.0
        ));
        assert!(!stone_image_data_blit(
            dst, src, 0, 0, 2, 1, 2, 0, 1.0, 1.0, 1.0, 1.0
        ));
        assert!(!stone_image_data_blit(
            dst, dst, 0, 0, 1, 1, 1, 0, 1.0, 1.0, 1.0, 1.0
        ));
        stone_destroy_image_data(src);
        stone_destroy_image_data(dst);
    }

    #[test]
    fn owned_rgba_rejects_invalid_shape_without_registering() {
        let before = stone_get_image_data_count();
        assert_eq!(store_owned_rgba(0, 1, vec![]), None);
        assert_eq!(store_owned_rgba(2, 2, vec![0; 15]), None);
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
        let (width, height) = IMAGE_DATAS
            .with(|ids| ids.borrow().get(&handle).map(|img| (img.width, img.height)))
            .expect("loaded image data must be live");
        assert_eq!((width, height), (32, 32));
        stone_destroy_image_data(handle);
    }

    #[test]
    fn png_bytes_load_reads_a_transient_payload() {
        let encoded = encode_test_png(2, 1, &[255, 0, 0, 255, 0, 128, 255, 64]);

        let handle =
            unsafe { stone_image_data_load_png_bytes(encoded.as_ptr(), encoded.len() as c_int) };
        assert_ne!(
            handle, 0,
            "memory PNG decode must return an ImageData handle"
        );
        assert_eq!(
            with_image_data(handle, |width, height, bytes| (
                width,
                height,
                bytes.to_vec()
            )),
            Some((2, 1, vec![255, 0, 0, 255, 0, 128, 255, 64])),
        );
        stone_destroy_image_data(handle);
    }
}

/// Fill a clipped rectangle without one FFI call per pixel.
#[no_mangle]
pub extern "C" fn stone_image_data_fill_rect_rgba8(
    handle: u64,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    r: i32,
    g: i32,
    b: i32,
    a: i32,
) -> bool {
    if width < 0 || height < 0 || [r, g, b, a].iter().any(|v| !(0..=255).contains(v)) {
        return false;
    }
    IMAGE_DATAS.with(|ids| {
        let mut ids = ids.borrow_mut();
        let Some(image) = ids.get_mut(&handle) else {
            return false;
        };
        let x0 = (x as i64).clamp(0, image.width as i64) as usize;
        let x1 = (x as i64 + width as i64).clamp(0, image.width as i64) as usize;
        let y0 = (y as i64).clamp(0, image.height as i64) as usize;
        let y1 = (y as i64 + height as i64).clamp(0, image.height as i64) as usize;
        let rgba = [r as u8, g as u8, b as u8, a as u8];
        for row in y0..y1 {
            for pixel in image.data
                [(row * image.width as usize + x0) * 4..(row * image.width as usize + x1) * 4]
                .chunks_exact_mut(4)
            {
                pixel.copy_from_slice(&rgba);
            }
        }
        true
    })
}
