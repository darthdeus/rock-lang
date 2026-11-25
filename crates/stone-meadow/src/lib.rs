//! Pure terrain and meadow cover sampling shared by native simulation and rendering.

#[inline(always)]
fn floor(x: f64) -> f64 {
    if x.abs().partial_cmp(&4503599627370496.0) != Some(std::cmp::Ordering::Less) {
        return x;
    }
    let whole = x as i64 as f64;
    if whole > x {
        whole - 1.0
    } else if whole == x {
        x
    } else {
        whole
    }
}
fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}
fn smooth(a: f64, b: f64, x: f64) -> f64 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
fn lattice(x: i64, y: i64) -> f64 {
    let key = (x as u32 & 0xffff) | ((y as u32 & 0x7fff) << 16);
    let a = (key ^ (key >> 16)).wrapping_mul(0x7feb352d) & 0x7fffffff;
    let b = (a ^ (a >> 15)).wrapping_mul(0x046ca68b) & 0x7fffffff;
    ((b ^ (b >> 16)) >> 21 & 0x3ff) as f64
}
pub fn noise(x: f64, y: f64) -> f64 {
    let fx = floor(x);
    let fy = floor(y);
    let ix = fx as i64;
    let iy = fy as i64;
    let tx = x - fx;
    let ty = y - fy;
    let sx = tx * tx * (3.0 - 2.0 * tx);
    let sy = ty * ty * (3.0 - 2.0 * ty);
    lerp(
        lerp(lattice(ix, iy), lattice(ix.wrapping_add(1), iy), sx),
        lerp(
            lattice(ix, iy.wrapping_add(1)),
            lattice(ix.wrapping_add(1), iy.wrapping_add(1)),
            sx,
        ),
        sy,
    ) / 1023.0
}
pub fn random(x: f64, y: f64, stream: f64, seed: f64) -> f64 {
    let n = (x * 127.1 + y * 311.7 + stream * 74.7 + seed).sin() * 43758.5453;
    n - floor(n)
}
pub fn field(f: &[f64], x: f64, y: f64) -> f64 {
    if f.len() < 9 || !(-0.5..1.5).contains(&x) || !(-0.5..1.5).contains(&y) {
        return f64::NAN;
    }
    let fx = x - 0.5;
    let fy = y - 0.5;
    let ix = floor(fx);
    let iy = floor(fy);
    let tx = fx - ix;
    let ty = fy - iy;
    let at = ((iy as i32 + 1) * 3 + ix as i32 + 1) as usize;
    let a = f[at] * (1.0 - tx) + f[at + 1] * tx;
    let b = f[at + 3] * (1.0 - tx) + f[at + 4] * tx;
    a * (1.0 - ty) + b * ty
}
pub fn edge_noise(x: f64, y: f64) -> f64 {
    (x * 7.13 + y * 3.71).sin() * 0.08 + (y * 12.7 - x * 4.1).sin() * 0.04
}
pub fn profile(x: f64, y: f64) -> [f64; 3] {
    let broad = noise(x / 6.5 + 13.7, y / 6.5 + 29.1);
    let clump = noise(x / 1.1 + 41.3, y / 1.1 + 7.9);
    let cover = smooth(0.28, 0.72, broad * 0.68 + clump * 0.32);
    let tall = smooth(0.40, 0.88, cover);
    [
        0.08 + cover * 0.885,
        0.16 + tall * 0.95,
        noise(x / 4.7 + 5.2, y / 4.7 + 61.8),
    ]
}

/// Footprint of an authored object, in the same coordinates as the sampled tile.
#[derive(Clone)]
pub struct Footprint {
    bounds: [f64; 4],
    shape: Shape,
}

#[derive(Clone)]
enum Shape {
    Circle { center: [f64; 2], radius: f64 },
    Polygon(Vec<[f64; 2]>),
}

impl Footprint {
    pub fn circle(bounds: [f64; 4], center: [f64; 2], radius: f64) -> Self {
        Self {
            bounds,
            shape: Shape::Circle { center, radius },
        }
    }

    pub fn polygon(bounds: [f64; 4], points: Vec<[f64; 2]>) -> Self {
        Self {
            bounds,
            shape: Shape::Polygon(points),
        }
    }

    /// Move the footprint into another space's local coordinate frame.
    pub fn translated(&self, [dx, dy]: [f64; 2]) -> Self {
        if dx == 0.0 && dy == 0.0 {
            return self.clone();
        }
        let [min_x, min_y, max_x, max_y] = self.bounds;
        let bounds = [min_x + dx, min_y + dy, max_x + dx, max_y + dy];
        match &self.shape {
            Shape::Circle {
                center: [x, y],
                radius,
            } => Self::circle(bounds, [x + dx, y + dy], *radius),
            Shape::Polygon(points) => Self::polygon(
                bounds,
                points.iter().map(|[x, y]| [x + dx, y + dy]).collect(),
            ),
        }
    }

    /// Whether a point in this footprint's coordinate frame is under its base.
    pub fn contains_point(&self, x: f64, y: f64) -> bool {
        let [min_x, min_y, max_x, max_y] = self.bounds;
        (min_x..=max_x).contains(&x) && (min_y..=max_y).contains(&y) && self.distance(x, y) == 0.0
    }

    pub fn distance(&self, x: f64, y: f64) -> f64 {
        let points = match &self.shape {
            Shape::Circle {
                center: [cx, cy],
                radius,
            } => {
                let dx = x - cx;
                let dy = y - cy;
                return ((dx * dx + dy * dy).sqrt() - radius).max(0.0);
            }
            Shape::Polygon(points) => points,
        };
        let Some(&last) = points.last() else {
            return 1_000_000.0;
        };
        let mut inside = false;
        if points.len() >= 3 {
            let mut previous = last;
            for &[ax, ay] in points {
                let [bx, by] = previous;
                if (ay > y) != (by > y) && x < (bx - ax) * (y - ay) / (by - ay) + ax {
                    inside = !inside;
                }
                previous = [ax, ay];
            }
        }
        if inside {
            return 0.0;
        }
        let mut nearest = 1_000_000.0f64;
        let mut previous = last;
        for &[bx, by] in points {
            let [ax, ay] = previous;
            let ex = bx - ax;
            let ey = by - ay;
            let dx = x - ax;
            let dy = y - ay;
            let along = ((dx * ex + dy * ey) / (ex * ex + ey * ey).max(0.000001)).clamp(0.0, 1.0);
            let rx = dx - ex * along;
            let ry = dy - ey * along;
            nearest = nearest.min((rx * rx + ry * ry).sqrt());
            previous = [bx, by];
        }
        nearest
    }
}

pub fn cover_profile(wx: f64, wy: f64, woodland: bool) -> [f64; 3] {
    let p = profile(wx, wy);
    if woodland {
        [0.08 + p[0] * 0.15, 0.14 + p[1] * 0.08, p[2]]
    } else {
        p
    }
}
pub fn cover_density_ceiling(woodland: bool) -> f64 {
    let open = 0.08 + 0.885;
    1.000001 * if woodland { 0.08 + open * 0.15 } else { open }
}
/// Shape an open-ground profile with the 3x3 ground, trail and shore fields.
pub fn cover(
    fields: &[f64; 27],
    bases: &[Footprint],
    [gx, gy]: [f64; 2],
    [x, y]: [f64; 2],
    [wx, wy]: [f64; 2],
    p: [f64; 3],
) -> [f64; 3] {
    let mut density = 1.0f64;
    let mut height = 1.0f64;
    if !bases.is_empty() {
        let fringe = 0.55 + noise(wx * 2.3 + 71.0, wy * 2.3 + 17.0) * 0.65;
        for base in bases {
            let [min_x, min_y, max_x, max_y] = base.bounds;
            if x < min_x - 1.4 || y < min_y - 1.4 || x > max_x + 1.4 || y > max_y + 1.4 {
                continue;
            }
            let distance = base.distance(x, y);
            if distance == 0.0 {
                density = 0.0;
                height = 0.0;
                break;
            }
            let weight = smooth(0.0, fringe, distance);
            let circle = matches!(base.shape, Shape::Circle { .. });
            density = density.min(lerp(if circle { 0.45 } else { 0.12 }, 1.0, weight));
            height = height.min(lerp(if circle { 0.30 } else { 0.18 }, 1.0, weight));
        }
    }
    let n = edge_noise(wx, wy);
    let edge = smooth(0.35, 0.88, field(&fields[..9], x - gx, y - gy) + n);
    let worn =
        (field(&fields[9..18], x - gx, y - gy) + n).max(field(&fields[18..27], x - gx, y - gy) + n);
    let shoulder = edge * (1.0 - smooth(0.08, 0.5, worn));
    [
        p[0] * lerp(0.12, 1.0, shoulder) * density,
        p[1] * lerp(0.22, 1.0, shoulder) * height,
        p[2],
    ]
}
#[cfg(test)]
mod tests {
    use super::{cover, cover_density_ceiling, cover_profile, floor, random};

    #[test]
    fn inlined_floor_matches_the_library_floor_bit_for_bit() {
        let mut values = vec![
            0.0,
            -0.0,
            0.5,
            -0.5,
            1.0,
            -1.0,
            -1.0000000000000002,
            0.9999999999999999,
            4503599627370495.5,
            -4503599627370495.5,
            4503599627370496.0,
            1e300,
            -1e300,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ];
        for i in 0..2000 {
            values.push((random(i as f64, 3.0, 7.0, 11.0) - 0.5) * 100000.0);
        }
        for x in values {
            assert_eq!(floor(x).to_bits(), x.floor().to_bits(), "floor({x})");
        }
    }

    #[test]
    fn density_stays_under_the_open_ground_density_and_its_ceiling() {
        for fields in [[1.0; 27], [5.0; 27], [0.0; 27]] {
            let mut fields = fields;
            fields[9..].fill(0.0);
            for i in 0..4000 {
                let x = 40.0 + random(i as f64, 1.0, 2.0, 3.0);
                let y = 70.0 + random(i as f64, 4.0, 5.0, 6.0);
                let (wx, wy) = (x + i as f64 * 0.37, y - i as f64 * 0.11);
                for woodland in [false, true] {
                    let open = cover_profile(wx, wy, woodland);
                    assert!(open[0] * 1.000001 <= cover_density_ceiling(woodland));
                    let shaped = cover(&fields, &[], [40.0, 70.0], [x, y], [wx, wy], open);
                    assert!(shaped[0] <= open[0] * 1.000001, "{shaped:?} above {open:?}");
                }
            }
        }
    }

    #[test]
    fn cover_and_distance_share_the_same_footprint() {
        let base = super::Footprint::polygon(
            [1.25, 2.25, 1.75, 2.75],
            vec![[1.25, 2.25], [1.75, 2.25], [1.75, 2.75], [1.25, 2.75]],
        );
        assert_eq!(base.distance(1.5, 2.5), 0.0);
        assert_eq!(base.distance(2.0, 2.5), 0.25);
        assert_eq!(base.translated([10.0, -8.0]).distance(12.0, -5.5), 0.25);
        let open = cover_profile(10.5, 20.5, false);
        let sample = cover(
            &[1.0; 27],
            &[base],
            [1.0, 2.0],
            [1.5, 2.5],
            [10.5, 20.5],
            open,
        );
        assert_eq!(sample, [0.0, 0.0, open[2]]);
    }
}
