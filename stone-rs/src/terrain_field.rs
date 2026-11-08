//! Subsampled signed terrain distance-field construction.

use std::sync::atomic::{AtomicBool, Ordering};

const FIELD_FAR: f32 = 1_000_000.0;
const FIELD_DIAG: f64 = std::f64::consts::SQRT_2;

#[inline]
fn cancelled(cancellation: &AtomicBool, stopping: &AtomicBool) -> bool {
    cancellation.load(Ordering::Acquire) || stopping.load(Ordering::Acquire)
}

#[inline]
fn solid_at(solid: &[u8], tile_width: usize, sub: usize, x: usize, y: usize) -> bool {
    solid[(y / sub) * tile_width + x / sub] != 0
}

pub(super) fn compute(
    width: usize,
    height: usize,
    sub: usize,
    solid: &[u8],
    cancellation: &AtomicBool,
    stopping: &AtomicBool,
) -> Option<Vec<f32>> {
    let sample_width = width.checked_mul(sub)?;
    let sample_height = height.checked_mul(sub)?;
    let sample_len = sample_width.checked_mul(sample_height)?;
    let tile_len = width.checked_mul(height)?;
    if solid.len() != tile_len {
        return None;
    }

    let mut distance = vec![FIELD_FAR; sample_len];

    // Seed the sample boundary and every cardinal solid/empty transition at
    // half a sample from the separating edge.
    for y in 0..sample_height {
        if cancelled(cancellation, stopping) {
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

    // The Rock buffer stores each cell as f32, while Rock's scalar `float`
    // arithmetic is f64. Widen neighbors before adding, keep each cell's mins
    // in f64, then round once when writing that cell back to the f32 field.
    for y in 0..sample_height {
        if cancelled(cancellation, stopping) {
            return None;
        }
        for x in 0..sample_width {
            let index = y * sample_width + x;
            let mut d = f64::from(distance[index]);
            if x > 0 {
                d = d.min(f64::from(distance[index - 1]) + 1.0);
            }
            if y > 0 {
                d = d.min(f64::from(distance[index - sample_width]) + 1.0);
                if x > 0 {
                    d = d.min(f64::from(distance[index - sample_width - 1]) + FIELD_DIAG);
                }
                if x + 1 < sample_width {
                    d = d.min(f64::from(distance[index - sample_width + 1]) + FIELD_DIAG);
                }
            }
            distance[index] = d as f32;
        }
    }

    for y in (0..sample_height).rev() {
        if cancelled(cancellation, stopping) {
            return None;
        }
        for x in (0..sample_width).rev() {
            let index = y * sample_width + x;
            let mut d = f64::from(distance[index]);
            if x + 1 < sample_width {
                d = d.min(f64::from(distance[index + 1]) + 1.0);
            }
            if y + 1 < sample_height {
                d = d.min(f64::from(distance[index + sample_width]) + 1.0);
                if x > 0 {
                    d = d.min(f64::from(distance[index + sample_width - 1]) + FIELD_DIAG);
                }
                if x + 1 < sample_width {
                    d = d.min(f64::from(distance[index + sample_width + 1]) + FIELD_DIAG);
                }
            }
            distance[index] = d as f32;
        }
    }

    for y in 0..sample_height {
        if cancelled(cancellation, stopping) {
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
