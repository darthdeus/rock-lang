//! Native packing for MAG's per-cell moving-light table.
//!
//! Inputs and outputs stay caller-owned. The caller measures into `counts`,
//! retains its full data-buffer capacity, and packs only the populated table
//! region; unused texel lanes and row padding are deliberately left as-is.

use std::mem::{align_of, size_of};

const MAX_EXACT_FLOAT_TEXELS: usize = 1 << 24;
const LIGHT_ROW_TEXELS: usize = 2;
const LANES_PER_TEXEL: usize = 4;

#[derive(Clone, Copy)]
struct TableShape {
    columns: usize,
    rows: usize,
    cells: usize,
    sources: usize,
    source_values: usize,
    header_texels: usize,
    epsilon: f64,
    cell_size: f64,
}

#[derive(Clone, Copy)]
struct SourceGeometry {
    x: f64,
    z: f64,
    radius: f64,
    min_col: usize,
    end_col: usize,
    min_row: usize,
    end_row: usize,
}

fn table_shape(size: i32, cell_size: i32, epsilon: f64, count: i32) -> Option<TableShape> {
    if size <= 0 || cell_size <= 0 || count < 0 || !epsilon.is_finite() || epsilon < 0.0 {
        return None;
    }

    let size = usize::try_from(size).ok()?;
    let cell_size_usize = usize::try_from(cell_size).ok()?;
    let sources = usize::try_from(count).ok()?;
    let columns = size.checked_add(cell_size_usize.checked_sub(1)?)? / cell_size_usize;
    let rows = columns;
    let cells = columns.checked_mul(rows)?;
    let source_values = sources.checked_mul(LANES_PER_TEXEL)?;
    let source_texels = sources.checked_mul(LIGHT_ROW_TEXELS)?;
    let header_texels = cells.checked_add(source_texels)?;

    // Every index stored in the rgba32f table must remain an exact f32 integer.
    if cells >= MAX_EXACT_FLOAT_TEXELS || header_texels >= MAX_EXACT_FLOAT_TEXELS {
        return None;
    }

    Some(TableShape {
        columns,
        rows,
        cells,
        sources,
        source_values,
        header_texels,
        epsilon,
        cell_size: cell_size as f64,
    })
}

fn pointer_range<T>(pointer: *const T, len: usize) -> Option<(usize, usize)> {
    if len == 0 {
        return Some((0, 0));
    }
    if pointer.is_null() || (pointer as usize) % align_of::<T>() != 0 {
        return None;
    }
    let bytes = len.checked_mul(size_of::<T>())?;
    let start = pointer as usize;
    Some((start, start.checked_add(bytes)?))
}

fn ranges_overlap(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

fn floor_clamped(value: f64, limit: usize) -> usize {
    let value = value.floor();
    if value <= 0.0 {
        0
    } else if value >= limit as f64 {
        limit
    } else {
        value as usize
    }
}

fn floor_plus_one_clamped(value: f64, limit: usize) -> usize {
    let end = value.floor() + 1.0;
    if end <= 0.0 {
        0
    } else if end >= limit as f64 {
        limit
    } else {
        end as usize
    }
}

fn source_geometry(
    shape: TableShape,
    positions: *const f32,
    source: usize,
) -> Option<SourceGeometry> {
    let start = source.checked_mul(LANES_PER_TEXEL)?;
    let x = unsafe { *positions.add(start) } as f64 / shape.cell_size;
    let y = unsafe { *positions.add(start + 1) } as f64;
    let z = unsafe { *positions.add(start + 2) } as f64 / shape.cell_size;
    let radius = unsafe { *positions.add(start + 3) } as f64 / shape.cell_size;
    if !x.is_finite() || !y.is_finite() || !z.is_finite() || !radius.is_finite() || radius < 0.0 {
        return None;
    }

    Some(SourceGeometry {
        x,
        z,
        radius,
        min_col: floor_clamped(x - radius - shape.epsilon, shape.columns),
        end_col: floor_plus_one_clamped(x + radius + shape.epsilon, shape.columns),
        min_row: floor_clamped(z - radius - shape.epsilon, shape.rows),
        end_row: floor_plus_one_clamped(z + radius + shape.epsilon, shape.rows),
    })
}

// A source reaches a cell when the closed-circle and axis-aligned-cell regions
// overlap; tangencies count as hits.
fn source_reaches_cell(source: SourceGeometry, col: usize, row: usize, epsilon: f64) -> bool {
    let radius = source.radius + epsilon;
    let col = col as f64;
    let row = row as f64;
    let dx = (col - source.x).max(source.x - (col + 1.0)).max(0.0);
    let dz = (row - source.z).max(source.z - (row + 1.0)).max(0.0);
    dx * dx + dz * dz <= radius * radius
}

fn validate_positions(shape: TableShape, positions: *const f32) -> bool {
    if shape.sources > 0 && pointer_range(positions, shape.source_values).is_none() {
        return false;
    }
    for source in 0..shape.sources {
        if source_geometry(shape, positions, source).is_none() {
            return false;
        }
    }
    true
}

/// Count each source's links to the receiving cells.
///
/// `counts` must point to at least `ceil(size / cell_size)^2` writable i32s.
/// Returns the total link count, or -1 for an invalid shape, pointer, or table
/// whose texel indices would not be exactly representable as f32 integers.
#[no_mangle]
pub extern "C" fn stone_light_table_measure(
    size: i32,
    cell_size: i32,
    epsilon: f64,
    count: i32,
    positions: *const f32,
    counts: *mut i32,
) -> i64 {
    let Some(shape) = table_shape(size, cell_size, epsilon, count) else {
        return -1;
    };
    let Some(counts_range) = pointer_range(counts.cast_const(), shape.cells) else {
        return -1;
    };
    let Some(positions_range) = pointer_range(positions, shape.source_values) else {
        return -1;
    };
    if ranges_overlap(counts_range, positions_range) || !validate_positions(shape, positions) {
        return -1;
    }

    for cell in 0..shape.cells {
        unsafe {
            *counts.add(cell) = 0;
        }
    }

    let max_links = MAX_EXACT_FLOAT_TEXELS - 1 - shape.header_texels;
    let mut links = 0usize;
    for source_index in 0..shape.sources {
        let Some(source) = source_geometry(shape, positions, source_index) else {
            return -1;
        };
        for row in source.min_row..source.end_row {
            for col in source.min_col..source.end_col {
                if !source_reaches_cell(source, col, row, shape.epsilon) {
                    continue;
                }
                let cell = row * shape.columns + col;
                let count_ptr = unsafe { counts.add(cell) };
                let previous = unsafe { *count_ptr };
                let Some(next) = previous.checked_add(1) else {
                    return -1;
                };
                links = match links.checked_add(1) {
                    Some(links) if links <= max_links => links,
                    _ => return -1,
                };
                unsafe {
                    *count_ptr = next;
                }
            }
        }
    }

    i64::try_from(links).unwrap_or(-1)
}

/// Pack cell headers, source texels, and source links into the caller's buffer.
///
/// `counts` and `offsets` must each hold `ceil(size / cell_size)^2` i32s;
/// source arrays hold `count` entries (or four floats per source); `data_len`
/// is the full f32 capacity of `data`. Only the active table region is written.
/// Returns false for invalid inputs or inconsistent counts.
#[no_mangle]
pub extern "C" fn stone_light_table_pack(
    size: i32,
    cell_size: i32,
    epsilon: f64,
    count: i32,
    positions: *const f32,
    colors: *const f32,
    owners: *const i32,
    source_rows: *const i32,
    source_slots: *const i32,
    counts: *const i32,
    offsets: *mut i32,
    data: *mut f32,
    data_len: i64,
) -> bool {
    let Some(shape) = table_shape(size, cell_size, epsilon, count) else {
        return false;
    };
    if data_len < 0 {
        return false;
    }

    let Some(positions_range) = pointer_range(positions, shape.source_values) else {
        return false;
    };
    let Some(colors_range) = pointer_range(colors, shape.source_values) else {
        return false;
    };
    let Some(owners_range) = pointer_range(owners, shape.sources) else {
        return false;
    };
    let Some(source_rows_range) = pointer_range(source_rows, shape.sources) else {
        return false;
    };
    let Some(source_slots_range) = pointer_range(source_slots, shape.sources) else {
        return false;
    };
    let Some(counts_range) = pointer_range(counts, shape.cells) else {
        return false;
    };
    let Some(offsets_range) = pointer_range(offsets.cast_const(), shape.cells) else {
        return false;
    };

    let mut links = 0usize;
    for cell in 0..shape.cells {
        let cell_count = unsafe { *counts.add(cell) };
        if cell_count < 0 {
            return false;
        }
        let Some(total) = links.checked_add(cell_count as usize) else {
            return false;
        };
        links = total;
    }
    let Some(texels) = shape.header_texels.checked_add(links) else {
        return false;
    };
    if texels >= MAX_EXACT_FLOAT_TEXELS {
        return false;
    }
    let Some(required_floats) = texels.checked_mul(LANES_PER_TEXEL) else {
        return false;
    };
    let Ok(data_capacity) = usize::try_from(data_len) else {
        return false;
    };
    if data_capacity < required_floats {
        return false;
    }
    let Some(data_range) = pointer_range(data, required_floats) else {
        return false;
    };
    if !validate_positions(shape, positions) {
        return false;
    }

    // Outputs must be disjoint from all inputs and from each other. The caller
    // may pass overlapping read-only source arrays, but not writable storage.
    let inputs = [
        positions_range,
        colors_range,
        owners_range,
        source_rows_range,
        source_slots_range,
        counts_range,
    ];
    for input in inputs {
        if ranges_overlap(input, offsets_range) || ranges_overlap(input, data_range) {
            return false;
        }
    }
    if ranges_overlap(offsets_range, data_range) {
        return false;
    }

    let mut cursor = shape.header_texels;
    for cell in 0..shape.cells {
        let cell_count = unsafe { *counts.add(cell) as usize };
        unsafe {
            *offsets.add(cell) = cursor as i32;
            *data.add(cell * LANES_PER_TEXEL) = cursor as f32;
            *data.add(cell * LANES_PER_TEXEL + 1) = cell_count as f32;
        }
        cursor += cell_count;
    }

    for source_index in 0..shape.sources {
        let start = source_index * LANES_PER_TEXEL;
        let texel = shape.cells + source_index * LIGHT_ROW_TEXELS;
        let row = unsafe { *source_rows.add(source_index) };
        if row >= 0 {
            unsafe {
                *data.add(texel * LANES_PER_TEXEL) = *owners.add(source_index) as f32;
                *data.add(texel * LANES_PER_TEXEL + 1) = row as f32;
                *data.add(texel * LANES_PER_TEXEL + 2) = *source_slots.add(source_index) as f32;
                *data.add(texel * LANES_PER_TEXEL + 3) = *positions.add(start + 3);
            }
        } else {
            for lane in 0..LANES_PER_TEXEL {
                unsafe {
                    *data.add(texel * LANES_PER_TEXEL + lane) = *positions.add(start + lane);
                }
            }
        }
        for lane in 0..LANES_PER_TEXEL {
            unsafe {
                *data.add((texel + 1) * LANES_PER_TEXEL + lane) = *colors.add(start + lane);
            }
        }
    }

    for source_index in 0..shape.sources {
        let Some(source) = source_geometry(shape, positions, source_index) else {
            return false;
        };
        let source_texel = shape.cells + source_index * LIGHT_ROW_TEXELS;
        for row in source.min_row..source.end_row {
            for col in source.min_col..source.end_col {
                if !source_reaches_cell(source, col, row, shape.epsilon) {
                    continue;
                }
                let cell = row * shape.columns + col;
                let cursor_ptr = unsafe { offsets.add(cell) };
                let write_texel = unsafe { *cursor_ptr };
                if write_texel < 0 || write_texel as usize >= texels {
                    return false;
                }
                unsafe {
                    *data.add(write_texel as usize * LANES_PER_TEXEL) = source_texel as f32;
                    *cursor_ptr = write_texel + 1;
                }
            }
        }
    }

    // Reject a counts buffer that did not come from this exact source snapshot.
    // Per-link writes above are still bounded by the validated table capacity.
    cursor = shape.header_texels;
    for cell in 0..shape.cells {
        cursor += unsafe { *counts.add(cell) as usize };
        if unsafe { *offsets.add(cell) as usize } != cursor {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENTINEL: f32 = -123.0;

    fn texel(data: &[f32], index: usize) -> [f32; LANES_PER_TEXEL] {
        let start = index * LANES_PER_TEXEL;
        [
            data[start],
            data[start + 1],
            data[start + 2],
            data[start + 3],
        ]
    }

    #[test]
    fn measure_and_pack_preserve_tangent_links_partial_edge_and_source_order() {
        // Size 5 with 4-unit cells creates a 2x2 table. Source 0 sits exactly
        // where all four cells meet; source 1 touches the partial right cell.
        let positions = [4.0, 1.0, 4.0, 0.0, 5.0, 1.0, 2.0, 0.0];
        let colors = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];
        let owners = [41, 22];
        // Deliberately descending rows: links retain input insertion order.
        let source_rows = [9, 2];
        let source_slots = [6, 3];
        let mut counts = [-1; 4];

        let links =
            stone_light_table_measure(5, 4, 0.001, 2, positions.as_ptr(), counts.as_mut_ptr());
        assert_eq!(links, 5);
        assert_eq!(counts, [1, 2, 1, 1]);

        let mut offsets = [-1; 4];
        let mut data = vec![SENTINEL; 60]; // 13 active texels plus retained capacity.
        assert!(stone_light_table_pack(
            5,
            4,
            0.001,
            2,
            positions.as_ptr(),
            colors.as_ptr(),
            owners.as_ptr(),
            source_rows.as_ptr(),
            source_slots.as_ptr(),
            counts.as_ptr(),
            offsets.as_mut_ptr(),
            data.as_mut_ptr(),
            data.len() as i64
        ));

        assert_eq!(offsets, [9, 11, 12, 13]);
        assert_eq!(texel(&data, 0), [8.0, 1.0, SENTINEL, SENTINEL]);
        assert_eq!(texel(&data, 1), [9.0, 2.0, SENTINEL, SENTINEL]);
        assert_eq!(texel(&data, 2), [11.0, 1.0, SENTINEL, SENTINEL]);
        assert_eq!(texel(&data, 3), [12.0, 1.0, SENTINEL, SENTINEL]);

        assert_eq!(texel(&data, 4), [41.0, 9.0, 6.0, 0.0]);
        assert_eq!(texel(&data, 5), [0.1, 0.2, 0.3, 0.4]);
        assert_eq!(texel(&data, 6), [22.0, 2.0, 3.0, 0.0]);
        assert_eq!(texel(&data, 7), [0.5, 0.6, 0.7, 0.8]);

        // Cell 1 contains both sources, still in source-array order despite
        // their reversed source_rows values. Other cells contain only source 0.
        assert_eq!(texel(&data, 8), [4.0, SENTINEL, SENTINEL, SENTINEL]);
        assert_eq!(texel(&data, 9), [4.0, SENTINEL, SENTINEL, SENTINEL]);
        assert_eq!(texel(&data, 10), [6.0, SENTINEL, SENTINEL, SENTINEL]);
        assert_eq!(texel(&data, 11), [4.0, SENTINEL, SENTINEL, SENTINEL]);
        assert_eq!(texel(&data, 12), [4.0, SENTINEL, SENTINEL, SENTINEL]);
        assert!(data[13 * LANES_PER_TEXEL..]
            .iter()
            .all(|value| *value == SENTINEL));
    }

    #[test]
    fn moving_source_packs_position_texel_and_leaves_padding_untouched() {
        let positions = [2.0, 3.0, 2.0, 1.0];
        let colors = [0.25, 0.5, 0.75, 1.0];
        let owners = [17];
        let source_rows = [-1];
        let source_slots = [5];
        let mut counts = [0];
        assert_eq!(
            stone_light_table_measure(4, 4, 0.001, 1, positions.as_ptr(), counts.as_mut_ptr()),
            1
        );
        assert_eq!(counts, [1]);

        let mut offsets = [0];
        let mut data = vec![SENTINEL; 20];
        assert!(stone_light_table_pack(
            4,
            4,
            0.001,
            1,
            positions.as_ptr(),
            colors.as_ptr(),
            owners.as_ptr(),
            source_rows.as_ptr(),
            source_slots.as_ptr(),
            counts.as_ptr(),
            offsets.as_mut_ptr(),
            data.as_mut_ptr(),
            data.len() as i64
        ));
        assert_eq!(offsets, [4]);
        assert_eq!(texel(&data, 1), [2.0, 3.0, 2.0, 1.0]);
        assert_eq!(texel(&data, 2), colors);
        assert_eq!(texel(&data, 3), [1.0, SENTINEL, SENTINEL, SENTINEL]);
        assert!(data[16..].iter().all(|value| *value == SENTINEL));
    }

    #[test]
    fn rejects_invalid_shapes_and_short_data_capacity() {
        let mut counts = [0; 1];
        assert_eq!(
            stone_light_table_measure(0, 4, 0.001, 0, std::ptr::null(), counts.as_mut_ptr()),
            -1
        );
        assert_eq!(
            stone_light_table_measure(4, 0, 0.001, 0, std::ptr::null(), counts.as_mut_ptr()),
            -1
        );
        assert_eq!(
            stone_light_table_measure(4, 4, f64::NAN, 0, std::ptr::null(), counts.as_mut_ptr()),
            -1
        );
        assert_eq!(
            stone_light_table_measure(4, 4, 0.001, 0, std::ptr::null(), std::ptr::null_mut()),
            -1
        );

        let mut offsets = [0];
        let mut data = [SENTINEL; 4];
        assert!(!stone_light_table_pack(
            4,
            4,
            0.001,
            0,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            counts.as_ptr(),
            offsets.as_mut_ptr(),
            data.as_mut_ptr(),
            3
        ));
    }
}
