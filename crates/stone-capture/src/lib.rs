//! Capture accepts raw RGBA rows in either orientation; `FrameStream` records
//! them unencoded off the frame.

mod frame_stream;

pub use frame_stream::{FrameStream, FrameStreamSummary};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowOrigin {
    TopDown,
    // GL's default framebuffer reads bottom-to-top.
    BottomUp,
}

fn rgba_len(width: u32, height: u32) -> Option<usize> {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
}

fn copy_reversed_rows(rgba: &[u8], row_bytes: usize, output: &mut Vec<u8>) {
    output.clear();
    output.reserve(rgba.len());
    for row in rgba.chunks_exact(row_bytes).rev() {
        output.extend_from_slice(row);
    }
}

pub fn save_rgba_png(
    path: &str,
    width: u32,
    height: u32,
    origin: RowOrigin,
    rgba: Vec<u8>,
    max_size: i32,
) -> Result<(u32, u32), String> {
    if width == 0 || height == 0 {
        return Err(format!("capture is zero-sized ({width}x{height})"));
    }
    let expected = rgba_len(width, height)
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
            let mut flipped = Vec::new();
            copy_reversed_rows(&rgba, width as usize * 4, &mut flipped);
            flipped
        }
    };
    let mut image = image::RgbaImage::from_raw(width, height, top_down)
        .ok_or_else(|| "failed to create image from buffer".to_string())?;

    let max_dim: usize = if max_size > 0 {
        max_size as usize
    } else {
        1920
    };
    let long_edge = width.max(height);
    if long_edge as usize > max_dim {
        let scale = max_dim as f32 / long_edge as f32;
        let (new_width, new_height) = if width > height {
            (max_dim as u32, (height as f32 * scale) as u32)
        } else {
            ((width as f32 * scale) as u32, max_dim as u32)
        };
        ::log::info!(
            target: "SCREENSHOT",
            "resizing screenshot from {width}x{height} to {new_width}x{new_height}"
        );
        image = image::imageops::resize(
            &image,
            new_width,
            new_height,
            image::imageops::FilterType::Lanczos3,
        );
    }

    if let Some(parent) = std::path::Path::new(path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "failed to create screenshot directory {}: {e}",
                parent.display()
            )
        })?;
    }

    image
        .save(path)
        .map_err(|e| format!("failed to save screenshot: {e}"))?;
    ::log::info!(target: "SCREENSHOT", "screenshot saved to {path}");
    Ok(image.dimensions())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resized_screenshot_creates_missing_parent_directories() {
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
            vec![255; 2 * 2 * 4],
            1,
        );
        let decoded = image::open(&target_path);
        let _ = std::fs::remove_dir_all(&temp_dir);

        let size = res.unwrap();
        assert_eq!(size, (1, 1));
        let decoded = decoded.unwrap();
        assert_eq!((decoded.width(), decoded.height()), size);
    }
}
