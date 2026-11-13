//! Pure terrain sampling and meadow geometry over copied, double-precision snapshots.
//! The game supplies fields, prop footprints, layout and damage addressing.

use std::slice;

// `f64::floor` is a library call on the baseline x86-64 target, and blade
// generation takes it about twenty times per candidate.
#[inline(always)]
fn floor(x: f64) -> f64 {
    if !(x.abs() < 4503599627370496.0) {
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
fn noise(x: f64, y: f64) -> f64 {
    let fx = floor(x);
    let fy = floor(y);
    let ix = fx as i64;
    let iy = fy as i64;
    let tx = x - fx;
    let ty = y - fy;
    let sx = tx * tx * (3.0 - 2.0 * tx);
    let sy = ty * ty * (3.0 - 2.0 * ty);
    lerp(
        lerp(lattice(ix, iy), lattice(ix + 1, iy), sx),
        lerp(lattice(ix, iy + 1), lattice(ix + 1, iy + 1), sx),
        sy,
    ) / 1023.0
}
fn random(x: f64, y: f64, stream: f64, seed: f64) -> f64 {
    let n = (x * 127.1 + y * 311.7 + stream * 74.7 + seed).sin() * 43758.5453;
    n - floor(n)
}
fn field(f: &[f64], x: f64, y: f64) -> f64 {
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
fn edge_noise(x: f64, y: f64) -> f64 {
    (x * 7.13 + y * 3.71).sin() * 0.08 + (y * 12.7 - x * 4.1).sin() * 0.04
}
fn profile(x: f64, y: f64) -> [f64; 3] {
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

// Each base has bounds, centre, radius, tree flag, vertex count, then XY pairs.
struct Snapshot {
    fields: [f64; 27],
    bases: Vec<Base>,
}
struct Base {
    data: Vec<f64>,
}
impl Base {
    fn distance(&self, x: f64, y: f64) -> f64 {
        let b = &self.data;
        if b[7] != 0.0 {
            let dx = x - b[4];
            let dy = y - b[5];
            return ((dx * dx + dy * dy).sqrt() - b[6]).max(0.0);
        }
        let n = b[8] as usize;
        let mut inside = false;
        if n >= 3 {
            let mut prev = n - 1;
            for i in 0..n {
                let ax = b[9 + i * 2];
                let ay = b[10 + i * 2];
                let bx = b[9 + prev * 2];
                let by = b[10 + prev * 2];
                if (ay > y) != (by > y) && x < (bx - ax) * (y - ay) / (by - ay) + ax {
                    inside = !inside;
                }
                prev = i;
            }
        }
        if inside {
            return 0.0;
        }
        let mut nearest = 1000000.0f64;
        let mut prev = n.saturating_sub(1);
        for i in 0..n {
            let ax = b[9 + prev * 2];
            let ay = b[10 + prev * 2];
            let ex = b[9 + i * 2] - ax;
            let ey = b[10 + i * 2] - ay;
            let dx = x - ax;
            let dy = y - ay;
            let along = ((dx * ex + dy * ey) / (ex * ex + ey * ey).max(0.000001)).clamp(0.0, 1.0);
            let rx = dx - ex * along;
            let ry = dy - ey * along;
            nearest = nearest.min((rx * rx + ry * ry).sqrt());
            prev = i;
        }
        nearest
    }
}
fn parse_bases(data: &[f64]) -> Option<Vec<Base>> {
    let mut bases = Vec::new();
    let mut at = 0;
    while at < data.len() {
        if data.len() - at < 9 {
            return None;
        }
        let n = data[at + 8];
        if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > 65536.0 {
            return None;
        }
        let end = at + 9 + n as usize * 2;
        if end > data.len() {
            return None;
        }
        bases.push(Base {
            data: data[at..end].to_vec(),
        });
        at = end;
    }
    Some(bases)
}
unsafe fn doubles(ptr: *const u8, len: i32) -> Option<Vec<f64>> {
    if len < 0 || len % 8 != 0 || len > 8 * 1024 * 1024 || (len > 0 && ptr.is_null()) {
        return None;
    }
    if len == 0 {
        return Some(Vec::new());
    }
    Some(
        slice::from_raw_parts(ptr, len as usize)
            .as_chunks::<8>()
            .0
            .iter()
            .map(|v| f64::from_ne_bytes(*v))
            .collect(),
    )
}
unsafe fn snapshot(ptr: *const u8, len: i32) -> Option<Snapshot> {
    let data = doubles(ptr, len)?;
    if data.len() < 27 {
        return None;
    }
    Some(Snapshot {
        fields: data[..27].try_into().unwrap(),
        bases: parse_bases(&data[27..])?,
    })
}
fn cover_profile(wx: f64, wy: f64, woodland: bool) -> [f64; 3] {
    let p = profile(wx, wy);
    if woodland {
        [0.08 + p[0] * 0.15, 0.14 + p[1] * 0.08, p[2]]
    } else {
        p
    }
}
// No position's open-ground density exceeds this, with room for rounding.
fn cover_density_ceiling(woodland: bool) -> f64 {
    let open = 0.08 + 0.885;
    1.000001 * if woodland { 0.08 + open * 0.15 } else { open }
}
fn cover(
    s: &Snapshot,
    gx: f64,
    gy: f64,
    x: f64,
    y: f64,
    wx: f64,
    wy: f64,
    p: [f64; 3],
) -> [f64; 3] {
    let mut density = 1.0f64;
    let mut height = 1.0f64;
    if !s.bases.is_empty() {
        let fringe = 0.55 + noise(wx * 2.3 + 71.0, wy * 2.3 + 17.0) * 0.65;
        for base in &s.bases {
            let b = &base.data;
            if x < b[0] - 1.4 || y < b[1] - 1.4 || x > b[2] + 1.4 || y > b[3] + 1.4 {
                continue;
            }
            let distance = base.distance(x, y);
            if distance == 0.0 {
                density = 0.0;
                height = 0.0;
                break;
            }
            let weight = smooth(0.0, fringe, distance);
            density = density.min(lerp(if b[7] != 0.0 { 0.45 } else { 0.12 }, 1.0, weight));
            height = height.min(lerp(if b[7] != 0.0 { 0.30 } else { 0.18 }, 1.0, weight));
        }
    }
    let n = edge_noise(wx, wy);
    let edge = smooth(0.35, 0.88, field(&s.fields[..9], x - gx, y - gy) + n);
    let worn = (field(&s.fields[9..18], x - gx, y - gy) + n)
        .max(field(&s.fields[18..27], x - gx, y - gy) + n);
    let shoulder = edge * (1.0 - smooth(0.08, 0.5, worn));
    [
        p[0] * lerp(0.12, 1.0, shoulder) * density,
        p[1] * lerp(0.22, 1.0, shoulder) * height,
        p[2],
    ]
}
unsafe fn output3(out: *mut u8, p: [f64; 3]) -> i32 {
    if out.is_null() {
        return -1;
    }
    for (i, value) in p.iter().enumerate() {
        std::ptr::copy_nonoverlapping(value.to_ne_bytes().as_ptr(), out.add(i * 8), 8);
    }
    0
}

#[no_mangle]
pub extern "C" fn stone_terrain_noise(x: f64, y: f64) -> f64 {
    noise(x, y)
}
#[no_mangle]
pub extern "C" fn stone_terrain_random(x: f64, y: f64, stream: f64, seed: f64) -> f64 {
    random(x, y, stream, seed)
}
#[no_mangle]
pub extern "C" fn stone_terrain_field(
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
    g: f64,
    h: f64,
    i: f64,
    x: f64,
    y: f64,
    wx: f64,
    wy: f64,
    noisy: i32,
) -> f64 {
    field(&[a, b, c, d, e, f, g, h, i], x, y) + if noisy != 0 { edge_noise(wx, wy) } else { 0.0 }
}
#[no_mangle]
pub unsafe extern "C" fn stone_meadow_profile(x: f64, y: f64, out: *mut u8) -> i32 {
    output3(out, profile(x, y))
}
#[no_mangle]
pub unsafe extern "C" fn stone_meadow_cover(
    input: *const u8,
    len: i32,
    gx: f64,
    gy: f64,
    x: f64,
    y: f64,
    wx: f64,
    wy: f64,
    woodland: i32,
    out: *mut u8,
) -> i32 {
    let Some(s) = snapshot(input, len) else {
        return -1;
    };
    if !(-0.5..1.5).contains(&(x - gx)) || !(-0.5..1.5).contains(&(y - gy)) {
        return -1;
    }
    let open = cover_profile(wx, wy, woodland != 0);
    output3(out, cover(&s, gx, gy, x, y, wx, wy, open))
}
#[no_mangle]
pub unsafe extern "C" fn stone_meadow_distance(input: *const u8, len: i32, x: f64, y: f64) -> f64 {
    let Some(data) = doubles(input, len) else {
        return f64::NAN;
    };
    let Some(bases) = parse_bases(&data) else {
        return f64::NAN;
    };
    if bases.len() != 1 {
        return f64::NAN;
    }
    bases[0].distance(x, y)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Growth {
    blade: [f32; 7],
    cell: i32,
}

// Params: layout origin XY, projected origin XY, tile XY, woodland, candidates,
// seed, projected tile scales XY, damage chunk XY, damage subdivision/side.
#[no_mangle]
pub unsafe extern "C" fn stone_meadow_blades(
    input: *const u8,
    len: i32,
    params: *const u8,
    params_len: i32,
    output: *mut u8,
    capacity: i32,
) -> i32 {
    let Some(s) = snapshot(input, len) else {
        return -1;
    };
    let Some(p) = doubles(params, params_len) else {
        return -1;
    };
    if p.len() != 15
        || p.iter().any(|v| !v.is_finite())
        || output.is_null()
        || capacity < 0
        || p[7] < 0.0
        || p[7].fract() != 0.0
        || p[7] > capacity as f64
        || p[13] <= 0.0
        || p[14] <= 0.0
    {
        return -1;
    }
    let ceiling = cover_density_ceiling(p[6] != 0.0);
    let mut count = 0;
    for i in 0..p[7] as i32 {
        let sx = p[0] + p[4] + (i % 8) as f64 / 8.0;
        let sy = p[1] + p[5] + (i / 8) as f64 / 8.0;
        let stream = (i / 64) as f64 * 97.0;
        // A candidate survives only if its draw is within the density where
        // it lands. Woodland's ceiling is low, so most are rejected on the
        // draw alone, before their position is worked out; then edges, wear
        // and props only thin the open-ground density, which rejects more
        // before those are sampled.
        let draw = random(sx, sy, 83.0 + stream, p[8]);
        if draw > ceiling {
            continue;
        }
        let x = p[4] + random(sx, sy, 73.0 + stream, p[8]);
        let y = p[5] + random(sx, sy, 79.0 + stream, p[8]);
        let open = cover_profile(p[0] + x, p[1] + y, p[6] != 0.0);
        if draw > open[0] * 1.000001 {
            continue;
        }
        let profile = cover(&s, p[4], p[5], x, y, p[0] + x, p[1] + y, open);
        if profile[0] <= 0.0 || draw > profile[0] {
            continue;
        }
        let variation = random(sx, sy, 71.0 + stream, p[8]);
        let stature = random(sx, sy, 89.0 + stream, p[8]);
        let cell_x = floor(x * p[13]) - p[11] * p[14];
        let cell_y = floor(y * p[13]) - p[12] * p[14];
        let growth = Growth {
            blade: [
                ((x + y) * p[9] + p[2]) as f32,
                ((x - y) * p[10] + p[3]) as f32,
                (0.055 + random(sx, sy, 87.0 + stream, p[8]) * 0.055) as f32,
                (profile[1] * (0.72 + stature * 0.56)) as f32,
                variation as f32,
                random(sx, sy, 91.0 + stream, p[8]) as f32,
                profile[2] as f32,
            ],
            cell: (cell_y * p[14] + cell_x) as i32,
        };
        std::ptr::copy_nonoverlapping(
            (&growth as *const Growth).cast::<u8>(),
            output.add(count * 32),
            32,
        );
        count += 1;
    }
    count as i32
}

// The partition and swap order below preserves LuaJIT table.sort's ordering
// of equal depths; a stable or differently partitioned sort changes coverage.
// Adapted from LuaJIT lib_table.c (Lua table library):
// Copyright (C) 2005-2017 Mike Pall.
// Copyright (C) 1994-2008 Lua.org, PUC-Rio.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.
type Blade = [f32; 7];
fn sort_blades(blades: &mut [Blade]) {
    if blades.len() < 2 {
        return;
    }
    let (mut low, mut high) = (0, blades.len() - 1);
    while low < high {
        if blades[high][1] > blades[low][1] {
            blades.swap(low, high);
        }
        if high - low == 1 {
            break;
        }
        let middle = (low + high) / 2;
        if blades[middle][1] > blades[low][1] {
            blades.swap(middle, low);
        } else if blades[high][1] > blades[middle][1] {
            blades.swap(middle, high);
        }
        if high - low == 2 {
            break;
        }
        let pivot = blades[middle][1];
        blades.swap(middle, high - 1);
        let (mut left, mut right) = (low, high - 1);
        loop {
            left += 1;
            while blades[left][1] > pivot {
                left += 1;
            }
            right -= 1;
            while pivot > blades[right][1] {
                right -= 1;
            }
            if right < left {
                break;
            }
            blades.swap(left, right);
        }
        blades.swap(high - 1, left);
        // Recurse into the smaller partition to bound stack depth.
        if left - low < high - left {
            sort_blades(&mut blades[low..left]);
            low = left + 1;
        } else {
            sort_blades(&mut blades[left + 1..=high]);
            high = left - 1;
        }
    }
}

/// Sort caller-owned, contiguous seven-float blade records by descending root Y.
/// The caller keeps the buffer exclusively writable for the synchronous call.
#[no_mangle]
pub unsafe extern "C" fn stone_meadow_sort(blades: *mut f32, count: i32) -> i32 {
    if count < 0 {
        return -1;
    }
    if count == 0 {
        return 0;
    }
    let count = count as usize;
    if blades.is_null()
        || !(blades as usize).is_multiple_of(std::mem::align_of::<Blade>())
        || count > isize::MAX as usize / std::mem::size_of::<Blade>()
    {
        return -1;
    }
    let blades = slice::from_raw_parts_mut(blades.cast::<Blade>(), count);
    // Validate before mutation: NaN breaks the partition's sentinel comparisons.
    if blades.iter().any(|blade| !blade[1].is_finite()) {
        return -1;
    }
    sort_blades(blades);
    0
}

#[cfg(test)]
mod sort_tests {
    use super::stone_meadow_sort;

    #[test]
    fn native_sort_rejects_invalid_buffers_without_mutation() {
        unsafe {
            assert_eq!(stone_meadow_sort(std::ptr::null_mut(), 0), 0);
            assert_eq!(stone_meadow_sort(std::ptr::null_mut(), 1), -1);
            assert_eq!(stone_meadow_sort(std::ptr::null_mut(), -1), -1);
            let mut blades = [[1.0f32; 7], [2.0; 7]];
            blades[1][1] = f32::NAN;
            let before = blades.map(|b| b.map(f32::to_bits));
            assert_eq!(stone_meadow_sort(blades.as_mut_ptr().cast(), 2), -1);
            assert_eq!(blades.map(|b| b.map(f32::to_bits)), before);
        }
    }
}

#[cfg(test)]
mod blade_tests {
    use super::{cover, cover_density_ceiling, cover_profile, floor, random, Snapshot};

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

    // Blade generation rejects a candidate on the density ceiling, then on
    // the open-ground density, before it samples edges, wear and props.
    #[test]
    fn density_stays_under_the_open_ground_density_and_its_ceiling() {
        for fields in [[1.0; 27], [5.0; 27], [0.0; 27]] {
            let mut fields = fields;
            fields[9..].fill(0.0);
            let s = Snapshot {
                fields,
                bases: Vec::new(),
            };
            for i in 0..4000 {
                let x = 40.0 + random(i as f64, 1.0, 2.0, 3.0);
                let y = 70.0 + random(i as f64, 4.0, 5.0, 6.0);
                let (wx, wy) = (x + i as f64 * 0.37, y - i as f64 * 0.11);
                for woodland in [false, true] {
                    let open = cover_profile(wx, wy, woodland);
                    assert!(open[0] * 1.000001 <= cover_density_ceiling(woodland));
                    let shaped = cover(&s, 40.0, 70.0, x, y, wx, wy, open);
                    assert!(shaped[0] <= open[0] * 1.000001, "{shaped:?} above {open:?}");
                }
            }
        }
    }
}
