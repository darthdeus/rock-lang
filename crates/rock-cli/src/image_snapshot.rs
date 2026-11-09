//! PNG decode and the `image_snapshot` diff used by the `rock sim` runner.

use anyhow::{Context, Result, bail};
use std::path::Path;

/// Decode a PNG into an RGBA8 byte buffer (4 bytes per pixel,
/// row-major). Stone screenshots are PNG; baselines are produced by
/// Stone too. Anything outside the formats Stone writes is an error.
pub(crate) fn decode_png_rgba8(path: &Path) -> Result<(u32, u32, Vec<u8>)> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .with_context(|| format!("reading PNG header for {}", path.display()))?;
    let mut buf = vec![
        0;
        reader.output_buffer_size().ok_or_else(|| anyhow::anyhow!(
            "PNG buffer size overflow for {}",
            path.display()
        ))?
    ];
    let info = reader
        .next_frame(&mut buf)
        .with_context(|| format!("decoding PNG frame for {}", path.display()))?;
    let bytes = buf[..info.buffer_size()].to_vec();
    let rgba = match (info.color_type, info.bit_depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => bytes,
        (png::ColorType::Rgb, png::BitDepth::Eight) => {
            // Expand RGB -> RGBA with full alpha.
            let mut out = Vec::with_capacity(bytes.len() / 3 * 4);
            for chunk in bytes.as_chunks::<3>().0 {
                out.extend_from_slice(chunk);
                out.push(255);
            }
            out
        }
        (ct, bd) => bail!(
            "unsupported PNG format in {}: color={:?} depth={:?}",
            path.display(),
            ct,
            bd
        ),
    };
    Ok((info.width, info.height, rgba))
}

/// Max over 32x32-pixel blocks of the per-block mean absolute channel
/// delta, normalized to [0, 1]. 0.0 = no above-floor difference (frames may
/// still differ by 1 LSB per channel), 1.0 = max possible delta in some
/// block. A whole-frame mean let a sprite-sized defect hide behind an
/// unchanged background while the tolerance stayed fixed, so the oracle is
/// the worst region, not the average pixel: a half-tile shift of one tile
/// fails the default `image_snapshot` tol where the mean passed.
/// Partial edge blocks average over their own pixels; no pixel is excluded.
/// Per-channel deltas of 1 or less count as zero: a one-LSB noise budget,
/// allowing small rounding differences between renderers. Deltas above the
/// floor retain their full magnitude.
pub fn image_diff(baseline: &Path, actual: &Path) -> Result<f64> {
    let (bw, bh, b) = decode_png_rgba8(baseline)?;
    let (aw, ah, a) = decode_png_rgba8(actual)?;
    if (bw, bh) != (aw, ah) {
        bail!(
            "image dim mismatch: baseline {}x{} vs actual {}x{}",
            bw,
            bh,
            aw,
            ah
        );
    }
    const BLOCK: u32 = 32;
    let mut worst: f64 = 0.0;
    for by in 0..bh.div_ceil(BLOCK) {
        for bx in 0..bw.div_ceil(BLOCK) {
            let x0 = bx * BLOCK;
            let y0 = by * BLOCK;
            let x1 = (x0 + BLOCK).min(bw);
            let y1 = (y0 + BLOCK).min(bh);
            let mut total: u64 = 0;
            for y in y0..y1 {
                let o = (y * bw + x0) as usize * 4;
                let n = (x1 - x0) as usize * 4;
                for (pb, pa) in b[o..o + n].iter().zip(a[o..o + n].iter()) {
                    let d = (*pb as i32 - *pa as i32).unsigned_abs() as u64;
                    total += if d <= 1 { 0 } else { d };
                }
            }
            let max = (x1 - x0) as u64 * (y1 - y0) as u64 * 4 * 255;
            worst = worst.max(total as f64 / max as f64);
        }
    }
    Ok(worst)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK_PX: u32 = 32;
    // Default image_snapshot tolerance in stone/engine/sim.rock.
    const DEFAULT_TOL: f64 = 0.001;

    fn write_rgba_png(path: &Path, w: u32, h: u32, px: &[u8]) {
        let file = std::fs::File::create(path).expect("create test png");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .expect("png header")
            .write_image_data(px)
            .expect("png pixels");
    }

    /// Control: per-channel deltas of exactly +1/-1 across full and partial
    /// blocks diff exactly zero. The pair is NOT bit-identical (asserted on
    /// the decoded bytes), so 0.0 means "no above-floor difference".
    #[test]
    fn deadband_plus_minus_one_counts_as_zero() {
        // 40x40 spans full 32px blocks plus partial edge blocks in both
        // dims, so the deadband is pinned on both block shapes at once.
        let (w, h) = (40u32, 40u32);
        let mut base = vec![0u8; (w * h * 4) as usize];
        for px in base.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[128, 128, 128, 128]);
        }
        let mut shifted = base.clone();
        for (i, px) in shifted.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = i as u32 % w;
            let y = i as u32 / w;
            px[0] += 1; // R always +1
            px[1] -= 1; // G always -1
            if (x + y).is_multiple_of(2) {
                px[2] += 1; // B mixed sign by checkerboard
            } else {
                px[2] -= 1;
            }
            match i % 3 {
                0 => {} // A keeps some channels at 0
                1 => px[3] += 1,
                _ => px[3] -= 1,
            }
        }
        let dir = tempfile::Builder::new()
            .prefix("rock-image-oracle-")
            .tempdir()
            .expect("tempdir");
        let base_path = dir.path().join("base.png");
        let shifted_path = dir.path().join("shifted.png");
        write_rgba_png(&base_path, w, h, &base);
        write_rgba_png(&shifted_path, w, h, &shifted);
        let (_, _, back) = decode_png_rgba8(&shifted_path).expect("decode shifted");
        assert_ne!(back, base, "pair must differ, or the zero below is vacuous");
        let diff = image_diff(&base_path, &shifted_path).expect("diff");
        assert_eq!(diff, 0.0, "every channel within +-1 must count as zero");
    }

    /// Control: per-channel deltas of exactly +2/-2 keep their full
    /// magnitude. A uniform +-2 shift over exactly one full block diffs
    /// 2/255, above the default snapshot tol: a wider deadband would read
    /// 0.0 and a floor subtraction or halving would read 1/255.
    #[test]
    fn deadband_plus_minus_two_keeps_full_magnitude() {
        // Exactly one full 32px block: magnitude without partial-block
        // interaction (that rule has its own control below).
        let (w, h) = (BLOCK_PX, BLOCK_PX);
        let mut base = vec![0u8; (w * h * 4) as usize];
        for px in base.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[128, 128, 128, 128]);
        }
        let mut shifted = base.clone();
        for px in shifted.as_chunks_mut::<4>().0 {
            px[0] += 2;
            px[1] -= 2;
            px[2] += 2;
            px[3] -= 2;
        }
        let dir = tempfile::Builder::new()
            .prefix("rock-image-oracle-")
            .tempdir()
            .expect("tempdir");
        let base_path = dir.path().join("base.png");
        let shifted_path = dir.path().join("shifted.png");
        write_rgba_png(&base_path, w, h, &base);
        write_rgba_png(&shifted_path, w, h, &shifted);
        let diff = image_diff(&base_path, &shifted_path).expect("diff");
        let expected = 2.0 / 255.0;
        assert!(
            (diff - expected).abs() < 1e-12,
            "uniform +-2 must diff exactly 2/255, oracle said {diff:.6}"
        );
        assert!(
            diff > DEFAULT_TOL,
            "a uniform +-2 shift must fail the default snapshot tol"
        );
    }

    /// Control: a partial edge block averages over its own pixels. A
    /// saturated 8x8 partial corner diffs 1/4: a full-block denominator
    /// would read 1/64 and a whole-frame mean 1/100.
    #[test]
    fn partial_edge_block_uses_own_pixel_count() {
        // 40x40: blocks are 32+8 per dim, so the far corner is an 8x8
        // partial block in both dims.
        let (w, h) = (40u32, 40u32);
        let mut base = vec![0u8; (w * h * 4) as usize];
        for px in base.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[0, 0, 0, 255]);
        }
        let mut shifted = base.clone();
        for y in 32..40 {
            for x in 32..40 {
                shifted[((y * w + x) * 4) as usize] = 255;
            }
        }
        let dir = tempfile::Builder::new()
            .prefix("rock-image-oracle-")
            .tempdir()
            .expect("tempdir");
        let base_path = dir.path().join("base.png");
        let shifted_path = dir.path().join("shifted.png");
        write_rgba_png(&base_path, w, h, &base);
        write_rgba_png(&shifted_path, w, h, &shifted);
        let diff = image_diff(&base_path, &shifted_path).expect("diff");
        assert!(
            (diff - 0.25).abs() < 1e-12,
            "saturated 8x8 partial corner must diff 1/4 over its own pixels, \
             oracle said {diff:.6}"
        );
        // Strip case: 48x32 holds a 16x32 partial strip; a mid-gray strip
        // diffs 128/(4*255) = 32/255 over its own pixels.
        let (w2, h2) = (48u32, 32u32);
        let mut base2 = vec![0u8; (w2 * h2 * 4) as usize];
        for px in base2.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[0, 0, 0, 255]);
        }
        let mut shifted2 = base2.clone();
        for y in 0..32 {
            for x in 32..48 {
                shifted2[((y * w2 + x) * 4 + 1) as usize] = 128;
            }
        }
        let base2_path = dir.path().join("base2.png");
        let shifted2_path = dir.path().join("shifted2.png");
        write_rgba_png(&base2_path, w2, h2, &base2);
        write_rgba_png(&shifted2_path, w2, h2, &shifted2);
        let diff2 = image_diff(&base2_path, &shifted2_path).expect("diff");
        let expected2 = 32.0 / 255.0;
        assert!(
            (diff2 - expected2).abs() < 1e-12,
            "mid-gray 16x32 partial strip must diff 32/255, oracle said {diff2:.6}"
        );
    }
}
