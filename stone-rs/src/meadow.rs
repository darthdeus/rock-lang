//! Pure terrain sampling and meadow geometry over copied, double-precision snapshots.
//! The game supplies fields, prop footprints, layout and damage addressing.

use crate::native_ffi::NativeMeadowGrowth;
use std::{mem::size_of, slice};
use stone_meadow::{
    cover, cover_density_ceiling, cover_profile, edge_noise, field, noise, profile, random,
    Footprint,
};

/// rock-ffi-array stone-native MEADOW_WORD
type MeadowWord = [f64; 1];
const WORD_BYTES: usize = size_of::<MeadowWord>();

/// rock-ffi-array stone-native MEADOW_SNAPSHOT_HEADER
type SnapshotHeader = [f64; 27];
/// rock-ffi-array stone-native MEADOW_BASE_HEADER
type BaseHeader = [f64; 9];
/// rock-ffi-array stone-native MEADOW_POINT
type MeadowPoint = [f64; 2];
/// rock-ffi-array stone-native MEADOW_PARAMS
type MeadowParams = [f64; 15];
/// rock-ffi-array stone-native MEADOW_PROFILE
type MeadowProfile = [f64; 3];

const SNAPSHOT_HEADER: usize = size_of::<SnapshotHeader>() / WORD_BYTES;
const BASE_HEADER: usize = size_of::<BaseHeader>() / WORD_BYTES;
const POINT_LANES: usize = size_of::<MeadowPoint>() / WORD_BYTES;

struct Snapshot {
    fields: SnapshotHeader,
    bases: Vec<Footprint>,
}

fn parse_bases(data: &[f64]) -> Option<Vec<Footprint>> {
    let mut bases = Vec::new();
    let mut at = 0;
    while at < data.len() {
        if data.len() - at < BASE_HEADER {
            return None;
        }
        let n = data[at + 8];
        if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > 65536.0 {
            return None;
        }
        let end = at + BASE_HEADER + n as usize * POINT_LANES;
        if end > data.len() {
            return None;
        }
        let bounds = data[at..at + 4].try_into().ok()?;
        let footprint = if data[at + 7] != 0.0 {
            Footprint::circle(bounds, [data[at + 4], data[at + 5]], data[at + 6])
        } else {
            let (points, _) = data[at + BASE_HEADER..end].as_chunks::<POINT_LANES>();
            Footprint::polygon(bounds, points.to_vec())
        };
        bases.push(footprint);
        at = end;
    }
    Some(bases)
}
unsafe fn doubles(ptr: *const u8, len: i32) -> Option<Vec<f64>> {
    if len < 0
        || len % WORD_BYTES as i32 != 0
        || len > 8 * 1024 * 1024
        || (len > 0 && ptr.is_null())
    {
        return None;
    }
    if len == 0 {
        return Some(Vec::new());
    }
    Some(
        slice::from_raw_parts(ptr, len as usize)
            .as_chunks::<WORD_BYTES>()
            .0
            .iter()
            .map(|v| f64::from_ne_bytes(*v))
            .collect(),
    )
}
unsafe fn snapshot(ptr: *const u8, len: i32) -> Option<Snapshot> {
    let data = doubles(ptr, len)?;
    if data.len() < SNAPSHOT_HEADER {
        return None;
    }
    Some(Snapshot {
        fields: data[..SNAPSHOT_HEADER].try_into().unwrap(),
        bases: parse_bases(&data[SNAPSHOT_HEADER..])?,
    })
}
unsafe fn output3(out: *mut u8, p: MeadowProfile) -> i32 {
    if out.is_null() {
        return -1;
    }
    for (i, value) in p.iter().enumerate() {
        std::ptr::copy_nonoverlapping(
            value.to_ne_bytes().as_ptr(),
            out.add(i * WORD_BYTES),
            WORD_BYTES,
        );
    }
    0
}

/// rock-ffi stone-native
#[no_mangle]
pub extern "C" fn stone_terrain_noise(x: f64, y: f64) -> f64 {
    noise(x, y)
}
/// rock-ffi stone-native
#[no_mangle]
pub extern "C" fn stone_terrain_random(x: f64, y: f64, stream: f64, seed: f64) -> f64 {
    random(x, y, stream, seed)
}
/// rock-ffi stone-native
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
/// rock-ffi stone-native
#[no_mangle]
pub unsafe extern "C" fn stone_meadow_profile(x: f64, y: f64, output: *mut u8) -> i32 {
    output3(output, profile(x, y))
}
/// rock-ffi stone-native
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
    output: *mut u8,
) -> i32 {
    let Some(s) = snapshot(input, len) else {
        return -1;
    };
    if !(-0.5..1.5).contains(&(x - gx)) || !(-0.5..1.5).contains(&(y - gy)) {
        return -1;
    }
    let open = cover_profile(wx, wy, woodland != 0);
    output3(
        output,
        cover(&s.fields, &s.bases, [gx, gy], [x, y], [wx, wy], open),
    )
}
/// rock-ffi stone-native
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

// Params: layout origin XY, projected origin XY, tile XY, woodland, candidates,
// seed, projected tile scales XY, damage chunk XY, damage subdivision/side.
/// rock-ffi stone-native
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
    let Ok(p) = <&MeadowParams>::try_from(p.as_slice()) else {
        return -1;
    };
    if p.iter().any(|v| !v.is_finite())
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
        let profile = cover(
            &s.fields,
            &s.bases,
            [p[4], p[5]],
            [x, y],
            [p[0] + x, p[1] + y],
            open,
        );
        if profile[0] <= 0.0 || draw > profile[0] {
            continue;
        }
        let variation = random(sx, sy, 71.0 + stream, p[8]);
        let stature = random(sx, sy, 89.0 + stream, p[8]);
        let cell_x = (x * p[13]).floor() - p[11] * p[14];
        let cell_y = (y * p[13]).floor() - p[12] * p[14];
        let growth = NativeMeadowGrowth {
            root_x: ((x + y) * p[9] + p[2]) as f32,
            root_y: ((x - y) * p[10] + p[3]) as f32,
            width: (0.055 + random(sx, sy, 87.0 + stream, p[8]) * 0.055) as f32,
            height: (profile[1] * (0.72 + stature * 0.56)) as f32,
            variation: variation as f32,
            angle: random(sx, sy, 91.0 + stream, p[8]) as f32,
            clump: profile[2] as f32,
            cell: (cell_y * p[14] + cell_x) as i32,
        };
        std::ptr::copy_nonoverlapping(
            (&raw const growth).cast::<u8>(),
            output.add(count * size_of::<NativeMeadowGrowth>()),
            size_of::<NativeMeadowGrowth>(),
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
/// rock-ffi-array stone-native MEADOW_BLADE
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
/// rock-ffi stone-native
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
    // Before mutation: NaN breaks the partition's sentinel comparisons.
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
