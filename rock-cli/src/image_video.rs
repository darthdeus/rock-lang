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

/// Mean absolute difference between two PNGs, normalized to [0, 1].
/// 0.0 = bit-identical, 1.0 = max possible delta.
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
    let mut total: u64 = 0;
    for (pb, pa) in b.iter().zip(a.iter()) {
        total += (*pb as i32 - *pa as i32).unsigned_abs() as u64;
    }
    let max = (bw as u64) * (bh as u64) * 4 * 255;
    Ok(total as f64 / max as f64)
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
