use std::ops::Range;

use crate::ffi_array::{slice, slice_mut, Span};

const MAX_EXACT_FLOAT_TEXELS: usize = 1 << 24;
const LIGHT_ROW_TEXELS: usize = 2;
/// rock-ffi-array stone-native LIGHT_TABLE_TEXEL
type LightTexel = [f32; 4];
const LANES_PER_TEXEL: usize = std::mem::size_of::<LightTexel>() / std::mem::size_of::<f32>();

#[derive(Clone, Copy)]
struct TableShape {
    columns: usize,
    cells: usize,
    sources: usize,
    source_values: usize,
    header_texels: usize,
    epsilon: f64,
    cell_size: f64,
}

/// A source's position and radius in cell units and the cells its bounds cover.
struct Reach {
    x: f64,
    z: f64,
    radius: f64,
    cols: Range<usize>,
    rows: Range<usize>,
}

fn table_shape(size: i32, cell_size: i32, epsilon: f64, count: i32) -> Option<TableShape> {
    if size <= 0 || cell_size <= 0 || count < 0 || !epsilon.is_finite() || epsilon < 0.0 {
        return None;
    }
    let (size, cell_size_usize, sources) = (size as usize, cell_size as usize, count as usize);
    let columns = size.div_ceil(cell_size_usize);
    let cells = columns * columns;
    let header_texels = cells + sources * LIGHT_ROW_TEXELS;
    // Every index stored in the rgba32f table must stay an exact f32 integer.
    if cells >= MAX_EXACT_FLOAT_TEXELS || header_texels >= MAX_EXACT_FLOAT_TEXELS {
        return None;
    }
    Some(TableShape {
        columns,
        cells,
        sources,
        source_values: sources * LANES_PER_TEXEL,
        header_texels,
        epsilon,
        cell_size: cell_size as f64,
    })
}

fn texel(data: &mut [f32], index: usize) -> &mut [f32] {
    &mut data[index * LANES_PER_TEXEL..][..LANES_PER_TEXEL]
}

fn clamped_cell(value: f64, limit: usize) -> usize {
    (value.max(0.0) as usize).min(limit)
}

fn reach(shape: TableShape, position: &LightTexel) -> Option<Reach> {
    let x = position[0] as f64 / shape.cell_size;
    let z = position[2] as f64 / shape.cell_size;
    let radius = position[3] as f64 / shape.cell_size;
    if !x.is_finite()
        || !position[1].is_finite()
        || !z.is_finite()
        || !radius.is_finite()
        || radius < 0.0
    {
        return None;
    }
    let extent = radius + shape.epsilon;
    let span = |center: f64| {
        clamped_cell((center - extent).floor(), shape.columns)
            ..clamped_cell((center + extent).floor() + 1.0, shape.columns)
    };
    Some(Reach {
        x,
        z,
        radius,
        cols: span(x),
        rows: span(z),
    })
}

fn reaches(shape: TableShape, positions: &[f32]) -> Option<Vec<Reach>> {
    positions
        .as_chunks::<LANES_PER_TEXEL>()
        .0
        .iter()
        .map(|position| reach(shape, position))
        .collect()
}

/// Visits every (source, cell) link in source order, rows then columns,
/// until `visit` returns false; returns whether the walk completed.
fn for_each_link(
    shape: TableShape,
    reaches: &[Reach],
    mut visit: impl FnMut(usize, usize) -> bool,
) -> bool {
    for (source, reach) in reaches.iter().enumerate() {
        let radius = reach.radius + shape.epsilon;
        for row in reach.rows.clone() {
            let row_f = row as f64;
            let dz = (row_f - reach.z).max(reach.z - (row_f + 1.0)).max(0.0);
            for col in reach.cols.clone() {
                let col_f = col as f64;
                let dx = (col_f - reach.x).max(reach.x - (col_f + 1.0)).max(0.0);
                if dx * dx + dz * dz <= radius * radius && !visit(source, row * shape.columns + col)
                {
                    return false;
                }
            }
        }
    }
    true
}

/// rock-ffi stone-native
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
    let (Some(counts_span), Some(positions_span)) = (
        Span::of(counts.cast_const(), shape.cells),
        Span::of(positions, shape.source_values),
    ) else {
        return -1;
    };
    if counts_span.overlaps(positions_span) {
        return -1;
    }
    let Some(reaches) = reaches(shape, unsafe { slice(positions, shape.source_values) }) else {
        return -1;
    };
    let counts = unsafe { slice_mut(counts, shape.cells) };
    counts.fill(0);

    let max_links = MAX_EXACT_FLOAT_TEXELS - 1 - shape.header_texels;
    let mut links = 0usize;
    let measured = for_each_link(shape, &reaches, |_, cell| {
        let Some(next) = counts[cell].checked_add(1) else {
            return false;
        };
        counts[cell] = next;
        links += 1;
        links <= max_links
    });
    if measured {
        links as i64
    } else {
        -1
    }
}

/// rock-ffi stone-native
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
    let Ok(data_capacity) = usize::try_from(data_len) else {
        return false;
    };
    let (sources, values, cells) = (shape.sources, shape.source_values, shape.cells);
    let inputs = [
        Span::of(positions, values),
        Span::of(colors, values),
        Span::of(owners, sources),
        Span::of(source_rows, sources),
        Span::of(source_slots, sources),
        Span::of(counts, cells),
    ];
    let Some(inputs) = inputs.into_iter().collect::<Option<Vec<_>>>() else {
        return false;
    };
    let Some(offsets_span) = Span::of(offsets.cast_const(), cells) else {
        return false;
    };
    let counts = unsafe { slice(counts, cells) };
    if counts.iter().any(|&count| count < 0) {
        return false;
    }
    let texels = shape.header_texels + counts.iter().map(|&count| count as usize).sum::<usize>();
    let required_floats = texels * LANES_PER_TEXEL;
    if texels >= MAX_EXACT_FLOAT_TEXELS || data_capacity < required_floats {
        return false;
    }
    let Some(data_span) = Span::of(data.cast_const(), required_floats) else {
        return false;
    };
    let positions = unsafe { slice(positions, values) };
    let Some(reaches) = reaches(shape, positions) else {
        return false;
    };
    // Outputs must be disjoint from all inputs and each other; read-only inputs may overlap.
    if offsets_span.overlaps(data_span)
        || inputs
            .iter()
            .any(|input| input.overlaps(offsets_span) || input.overlaps(data_span))
    {
        return false;
    }
    let (colors, owners, source_rows, source_slots) = unsafe {
        (
            slice(colors, values),
            slice(owners, sources),
            slice(source_rows, sources),
            slice(source_slots, sources),
        )
    };
    let offsets = unsafe { slice_mut(offsets, cells) };
    let data = unsafe { slice_mut(data, required_floats) };

    let mut cursor = shape.header_texels;
    for (cell, &count) in counts.iter().enumerate() {
        offsets[cell] = cursor as i32;
        texel(data, cell)[..2].copy_from_slice(&[cursor as f32, count as f32]);
        cursor += count as usize;
    }

    for (source, position) in positions
        .as_chunks::<LANES_PER_TEXEL>()
        .0
        .iter()
        .enumerate()
    {
        let row_texel = cells + source * LIGHT_ROW_TEXELS;
        let row = source_rows[source];
        if row >= 0 {
            texel(data, row_texel).copy_from_slice(&[
                owners[source] as f32,
                row as f32,
                source_slots[source] as f32,
                position[3],
            ]);
        } else {
            texel(data, row_texel).copy_from_slice(position);
        }
        texel(data, row_texel + 1)
            .copy_from_slice(&colors[source * LANES_PER_TEXEL..][..LANES_PER_TEXEL]);
    }

    let packed = for_each_link(shape, &reaches, |source, cell| {
        let write = offsets[cell];
        if write < 0 || write as usize >= texels {
            return false;
        }
        data[write as usize * LANES_PER_TEXEL] = (cells + source * LIGHT_ROW_TEXELS) as f32;
        offsets[cell] = write + 1;
        true
    });

    cursor = shape.header_texels;
    packed
        && counts.iter().zip(offsets.iter()).all(|(&count, &offset)| {
            cursor += count as usize;
            offset as usize == cursor
        })
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
        let positions = [4.0, 1.0, 4.0, 0.0, 5.0, 1.0, 2.0, 0.0];
        let colors = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];
        let owners = [41, 22];
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
