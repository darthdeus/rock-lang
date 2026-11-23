//! Subsampled signed terrain distance-field construction.

use std::sync::atomic::{AtomicBool, Ordering};

const FIELD_FAR: f32 = 1_000_000.0;
const FIELD_DIAG: f64 = std::f64::consts::SQRT_2;

#[inline]
fn solid_at(solid: &[u8], tile_width: usize, sub: usize, x: usize, y: usize) -> bool {
    solid[(y / sub) * tile_width + x / sub] != 0
}

pub(super) fn compute(
    width: usize,
    height: usize,
    sub: usize,
    solid: &[u8],
    cancelled: &AtomicBool,
) -> Option<Vec<f32>> {
    let sample_width = width.checked_mul(sub)?;
    let sample_height = height.checked_mul(sub)?;
    let sample_len = sample_width.checked_mul(sample_height)?;
    let tile_len = width.checked_mul(height)?;
    if solid.len() != tile_len {
        return None;
    }

    let mut distance = vec![FIELD_FAR; sample_len];

    for y in 0..sample_height {
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        for x in 0..sample_width {
            let index = y * sample_width + x;
            let here = solid_at(solid, width, sub, x, y);
            let edge =
                (x == 0 || y == 0 || x == sample_width - 1 || y == sample_height - 1) && !here;
            let split = edge
                || (x > 0 && solid_at(solid, width, sub, x - 1, y) != here)
                || (x + 1 < sample_width && solid_at(solid, width, sub, x + 1, y) != here)
                || (y > 0 && solid_at(solid, width, sub, x, y - 1) != here)
                || (y + 1 < sample_height && solid_at(solid, width, sub, x, y + 1) != here);
            distance[index] = if split { 0.5 } else { FIELD_FAR };
        }
    }

    for backward in [false, true] {
        let indices = |len| (0..len).map(move |i| if backward { len - 1 - i } else { i });
        for y in indices(sample_height) {
            if cancelled.load(Ordering::Acquire) {
                return None;
            }
            let (above, rest) = distance.split_at_mut(y * sample_width);
            let (row, below) = rest.split_at_mut(sample_width);
            let previous = if backward {
                below.get(..sample_width)
            } else {
                above.get(above.len().wrapping_sub(sample_width)..)
            };
            let mut side = FIELD_FAR;
            for x in indices(sample_width) {
                // Widen to Rock's f64 float, round once on write.
                let mut d = f64::from(row[x]).min(f64::from(side) + 1.0);
                if let Some(previous) = previous {
                    // Clamping a missing diagonal gives the vertical neighbor,
                    // whose straight step is always cheaper.
                    let diagonal =
                        previous[x.saturating_sub(1)].min(previous[(x + 1).min(sample_width - 1)]);
                    d = d
                        .min(f64::from(previous[x]) + 1.0)
                        .min(f64::from(diagonal) + FIELD_DIAG);
                }
                side = d as f32;
                row[x] = side;
            }
        }
    }

    for y in 0..sample_height {
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        for x in 0..sample_width {
            if solid_at(solid, width, sub, x, y) {
                let index = y * sample_width + x;
                distance[index] = -distance[index];
            }
        }
    }

    Some(distance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sweeps_propagate_signed_distances_to_all_corners() {
        let mut solid = [1; 49];
        solid[24] = 0;
        let field = compute(7, 7, 1, &solid, &AtomicBool::new(false)).unwrap();
        assert_eq!(field[24], 0.5);
        for corner in [0, 6, 42, 48] {
            assert_eq!(field[corner], -4.3284273);
        }
    }
}
