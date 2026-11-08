//! Background image decode.
//!
//! Boot loads 211 images in mag and 89% of that wall time is CPU-side PNG
//! decode, not GL upload. File reads and decode can run on workers; uploads
//! must stay on the GL thread. `stone_load_image` probes dimensions immediately
//! so the caller gets a valid handle; prefetched payloads are read on the pool,
//! while Required loads hand their already-read bytes to it. The decoded
//! pixels come back through `drain_with` for upload at the next flush point.
//! The requested storage format is applied after any declared reduction.

use std::io::Cursor;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, OnceLock};

pub struct Decoded {
    pub id: u64,
    /// `None` when decode failed; the reason is already logged. Failures still
    /// come back as a job so a drain counting outstanding work cannot hang.
    ///
    /// These are the pixels as they will be STORED, which is `capped_dims` of
    /// `width` x `height` -- the reduction runs on the pool thread that decoded
    /// them, not on the GL thread that uploads them.
    pub pixels: Option<Vec<u8>>,
    /// The image's LOGICAL size, always the file's own, cap or no cap.
    pub width: u32,
    pub height: u32,
    /// What the pool actually stored, which is `storage_dims` of `width` x
    /// `height`. Carried rather than recomputed on the GL thread because only
    /// the submitting load knows whether the image declared exact texels.
    pub stored: (u32, u32),
    pub(crate) format: crate::image_texture::ImageFormat,
}

struct Queue {
    tx: Sender<Decoded>,
    rx: Mutex<Receiver<Decoded>>,
    /// Jobs submitted but not yet received. Only this count says when a drain
    /// is complete -- the channel being momentarily empty does not.
    outstanding: Mutex<usize>,
}

fn queue() -> &'static Queue {
    static Q: OnceLock<Queue> = OnceLock::new();
    Q.get_or_init(|| {
        let (tx, rx) = channel();
        Queue {
            tx,
            rx: Mutex::new(rx),
            outstanding: Mutex::new(0),
        }
    })
}

/// `STONE_ASSET_STUB=1` loads images HEADERS-ONLY: dimensions come from the
/// container header and the texture is defined as ONE WHITE TEXEL, but the
/// file's pixels are never decoded or uploaded. Everything that asks an image
/// its size keeps working -- which is most game code, since sheet/frame math is
/// all derived from `image_dims` -- and sampling reads white.
///
/// The sim runner turns this on for EVERY sim child, rendering included: a sim
/// asserts layout, silhouettes and gameplay, not texel colour. One whose
/// assertion genuinely reads the file's own texels declares
/// `@sim_env("STONE_ASSET_STUB", "0")`, which turns it off for that process.
pub fn stub_images() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("STONE_ASSET_STUB").as_deref() == Ok("1"))
}

/// `STONE_IMAGE_MAX_SIDE=<n>` caps the GL STORAGE of every image loaded from an
/// asset at `n` texels on its longer side. The image's LOGICAL size is
/// untouched: `stone_get_image_size` still answers the file's own dimensions, so
/// every sheet frame, pixel rect and UV derived from them is unchanged and only
/// sampling resolution drops. 0 or unset is no cap.
///
/// The sim runner publishes 256 to every child. A rendering mag worker holds
/// 3.2 GB of RGBA8 texture pixels, which is what stops a second suite from
/// fitting in 36 GB of unified memory; a sim asserts layout
/// and silhouettes, not texel detail. Production boots (`rock run`, `rock
/// frame`, `rock perf`, `rock stone`) publish nothing and keep full resolution.
/// A sim that genuinely needs the file's own texels declares
/// `@sim_env("STONE_IMAGE_MAX_SIDE", "0")`.
pub fn max_side() -> u32 {
    static N: OnceLock<u32> = OnceLock::new();
    *N.get_or_init(|| {
        let Ok(raw) = std::env::var("STONE_IMAGE_MAX_SIDE") else {
            return 0;
        };
        if raw.trim().is_empty() {
            return 0;
        }
        match raw.trim().parse::<u32>() {
            Ok(n) => n,
            Err(e) => {
                crate::log_once!(
                    "IMAGE",
                    "STONE_IMAGE_MAX_SIDE={:?} is not a texel count: {}; loading at full size",
                    raw,
                    e
                );
                0
            }
        }
    })
}

/// GL storage dimensions for a `width` x `height` image under the ambient cap.
pub fn capped_dims(width: u32, height: u32) -> (u32, u32) {
    capped_dims_for(max_side(), width, height)
}

/// GL storage dimensions for an image that may have declared its texels exact.
///
/// `exact_texels` is a load-site declaration that the file's texels are read
/// individually rather than sampled as colour -- a lookup table, an index
/// structure, anything where texel N means something on its own. Averaging
/// those is not a loss of detail, it is a different value, so the cap is
/// withheld entirely rather than softened. mag's relight caster bakes are the
/// standing case: they pack a bounding-volume hierarchy whose per-node escape
/// index, once averaged, sends the traversal back to a node it already visited
/// and hangs the GPU.
pub fn storage_dims(exact_texels: bool, width: u32, height: u32) -> (u32, u32) {
    if exact_texels {
        (width, height)
    } else {
        capped_dims(width, height)
    }
}

/// `store_at_cap` for an image that may have declared its texels exact. `Some`
/// with the pixels unchanged when it did.
pub fn store_at_storage(
    exact_texels: bool,
    rgba: Vec<u8>,
    width: u32,
    height: u32,
) -> Option<Vec<u8>> {
    if exact_texels {
        Some(rgba)
    } else {
        store_at_cap(rgba, width, height)
    }
}

/// Aspect-preserving reduction of `width` x `height` so its longer side is
/// `cap`, rounded to nearest and never below one texel: 4096x1024 -> 256x64,
/// 1000x300 -> 256x77. The input unchanged when `cap` is 0 or already fits.
pub fn capped_dims_for(cap: u32, width: u32, height: u32) -> (u32, u32) {
    let longer = width.max(height);
    if cap == 0 || longer <= cap || width == 0 || height == 0 {
        return (width, height);
    }
    (
        scale_side(width, cap, longer),
        scale_side(height, cap, longer),
    )
}

fn scale_side(side: u32, cap: u32, longer: u32) -> u32 {
    let longer = u64::from(longer);
    let scaled = (u64::from(side) * u64::from(cap) + longer / 2) / longer;
    (scaled as u32).max(1)
}

/// Area-average RGBA8 `rgba` (`width` x `height`) down to `dst_w` x `dst_h`.
///
/// A box filter rather than point sampling because the sprite paths sample with
/// NEAREST as often as LINEAR, so dropping 255 of every 256 texels of a 16x
/// reduction would turn a silhouette into noise. Straight per-channel mean
/// including alpha: a transparent source texel's colour bleeds into the average,
/// which shows on a sprite edge and is the accepted cost of a sim-only cap.
///
/// `None` on input this filter cannot describe -- a length that is not the
/// stated size, a zero destination side, or an enlargement. Each is a caller
/// bug, so it is logged and the caller keeps the pixels it has rather than
/// leaving a texture with no storage defined.
pub fn box_downsample(
    rgba: &[u8],
    width: u32,
    height: u32,
    dst_w: u32,
    dst_h: u32,
) -> Option<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    let (dw, dh) = (dst_w as usize, dst_h as usize);
    if dw == 0 || dh == 0 || dw > w || dh > h || rgba.len() != w * h * 4 {
        crate::log_once!(
            "IMAGE",
            "box_downsample: cannot reduce {}x{} ({} bytes) to {}x{}",
            width,
            height,
            rgba.len(),
            dst_w,
            dst_h
        );
        return None;
    }
    let mut out = vec![0u8; dw * dh * 4];
    for dy in 0..dh {
        let y0 = dy * h / dh;
        let y1 = (((dy + 1) * h) / dh).max(y0 + 1);
        for dx in 0..dw {
            let x0 = dx * w / dw;
            let x1 = (((dx + 1) * w) / dw).max(x0 + 1);
            let mut sum = [0u32; 4];
            for y in y0..y1 {
                let row = y * w * 4;
                for x in x0..x1 {
                    let i = row + x * 4;
                    sum[0] += u32::from(rgba[i]);
                    sum[1] += u32::from(rgba[i + 1]);
                    sum[2] += u32::from(rgba[i + 2]);
                    sum[3] += u32::from(rgba[i + 3]);
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (dy * dw + dx) * 4;
            for c in 0..4 {
                out[o + c] = ((sum[c] + n / 2) / n) as u8;
            }
        }
    }
    Some(out)
}

/// Reduce freshly decoded pixels to their GL storage size, or hand them back
/// unchanged when nothing is capped.
///
/// `None` only for a reduction `box_downsample` refused, which is a caller bug
/// and already logged. It is reported as a failed decode rather than passed on,
/// because the storage size is a pure function of the logical size: pixels that
/// do not match it would be uploaded by a `tex_image_2d` reading past them.
pub fn store_at_cap(rgba: Vec<u8>, width: u32, height: u32) -> Option<Vec<u8>> {
    let (sw, sh) = capped_dims(width, height);
    if (sw, sh) == (width, height) {
        return Some(rgba);
    }
    box_downsample(&rgba, width, height, sw, sh)
}

/// Read `width`/`height` from the container header without decoding pixels.
/// Format-generic (png/bmp/jpeg) so it cannot drift from what `image` accepts.
pub fn probe_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Decode compressed bytes to RGBA8. `path` is for the failure log only.
pub fn decode_rgba_bytes(data: &[u8], path: &str) -> Option<(Vec<u8>, u32, u32)> {
    let img = match image::load_from_memory(data) {
        Ok(img) => img.into_rgba8(),
        Err(e) => {
            crate::log_once!("IMAGE", "Failed to decode image {}: {}", path, e);
            return None;
        }
    };
    let (width, height) = (img.width(), img.height());
    Some((img.into_raw(), width, height))
}

pub(crate) enum DecodeSource {
    Bytes(Vec<u8>),
    // Dimensions admitted from the header before the main thread minted a handle.
    Asset { dimensions: (u32, u32) },
}

/// Queue asset reading and decode on the rayon pool under `id`. Non-blocking.
pub(crate) fn submit(
    id: u64,
    path: String,
    source: DecodeSource,
    exact_texels: bool,
    format: crate::image_texture::ImageFormat,
) {
    let q = queue();
    *q.outstanding.lock().expect("outstanding poisoned") += 1;
    let tx = q.tx.clone();
    rayon::spawn(move || {
        let (data, expected) = match source {
            DecodeSource::Bytes(data) => (Some(data), None),
            DecodeSource::Asset { dimensions } => {
                let data = crate::asset_registry::resolve_asset(&path);
                if data.is_none() {
                    crate::log_once!("IMAGE", "Failed to read prefetched image {}", path);
                }
                (data, Some(dimensions))
            }
        };
        let pixels = data.and_then(|data| decode_rgba_bytes(&data, &path));
        let decoded = match pixels {
            Some((_, width, height)) if expected.is_some_and(|size| size != (width, height)) => {
                crate::log_once!("IMAGE", "prefetched image {} changed dimensions after header admission: expected {:?}, got {}x{}",
                    path, expected, width, height);
                Decoded {
                    id,
                    pixels: None,
                    width: 0,
                    height: 0,
                    stored: (0, 0),
                    format,
                }
            }
            // The cap reduces here, on the pool thread, so the GL thread never
            // pays a pass over the full-size pixels -- mag's bank is 3.2 GB.
            Some((rgba, width, height)) => {
                let stored = storage_dims(exact_texels, width, height);
                let pixels = store_at_storage(exact_texels, rgba, width, height)
                    .and_then(|stored| format.pack_rgba8_owned(stored));
                if pixels.is_none() {
                    crate::log_once!(
                        "IMAGE",
                        "decoded image {} could not be packed as {:?}",
                        path,
                        format
                    );
                }
                Decoded {
                    id,
                    pixels,
                    width,
                    height,
                    stored,
                    format,
                }
            }
            None => Decoded {
                id,
                pixels: None,
                width: 0,
                height: 0,
                stored: (0, 0),
                format,
            },
        };
        // The receiver is a process-lifetime static, so a send failure is not
        // reachable; surface it rather than silently losing an upload.
        if let Err(e) = tx.send(decoded) {
            crate::log_once!("IMAGE", "decode result for {} lost: {}", path, e);
        }
    });
}

/// Take whatever has finished decoding without blocking. Lets the caller upload
/// results while later decodes are still running, instead of batching every
/// buffer to the end where they have all fallen out of cache.
pub fn try_drain() -> Vec<Decoded> {
    let q = queue();
    let mut n = q.outstanding.lock().expect("outstanding poisoned");
    if *n == 0 {
        return Vec::new();
    }
    let rx = q.rx.lock().expect("rx poisoned");
    let mut out = Vec::new();
    while *n > 0 {
        match rx.try_recv() {
            Ok(d) => {
                *n -= 1;
                out.push(d);
            }
            Err(_) => break,
        }
    }
    out
}

pub fn outstanding_count() -> usize {
    *queue().outstanding.lock().expect("outstanding poisoned")
}

/// Block until every submitted job has come back, handing each to `each` as
/// it is received so the caller can upload and drop it before the next
/// arrives. Returns the microseconds spent blocked in receive, excluding
/// `each`. Non-blocking when nothing is outstanding.
pub fn drain_with(mut each: impl FnMut(Decoded)) -> u64 {
    let q = queue();
    let pending = {
        let mut n = q.outstanding.lock().expect("outstanding poisoned");
        std::mem::replace(&mut *n, 0)
    };
    if pending == 0 {
        return 0;
    }
    let rx = q.rx.lock().expect("rx poisoned");
    let mut waited = std::time::Duration::ZERO;
    for received in 0..pending {
        let start = std::time::Instant::now();
        let next = rx.recv();
        waited += start.elapsed();
        match next {
            Ok(d) => each(d),
            // Unreachable while the static sender lives; stop rather than spin.
            Err(e) => {
                crate::log_once!(
                    "IMAGE",
                    "decode queue closed with {} job(s) pending: {}",
                    pending - received,
                    e
                );
                break;
            }
        }
    }
    waited.as_micros() as u64
}

/// Block for the next completion, whichever request it belongs to. None when
/// nothing is outstanding.
pub fn recv_one() -> Option<Decoded> {
    let q = queue();
    let mut n = q.outstanding.lock().expect("outstanding poisoned");
    if *n == 0 {
        return None;
    }
    match q.rx.lock().expect("rx poisoned").recv() {
        Ok(d) => {
            *n -= 1;
            Some(d)
        }
        // Unreachable while the static sender lives.
        Err(e) => {
            crate::log_once!(
                "IMAGE",
                "decode queue closed with {} job(s) pending: {}",
                *n,
                e
            );
            None
        }
    }
}

/// Wait for one completion, never for the whole queue. A missing request is an error.
pub fn wait_one() -> Result<Decoded, String> {
    let q = queue();
    let mut n = q.outstanding.lock().expect("outstanding poisoned");
    if *n == 0 {
        return Err("no worker can complete the scheduled image request".into());
    }
    #[cfg(test)]
    if let Some(notify) = TEST_WAIT_NOTICE.lock().unwrap().take() {
        notify.send(()).expect("decode test receiver");
    }
    let result =
        q.rx.lock()
            .expect("rx poisoned")
            .recv_timeout(std::time::Duration::from_secs(60))
            .map_err(|e| format!("waiting for scheduled image decode: {e}"))?;
    *n -= 1;
    Ok(result)
}

#[cfg(test)]
static TEST_WAIT_NOTICE: Mutex<Option<Sender<()>>> = Mutex::new(None);

#[cfg(test)]
pub(crate) fn test_completions(count: usize) -> (Sender<Decoded>, Receiver<()>) {
    *queue().outstanding.lock().unwrap() += count;
    let (tx, rx) = channel();
    *TEST_WAIT_NOTICE.lock().unwrap() = Some(tx);
    (queue().tx.clone(), rx)
}

#[cfg(test)]
mod cap_tests {
    use super::{box_downsample, capped_dims_for, decode_rgba_bytes, probe_dimensions};

    /// A 4x4 split into four 2x2 blocks, each block a single constant so the
    /// expected mean needs no arithmetic. Channel c of every texel is set to
    /// `block * 16 + c` so a transposed or misindexed block is visible.
    fn quad_4x4() -> Vec<u8> {
        let mut rgba = vec![0u8; 4 * 4 * 4];
        for y in 0..4usize {
            for x in 0..4usize {
                let block = (y / 2) * 2 + (x / 2);
                for c in 0..4usize {
                    rgba[(y * 4 + x) * 4 + c] = (block * 16 + c) as u8;
                }
            }
        }
        rgba
    }

    #[test]
    fn box_downsample_averages_each_source_block() {
        let out = box_downsample(&quad_4x4(), 4, 4, 2, 2).expect("4x4 reduces to 2x2");
        assert_eq!(
            out,
            vec![0, 1, 2, 3, 16, 17, 18, 19, 32, 33, 34, 35, 48, 49, 50, 51,],
            "each destination texel is its own 2x2 block's mean"
        );
    }

    #[test]
    fn box_downsample_rounds_a_fractional_mean_to_nearest() {
        // One 2x1 block per destination texel: (0,3) -> 1.5 -> 2, (4,5) -> 4.5 -> 5.
        let rgba = vec![0, 0, 0, 0, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5];
        let out = box_downsample(&rgba, 4, 1, 2, 1).expect("4x1 reduces to 2x1");
        assert_eq!(out, vec![2, 2, 2, 2, 5, 5, 5, 5]);
    }

    /// 5 -> 2 is 2 source texels for the first destination and 3 for the second:
    /// the block for destination x spans `[x*w/dw, (x+1)*w/dw)`, so a ratio that
    /// is not an integer still covers every source texel exactly once.
    #[test]
    fn box_downsample_covers_every_texel_at_a_non_integer_ratio() {
        let rgba = vec![
            10, 10, 10, 10, 20, 20, 20, 20, 30, 30, 30, 30, 60, 60, 60, 60, 90, 90, 90, 90,
        ];
        let out = box_downsample(&rgba, 5, 1, 2, 1).expect("5x1 reduces to 2x1");
        assert_eq!(out, vec![15, 15, 15, 15, 60, 60, 60, 60]);
    }

    #[test]
    fn box_downsample_refuses_a_length_that_is_not_the_stated_size() {
        assert!(
            box_downsample(&[0u8; 12], 4, 4, 2, 2).is_none(),
            "12 bytes is not 4x4 RGBA"
        );
        assert!(
            box_downsample(&quad_4x4(), 4, 4, 0, 2).is_none(),
            "a zero side stores nothing"
        );
        assert!(
            box_downsample(&quad_4x4(), 4, 4, 8, 8).is_none(),
            "this filter only reduces"
        );
    }

    #[test]
    fn no_cap_leaves_the_dimensions_alone() {
        assert_eq!(
            capped_dims_for(0, 4096, 1024),
            (4096, 1024),
            "0 is unset, not a cap of zero"
        );
        assert_eq!(
            capped_dims_for(256, 256, 64),
            (256, 64),
            "already at the cap"
        );
        assert_eq!(
            capped_dims_for(256, 32, 16),
            (32, 16),
            "well inside the cap"
        );
        assert_eq!(
            capped_dims_for(256, 0, 0),
            (0, 0),
            "a sizeless image is not scaled"
        );
    }

    #[test]
    fn cap_scales_the_longer_side_to_the_cap_whichever_axis_it_is() {
        assert_eq!(capped_dims_for(256, 4096, 1024), (256, 64));
        assert_eq!(capped_dims_for(256, 1024, 4096), (64, 256));
        assert_eq!(capped_dims_for(256, 2048, 2048), (256, 256));
    }

    #[test]
    fn cap_rounds_the_shorter_side_to_nearest() {
        // 300 * 256 / 1000 = 76.8.
        assert_eq!(capped_dims_for(256, 1000, 300), (256, 77));
        // 257 * 256 / 1000 = 65.79.
        assert_eq!(capped_dims_for(256, 1000, 257), (256, 66));
    }

    #[test]
    fn cap_never_produces_a_zero_side() {
        // 3 * 256 / 4096 = 0.19, which must still be a storable texel.
        assert_eq!(capped_dims_for(256, 4096, 3), (256, 1));
        assert_eq!(capped_dims_for(1, 4096, 1024), (1, 1));
    }

    /// The decode-failure fixture's corruption: a PNG with one flipped IDAT
    /// payload byte probes its dimensions (headers byte-identical, so the load
    /// defers its decode) but never decodes (the payload fails CRC/inflate).
    /// Deterministic under any encoder layout, unlike a blind truncation.
    #[test]
    fn idat_corruption_probes_but_does_not_decode() {
        use image::ImageEncoder;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                &[0u8, 255, 0, 255].repeat(8),
                4,
                2,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        let mut cut = png.clone();
        let mut off = 8;
        loop {
            let len = u32::from_be_bytes(cut[off..off + 4].try_into().unwrap()) as usize;
            if &cut[off + 4..off + 8] == b"IDAT" {
                assert!(len > 0, "empty IDAT");
                let at = off + 8 + len / 2;
                cut[at] = cut[at].wrapping_add(1);
                break;
            }
            off += 12 + len;
            assert!(off + 8 <= cut.len(), "no IDAT in encoded png");
        }
        assert_eq!(
            probe_dimensions(&cut),
            Some((4, 2)),
            "the header survives corruption"
        );
        assert!(
            decode_rgba_bytes(&cut, "corrupt-test").is_none(),
            "the body does not decode"
        );
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;

    fn write_png(path: &std::path::Path, w: u32, h: u32) {
        let mut image = image::RgbaImage::new(w, h);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgba([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255]);
        }
        image.save(path).expect("write png");
    }

    struct QueueCleanup;

    impl Drop for QueueCleanup {
        fn drop(&mut self) {
            drain_with(drop);
        }
    }

    #[test]
    fn drain_returns_every_submitted_job_with_serial_stored_pixels() {
        if in_isolated_child("drain_returns_every_submitted_job_with_serial_stored_pixels") {
            return;
        }
        let dir = tempfile::Builder::new()
            .prefix("stone_image_decode_drain_")
            .tempdir()
            .expect("mkdir");
        let _cleanup = QueueCleanup;
        let sizes = [(16u32, 16u32), (33, 17), (64, 8), (7, 71), (128, 32)];
        let mut expected = Vec::new();
        for (i, (w, h)) in sizes.iter().enumerate() {
            let path = dir.path().join(format!("drain_{i}.png"));
            write_png(&path, *w, *h);
            let path = path.to_string_lossy().to_string();
            let bytes = std::fs::read(&path).expect("read");
            let (rgba, width, height) = decode_rgba_bytes(&bytes, &path).expect("serial decode");
            let stored = storage_dims(false, width, height);
            let pixels = store_at_storage(false, rgba, width, height).expect("store serial decode");
            expected.push((i as u64, pixels, (width, height), stored));
            submit(
                i as u64,
                path,
                if i % 2 == 0 {
                    DecodeSource::Bytes(bytes)
                } else {
                    DecodeSource::Asset {
                        dimensions: (*w, *h),
                    }
                },
                false,
                crate::image_texture::ImageFormat::Rgba8,
            );
        }

        let mut got = Vec::new();
        drain_with(|decoded| got.push(decoded));
        assert_eq!(
            got.len(),
            sizes.len(),
            "drain must return every outstanding job"
        );
        got.sort_by_key(|decoded| decoded.id);

        for ((id, want, dimensions, stored), decoded) in expected.iter().zip(got.iter()) {
            assert_eq!(decoded.id, *id);
            assert_eq!((decoded.width, decoded.height), *dimensions);
            assert_eq!(decoded.stored, *stored);
            assert_eq!(
                decoded.pixels.as_ref(),
                Some(want),
                "parallel decode of job {id} must match serial stored pixels"
            );
        }

        let mut remaining = Vec::new();
        drain_with(|decoded| remaining.push(decoded));
        assert!(remaining.is_empty(), "a second drain has nothing left");

        submit(
            9001,
            "corrupt.png".to_string(),
            DecodeSource::Bytes(b"garbage".to_vec()),
            false,
            crate::image_texture::ImageFormat::Rgba8,
        );
        let mut got = Vec::new();
        drain_with(|decoded| got.push(decoded));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, 9001);
        assert!(
            got[0].pixels.is_none(),
            "a corrupt payload decodes to None, not a buffer"
        );

        let changed = dir.path().join("changed.png");
        write_png(&changed, 7, 5);
        submit(
            9002,
            changed.to_string_lossy().into_owned(),
            DecodeSource::Asset { dimensions: (4, 2) },
            true,
            crate::image_texture::ImageFormat::Rgba8,
        );
        submit(
            9003,
            dir.path()
                .join("missing.png")
                .to_string_lossy()
                .into_owned(),
            DecodeSource::Asset { dimensions: (4, 2) },
            true,
            crate::image_texture::ImageFormat::Rgba8,
        );
        let mut failures = Vec::new();
        drain_with(|decoded| failures.push(decoded));
        failures.sort_by_key(|decoded| decoded.id);
        assert_eq!(
            failures
                .iter()
                .map(|decoded| decoded.id)
                .collect::<Vec<_>>(),
            [9002, 9003]
        );
        assert!(
            failures.iter().all(|decoded| decoded.pixels.is_none()),
            "missing or changed assets still return one failed completion per admitted handle"
        );
        assert_eq!(outstanding_count(), 0);
    }

    fn in_isolated_child(name: &str) -> bool {
        const CHILD: &str = "STONE_IMAGE_DECODE_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return false;
        }
        let test = format!("image_decode::queue_tests::{name}");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([test.as_str(), "--exact", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .expect("run isolated decode fixture");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
            "the child must select {name}"
        );
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }
}
