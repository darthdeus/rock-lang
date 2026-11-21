//! Image decoding runs on workers; uploads must stay on the GL thread.

use std::io::Cursor;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, OnceLock};

use crate::image_texture::ImageFormat;

pub struct Decoded {
    pub id: u64,
    pub(crate) pixels: Option<PackedPixels>,
}

pub(crate) struct PackedPixels {
    pub(crate) logical: (u32, u32),
    pub(crate) stored: (u32, u32),
    pub(crate) format: ImageFormat,
    pub(crate) data: Vec<u8>,
    pub(crate) offset: usize,
}

impl PackedPixels {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.data[self.offset..]
    }
}

pub(crate) fn prepare_pixels(
    data: Vec<u8>,
    path: &str,
    format: ImageFormat,
    exact_texels: bool,
    expected: Option<(u32, u32)>,
) -> Option<PackedPixels> {
    let (logical, pixels, offset) = if let Some(block_format) = format.blocks() {
        let Some((size, blocks)) = block_format.blocks(&data) else {
            crate::log_once!("IMAGE", "invalid DDS block payload: {}", path);
            return None;
        };
        let offset = data.len() - blocks.len();
        (size, data, offset)
    } else {
        let (rgba, width, height) = decode_rgba_bytes(&data, path)?;
        drop(data);
        ((width, height), rgba, 0)
    };
    if expected.is_some_and(|size| size != logical) {
        crate::log_once!(
            "IMAGE",
            "prefetched image {} changed dimensions after header admission: expected {:?}, got {}x{}",
            path, expected, logical.0, logical.1
        );
        return None;
    }
    let (w, h) = logical;
    let stored = storage_dims(exact_texels, w, h);
    let data = if format.blocks().is_some() {
        pixels
    } else if let Some(packed) =
        pack_at_storage(pixels, logical, stored).and_then(|bytes| format.pack_rgba8_owned(bytes))
    {
        packed
    } else {
        crate::log_once!(
            "IMAGE",
            "decoded image {} could not be packed as {:?}",
            path,
            format
        );
        return None;
    };
    Some(PackedPixels {
        logical,
        stored,
        format,
        data,
        offset,
    })
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

/// Stub images retain logical dimensions but sample as one white texel.
/// Simulations requiring source texels can disable `STONE_ASSET_STUB`.
pub fn stub_images() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("STONE_ASSET_STUB").as_deref() == Ok("1"))
}

/// `STONE_IMAGE_MAX_SIDE` caps texture storage, not logical dimensions.
/// Zero or unset leaves storage uncapped.
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

/// Exact texels cannot be averaged: they may encode indices or other data.
pub fn storage_dims(exact_texels: bool, width: u32, height: u32) -> (u32, u32) {
    if exact_texels {
        (width, height)
    } else {
        capped_dims_for(max_side(), width, height)
    }
}

/// Refuse a failed reduction: uploading pixels smaller than the declared storage
/// dimensions would read past the buffer.
pub fn store_at_storage(
    exact_texels: bool,
    rgba: Vec<u8>,
    width: u32,
    height: u32,
) -> Option<Vec<u8>> {
    pack_at_storage(
        rgba,
        (width, height),
        storage_dims(exact_texels, width, height),
    )
}

fn pack_at_storage(rgba: Vec<u8>, logical: (u32, u32), stored: (u32, u32)) -> Option<Vec<u8>> {
    if logical == stored {
        Some(rgba)
    } else {
        box_downsample(&rgba, logical.0, logical.1, stored.0, stored.1)
    }
}

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

/// Read `width`/`height` from the container header without decoding pixels.
/// Format-generic (png/bmp/jpeg) so it cannot drift from what `image` accepts.
pub fn probe_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.starts_with(b"DDS ") {
        return stone_texture::dimensions(data);
    }
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
    Asset { dimensions: (u32, u32) },
}

pub(crate) fn fixed_perf_work() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("PERF_MODE").as_deref() == Ok("1")
            && std::env::var("PERF_FIXED_WORK").as_deref() == Ok("1")
    })
}

/// Queue a decode under `id`. Fixed-work perf runs complete it before returning.
pub(crate) fn submit(
    id: u64,
    path: String,
    source: DecodeSource,
    exact_texels: bool,
    format: ImageFormat,
) {
    let q = queue();
    *q.outstanding.lock().expect("outstanding poisoned") += 1;
    let tx = q.tx.clone();
    let decode = move || {
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
        let pixels =
            data.and_then(|data| prepare_pixels(data, &path, format, exact_texels, expected));
        let decoded = Decoded { id, pixels };
        if let Err(e) = tx.send(decoded) {
            crate::log_once!("IMAGE", "decode result for {} lost: {}", path, e);
        }
    };
    if fixed_perf_work() {
        decode();
    } else {
        rayon::spawn(decode);
    }
}

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

#[cfg(test)]
pub(crate) fn outstanding_count() -> usize {
    *queue().outstanding.lock().expect("outstanding poisoned")
}

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

#[cfg(test)]
pub(crate) fn test_completions(count: usize) -> Sender<Decoded> {
    *queue().outstanding.lock().unwrap() += count;
    queue().tx.clone()
}

#[cfg(test)]
mod cap_tests {
    #[test]
    fn compressed_storage_contains_the_blocks_without_the_container_header() {
        use super::{prepare_pixels, ImageFormat};

        let size = (5, 3);
        for (format, header, bytes) in [
            (ImageFormat::Bc1, stone_texture::bc1_header(5, 3), 16),
            (ImageFormat::Bc5, stone_texture::bc5_header(5, 3), 32),
        ] {
            let mut source = header.unwrap().to_vec();
            let payload: Vec<_> = (0..bytes).collect();
            source.extend_from_slice(&payload);
            let decoded =
                prepare_pixels(source.clone(), "fixture.dds", format, true, Some(size)).unwrap();
            assert_eq!(decoded.logical, size);
            assert_eq!(decoded.stored, size);
            assert_eq!(decoded.bytes(), payload);
            assert!(prepare_pixels(source, "fixture.dds", format, true, Some((4, 3))).is_none());
        }
    }
    use super::{box_downsample, capped_dims_for, decode_rgba_bytes, probe_dimensions};

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
    fn box_downsample_rounds_and_covers_every_texel() {
        type Case = (&'static str, &'static [u8], u32, u32, &'static [u8]);
        let cases: [Case; 2] = [
            (
                "a fractional mean rounds to nearest",
                &[0, 0, 0, 0, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5],
                4,
                2,
                &[2, 2, 2, 2, 5, 5, 5, 5],
            ),
            (
                "a non-integer ratio covers every texel",
                &[
                    10, 10, 10, 10, 20, 20, 20, 20, 30, 30, 30, 30, 60, 60, 60, 60, 90, 90, 90, 90,
                ],
                5,
                2,
                &[15, 15, 15, 15, 60, 60, 60, 60],
            ),
        ];
        for (name, rgba, width, out_width, expected) in cases {
            let out = box_downsample(rgba, width, 1, out_width, 1).expect(name);
            assert_eq!(out, expected, "{name}");
        }
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
    fn capped_dims_scale_the_longer_side_and_round_the_shorter() {
        let cases = [
            (
                "0 is unset, not a cap of zero",
                (0, 4096, 1024),
                (4096, 1024),
            ),
            ("already at the cap", (256, 256, 64), (256, 64)),
            ("well inside the cap", (256, 32, 16), (32, 16)),
            ("a sizeless image is not scaled", (256, 0, 0), (0, 0)),
            ("wide", (256, 4096, 1024), (256, 64)),
            ("tall", (256, 1024, 4096), (64, 256)),
            ("square", (256, 2048, 2048), (256, 256)),
            ("shorter side rounds down", (256, 1000, 300), (256, 77)),
            ("shorter side rounds up", (256, 1000, 257), (256, 66)),
            ("never a zero side", (256, 4096, 3), (256, 1)),
            ("a cap of one", (1, 4096, 1024), (1, 1)),
        ];
        for (name, (cap, width, height), expected) in cases {
            assert_eq!(capped_dims_for(cap, width, height), expected, "{name}");
        }
    }

    /// The decode-failure fixture's corruption: a PNG with one flipped IDAT
    /// payload byte probes its dimensions (headers byte-identical, so the load
    /// defers its decode) but never decodes (the payload fails CRC/inflate).
    /// Deterministic under any encoder layout, unlike a blind truncation.
    #[test]
    fn idat_corruption_probes_but_does_not_decode() {
        let mut cut = crate::image_data::encode_test_png(4, 2, &[0, 255, 0, 255]);
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
        use crate::test_support::{assert_child_passes, child_test};
        const TEST: &str =
            "image_decode::queue_tests::drain_returns_every_submitted_job_with_serial_stored_pixels";
        const CHILD: &str = "STONE_IMAGE_DECODE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            for fixed in ["0", "1"] {
                assert_child_passes(
                    child_test(TEST, CHILD, "1")
                        .env("PERF_MODE", "1")
                        .env("PERF_FIXED_WORK", fixed),
                    TEST,
                );
            }
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

        let mut got = if fixed_perf_work() {
            let completed = try_drain();
            assert!(completed.iter().map(|d| d.id).eq(0..sizes.len() as u64));
            completed
        } else {
            let mut completed = Vec::new();
            drain_with(|decoded| completed.push(decoded));
            completed
        };
        assert_eq!(
            got.len(),
            sizes.len(),
            "drain must return every outstanding job"
        );
        got.sort_by_key(|decoded| decoded.id);

        for ((id, want, dimensions, stored), decoded) in expected.iter().zip(got.iter()) {
            assert_eq!(decoded.id, *id);
            let packed = decoded.pixels.as_ref().unwrap();
            assert_eq!(packed.logical, *dimensions);
            assert_eq!(packed.stored, *stored);
            assert_eq!(
                packed.bytes(),
                want.as_slice(),
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
}
