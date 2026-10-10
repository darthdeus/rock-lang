//! PNG decode/diff and ffprobe/ffmpeg-SSIM utilities used by the
//! `rock sim` runner. Lives outside `run_sim.rs` because the
//! image-and-video subprocess work is generic enough to be reused
//! elsewhere; was extracted from the (deleted) `.sim` runner.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

/// Decode a PNG into an RGBA8 byte buffer (4 bytes per pixel,
/// row-major). Stone screenshots are PNG; baselines are produced by
/// Stone too. Anything outside the formats Stone writes is an error.
pub fn decode_png_rgba8(path: &Path) -> Result<(u32, u32, Vec<u8>)> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .with_context(|| format!("reading PNG header for {}", path.display()))?;
    let mut buf = vec![
        0;
        reader
            .output_buffer_size()
            .ok_or_else(|| anyhow::anyhow!("PNG buffer size overflow for {}", path.display()))?
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
            for chunk in bytes.chunks_exact(3) {
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
/// delta, normalized to [0, 1]. 0.0 = bit-identical, 1.0 = max possible
/// delta in some block. A whole-frame mean let a sprite-sized defect hide
/// behind an unchanged background while the tolerance stayed fixed, so the
/// oracle is the worst region, not the average pixel: a half-tile shift of
/// one tile fails the default `image_snapshot` tol where the mean passed.
/// Partial edge blocks average over their own pixels; no pixel is excluded.
/// Per-channel deltas of 1 or less count as zero: that is the RGBA8
/// quantization floor (rounding vs truncation across GL implementations),
/// invisible by construction, and committed cross-machine goldens otherwise
/// fail on it. Deltas above the floor keep their full magnitude.
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

/// Pull `duration`, `width`, `height`, `codec`, `nb_frames` out of
/// ffprobe's JSON output for a video file. Used by `expect_video`.
pub fn ffprobe_meta(path: &str) -> Result<std::collections::HashMap<String, Value>> {
    let out = Command::new("ffprobe")
        .args([
            "-v", "quiet",
            "-print_format", "json",
            "-show_format",
            "-show_streams",
            path,
        ])
        .output()
        .context("spawning ffprobe (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "ffprobe failed on `{}`: {}",
            path,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let parsed: Value = serde_json::from_slice(&out.stdout).context("parsing ffprobe JSON")?;

    let mut m = std::collections::HashMap::new();
    if let Some(d) = parsed
        .pointer("/format/duration")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
    {
        m.insert("duration".to_string(), json!(d));
    }
    if let Some(streams) = parsed.get("streams").and_then(Value::as_array) {
        for s in streams {
            if s.get("codec_type").and_then(Value::as_str) == Some("video") {
                if let Some(w) = s.get("width").and_then(Value::as_i64) {
                    m.insert("width".to_string(), json!(w));
                }
                if let Some(h) = s.get("height").and_then(Value::as_i64) {
                    m.insert("height".to_string(), json!(h));
                }
                if let Some(c) = s.get("codec_name").and_then(Value::as_str) {
                    m.insert("codec".to_string(), json!(c));
                }
                if let Some(nf) = s.get("nb_frames") {
                    if let Some(i) = nf.as_i64() {
                        m.insert("nb_frames".to_string(), json!(i));
                    } else if let Some(s) = nf.as_str() {
                        if let Ok(i) = s.parse::<i64>() {
                            m.insert("nb_frames".to_string(), json!(i));
                        }
                    }
                }
                break;
            }
        }
    }
    if m.is_empty() {
        bail!("ffprobe produced no usable metadata for `{}`", path);
    }
    Ok(m)
}

/// Run ffmpeg's SSIM filter, return the overall "All" score in [0,1].
/// SSIM of 1.0 = perceptually identical.
pub fn ffmpeg_ssim(a: &str, b: &str) -> Result<f64> {
    // Reject dimension mismatches early. ffmpeg would silently
    // rescale, producing a valid SSIM score against a differently-
    // sized reference.
    let meta_a = ffprobe_meta(a).with_context(|| format!("ffprobe `{}`", a))?;
    let meta_b = ffprobe_meta(b).with_context(|| format!("ffprobe `{}`", b))?;
    let wa = meta_a.get("width");
    let ha = meta_a.get("height");
    let wb = meta_b.get("width");
    let hb = meta_b.get("height");
    if wa != wb || ha != hb {
        bail!(
            "expect_videos_match: dimension mismatch -- a is {:?}x{:?}, b is {:?}x{:?}",
            wa,
            ha,
            wb,
            hb
        );
    }

    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-nostats",
            "-i",
            a,
            "-i",
            b,
            "-lavfi",
            "[0:v][1:v]ssim",
            "-f",
            "null",
            "-",
        ])
        .output()
        .context("spawning ffmpeg (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "ffmpeg ssim failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // SSIM summary is on stderr, e.g.:
    //   [Parsed_ssim_0 @ 0x...] SSIM Y:0.97 U:0.99 V:0.99 All:0.98 (19.12 dB)
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in stderr.lines() {
        if let Some(i) = line.find("All:") {
            let tail = &line[i + 4..];
            let score: String = tail
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(f) = score.parse::<f64>() {
                return Ok(f);
            }
        }
    }
    bail!("ffmpeg ssim: no `All:` score in output:\n{}", stderr);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Match3 production tile geometry: `TILE_SIZE` in games/match3/main.rock.
    /// The trap shifts one tile-sized window of real board pixels by half a tile.
    const TILE_PX: u32 = 64;
    const HALF_TILE_PX: u32 = 32;
    /// Production default `image_snapshot` tolerance: the one-argument macro in
    /// stone/engine/sim.rock passes 0.001.
    const DEFAULT_TOL: f64 = 0.001;
    /// Oracle block pitch: half a match3 tile, so a sprite-sized defect always
    /// covers a large fraction of some block instead of diluting across the frame.
    const BLOCK_PX: u32 = 32;

    fn match3_boot_golden() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../games/match3/tests/sims/smoke.snap/boot.png")
    }

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

    /// The test's own loops over raw pixels: whole-frame mean absolute channel
    /// delta plus the max over origin-aligned 32px blocks. Independent of
    /// `image_diff`; pins the fixture to the stated trap (hidden from the mean,
    /// visible to a region) instead of trusting the oracle under test.
    /// Only `window` (x0, y0, size) differs between `base` and `shifted`.
    /// The mean counts every LSB (that is the old oracle's quantity); the block
    /// scan zeroes per-channel deltas of 1 or less (the new oracle's rounding
    /// floor), so the fixture proves a VISIBLE regional signal, not ±1 noise.
    fn independent_means(
        base: &[u8],
        shifted: &[u8],
        w: u32,
        x0: u32,
        y0: u32,
        size: u32,
    ) -> (f64, f64) {
        let h = (base.len() / 4 / w as usize) as u32;
        let mut frame_total: u64 = 0;
        let mut blocks = std::collections::HashMap::new();
        for dy in 0..size {
            for dx in 0..size {
                let x = x0 + dx;
                let y = y0 + dy;
                let o = ((y * w + x) * 4) as usize;
                let mut px_total: u64 = 0;
                let mut px_visible: u64 = 0;
                for c in 0..4 {
                    let d =
                        (base[o + c] as i32 - shifted[o + c] as i32).unsigned_abs() as u64;
                    px_total += d;
                    px_visible += if d <= 1 { 0 } else { d };
                }
                frame_total += px_total;
                *blocks.entry((x / BLOCK_PX, y / BLOCK_PX)).or_insert(0u64) += px_visible;
            }
        }
        let frame_mean = frame_total as f64 / (w as u64 * h as u64 * 4 * 255) as f64;
        let mut block_max: f64 = 0.0;
        for ((bx, by), total) in blocks {
            let bw = (BLOCK_PX.min(w - bx * BLOCK_PX)) as u64;
            let bh = (BLOCK_PX.min(h - by * BLOCK_PX)) as u64;
            block_max = block_max.max(total as f64 / (bw * bh * 4 * 255) as f64);
        }
        (frame_mean, block_max)
    }

    /// A half-tile shift of a tile-sized region of production output must fail
    /// the default snapshot tolerance. The whole-frame mean hides it (RED on the
    /// mean oracle); a regional oracle sees the shifted block (GREEN).
    #[test]
    fn half_tile_shift_of_one_tile_fails_default_tol() {
        let golden = match3_boot_golden();
        let (w, h, base) = decode_png_rgba8(&golden).expect("decode production golden");
        assert!(w >= TILE_PX && h >= TILE_PX, "golden holds a tile: {w}x{h}");

        // First window (deterministic scan) whose shift hides in the frame mean
        // yet shows in some block. Outside the window the delta is zero, so the
        // independent mean below is the whole-frame mean of the pair.
        let mut trap: Option<(u32, u32, Vec<u8>, f64, f64)> = None;
        'scan: for y0 in (0..=h - TILE_PX).step_by(8) {
            for x0 in (0..=w - TILE_PX).step_by(8) {
                let mut shifted = base.clone();
                for dy in 0..TILE_PX {
                    for dx in 0..TILE_PX {
                        let sx = x0 + (dx + HALF_TILE_PX) % TILE_PX;
                        let sy = y0 + (dy + HALF_TILE_PX) % TILE_PX;
                        let s = ((sy * w + sx) * 4) as usize;
                        let d = (((y0 + dy) * w + x0 + dx) * 4) as usize;
                        shifted[d..d + 4].copy_from_slice(&base[s..s + 4]);
                    }
                }
                let (mean, block) = independent_means(&base, &shifted, w, x0, y0, TILE_PX);
                if mean <= DEFAULT_TOL && block > DEFAULT_TOL {
                    trap = Some((x0, y0, shifted, mean, block));
                    break 'scan;
                }
            }
        }
        let (x0, y0, shifted, mean, block) = trap.expect(
            "golden must hold a tile whose half-tile shift hides in the mean but shows in a block",
        );

        let dir = tempfile::Builder::new()
            .prefix("rock-image-oracle-")
            .tempdir()
            .expect("tempdir");
        let shifted_path = dir.path().join("shifted.png");
        write_rgba_png(&shifted_path, w, h, &shifted);

        let diff = image_diff(&golden, &shifted_path).expect("diff");
        assert!(
            diff > DEFAULT_TOL,
            "window ({x0},{y0}): independent frame mean {mean:.6} hides the shift, \
             independent block max {block:.6} sees it; oracle said {diff:.6}"
        );
    }

    /// Control: identical frames diff exactly zero, so the regional oracle adds
    /// no false positives on clean reruns.
    #[test]
    fn identical_frames_diff_zero() {
        let golden = match3_boot_golden();
        let diff = image_diff(&golden, &golden).expect("diff");
        assert_eq!(diff, 0.0, "identical frames must diff exactly zero");
    }

    /// Control: dimension mismatches stay hard errors, and the test PNG writer
    /// round-trips through the production decoder.
    #[test]
    fn dim_mismatch_stays_an_error() {
        let golden = match3_boot_golden();
        let (w, h, base) = decode_png_rgba8(&golden).expect("decode production golden");
        let dir = tempfile::Builder::new()
            .prefix("rock-image-oracle-")
            .tempdir()
            .expect("tempdir");
        let same = dir.path().join("same.png");
        write_rgba_png(&same, w, h, &base);
        let (_, _, back) = decode_png_rgba8(&same).expect("decode written png");
        assert_eq!(back, base, "test writer must round-trip through production decode");
        let tiny = dir.path().join("tiny.png");
        write_rgba_png(&tiny, 1, 1, &[0, 0, 0, 255]);
        assert!(
            image_diff(&golden, &tiny).is_err(),
            "a size change must stay a hard error, not a diff"
        );
    }
}
