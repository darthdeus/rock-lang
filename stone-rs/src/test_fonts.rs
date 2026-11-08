//! TrueType fonts built in code for tests that need a font with a specific
//! property the embedded DroidSans fallback lacks. The bytes are
//! deterministic, so a test may compare them or write them to disk and expect
//! the same file every run.
//!
//! Only the tables `ttf_parser::Face` reads are written: head, hhea, maxp
//! (version 0.5), hmtx, loca, glyf and a single format 12 Unicode cmap. Glyph
//! outlines are axis-aligned rectangles, one contour per horizontal run of
//! set cells, wound clockwise as TrueType outer contours are.

use std::path::PathBuf;

const UNITS_PER_EM: u16 = 1000;

/// Font units per pixel-font cell: 8 cells per em, so every size that is a
/// multiple of 8px puts each cell on whole pixels (48px: 6 pixels a cell).
pub(crate) const PIXEL_CELL: i16 = 125;
const PIXEL_COLS: usize = 5;
const PIXEL_ROWS: usize = 7;

struct Glyph {
    advance: u16,
    /// Rectangles (x0, y0, x1, y1) in font units, y up.
    rects: Vec<(i16, i16, i16, i16)>,
}

/// A pixel font covering printable ASCII. Every glyph is a 5x7 cell bitmap
/// whose left column is always set, so each one carries ink; the remaining
/// cells come from a fixed hash of the codepoint. The shapes are not letters:
/// tests use this font for its grid, not its legibility.
pub(crate) fn pixel_font() -> Vec<u8> {
    let mut glyphs = vec![Glyph {
        advance: 0,
        rects: Vec::new(),
    }];
    let mut cmap = Vec::new();
    let advance = (PIXEL_COLS as i16 + 1) * PIXEL_CELL;
    for cp in 0x20u32..=0x7E {
        let rects = if cp == 0x20 {
            Vec::new()
        } else {
            pixel_runs(&pixel_bitmap(cp))
        };
        cmap.push((cp, glyphs.len() as u16));
        glyphs.push(Glyph {
            advance: advance as u16,
            rects,
        });
    }
    build_font(&glyphs, &cmap, 7 * PIXEL_CELL, -2 * PIXEL_CELL)
}

/// A font with one filled symbol at U+1F600 and no other mapping: ASCII
/// resolves to .notdef. Its codepoint lies past the BMP, so only the format 12
/// cmap reaches it.
pub(crate) fn symbol_only_font() -> Vec<u8> {
    let glyphs = vec![
        Glyph {
            advance: 0,
            rects: Vec::new(),
        },
        Glyph {
            advance: 1000,
            rects: vec![(100, -100, 900, 700)],
        },
    ];
    build_font(&glyphs, &[(0x1F600, 1)], 800, -200)
}

/// Writes `bytes` once under the system temp directory, named by content, and
/// returns the path. Tests that hand Stone a font path (reload) share it.
pub(crate) fn font_file(name: &str, bytes: &[u8]) -> PathBuf {
    let digest = bytes.iter().fold(0xcbf29ce484222325u64, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    });
    let dir = std::env::temp_dir().join("stone-rs-test-fonts");
    std::fs::create_dir_all(&dir).expect("test font directory");
    let path = dir.join(format!("{name}-{digest:016x}.ttf"));
    if std::fs::read(&path).ok().as_deref() != Some(bytes) {
        let staging = dir.join(format!("{name}-{digest:016x}.{}.tmp", std::process::id()));
        std::fs::write(&staging, bytes).expect("write test font");
        std::fs::rename(&staging, &path).expect("publish test font");
    }
    path
}

fn pixel_bitmap(cp: u32) -> [[bool; PIXEL_COLS]; PIXEL_ROWS] {
    let mut bits = [[false; PIXEL_COLS]; PIXEL_ROWS];
    for (row, line) in bits.iter_mut().enumerate() {
        for (col, cell) in line.iter_mut().enumerate() {
            let h = (cp.wrapping_mul(2654435761) ^ ((row * 7 + col) as u32).wrapping_mul(40503))
                .rotate_left(row as u32 + 3);
            *cell = col == 0 || h % 3 != 0;
        }
    }
    bits
}

/// Row 0 is the top row; it sits PIXEL_ROWS cells above the baseline.
fn pixel_runs(bits: &[[bool; PIXEL_COLS]; PIXEL_ROWS]) -> Vec<(i16, i16, i16, i16)> {
    let mut rects = Vec::new();
    for (row, line) in bits.iter().enumerate() {
        let y1 = (PIXEL_ROWS - row) as i16 * PIXEL_CELL;
        let mut col = 0;
        while col < PIXEL_COLS {
            if !line[col] {
                col += 1;
                continue;
            }
            let start = col;
            while col < PIXEL_COLS && line[col] {
                col += 1;
            }
            rects.push((
                start as i16 * PIXEL_CELL,
                y1 - PIXEL_CELL,
                col as i16 * PIXEL_CELL,
                y1,
            ));
        }
    }
    rects
}

fn bbox(rects: &[(i16, i16, i16, i16)]) -> (i16, i16, i16, i16) {
    rects
        .iter()
        .fold((i16::MAX, i16::MAX, i16::MIN, i16::MIN), |b, r| {
            (b.0.min(r.0), b.1.min(r.1), b.2.max(r.2), b.3.max(r.3))
        })
}

fn glyf_entry(glyph: &Glyph) -> Vec<u8> {
    if glyph.rects.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let (x_min, y_min, x_max, y_max) = bbox(&glyph.rects);
    put_i16(&mut out, glyph.rects.len() as i16);
    for v in [x_min, y_min, x_max, y_max] {
        put_i16(&mut out, v);
    }
    for i in 0..glyph.rects.len() {
        put_u16(&mut out, (i * 4 + 3) as u16);
    }
    put_u16(&mut out, 0); // instructionLength
    let points: Vec<(i16, i16)> = glyph
        .rects
        .iter()
        .flat_map(|&(x0, y0, x1, y1)| [(x0, y0), (x0, y1), (x1, y1), (x1, y0)])
        .collect();
    // Flag 0x01: on-curve; x and y are full int16 deltas.
    out.extend(std::iter::repeat_n(0x01u8, points.len()));
    let (mut px, mut py) = (0i16, 0i16);
    let mut ys = Vec::new();
    for &(x, y) in &points {
        put_i16(&mut out, x - px);
        put_i16(&mut ys, y - py);
        (px, py) = (x, y);
    }
    out.extend(ys);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

fn build_font(glyphs: &[Glyph], cmap: &[(u32, u16)], ascender: i16, descender: i16) -> Vec<u8> {
    let num_glyphs = glyphs.len() as u16;
    let mut glyf = Vec::new();
    let mut loca = Vec::new();
    for glyph in glyphs {
        put_u32(&mut loca, glyf.len() as u32);
        glyf.extend(glyf_entry(glyph));
    }
    put_u32(&mut loca, glyf.len() as u32);

    let all: Vec<_> = glyphs
        .iter()
        .flat_map(|g| g.rects.iter().copied())
        .collect();
    let (x_min, y_min, x_max, y_max) = if all.is_empty() {
        (0, 0, 0, 0)
    } else {
        bbox(&all)
    };
    let advance_max = glyphs.iter().map(|g| g.advance).max().unwrap_or(0);

    let mut head = Vec::new();
    put_u32(&mut head, 0x0001_0000); // version
    put_u32(&mut head, 0x0001_0000); // fontRevision
    put_u32(&mut head, 0); // checkSumAdjustment
    put_u32(&mut head, 0x5F0F_3CF5); // magicNumber
    put_u16(&mut head, 0x000B); // flags
    put_u16(&mut head, UNITS_PER_EM);
    head.extend([0u8; 16]); // created, modified
    for v in [x_min, y_min, x_max, y_max] {
        put_i16(&mut head, v);
    }
    put_u16(&mut head, 0); // macStyle
    put_u16(&mut head, 8); // lowestRecPPEM
    put_i16(&mut head, 2); // fontDirectionHint
    put_i16(&mut head, 1); // indexToLocFormat: long offsets
    put_i16(&mut head, 0); // glyphDataFormat

    let mut hhea = Vec::new();
    put_u32(&mut hhea, 0x0001_0000);
    put_i16(&mut hhea, ascender);
    put_i16(&mut hhea, descender);
    put_i16(&mut hhea, 0); // lineGap
    put_u16(&mut hhea, advance_max);
    put_i16(&mut hhea, x_min); // minLeftSideBearing
    put_i16(&mut hhea, 0); // minRightSideBearing
    put_i16(&mut hhea, x_max); // xMaxExtent
    put_i16(&mut hhea, 1); // caretSlopeRise
    put_i16(&mut hhea, 0); // caretSlopeRun
    hhea.extend([0u8; 10]); // caretOffset, 4 reserved
    put_i16(&mut hhea, 0); // metricDataFormat
    put_u16(&mut hhea, num_glyphs);

    let mut maxp = Vec::new();
    put_u32(&mut maxp, 0x0000_5000);
    put_u16(&mut maxp, num_glyphs);

    let mut hmtx = Vec::new();
    for glyph in glyphs {
        put_u16(&mut hmtx, glyph.advance);
        put_i16(
            &mut hmtx,
            if glyph.rects.is_empty() {
                0
            } else {
                bbox(&glyph.rects).0
            },
        );
    }

    let mut groups: Vec<(u32, u32, u32)> = Vec::new();
    for &(cp, gid) in cmap {
        match groups.last_mut() {
            Some(last) if last.1 + 1 == cp && last.2 + (cp - last.0) == gid as u32 => last.1 = cp,
            _ => groups.push((cp, cp, gid as u32)),
        }
    }
    let mut cmap_table = Vec::new();
    put_u16(&mut cmap_table, 0); // version
    put_u16(&mut cmap_table, 1); // numTables
    put_u16(&mut cmap_table, 3); // platform: Windows
    put_u16(&mut cmap_table, 10); // encoding: Unicode full repertoire
    put_u32(&mut cmap_table, 12); // subtable offset
    put_u16(&mut cmap_table, 12); // format
    put_u16(&mut cmap_table, 0);
    put_u32(&mut cmap_table, 16 + 12 * groups.len() as u32);
    put_u32(&mut cmap_table, 0); // language
    put_u32(&mut cmap_table, groups.len() as u32);
    for (start, end, gid) in groups {
        put_u32(&mut cmap_table, start);
        put_u32(&mut cmap_table, end);
        put_u32(&mut cmap_table, gid);
    }

    // Table records must be sorted by tag.
    let tables: [(&[u8; 4], Vec<u8>); 7] = [
        (b"cmap", cmap_table),
        (b"glyf", glyf),
        (b"head", head),
        (b"hhea", hhea),
        (b"hmtx", hmtx),
        (b"loca", loca),
        (b"maxp", maxp),
    ];
    let mut out = Vec::new();
    put_u32(&mut out, 0x0001_0000);
    let n = tables.len() as u16;
    let entry_selector = 15 - n.leading_zeros() as u16;
    let search_range = (1u16 << entry_selector) * 16;
    put_u16(&mut out, n);
    put_u16(&mut out, search_range);
    put_u16(&mut out, entry_selector);
    put_u16(&mut out, n * 16 - search_range);
    let mut offset = 12 + 16 * tables.len() as u32;
    let mut body = Vec::new();
    for (tag, data) in &tables {
        out.extend_from_slice(*tag);
        put_u32(&mut out, checksum(data));
        put_u32(&mut out, offset);
        put_u32(&mut out, data.len() as u32);
        body.extend_from_slice(data);
        while body.len() % 4 != 0 {
            body.push(0);
        }
        offset = 12 + 16 * tables.len() as u32 + body.len() as u32;
    }
    out.extend(body);
    out
}

fn checksum(data: &[u8]) -> u32 {
    data.chunks(4).fold(0u32, |sum, chunk| {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum.wrapping_add(u32::from_be_bytes(word))
    })
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_i16(out: &mut Vec<u8>, v: i16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_fonts_parse_with_their_declared_coverage() {
        let pixel = pixel_font();
        let face = ttf_parser::Face::parse(&pixel, 0).expect("the pixel font parses");
        assert_eq!(face.units_per_em(), UNITS_PER_EM);
        let x = face.glyph_index('X').expect("printable ASCII is mapped");
        let bounds = face.glyph_bounding_box(x).expect("X has an outline");
        assert_eq!(bounds.y_max, 7 * PIXEL_CELL, "cap height is seven cells");
        assert!(face.glyph_index('\u{1F600}').is_none());

        let symbol = symbol_only_font();
        let face = ttf_parser::Face::parse(&symbol, 0).expect("the symbol font parses");
        assert!(face.glyph_index('X').is_none(), "no ASCII mapping");
        let grin = face.glyph_index('\u{1F600}').expect("the symbol is mapped");
        assert!(
            face.glyph_bounding_box(grin).is_some(),
            "and has an outline"
        );
    }
}
