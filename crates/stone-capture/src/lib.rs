//! The CPU-RGBA capture policy: one place that turns presented pixels into
//! the PNG a screenshot means. Backends supply raw RGBA rows and say which
//! way is up; everything else -- flip, the long-edge cap with its
//! STONE_SCREENSHOT_MAX override, Lanczos resampling, PNG encoding and the
//! saved/failed logging -- lives here. `FrameStream` is the recording
//! counterpart: the same rows, unencoded, written off the frame.

mod frame_stream;

pub use frame_stream::{FrameStream, FrameStreamSummary};

/// Which way the supplied rows run. GL's default framebuffer reads bottom-to-top.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowOrigin {
    TopDown,
    BottomUp,
}

/// Write `rgba` (tightly packed, `width * height * 4` bytes) as a top-left
/// origin PNG at `path`. `max_size <= 0` selects the historical 1920 long-edge
/// default; `STONE_SCREENSHOT_MAX` overrides either.
pub fn save_rgba_png(
    path: &str,
    width: u32,
    height: u32,
    origin: RowOrigin,
    rgba: Vec<u8>,
    max_size: i32,
) -> Result<(u32, u32), String> {
    let (width, height) = (width as usize, height as usize);
    if width == 0 || height == 0 {
        return Err(format!("capture is zero-sized ({width}x{height})"));
    }
    let expected = width
        .checked_mul(height)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| format!("capture size overflows ({width}x{height})"))?;
    if rgba.len() != expected {
        return Err(format!(
            "capture supplied {} bytes for {width}x{height} ({expected} expected)",
            rgba.len()
        ));
    }

    let top_down = match origin {
        RowOrigin::TopDown => rgba,
        RowOrigin::BottomUp => {
            let row = width * 4;
            let mut flipped = vec![0u8; expected];
            for y in 0..height {
                let src = (height - 1 - y) * row;
                let dst = y * row;
                flipped[dst..dst + row].copy_from_slice(&rgba[src..src + row]);
            }
            flipped
        }
    };

    let max_dim: usize = std::env::var("STONE_SCREENSHOT_MAX")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(if max_size > 0 {
            max_size as usize
        } else {
            1920
        });
    let (final_width, final_height, buffer) = if width > max_dim || height > max_dim {
        let (new_width, new_height) = if width > height {
            (
                max_dim as u32,
                (height as f32 * (max_dim as f32 / width as f32)) as u32,
            )
        } else {
            (
                (width as f32 * (max_dim as f32 / height as f32)) as u32,
                max_dim as u32,
            )
        };
        ::log::info!(
            target: "SCREENSHOT",
            "resizing screenshot from {}x{} to {}x{}", width, height, new_width, new_height
        );
        let img = image::RgbaImage::from_raw(width as u32, height as u32, top_down)
            .ok_or_else(|| "failed to create image from buffer".to_string())?;
        let resized = image::imageops::resize(
            &img,
            new_width,
            new_height,
            image::imageops::FilterType::Lanczos3,
        );
        (new_width, new_height, resized.into_raw())
    } else {
        (width as u32, height as u32, top_down)
    };

    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "failed to create screenshot directory {}: {e}",
                    parent.display()
                )
            })?;
        }
    }

    image::save_buffer(
        path,
        &buffer,
        final_width,
        final_height,
        image::ExtendedColorType::Rgba8,
    )
    .map_err(|e| format!("failed to save screenshot: {e}"))?;
    ::log::info!(target: "SCREENSHOT", "screenshot saved to {}", path);
    Ok((final_width, final_height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_rgba_png_creates_missing_parent_directories() {
        let temp_dir =
            std::env::temp_dir().join(format!("stone_capture_test_{}", std::process::id()));
        let nested_dir = temp_dir.join("a").join("b").join("c");
        let target_path = nested_dir.join("test.png");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let res = save_rgba_png(
            target_path.to_str().unwrap(),
            2,
            2,
            RowOrigin::TopDown,
            vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
            0,
        );
        let exists = target_path.is_file();
        let _ = std::fs::remove_dir_all(&temp_dir);

        assert!(res.is_ok(), "save_rgba_png failed: {:?}", res);
        assert!(exists, "saved image does not exist");
    }
}
