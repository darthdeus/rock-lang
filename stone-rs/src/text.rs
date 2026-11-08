#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_float, c_int, c_uint, CStr};
use std::rc::Rc;

use crate::glyph_raster::{
    self, Face, FaceSet, FontUnitMetrics, GlyphUnitMetrics, RasterFont, Synth,
};
use crate::handles::Class;
use crate::{track_text_draw_call, with_gl, SHADERS};
use crate::{LoadedImage, IMAGES};

/// Glyph info for the font atlas, indexed by the Rock text slot.
#[derive(Clone, Copy, Default)]
struct GlyphInfo {
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32, // UV coordinates in atlas
    width: f32,
    height: f32, // Glyph size in pixels
    bearing_x: f32,
    bearing_y: f32, // Glyph bearing
    advance: f32,   // Horizontal advance in pixels
    valid: bool,    // Whether this glyph exists
}

const GLYPH_FIRST: usize = 32; // First printable ASCII (space)
const GLYPH_LAST: usize = 126; // Last printable ASCII (~)
const GLYPH_ACCENT_A_ACUTE_SLOT: usize = 127;
/// Slots below this are the fixed identity block; declared extras append above.
const GLYPH_ASCII_SLOTS: usize = 128;

fn ascii_slot(ch: char) -> Option<usize> {
    let code = ch as usize;
    if (GLYPH_FIRST..=GLYPH_LAST).contains(&code) {
        return Some(code);
    }
    if ch == '\u{00c1}' {
        return Some(GLYPH_ACCENT_A_ACUTE_SLOT);
    }
    None
}

/// Which codepoints a font bakes. Slots `0..GLYPH_ASCII_SLOTS` are the identity
/// block (32..=126 by codepoint, U+00C1 at 127) so the atlas grid, the face
/// row-blocks and Rock's positional cap-height probe are unaffected by what a
/// caller declares; declared extras append above it in first-appearance order.
///
/// The atlas is a pure function of the raster inputs, so this rides along with
/// the font bytes, size and faces everywhere those three travel together.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct Charset {
    extras: Vec<char>,
    extra_slots: HashMap<char, usize>,
}

impl Charset {
    /// Extras arrive as UTF-8 text rather than an array: a codepoint list IS a
    /// string, and it crosses the FFI without marshalling.
    fn parse(extra: &str) -> Charset {
        let mut set = Charset::default();
        for ch in extra.chars() {
            // Silently dropped rather than rejected: both mean "already
            // reachable", and a caller listing 'A' has asked for nothing new.
            if ascii_slot(ch).is_some() || set.extra_slots.contains_key(&ch) {
                continue;
            }
            set.extra_slots
                .insert(ch, GLYPH_ASCII_SLOTS + set.extras.len());
            set.extras.push(ch);
        }
        set
    }

    fn slot_count(&self) -> usize {
        GLYPH_ASCII_SLOTS + self.extras.len()
    }

    fn slot_of(&self, ch: char) -> Option<usize> {
        ascii_slot(ch).or_else(|| self.extra_slots.get(&ch).copied())
    }

    /// The codepoint a slot bakes, or None for a hole in the identity block
    /// (0..32, which no charset reaches).
    fn char_at(&self, slot: usize) -> Option<char> {
        if slot == GLYPH_ACCENT_A_ACUTE_SLOT {
            return Some('\u{00c1}');
        }
        if (GLYPH_FIRST..=GLYPH_LAST).contains(&slot) {
            return Some(slot as u8 as char);
        }
        self.extras
            .get(slot.checked_sub(GLYPH_ASCII_SLOTS)?)
            .copied()
    }

    /// Every slot that carries a codepoint, in slot order.
    fn baked_slots(&self) -> Vec<usize> {
        let mut slots: Vec<usize> = (GLYPH_FIRST..=GLYPH_LAST).collect();
        slots.push(GLYPH_ACCENT_A_ACUTE_SLOT);
        slots.extend(GLYPH_ASCII_SLOTS..self.slot_count());
        slots
    }
}

// Shared text-rendering geometry. One per process; created by
// stone_text_init. Each loaded font has its own atlas texture but
// reuses this VAO/VBO for the draw call. The shader program is NOT owned
// here: it is the Rock-authored text_shader (text_shader.rock), resolved
// fresh per draw from the handle Rock passes (see draw_text_line_internal).
struct TextPipeline {
    vao: glow::VertexArray,
    vbo: glow::Buffer,
}

// Per-font state. One entry per stone_load_font success; font_id is the
// 1-based index into FONTS (0 reserved as the "invalid font" sentinel
// returned by failed loads).
struct FontEntry {
    /// `None` for a metrics-only font: `stone_load_font_metrics` rasterizes the
    /// atlas on the CPU but uploads nothing, so there is no texture to bind.
    /// The type carries that, rather than a flag each GL reader must remember to
    /// check -- every one of them is a `match` the compiler pointed at.
    atlas_texture: Option<glow::Texture>,
    /// Per face, then per text slot. An undeclared face is entirely invalid;
    /// nothing substitutes `Regular` for it.
    glyphs: [Vec<GlyphInfo>; 4],
    faces: FaceSet,
    charset: Charset,
    /// Font-unit metrics, prepared once per raster from the same bytes at
    /// unit scale. `stone_get_font_unit_metrics` reads this; it never parses.
    unit_metrics: FontUnitMetrics,
    /// Per face, then per text slot: the `stone_get_glyph_unit_metrics`
    /// answers, prepared once per raster. `None` is a hole in the charset or
    /// a codepoint the file does not cover -- the same rejections the
    /// per-query parse answered with. All four faces are baked, declared or
    /// not, because the ABI never checked declaration.
    glyph_unit: [Vec<Option<GlyphUnitMetrics>>; 4],
    font_data: Rc<Vec<u8>>,
    source_path: String,
    font_pixel_size: i32,
    /// The compat atlas's CPU raster is not retained once uploaded.
    atlas_width: i32,
    atlas_height: i32,
    /// Independent raster products for one stable face/font-unit identity.
    /// The first coverage instance aliases the compatibility atlas above;
    /// later target buckets own their own GL textures.
    instances: Vec<AtlasInstance>,
    face_revision: u64,
    /// Set once `stone_font_release_coverage` drops GL coverage this entry
    /// held. A metrics-shaped entry (no atlas, no instances: headless)
    /// releases nothing native and leaves this false, so its size and
    /// reload behavior are exactly what they were before the release API
    /// existed.
    coverage_released: bool,
}

#[derive(Clone)]
struct AtlasInstance {
    texture: glow::Texture,
    glyphs: [Vec<GlyphInfo>; 4],
    width: i32,
    height: i32,
    pixel_size: i32,
    image: Option<u64>,
    face_revision: u64,
}

impl AtlasInstance {
    /// Bytes of the R8 raster its texture holds; no CPU copy is retained.
    fn byte_len(&self) -> u64 {
        self.width.max(0) as u64 * self.height.max(0) as u64
    }
}

fn delete_font_textures(entries: &[FontEntry]) {
    with_gl(|gl| unsafe {
        for entry in entries {
            if let Some(atlas) = entry.atlas_texture {
                gl.delete_texture(atlas);
            }
            for instance in &entry.instances {
                if Some(instance.texture) != entry.atlas_texture {
                    gl.delete_texture(instance.texture);
                }
            }
        }
    });
}

thread_local! {
    static TEXT_PIPELINE: RefCell<Option<TextPipeline>> = const { RefCell::new(None) };
    // Slot becomes None on stone_unload_font. font_ids are 1-based
    // and monotonic (no slot reuse) — a stale id past unload looks up
    // to None → silent no-op via with_font.
    static FONTS: RefCell<Vec<Option<FontEntry>>> = const { RefCell::new(Vec::new()) };
    // Cached IMAGES handle per font, populated lazily by
    // stone_get_font_atlas_image. Parallel to FONTS.
    static FONT_ATLAS_IMAGES: RefCell<Vec<Option<u64>>> = const { RefCell::new(Vec::new()) };
}

// Embedded fallback font (DroidSans.ttf, 187KB)
pub(crate) const FALLBACK_FONT: &[u8] =
    include_bytes!("../../packages/stone/engine/assets/fonts/DroidSans.ttf");

// Resolve a 1-based font_id to a Vec index. Returns None if out of range.
fn font_index(font_id: u32) -> Option<usize> {
    if font_id == 0 {
        return None;
    }
    let idx = (font_id - 1) as usize;
    FONTS.with(|f| {
        let fonts = f.borrow();
        if idx < fonts.len() && fonts[idx].is_some() {
            Some(idx)
        } else {
            None
        }
    })
}

#[no_mangle]
pub extern "C" fn stone_text_init() -> bool {
    // Hot reload re-enters init; free prior GL resources before we
    // overwrite TEXT_PIPELINE / FONTS, otherwise the old atlas textures
    // and the old shader/VAO/VBO leak across every reload.
    TEXT_PIPELINE.with(|tp| {
        if let Some(pipeline) = tp.borrow_mut().take() {
            with_gl(|gl| unsafe {
                gl.delete_vertex_array(pipeline.vao);
                gl.delete_buffer(pipeline.vbo);
            });
        }
    });
    let prior_fonts: Vec<FontEntry> =
        FONTS.with(|f| f.borrow_mut().drain(..).filter_map(|opt| opt).collect());
    if !prior_fonts.is_empty() {
        delete_font_textures(&prior_fonts);
    }
    forget_instance_images(&prior_fonts);
    // Before the slots are cleared: every cached atlas image points at a
    // texture the loop above just deleted, so it has to leave `IMAGES` too.
    forget_all_atlas_images();
    FONT_ATLAS_IMAGES.with(|i| i.borrow_mut().clear());

    with_gl(|gl| unsafe { init_geometry(gl) }).unwrap_or(false)
}

unsafe fn init_geometry(gl: &crate::DebugGl) -> bool {
    // No shader compile here -- the text program is the Rock-authored
    // text_shader (text_shader.rock), compiled through the rock_shader
    // registry and bound per draw by handle (draw_text_line_internal).

    // Create VAO and VBO for text rendering
    let vao = match gl.create_vertex_array() {
        Ok(vao) => vao,
        Err(error) => {
            ::log::error!(target: "STONE", "stone_text_init: failed to create VAO: {error}");
            return false;
        }
    };
    let vbo = match gl.create_buffer() {
        Ok(vbo) => vbo,
        Err(error) => {
            ::log::error!(target: "STONE", "stone_text_init: failed to create VBO: {error}");
            gl.delete_vertex_array(vao);
            return false;
        }
    };

    gl.bind_vertex_array(Some(vao));
    gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));

    // Allocate buffer (6 vertices * 4 floats per vertex = 24 floats)
    let empty_data: [f32; 24] = [0.0; 24];
    if gl
        .buffer_data_u8_slice(
            glow::ARRAY_BUFFER,
            bytemuck::cast_slice(&empty_data),
            glow::DYNAMIC_DRAW,
        )
        .is_err()
    {
        gl.bind_vertex_array(None);
        gl.delete_buffer(vbo);
        gl.delete_vertex_array(vao);
        return false;
    }

    // Two Vec2 attrs over the interleaved [pos.xy, tex.xy] buffer, lockstep
    // with text_shader.rock's TextVert (loc 0 = pos, loc 1 = texcoord;
    // stride 16). Same bytes as the retired single packed-vec4 attr.
    let stride = 4 * std::mem::size_of::<f32>() as i32;
    gl.enable_vertex_attrib_array(0);
    gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);
    gl.enable_vertex_attrib_array(1);
    gl.vertex_attrib_pointer_f32(
        1,
        2,
        glow::FLOAT,
        false,
        stride,
        2 * std::mem::size_of::<f32>() as i32,
    );

    gl.bind_buffer(glow::ARRAY_BUFFER, None);
    gl.bind_vertex_array(None);

    TEXT_PIPELINE.with(|tp| {
        *tp.borrow_mut() = Some(TextPipeline { vao, vbo });
    });

    true
}

#[no_mangle]
pub extern "C" fn stone_text_shutdown() {
    TEXT_PIPELINE.with(|tp| {
        if let Some(pipeline) = tp.borrow_mut().take() {
            with_gl(|gl| unsafe {
                gl.delete_vertex_array(pipeline.vao);
                gl.delete_buffer(pipeline.vbo);
            });
        }
    });
    FONTS.with(|fonts| {
        let drained: Vec<FontEntry> = fonts.borrow_mut().drain(..).filter_map(|opt| opt).collect();
        forget_instance_images(&drained);
        delete_font_textures(&drained);
    });
    forget_all_atlas_images();
    FONT_ATLAS_IMAGES.with(|imgs| imgs.borrow_mut().clear());
}

// Representative glyphs for the pixel-grid-alignment probe: caps, round,
// diagonal, and a descender.
const GRID_PROBE: &[u8] = b"HMoxnwgB";

// Fraction of the probe glyphs' *ink* texels (coverage > 0) that are only
// partially covered (1..=254). A pixel font rasterized on its native grid
// is near-binary (~0); off the grid -- or any smooth font -- antialiases
// edges into many partial texels. Returns 0 if nothing rasterized.
fn grid_gray_fraction(font_data: &[u8], px: f32) -> f32 {
    match RasterFont::new(font_data, px) {
        Ok(f) => glyph_raster::gray_fraction(&f, GRID_PROBE, Synth::REGULAR),
        Err(_) => 0.0,
    }
}

const GRID_CLEAN: f32 = 0.05; // <=5% partial ink = effectively a pixel grid
const GRID_MARGIN: f32 = 0.15; // chosen must be this much grayer than clean

// Decide whether `size` rasterizes a pixel font off its native grid. Returns
// a warning message when the
// chosen size antialiases but a much cleaner (near-binary) size exists
// below it, meaning the font *can* be crisp so this size is a mistake.
// Smooth fonts never go near-binary, so they return None. Pure (no I/O)
// so it is unit-testable; warn_if_offgrid does the logging.
fn offgrid_report(font_data: &[u8], size: i32, path: &str) -> Option<String> {
    if size < 8 {
        return None;
    }
    let chosen = grid_gray_fraction(font_data, size as f32);
    if chosen <= GRID_CLEAN {
        return None; // already crisp (native size or a clean multiple)
    }
    // Cleanest integer ppem at or below the chosen size.
    let mut best_px = size;
    let mut best_gray = chosen;
    for px in 6..size {
        let g = grid_gray_fraction(font_data, px as f32);
        if g < best_gray {
            best_gray = g;
            best_px = px;
        }
    }
    if best_gray < GRID_CLEAN && chosen - best_gray > GRID_MARGIN {
        let mult = (size as f32 / best_px as f32).round().max(1.0) as i32;
        let suggest = best_px * mult;
        Some(format!(
            "font '{}' rasterized at {}px is off the pixel grid ({:.0}% antialiased edge texels vs {:.0}% at {}px); for crisp glyphs use {}px (a multiple of the ~{}px native grid)",
            path, size, chosen * 100.0, best_gray * 100.0, best_px, suggest, best_px
        ))
    } else {
        None
    }
}

// Warn (never fail) on an off-grid pixel-font rasterization. Best-effort,
// startup-only. See offgrid_report.
fn warn_if_offgrid(font_data: &[u8], size: i32, path: &str) {
    if let Some(msg) = offgrid_report(font_data, size, path) {
        ::log::warn!(target: "TEXT", "{}", msg);
    }
}

// Build a FontEntry from font bytes. Returns None on parse failure.
// CPU rasterization result, split from the GL upload so load (fresh texture) and
// reload (re-upload into the existing texture) share the same glyph layout.
struct RasterizedAtlas {
    atlas_data: Vec<u8>,
    glyphs: [Vec<GlyphInfo>; 4],
    atlas_width: u32,
    atlas_height: u32,
    /// The unit-metric answers for these bytes, baked beside the atlas so
    /// every commit path (load, reload, resize) publishes them
    /// with the raster they describe. Read-only raster reproductions (the
    /// RGBA8 copy) bake and drop them.
    unit_metrics: FontUnitMetrics,
    glyph_unit: [Vec<Option<GlyphUnitMetrics>>; 4],
}

// A pixel font is binary only on its own outlines, so declaring a face that
// re-renders them off the grid is the same mistake warn_if_offgrid catches for
// the size -- and it is silent without this, because the regular face the
// size check probes stays clean either way.
fn warn_if_synthesizing_a_pixel_font(font_data: &[u8], size: i32, faces: FaceSet, path: &str) {
    if !faces.synthesizes_anything() || size < 8 {
        return;
    }
    let regular = grid_gray_fraction(font_data, size as f32);
    if regular > GRID_CLEAN {
        return;
    }
    let font = match RasterFont::new(font_data, size as f32) {
        Ok(f) => f,
        Err(_) => return,
    };
    // Every declared face is measured, not just the first: an integer
    // widening lands back on the grid (Silkscreen at 48px synthesizes bold
    // at 0.006), so bold can be clean while oblique on the same font is
    // 0.263.
    // Picking one face to probe reports the clean one and stays silent.
    for face in faces.declared().filter(|f| *f != Face::Regular) {
        let gray = glyph_raster::gray_fraction(&font, GRID_PROBE, face.synth(size as f32));
        if gray - regular > GRID_MARGIN {
            ::log::warn!(
                target: "TEXT",
                "font '{}' rasterizes binary at {}px ({:.0}% antialiased edge texels) but its declared {:?} face is {:.0}%; a pixel family should declare no synthesis",
                path, size, regular * 100.0, face, gray * 100.0
            );
        }
    }
}

fn rasterize_glyphs(
    font_data: &[u8],
    size: i32,
    faces: FaceSet,
    charset: &Charset,
    path_for_log: &str,
) -> Option<RasterizedAtlas> {
    rasterize_glyphs_with(font_data, size, faces, charset, path_for_log, true)
}

/// `warn` is false only to reproduce a raster already made (and warned
/// about) at load, resize or reload.
fn rasterize_glyphs_with(
    font_data: &[u8],
    size: i32,
    faces: FaceSet,
    charset: &Charset,
    path_for_log: &str,
    warn: bool,
) -> Option<RasterizedAtlas> {
    let pixel_size = size as f32;
    let font = match RasterFont::new(font_data, pixel_size) {
        Ok(f) => f,
        Err(e) => {
            ::log::error!(target: "TEXT", "rasterize_glyphs: failed to parse font '{}': {}", path_for_log, e);
            return None;
        }
    };

    if warn {
        warn_if_offgrid(font_data, size, path_for_log);
        warn_if_synthesizing_a_pixel_font(font_data, size, faces, path_for_log);
    }

    // The layout answers beside the atlas: one unit-scale parse answers every
    // per-glyph ABI query the font will ever serve, so the queries stop
    // re-parsing the file. `unit_metrics` never touches the scale, and
    // `glyph_unit_metrics` divides the pixel-space synthesis back out, so
    // parsing at 1.0 answers exactly what the old per-query
    // `RasterFont::new(&data, 1.0)` answered -- bit-identical, including the
    // raster-size independence the unit tests pin. All four faces are baked
    // because the ABI serves undeclared faces rather than rejecting them.
    let unit_font = match RasterFont::new(font_data, 1.0) {
        Ok(f) => f,
        Err(e) => {
            ::log::error!(target: "TEXT", "unit metrics: failed to parse font '{}': {}", path_for_log, e);
            return None;
        }
    };
    let unit_metrics = unit_font.unit_metrics();
    let mut glyph_unit: [Vec<Option<GlyphUnitMetrics>>; 4] =
        std::array::from_fn(|_| vec![None; charset.slot_count()]);
    for face in Face::ALL {
        let synth = face.synth(1.0);
        let table = &mut glyph_unit[face.index()];
        for slot in 0..table.len() {
            if let Some(ch) = charset.char_at(slot) {
                table[slot] = unit_font.glyph_unit_metrics(ch, synth);
            }
        }
    }

    // Rasterize once, then lay out: an outline rasterizer has no cheap
    // metrics-only query, so a sizing pass would rasterize everything twice.
    // The declared faces are baked together because the CELL has to hold the
    // widest of them -- an oblique pushes ink past its own advance, so sizing
    // cells from Regular alone would bleed bold-italic into its neighbour.
    let slots = charset.baked_slots();
    let glyph_count = slots.len();
    let baked: Vec<Face> = faces.declared().collect();
    let rendered: Vec<Vec<Option<glyph_raster::RasterGlyph>>> = baked
        .iter()
        .map(|face| {
            let synth = face.synth(pixel_size);
            slots
                .iter()
                .map(|&slot| {
                    charset
                        .char_at(slot)
                        .and_then(|ch| font.rasterize(ch, synth))
                })
                .collect()
        })
        .collect();

    let mut max_glyph_width = 0u32;
    let mut max_glyph_height = 0u32;
    for g in rendered.iter().flatten().flatten() {
        max_glyph_width = max_glyph_width.max(g.width as u32);
        max_glyph_height = max_glyph_height.max(g.height as u32);
    }

    let glyphs_per_row = 16;
    let atlas_width = (max_glyph_width + 2) * glyphs_per_row; // +2 for padding
    let rows_per_face = glyph_count.div_ceil(glyphs_per_row as usize);
    let atlas_height = (max_glyph_height + 2) * (rows_per_face * baked.len()) as u32;

    if !atlas_dimensions_supported(atlas_width, atlas_height) {
        return None;
    }

    let mut atlas_data = vec![0u8; (atlas_width * atlas_height) as usize];

    let mut glyphs: [Vec<GlyphInfo>; 4] =
        std::array::from_fn(|_| vec![GlyphInfo::default(); charset.slot_count()]);
    let cell_width = (max_glyph_width + 2) as usize;
    let cell_height = (max_glyph_height + 2) as usize;

    for (face_slot, face) in baked.iter().enumerate() {
        // Faces occupy whole row-blocks in declaration order, so the layout of
        // a regular-only bake is exactly what it was before faces existed.
        let face_row = face_slot * rows_per_face;
        for (glyph_idx, &glyph_slot) in slots.iter().enumerate() {
            // A codepoint the font does not cover stays invalid rather than
            // borrowing .notdef's box: the draw path skips it explicitly.
            let g = match &rendered[face_slot][glyph_idx] {
                Some(g) => g,
                None => continue,
            };

            let col = glyph_idx % glyphs_per_row as usize;
            let row = face_row + glyph_idx / glyphs_per_row as usize;

            let x_offset = col * cell_width + 1; // +1 for padding
            let y_offset = row * cell_height + 1;

            for gy in 0..g.height {
                for gx in 0..g.width {
                    let src_idx = gy * g.width + gx;
                    let dst_x = x_offset + gx;
                    let dst_y = y_offset + gy;
                    let dst_idx = dst_y * atlas_width as usize + dst_x;
                    if dst_idx < atlas_data.len() && src_idx < g.data.len() {
                        atlas_data[dst_idx] = g.data[src_idx];
                    }
                }
            }

            let u0 = x_offset as f32 / atlas_width as f32;
            let v0 = y_offset as f32 / atlas_height as f32;
            let u1 = (x_offset + g.width) as f32 / atlas_width as f32;
            let v1 = (y_offset + g.height) as f32 / atlas_height as f32;

            glyphs[face.index()][glyph_slot] = GlyphInfo {
                u0,
                v0,
                u1,
                v1,
                width: g.width as f32,
                height: g.height as f32,
                bearing_x: g.left,
                bearing_y: g.top,
                advance: g.advance,
                valid: true,
            };
        }
    }

    Some(RasterizedAtlas {
        atlas_data,
        glyphs,
        atlas_width,
        atlas_height,
        unit_metrics,
        glyph_unit,
    })
}

fn atlas_dimensions_fit(width: u32, height: u32, limit: i32) -> bool {
    limit > 0 && width > 0 && height > 0 && width <= limit as u32 && height <= limit as u32
}

#[test]
fn atlas_dimensions_respect_both_device_axes() {
    assert!(atlas_dimensions_fit(16384, 16384, 16384));
    assert!(!atlas_dimensions_fit(28352, 12300, 16384));
    assert!(!atlas_dimensions_fit(12300, 28352, 16384));
    assert!(!atlas_dimensions_fit(1, 1, 0));
    assert!(!atlas_dimensions_fit(0, 1, 16384));
}

fn atlas_dimensions_supported(width: u32, height: u32) -> bool {
    let Some(limit) = with_gl(|gl| unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) }) else {
        // Headless CPU bakes have no device constraint; upload validates again.
        return true;
    };
    if atlas_dimensions_fit(width, height, limit) {
        return true;
    }
    ::log::error!(target: "TEXT", "font atlas {}x{} exceeds GL texture limit {}; refusing allocation/upload", width, height, limit);
    false
}

fn upload_atlas_texture(data: &[u8], width: u32, height: u32) -> Option<glow::Texture> {
    if !atlas_dimensions_supported(width, height) {
        return None;
    }
    with_gl(|gl| unsafe {
        let tex = gl.create_texture().ok()?;
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::R8 as i32,
            width as i32,
            height as i32,
            0,
            glow::RED,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(data)),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
        let filter = glow::NEAREST as i32;
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, filter);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, filter);
        Some(tex)
    })
    .flatten()
}

/// What a load asks the rasterizer to produce. The CPU raster and the unit
/// metrics are identical either way -- `Metrics` differs only in owning no GL
/// texture and no atlas instance, which is what makes a font loadable in a
/// process that has no window (the `rock test <game>` luajit process).
#[derive(Clone, Copy, PartialEq, Eq)]
enum AtlasResidency {
    /// Upload the atlas and keep the compatibility instance. Drawing works.
    Gpu,
    /// Keep the raster on the CPU only. Measurement and layout work; every
    /// GL-bound reader refuses this entry by name.
    Metrics,
}

fn rasterize_font(
    font_data: Rc<Vec<u8>>,
    size: i32,
    faces: FaceSet,
    charset: Charset,
    path_for_log: &str,
    residency: AtlasResidency,
) -> Option<FontEntry> {
    let r = rasterize_glyphs(&font_data, size, faces, &charset, path_for_log)?;

    // Allocate a fresh GL texture and upload the atlas.
    let atlas_texture = if residency == AtlasResidency::Metrics {
        None
    } else {
        Some(with_gl(|gl| unsafe {
            let tex = gl
                .create_texture()
                .expect("Failed to create font atlas texture");
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));

            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::R8 as i32,
                r.atlas_width as i32,
                r.atlas_height as i32,
                0,
                glow::RED,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&r.atlas_data)),
            );

            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            // The atlas's own filter is never observed: every text batch binds the
            // sampler its Font declared (Font.sampling).
            let filter = glow::NEAREST as i32;
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, filter);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, filter);
            tex
        })?)
    };

    let glyphs = r.glyphs;
    let unit_metrics = r.unit_metrics;
    let glyph_unit = r.glyph_unit;
    // A metrics-only entry owns no instance: an instance IS a GL texture, and
    // the coverage/target buckets are only ever asked for by a draw.
    let instances = match atlas_texture {
        Some(texture) => vec![AtlasInstance {
            texture,
            glyphs: glyphs.clone(),
            width: r.atlas_width as i32,
            height: r.atlas_height as i32,
            pixel_size: size,
            image: None,
            face_revision: 0,
        }],
        None => Vec::new(),
    };

    Some(FontEntry {
        atlas_texture,
        glyphs,
        faces,
        charset,
        unit_metrics,
        glyph_unit,
        font_data,
        source_path: path_for_log.to_owned(),
        font_pixel_size: size,
        atlas_width: r.atlas_width as i32,
        atlas_height: r.atlas_height as i32,
        instances,
        face_revision: 0,
        coverage_released: false,
    })
}

#[cfg(test)]
thread_local! {
    /// Forced upload success. Headless, `with_gl` answers None and every
    /// upload fails, so every reload would bail before mutating anything and
    /// the commit ordering would be untestable. The reload-atomicity tests set
    /// this, standing in for a live context; production never sets it.
    static FORCE_UPLOADS_OK: Cell<bool> = const { Cell::new(false) };
    /// Forced dependent-atlas rebuild failure. Headless, a dependent rasterize
    /// cannot fail once the main bake succeeded (no GL size limit), so the
    /// reload-atomicity tests force it, standing in for a per-size failure
    /// such as an instance atlas exceeding MAX_TEXTURE_SIZE.
    static FAIL_DEPENDENT_RASTER: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
fn force_uploads_ok_for_test() -> bool {
    FORCE_UPLOADS_OK.with(|f| f.get())
}

#[cfg(test)]
fn fail_dependent_raster_for_test() -> bool {
    FAIL_DEPENDENT_RASTER.with(|f| f.get())
}

/// Run a texture upload against the live context, `None` without one. Same
/// shape as `with_gl` so the call sites keep their success checks.
fn run_gl_upload(upload: impl FnOnce(&crate::DebugGl)) -> Option<()> {
    #[cfg(test)]
    if force_uploads_ok_for_test() {
        return Some(());
    }
    with_gl(|gl| upload(gl))
}

fn apply_raster(idx: usize, pixel_size: i32, raster: RasterizedAtlas) -> bool {
    let texture: Option<glow::Texture> = match FONTS.with(|f| {
        f.borrow()
            .get(idx)
            .and_then(|o| o.as_ref())
            .map(|e| (e.atlas_texture, e.coverage_released))
    }) {
        // The outer Option is "is there a font"; the inner one is "does it own a
        // texture". A metrics-only font is refused here rather than silently
        // reporting a successful re-raster: its CPU glyphs would go stale
        // against the new pixel size with nothing saying so. A released font
        // refreshes its CPU raster below and keeps the GPU down.
        Some((Some(t), _)) => Some(t),
        Some((None, true)) => None,
        Some((None, false)) => {
            ::log::error!(target: "TEXT",
                "re-raster refused: font {} is metrics-only and owns no atlas texture",
                idx + 1);
            return false;
        }
        None => return false,
    };

    if let Some(texture) = texture {
        let uploaded = run_gl_upload(|gl| unsafe {
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::R8 as i32,
                raster.atlas_width as i32,
                raster.atlas_height as i32,
                0,
                glow::RED,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&raster.atlas_data)),
            );
        });
        if uploaded.is_none() {
            return false;
        }
    }

    FONTS.with(|fonts| {
        if let Some(entry) = fonts.borrow_mut().get_mut(idx).and_then(|o| o.as_mut()) {
            entry.glyphs = raster.glyphs.clone();
            // The unit answers move with the raster they describe: a failed
            // reload never reaches this commit, so the old answers stand with
            // the old bytes.
            entry.unit_metrics = raster.unit_metrics;
            entry.glyph_unit = raster.glyph_unit;
            entry.font_pixel_size = pixel_size;
            entry.atlas_width = raster.atlas_width as i32;
            entry.atlas_height = raster.atlas_height as i32;
            if let Some(instance) = entry.instances.first_mut() {
                instance.glyphs = raster.glyphs;
                instance.width = raster.atlas_width as i32;
                instance.height = raster.atlas_height as i32;
                instance.pixel_size = pixel_size;
                if let Some(image) = instance.image {
                    IMAGES.with(|images| {
                        if let Some(loaded) = images.borrow_mut().get_mut(&image) {
                            loaded.width = raster.atlas_width;
                            loaded.height = raster.atlas_height;
                        }
                    });
                }
            }
        }
    });

    let cached_img = FONT_ATLAS_IMAGES.with(|imgs| imgs.borrow().get(idx).copied().flatten());
    if let Some(img_id) = cached_img {
        IMAGES.with(|images| {
            if let Some(li) = images.borrow_mut().get_mut(&img_id) {
                li.width = raster.atlas_width;
                li.height = raster.atlas_height;
            }
        });
    }

    true
}

fn replace_atlas_texture(texture: glow::Texture, data: &[u8], width: u32, height: u32) -> bool {
    run_gl_upload(|gl| unsafe {
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::R8 as i32,
            width as i32,
            height as i32,
            0,
            glow::RED,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(data)),
        );
    })
    .is_some()
}

struct BuiltInstanceRaster {
    index: usize,
    texture: glow::Texture,
    raster: RasterizedAtlas,
}

/// Rebuild every extra coverage instance from new bytes WITHOUT publishing
/// any of them. `None` is a failed rebuild, after which the caller must not
/// commit anything; an empty vec is a font with no extra instances.
fn build_instance_rasters(
    idx: usize,
    font_data: &[u8],
    faces: FaceSet,
    charset: &Charset,
    source_path: &str,
) -> Option<Vec<BuiltInstanceRaster>> {
    let descriptors = FONTS.with(|fonts| {
        fonts
            .borrow()
            .get(idx)
            .and_then(|entry| entry.as_ref())
            .map(|entry| {
                entry
                    .instances
                    .iter()
                    .enumerate()
                    .skip(1)
                    .map(|(index, instance)| (index, instance.pixel_size, instance.texture))
                    .collect::<Vec<_>>()
            })
    });
    let Some(descriptors) = descriptors else {
        return None;
    };
    let mut built = Vec::with_capacity(descriptors.len());
    for (index, pixel_size, texture) in descriptors {
        let Some(raster) = rasterize_glyphs(font_data, pixel_size, faces, charset, source_path)
        else {
            return None;
        };
        #[cfg(test)]
        if fail_dependent_raster_for_test() {
            return None;
        }
        built.push(BuiltInstanceRaster {
            index,
            texture,
            raster,
        });
    }
    Some(built)
}

/// Publish rebuilt coverage instances. Runs only after every build succeeded.
fn commit_instance_rasters(
    idx: usize,
    built: Vec<BuiltInstanceRaster>,
    face_revision: u64,
) -> bool {
    for item in built {
        let BuiltInstanceRaster {
            index,
            texture,
            raster,
        } = item;
        let glyphs = raster.glyphs;
        let width = raster.atlas_width;
        let height = raster.atlas_height;
        if !replace_atlas_texture(texture, &raster.atlas_data, width, height) {
            return false;
        }
        FONTS.with(|fonts| {
            if let Some(instance) = fonts
                .borrow_mut()
                .get_mut(idx)
                .and_then(|entry| entry.as_mut())
                .and_then(|entry| entry.instances.get_mut(index))
            {
                instance.glyphs = glyphs;
                instance.width = width as i32;
                instance.height = height as i32;
                instance.face_revision = face_revision;
                if let Some(image) = instance.image {
                    IMAGES.with(|images| {
                        if let Some(loaded) = images.borrow_mut().get_mut(&image) {
                            loaded.width = width;
                            loaded.height = height;
                        }
                    });
                }
            }
        });
    }
    true
}

#[no_mangle]
pub extern "C" fn stone_load_font(
    path: *const c_char,
    size: c_int,
    face_mask: c_int,
    extra_chars: *const c_char,
) -> c_uint {
    load_font_with_residency(path, size, face_mask, extra_chars, AtlasResidency::Gpu)
}

/// Load a font for MEASUREMENT ONLY: same file, same charset, same CPU raster
/// and the same font-unit metrics as `stone_load_font`, but no GL texture and no
/// atlas instance. This is the only font load that succeeds in a process with no
/// window, which is what lets `rock test <game>` -- a bare luajit that never
/// creates a context -- measure and lay out text. Drawing with the returned id
/// is refused by name, not silently.
#[no_mangle]
pub extern "C" fn stone_load_font_metrics(
    path: *const c_char,
    size: c_int,
    face_mask: c_int,
    extra_chars: *const c_char,
) -> c_uint {
    load_font_with_residency(path, size, face_mask, extra_chars, AtlasResidency::Metrics)
}

fn load_font_with_residency(
    path: *const c_char,
    size: c_int,
    face_mask: c_int,
    extra_chars: *const c_char,
    residency: AtlasResidency,
) -> c_uint {
    if path.is_null() {
        ::log::error!(target: "TEXT", "stone_load_font: null path");
        return 0;
    }

    let path_str = unsafe { CStr::from_ptr(path).to_str().unwrap_or("") };
    if path_str.is_empty() {
        ::log::error!(target: "TEXT", "stone_load_font: empty path");
        return 0;
    }

    let extra_str = if extra_chars.is_null() {
        ""
    } else {
        unsafe { CStr::from_ptr(extra_chars).to_str().unwrap_or("") }
    };
    load_font_with_residency_str(path_str, size, face_mask, extra_str, residency)
}

fn load_font_with_residency_str(
    path_str: &str,
    size: c_int,
    face_mask: c_int,
    extra_str: &str,
    residency: AtlasResidency,
) -> c_uint {
    // Override dir, then embedded registry, then disk; embedded font as a last resort.
    let font_data = match crate::asset_registry::resolve_asset(path_str) {
        Some(data) => data,
        None => {
            // Once per site, through the one dedup store — falling back to the
            // embedded font is expected for games that don't ship one (pac,
            // sims, etc.). A `Once` cannot be reset at all, which is what put
            // this beside the others in the boundary's residual list.
            crate::log_once!("TEXT",
                "stone_load_font: failed to read font file '{}' — using embedded fallback (DroidSans.ttf)",
                path_str);
            FALLBACK_FONT.to_vec()
        }
    };

    let charset = Charset::parse(extra_str);

    let entry = match rasterize_font(
        Rc::new(font_data),
        size,
        FaceSet::from_mask(face_mask),
        charset,
        path_str,
        residency,
    ) {
        Some(e) => e,
        None => return 0,
    };

    let font_id = FONTS.with(|fonts| {
        let mut fonts = fonts.borrow_mut();
        fonts.push(Some(entry));
        fonts.len() as u32
    });
    FONT_ATLAS_IMAGES.with(|imgs| imgs.borrow_mut().push(None));
    font_id
}

/// Re-rasterize `path` into the atlas texture already bound to `font_id` at
/// `pixel_size` -- font_id and the GL texture object are preserved, so the
/// cached atlas Image and every Font handle stay valid. Returns false (keeping
/// the old atlas) on unknown font / missing / corrupt file; no embedded fallback.
#[no_mangle]
pub extern "C" fn stone_reload_font(
    font_id: c_uint,
    path: *const c_char,
    pixel_size: c_int,
) -> bool {
    if path.is_null() {
        return false;
    }
    if pixel_size < 1 {
        return false;
    }
    let path_str = unsafe { CStr::from_ptr(path).to_str().unwrap_or("") };
    if path_str.is_empty() {
        return false;
    }
    let idx = match font_index(font_id) {
        Some(i) => i,
        None => return false,
    };

    let font_data = match crate::asset_registry::resolve_asset(path_str) {
        Some(d) => Rc::new(d),
        None => {
            ::log::warn!(target: "TEXT", "stone_reload_font: failed to read '{}'", path_str);
            return false;
        }
    };

    let declared = FONTS.with(|fonts| {
        fonts
            .borrow()
            .get(idx)
            .and_then(|o| o.as_ref())
            .map(|e| (e.faces, e.charset.clone()))
    });
    let (faces, charset) = match declared {
        Some(d) => d,
        None => return false,
    };
    let next_revision = FONTS.with(|fonts| {
        fonts
            .borrow()
            .get(idx)
            .and_then(|entry| entry.as_ref())
            .map(|entry| entry.face_revision.saturating_add(1))
            .unwrap_or(1)
    });
    let raster = match rasterize_glyphs(&font_data, pixel_size, faces, &charset, path_str) {
        Some(r) => r,
        None => return false,
    };
    // Build every dependent product BEFORE publishing any of them: once the
    // main atlas is committed, a failing instance rebuild would leave the new
    // pixels over the old bytes. The uploads below share one GL context, so
    // they cannot fail piecemeal -- a build that succeeded everywhere commits
    // everywhere.
    let built_instances = match build_instance_rasters(idx, &font_data, faces, &charset, path_str) {
        Some(v) => v,
        None => {
            ::log::warn!(target: "TEXT", "stone_reload_font: failed to refresh raster instances for '{}'", path_str);
            return false;
        }
    };
    if !apply_raster(idx, pixel_size, raster) {
        return false;
    }
    if !commit_instance_rasters(idx, built_instances, next_revision) {
        return false;
    }

    FONTS.with(|fonts| {
        if let Some(entry) = fonts.borrow_mut().get_mut(idx).and_then(|o| o.as_mut()) {
            entry.font_data = font_data;
            entry.source_path = path_str.to_owned();
            entry.face_revision = next_revision;
            if let Some(instance) = entry.instances.first_mut() {
                instance.face_revision = entry.face_revision;
            }
        }
    });

    true
}

/// Re-rasterize a font from its retained source bytes at a new physical em.
/// The font and atlas texture handles remain stable.
#[no_mangle]
pub extern "C" fn stone_resize_font(font_id: c_uint, pixel_size: c_int) -> bool {
    if pixel_size < 1 {
        return false;
    }
    let idx = match font_index(font_id) {
        Some(i) => i,
        None => return false,
    };
    let source = FONTS.with(|fonts| {
        fonts
            .borrow()
            .get(idx)
            .and_then(|o| o.as_ref())
            .map(|entry| {
                (
                    entry.font_data.clone(),
                    entry.source_path.clone(),
                    entry.faces,
                    entry.charset.clone(),
                )
            })
    });
    let (font_data, source_path, faces, charset) = match source {
        Some(s) => s,
        None => return false,
    };
    let raster = match rasterize_glyphs(&font_data, pixel_size, faces, &charset, &source_path) {
        Some(r) => r,
        None => return false,
    };
    if !apply_raster(idx, pixel_size, raster) {
        return false;
    }
    true
}

// Run a closure with the pipeline + a specific font entry. No-op if
// either is missing.
/// Read one loaded font. The only precondition is that the font EXISTS -- no
/// text pipeline, no GL, no window.
///
/// Every metrics reader below used `with_font`, which also demands
/// `TEXT_PIPELINE`. A VAO/VBO says nothing about a glyph advance, so that
/// coupling made a perfectly loaded font answer its caller's default -- slot
/// count 0, pixel size 0, `stone_get_glyph_info` false -- in any process that
/// had not created a window. Silent, and indistinguishable from an unloaded
/// font. Drawing keeps `with_font`, because a draw genuinely needs the pipeline.
fn with_font_entry<R>(font_id: u32, default: R, f: impl FnOnce(&FontEntry) -> R) -> R {
    let idx = match font_index(font_id) {
        Some(i) => i,
        None => return default,
    };
    FONTS.with(|fonts| {
        let fonts = fonts.borrow();
        // font_index already confirmed Some(_); this expect documents
        // the invariant and panics loudly if it's ever violated.
        let entry = fonts[idx]
            .as_ref()
            .expect("font_index returned a None slot");
        f(entry)
    })
}

/// Read one loaded font TOGETHER WITH the shared text pipeline. For the draw
/// path only; a reader that does not touch the VAO belongs on
/// [`with_font_entry`].
fn with_font<R>(font_id: u32, default: R, f: impl FnOnce(&TextPipeline, &FontEntry) -> R) -> R {
    TEXT_PIPELINE.with(|tp| {
        let tp_ref = tp.borrow();
        let pipeline = match tp_ref.as_ref() {
            Some(p) => p,
            None => return default,
        };
        with_font_entry(font_id, default, |entry| f(pipeline, entry))
    })
}

// Internal function that accepts optional baseline override
// baseline_override: if >= 0, use this value instead of calculating from text
fn draw_text_line_internal(
    shader: u64,
    font_id: u32,
    text: &str,
    x: f32,
    y: f32,
    scale_x: f32,
    scale_y: f32,
    baseline_override: f32,
) {
    with_font(font_id, (), |pipeline, entry| {
        // The Rock-authored text_shader program, resolved fresh by handle
        // (Rock passes get_shader_handle(text_shader)). Fail-closed: an
        // unregistered/unknown handle skips loudly rather than drawing wrong.
        let program = match SHADERS.with(|s| s.borrow().get(&shader).map(|sh| sh.program)) {
            Some(p) => p,
            None => {
                crate::log_once!(
                    "TEXT",
                    "text shader program (handle {}) not in registry; skipping text draw",
                    shader
                );
                return;
            }
        };
        // Fail-closed for the same reason the unknown shader handle does:
        // binding texture 0 instead would draw the white pixel through the glyph
        // quads, which reads as a rendering bug rather than as the wrong font.
        let Some(atlas) = entry.atlas_texture else {
            crate::log_once!(
                "TEXT",
                "font {} is metrics-only (loaded through stone_load_font_metrics) and cannot be drawn with; skipping text draw",
                font_id
            );
            return;
        };
        with_gl(|gl| unsafe {
            gl.use_program(Some(program));
            // This path goes current without `stone_set_shader`, so it owes the
            // same re-bind: `glBindBufferBase` binds a GLOBAL index, and without
            // this the text program reads whichever program's buffer sits there.
            crate::uniform_block::bind_block_for_program(gl, shader);

            // The camera matrices arrive in the ENGINE block at binding 0,
            // filled by `stone_draw_text_for` before this call -- there is no
            // by-name matrix write here any more, and a block member has no
            // location to write to anyway.

            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(atlas));
            gl.bind_vertex_array(Some(pipeline.vao));

            gl.enable(glow::BLEND);
            gl.blend_func_separate(
                glow::SRC_ALPHA,
                glow::ONE_MINUS_SRC_ALPHA,
                glow::ONE,
                glow::ONE_MINUS_SRC_ALPHA,
            );

            // Calculate scale factor from font pixel size
            // Preserve sign of scale (C version does this too)
            let sx = scale_x;
            let sy = scale_y;
            let abs_sy = sy.abs(); // For baseline calculations

            // Use provided baseline or calculate from text
            let max_ascent = if baseline_override >= 0.0 {
                baseline_override
            } else {
                // Find max bearing_y (ascent) from all characters
                let mut max_bearing = 0.0f32;
                for c in text.chars() {
                    let Some(slot) = entry.charset.slot_of(c) else {
                        continue;
                    };
                    let gi = &entry.glyphs[Face::Regular.index()][slot];
                    if gi.valid {
                        let ascent = gi.bearing_y * abs_sy; // Use abs_sy like C version
                        if ascent > max_bearing {
                            max_bearing = ascent;
                        }
                    }
                }
                max_bearing
            };

            let mut all_vertices: Vec<f32> = Vec::with_capacity(text.len() * 24);
            let mut text_x = x;

            for c in text.chars() {
                let Some(slot) = entry.charset.slot_of(c) else {
                    continue;
                };
                let gi = &entry.glyphs[Face::Regular.index()][slot];
                if !gi.valid || gi.width == 0.0 || gi.height == 0.0 {
                    text_x += gi.advance * sx;
                    continue;
                }

                let w = gi.width * sx;
                let h = gi.height * sy;
                let xpos = text_x + gi.bearing_x * sx;
                let ypos = if sy < 0.0 {
                    y - gi.bearing_y * sy
                } else {
                    y + (max_ascent - gi.bearing_y * abs_sy)
                };

                let u0 = gi.u0;
                let v0 = gi.v0;
                let u1 = gi.u1;
                let v1 = gi.v1;

                all_vertices.extend_from_slice(&[
                    xpos,
                    ypos,
                    u0,
                    v0,
                    xpos,
                    ypos + h,
                    u0,
                    v1,
                    xpos + w,
                    ypos + h,
                    u1,
                    v1,
                    xpos,
                    ypos,
                    u0,
                    v0,
                    xpos + w,
                    ypos + h,
                    u1,
                    v1,
                    xpos + w,
                    ypos,
                    u1,
                    v0,
                ]);

                text_x += gi.advance * sx;
            }

            if !all_vertices.is_empty() {
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(pipeline.vbo));
                if gl
                    .buffer_data_u8_slice(
                        glow::ARRAY_BUFFER,
                        bytemuck::cast_slice(&all_vertices),
                        glow::STREAM_DRAW,
                    )
                    .is_err()
                {
                    gl.bind_buffer(glow::ARRAY_BUFFER, None);
                    gl.bind_vertex_array(None);
                    return;
                }
                gl.bind_buffer(glow::ARRAY_BUFFER, None);
                gl.draw_arrays(glow::TRIANGLES, 0, (all_vertices.len() / 4) as i32);
                crate::uniform_block::mark_program_drawn(program);
                track_text_draw_call();
            }

            gl.bind_vertex_array(None);
        });
    });
}

fn draw_text_lines(shader: u64, font_id: u32, text: &str, x: f32, y: f32, sx: f32, sy: f32) {
    if text.is_empty() {
        return;
    }

    let line_height = with_font(font_id, 20.0, |_, entry| {
        entry.font_pixel_size as f32 * sy.abs() * 1.2
    });
    let mut current_y = y;
    for line in text.split('\n') {
        if !line.is_empty() {
            draw_text_line_internal(shader, font_id, line, x, current_y, sx, sy, -1.0);
        }
        current_y += line_height;
    }
}

#[no_mangle]
pub extern "C" fn stone_draw_text(
    shader: c_uint,
    font_id: c_uint,
    text: *const c_char,
    x: c_float,
    y: c_float,
    sx: c_float,
    sy: c_float,
) {
    if text.is_null() {
        return;
    }

    let text_str = unsafe { CStr::from_ptr(text).to_str().unwrap_or("") };
    if text_str.is_empty() {
        return;
    }

    draw_text_lines(shader.into(), font_id, text_str, x, y, sx, sy);
}

/// Out-params, NOT struct-by-value return. LuaJIT cannot JIT-compile a
/// call to a C function that returns a struct by value ("NYI:
/// unsupported C function type") -- it aborts the enclosing trace and
/// runs interpreted (~127x; the returned struct cdata also allocates).
/// Pointer out-params ARE JIT-able; the Lua wrapper reuses two
/// file-scope scratch buffers so there is zero per-call allocation
/// either.
#[no_mangle]
pub extern "C" fn stone_measure_text(
    font_id: c_uint,
    text: *const c_char,
    scale: c_float,
    out_width: *mut c_float,
    out_height: *mut c_float,
) {
    if out_width.is_null() && out_height.is_null() {
        return;
    }

    let text_str = if text.is_null() {
        ""
    } else {
        unsafe { CStr::from_ptr(text).to_str().unwrap_or("") }
    };

    let result = with_font_entry(font_id, (0.0f32, 0.0f32), |entry| {
        let line_height = entry.font_pixel_size as f32 * scale * 1.2;
        let mut max_line_width = 0.0f32;
        let mut max_glyph_height = 0.0f32;
        let mut line_count = 0;

        for line in text_str.split('\n') {
            let mut line_width = 0.0f32;
            for c in line.chars() {
                let Some(slot) = entry.charset.slot_of(c) else {
                    continue;
                };
                let gi = &entry.glyphs[Face::Regular.index()][slot];
                line_width += gi.advance * scale;
                let glyph_height = gi.height * scale;
                if glyph_height > max_glyph_height {
                    max_glyph_height = glyph_height;
                }
            }
            if line_width > max_line_width {
                max_line_width = line_width;
            }
            line_count += 1;
        }

        let height = if line_count <= 1 {
            max_glyph_height
        } else {
            (line_count - 1) as f32 * line_height + max_glyph_height
        };
        (max_line_width, height)
    });

    unsafe {
        if !out_width.is_null() {
            *out_width = result.0;
        }
        if !out_height.is_null() {
            *out_height = result.1;
        }
    }
}

/// Get the baseline (max ascent in pixels) for a string at given scale
/// Returns -1.0 if no valid glyphs found
#[no_mangle]
pub extern "C" fn stone_get_text_baseline(
    font_id: c_uint,
    text: *const c_char,
    scale: c_float,
) -> c_float {
    if text.is_null() {
        return -1.0;
    }

    let text_str = unsafe { CStr::from_ptr(text).to_str().unwrap_or("") };
    if text_str.is_empty() {
        return -1.0;
    }

    with_font_entry(font_id, -1.0, |entry| {
        let sy = scale.abs();
        let mut max_ascent = 0.0f32;
        let mut found_valid = false;

        for c in text_str.chars() {
            let Some(slot) = entry.charset.slot_of(c) else {
                continue;
            };
            let gi = &entry.glyphs[Face::Regular.index()][slot];
            if gi.valid {
                let ascent = gi.bearing_y * sy;
                if ascent > max_ascent {
                    max_ascent = ascent;
                }
                found_valid = true;
            }
        }

        if found_valid {
            max_ascent
        } else {
            -1.0
        }
    })
}

/// Draw text with explicit baseline for per-character rendering
#[no_mangle]
pub extern "C" fn stone_draw_text_ex(
    shader: c_uint,
    font_id: c_uint,
    text: *const c_char,
    x: c_float,
    y: c_float,
    sx: c_float,
    sy: c_float,
    baseline: c_float,
) {
    if text.is_null() {
        return;
    }

    let text_str = unsafe { CStr::from_ptr(text).to_str().unwrap_or("") };
    if text_str.is_empty() {
        return;
    }

    // For stone_draw_text_ex, we don't split on newlines - it's meant for single characters/lines
    // with explicit baseline control
    draw_text_line_internal(shader.into(), font_id, text_str, x, y, sx, sy, baseline);
}

// =============================================================================
// Font atlas exposure for unified text batching
// =============================================================================

/// Register the font atlas texture in the IMAGES table and apply swizzle
/// so the GL_RED atlas reads as (r,r,r,r) in the sprite batch shader.
/// Returns a cached image handle (same ID on repeated calls).
fn retain_font_instance_image(image: &mut Option<u64>, atlas_image: u64) {
    if image.is_none() {
        *image = Some(atlas_image);
    }
}

#[no_mangle]
pub extern "C" fn stone_get_font_atlas_image(font_id: c_uint) -> u64 {
    let idx = match font_index(font_id) {
        Some(i) => i,
        None => return 0,
    };

    // Fast path: cached.
    let cached = FONT_ATLAS_IMAGES.with(|imgs| imgs.borrow().get(idx).copied().flatten());
    if let Some(id) = cached {
        return id;
    }

    // A metrics-only font has no texture to publish. Minting an Image over
    // texture 0 would hand Rock a handle that samples the white pixel, and
    // `register_image` would then declare a sampling for it process-wide. The
    // refusal is also NOT cached below: 0 is the absence of an image, not an
    // image id, and caching it would make the answer permanent for a font that
    // a later GPU load could legitimately publish.
    let has_atlas = FONTS.with(|fonts| {
        fonts.borrow()[idx]
            .as_ref()
            .expect("font_index returned a None slot")
            .atlas_texture
            .is_some()
    });
    if !has_atlas {
        let released = FONTS.with(|fonts| {
            fonts.borrow()[idx]
                .as_ref()
                .expect("font_index returned a None slot")
                .coverage_released
        });
        if released {
            crate::log_once!(
                "TEXT",
                "font {} released its coverage and has no atlas image; the caller must not ask a released font to draw coverage",
                font_id
            );
        } else {
            crate::log_once!(
                "TEXT",
                "font {} is metrics-only and has no atlas image; the caller must not ask a metrics font to draw",
                font_id
            );
        }
        return 0;
    }

    let id = FONTS.with(|fonts| {
        let fonts = fonts.borrow();
        let entry = fonts[idx]
            .as_ref()
            .expect("font_index returned a None slot");
        let atlas = entry
            .atlas_texture
            .expect("the metrics-only refusal above proved this font owns a texture");

        // Apply texture swizzle so GL_RED atlas reads as (r,r,r,r)
        with_gl(|gl| unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(atlas));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_SWIZZLE_R, glow::RED as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_SWIZZLE_G, glow::RED as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_SWIZZLE_B, glow::RED as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_SWIZZLE_A, glow::RED as i32);
        });

        // Register in IMAGES table (same pattern as stone_get_white_image)
        let id = crate::handles::mint(Class::Image);
        IMAGES.with(|images| {
            images.borrow_mut().insert(
                id,
                LoadedImage {
                    texture: atlas,
                    width: entry.atlas_width as u32,
                    height: entry.atlas_height as u32,
                    repeat: false,
                    format: crate::image_texture::ImageFormat::R8,
                    stub: false,
                },
            );
        });
        id
    });

    FONT_ATLAS_IMAGES.with(|imgs| {
        let mut imgs = imgs.borrow_mut();
        if idx < imgs.len() {
            imgs[idx] = Some(id);
        }
    });
    FONTS.with(|fonts| {
        if let Some(instance) = fonts
            .borrow_mut()
            .get_mut(idx)
            .and_then(|entry| entry.as_mut())
            .and_then(|entry| entry.instances.first_mut())
        {
            retain_font_instance_image(&mut instance.image, id);
        }
    });
    id
}

/// Return the generated atlas extent without exposing the GL image handle.
#[no_mangle]
pub extern "C" fn stone_get_font_atlas_size(
    font_id: c_uint,
    width: *mut c_int,
    height: *mut c_int,
) -> bool {
    let idx = match font_index(font_id) {
        Some(i) => i,
        None => return false,
    };
    FONTS.with(|fonts| {
        let fonts = fonts.borrow();
        let Some(entry) = fonts.get(idx).and_then(|slot| slot.as_ref()) else {
            return false;
        };
        if entry.coverage_released {
            return false;
        }
        if entry.atlas_width <= 0 || entry.atlas_height <= 0 {
            return false;
        }
        let w = entry.atlas_width;
        let h = entry.atlas_height;
        if width.is_null() || height.is_null() {
            return false;
        }
        unsafe {
            *width = w;
            *height = h;
        }
        true
    })
}

/// Retained CPU raster bytes for this font's atlases: the reader for the
/// census CPU row. Zero, because no CPU copy of any atlas survives the
/// upload and there is nothing resident to count.
///
/// Do not rebuild this row from `stone_get_font_instance_byte_len`: that is
/// the GL texture's own GPU-side bytes.
#[no_mangle]
pub extern "C" fn stone_get_font_cpu_byte_len(font_id: c_uint) -> u64 {
    if font_index(font_id).is_none() {
        return 0;
    }
    0
}

// Counting rule for the residency census: R8 coverage aliases count 1 byte
// per texel on the GPU side, and the CPU font row counts retained raster
// buffers only (zero: no CPU copy survives the upload).
#[cfg(test)]
mod font_census_tests {
    use super::*;
    use crate::image_texture::{live_texture_bytes, stone_get_image_size};

    const CENSUS_MAIN_PX: i32 = 32;
    const CENSUS_EXTRA_PX: i32 = 48;

    fn fake_texture(n: u32) -> glow::Texture {
        glow::NativeTexture(std::num::NonZeroU32::new(n).expect("non-zero texture name"))
    }

    /// Restores every registry the installer touches, on success and on
    /// failure: Drop runs before a failed assertion unwinds past the test.
    /// Lengths are captured at install time, so slots other tests leaked on
    /// this thread are left alone.
    struct CensusFonts {
        fonts_len: usize,
        atlas_images_len: usize,
        images: Vec<u64>,
    }

    impl CensusFonts {
        fn capture() -> Self {
            CensusFonts {
                fonts_len: FONTS.with(|f| f.borrow().len()),
                atlas_images_len: FONT_ATLAS_IMAGES.with(|i| i.borrow().len()),
                images: Vec::new(),
            }
        }

        fn note_image(&mut self, image: u64) {
            self.images.push(image);
        }
    }

    impl Drop for CensusFonts {
        fn drop(&mut self) {
            for image in &self.images {
                IMAGES.with(|images| images.borrow_mut().remove(image));
            }
            FONTS.with(|fonts| fonts.borrow_mut().truncate(self.fonts_len));
            FONT_ATLAS_IMAGES.with(|imgs| imgs.borrow_mut().truncate(self.atlas_images_len));
        }
    }

    /// A GPU-shaped font the way `rasterize_font` lays one out, minus the GL
    /// work: the bakes are real, the texture names are fake, and the atlas
    /// image getters never touch GL headless -- the swizzle is best-effort
    /// and the IMAGES registration is unconditional.
    fn install_census_font() -> c_uint {
        let bytes = FALLBACK_FONT.to_vec();
        let faces = FaceSet::default();
        let charset = Charset::default();
        let main = rasterize_glyphs(&bytes, CENSUS_MAIN_PX, faces, &charset, "DroidSans")
            .expect("DroidSans bakes at 32px");
        let extra = rasterize_glyphs(&bytes, CENSUS_EXTRA_PX, faces, &charset, "DroidSans")
            .expect("DroidSans bakes at 48px");
        let glyphs = main.glyphs;
        let entry = FontEntry {
            atlas_texture: Some(fake_texture(5201)),
            glyphs: glyphs.clone(),
            faces,
            charset,
            unit_metrics: main.unit_metrics,
            glyph_unit: main.glyph_unit.clone(),
            font_data: Rc::new(bytes),
            source_path: "DroidSans".to_owned(),
            font_pixel_size: CENSUS_MAIN_PX,
            atlas_width: main.atlas_width as i32,
            atlas_height: main.atlas_height as i32,
            instances: vec![
                AtlasInstance {
                    texture: fake_texture(5201),
                    glyphs: glyphs.clone(),
                    width: main.atlas_width as i32,
                    height: main.atlas_height as i32,
                    pixel_size: CENSUS_MAIN_PX,
                    image: None,
                    face_revision: 0,
                },
                AtlasInstance {
                    texture: fake_texture(5202),
                    glyphs: extra.glyphs,
                    width: extra.atlas_width as i32,
                    height: extra.atlas_height as i32,
                    pixel_size: CENSUS_EXTRA_PX,
                    image: None,
                    face_revision: 0,
                },
            ],
            face_revision: 0,
            coverage_released: false,
        };
        let font_id = FONTS.with(|fonts| {
            let mut fonts = fonts.borrow_mut();
            fonts.push(Some(entry));
            fonts.len() as c_uint
        });
        FONT_ATLAS_IMAGES.with(|imgs| imgs.borrow_mut().push(None));
        font_id
    }

    /// Expected texel counts, re-baked in the test body so the oracle is the
    /// rasterizer's answer, not the fixture's.
    fn oracle_texels(pixel_size: i32) -> u64 {
        let bake = rasterize_glyphs(
            FALLBACK_FONT,
            pixel_size,
            FaceSet::default(),
            &Charset::default(),
            "DroidSans",
        )
        .expect("the oracle bake matches the installer");
        bake.atlas_width as u64 * bake.atlas_height as u64
    }

    /// Coverage atlases upload R8, one byte per texel, so the live-image
    /// census must count the alias at 1 B/texel, not the RGBA8 4.
    #[test]
    fn coverage_aliases_count_one_byte_per_texel_live() {
        let mut guard = CensusFonts::capture();
        let font = install_census_font();
        let texels = oracle_texels(CENSUS_MAIN_PX);
        assert!(
            texels > 0,
            "the oracle bake is a real atlas, not an empty fixture"
        );

        let before = live_texture_bytes();
        let image = stone_get_font_atlas_image(font);
        assert_ne!(image, 0, "the atlas image registers headless");
        guard.note_image(image);

        let (mut w, mut h) = (0 as c_int, 0 as c_int);
        stone_get_image_size(image, &mut w, &mut h);
        assert_eq!(
            w as u64 * h as u64,
            texels,
            "the alias carries the bake's extent"
        );
        assert_eq!(
            live_texture_bytes() - before,
            texels,
            "one R8 texel is one byte in the live total"
        );
        assert_eq!(
            crate::image_texture::stone_get_image_byte_len(image),
            texels,
            "the per-image census agrees with the live total"
        );

        // A later target bucket mints its own alias over its own R8 texture.
        let extra_texels = oracle_texels(CENSUS_EXTRA_PX);
        assert!(extra_texels > 0, "the second bake is a real atlas too");
        let before_extra = live_texture_bytes();
        let extra_image = stone_get_font_instance_image(atlas_instance_id(font, 1));
        assert_ne!(extra_image, 0, "the bucket image registers headless");
        assert_ne!(extra_image, image, "a later bucket owns its texture");
        guard.note_image(extra_image);
        assert_eq!(
            live_texture_bytes() - before_extra,
            extra_texels,
            "a later bucket counts at 1 B/texel too"
        );
        assert_eq!(
            crate::image_texture::stone_get_image_byte_len(extra_image),
            extra_texels,
            "the per-image census agrees per bucket"
        );
    }

    /// The CPU font row counts retained raster buffers only. None survives
    /// the upload, so it is zero -- beside nonzero neighbors proving the
    /// font holds real bytes on the GPU side.
    #[test]
    fn font_cpu_row_counts_retained_rasters_only() {
        let _guard = CensusFonts::capture();
        let font = install_census_font();
        let texels = oracle_texels(CENSUS_MAIN_PX);
        assert!(
            texels > 0,
            "the oracle bake is a real atlas, not an empty fixture"
        );

        assert_eq!(
            stone_get_font_instance_byte_len(atlas_instance_id(font, 0)),
            texels,
            "the GPU side holds the raster bytes"
        );
        assert_eq!(
            stone_get_font_cpu_byte_len(font),
            0,
            "no CPU raster is retained after upload"
        );
    }

    /// S10: releasing the logo coverage drops the compat atlas and every
    /// bucket alias from the live total while the face (TTF bytes,
    /// metrics) stays resident for the menu and for measurement.
    #[test]
    fn release_coverage_drops_aliases_and_keeps_the_face() {
        let mut guard = CensusFonts::capture();
        let font = install_census_font();
        let main_texels = oracle_texels(CENSUS_MAIN_PX);
        let extra_texels = oracle_texels(CENSUS_EXTRA_PX);
        assert!(
            main_texels > 0 && extra_texels > 0,
            "both oracle bakes are real atlases"
        );

        let compat = stone_get_font_atlas_image(font);
        assert_ne!(compat, 0, "the compat alias registers headless");
        guard.note_image(compat);
        let bucket = stone_get_font_instance_image(atlas_instance_id(font, 1));
        assert_ne!(bucket, 0, "the bucket alias registers headless");
        assert_ne!(bucket, compat, "a later bucket owns its texture");
        guard.note_image(bucket);

        let data_len = stone_font_data_len(font);
        assert!(data_len > 1000, "the installed face retains its TTF bytes");
        let mut glyph_before = [0.0 as c_float; 9];
        assert!(
            stone_get_glyph_info(font, 0, b'A' as c_int, glyph_before.as_mut_ptr()),
            "the installed 'A' measures"
        );
        assert!(glyph_before[8] > 0.0, "the installed advance is positive");

        let before = live_texture_bytes();
        assert!(
            stone_font_release_coverage(font),
            "a live font releases its coverage"
        );
        assert_eq!(
            before - live_texture_bytes(),
            main_texels + extra_texels,
            "compat and bucket aliases leave the live total at 1 B/texel"
        );
        assert_eq!(
            stone_get_font_atlas_image_cached(font),
            0,
            "the compat alias is retired, not re-minted"
        );
        assert_eq!(
            stone_font_atlas_instance_count(font),
            0,
            "no bucket survives the release"
        );
        let (mut w, mut h) = (0 as c_int, 0 as c_int);
        assert!(
            !stone_get_font_atlas_size(font, &mut w, &mut h),
            "a released atlas publishes no extent"
        );
        assert_eq!(
            stone_font_data_len(font),
            data_len,
            "the TTF bytes stay resident"
        );
        let mut glyph_after = [0.0 as c_float; 9];
        assert!(
            stone_get_glyph_info(font, 0, b'A' as c_int, glyph_after.as_mut_ptr()),
            "metrics answer after the release"
        );
        assert_eq!(glyph_after, glyph_before, "the release moves no metric");
        assert!(
            stone_font_release_coverage(font),
            "releasing twice is idempotent"
        );
        assert!(
            !stone_font_release_coverage(999999),
            "an unknown font releases nothing"
        );
    }

    /// S10: a hot reload after the release refreshes the retained products
    /// (bytes, metrics) without re-uploading the coverage it dropped.
    #[test]
    fn reload_after_release_refreshes_metrics_and_keeps_coverage_down() {
        let mut guard = CensusFonts::capture();
        let font = install_census_font();
        let compat = stone_get_font_atlas_image(font);
        assert_ne!(compat, 0, "the compat alias registers headless");
        guard.note_image(compat);
        assert!(
            stone_font_release_coverage(font),
            "a live font releases its coverage"
        );
        let after_release = live_texture_bytes();

        let path = crate::test_fonts::font_file("pixel", &crate::test_fonts::pixel_font());
        let file_len = std::fs::read(&path)
            .expect("the pixel font fixture reads")
            .len() as i64;
        let cpath = std::ffi::CString::new(path.to_str().expect("the fixture path is UTF-8"))
            .expect("the fixture path has no NUL");
        assert!(
            stone_reload_font(font, cpath.as_ptr(), 24),
            "reload refreshes the retained products of a released font"
        );
        assert_eq!(
            stone_font_data_len(font),
            file_len,
            "reload swaps the retained bytes"
        );
        let (mut w, mut h) = (0 as c_int, 0 as c_int);
        assert!(
            !stone_get_font_atlas_size(font, &mut w, &mut h),
            "coverage stays down across the reload"
        );
        assert_eq!(
            stone_get_font_atlas_image_cached(font),
            0,
            "the reload mints no compat alias"
        );
        assert_eq!(
            live_texture_bytes(),
            after_release,
            "the reload re-uploads nothing"
        );
        let mut glyph = [0.0 as c_float; 9];
        assert!(
            stone_get_glyph_info(font, 0, b'A' as c_int, glyph.as_mut_ptr()),
            "the reloaded metrics answer"
        );
        assert!(glyph[8] > 0.0, "the reloaded advance is positive");
    }
}

/// Counts this thread's live heap bytes so a fixture can see what a call
/// retains. Pass-through to the system allocator otherwise.
#[cfg(test)]
pub(crate) mod retained_bytes {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static LIVE: Cell<isize> = const { Cell::new(0) };
    }

    struct Counting;

    fn note(delta: isize) {
        // TLS is gone during thread teardown; those frees are not measured.
        let _ = LIVE.try_with(|live| live.set(live.get() + delta));
    }

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note(layout.size() as isize);
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            note(-(layout.size() as isize));
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            note(layout.size() as isize);
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            note(new_size as isize - layout.size() as isize);
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static COUNTING: Counting = Counting;

    pub(crate) fn live() -> isize {
        LIVE.with(Cell::get)
    }
}

#[cfg(test)]
mod atlas_retention_tests {
    use super::*;
    use std::ffi::CString;

    fn load_metrics_font(size: c_int) -> c_uint {
        let path = CString::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../packages/stone/engine/assets/fonts/DroidSans.ttf"
        ))
        .expect("path");
        let extra = CString::new("").expect("extra");
        load_font_with_residency(
            path.as_ptr(),
            size,
            0,
            extra.as_ptr(),
            AtlasResidency::Metrics,
        )
    }

    fn atlas_extent(font_id: c_uint) -> (i32, i32) {
        let (mut w, mut h) = (0, 0);
        assert!(stone_get_font_atlas_size(font_id, &mut w, &mut h));
        (w, h)
    }

    /// The uploaded atlas lives in its texture; the CPU raster it came from
    /// must not stay resident beside it. A metrics load shares the entry
    /// shape with a GPU load and needs no context.
    #[test]
    fn a_loaded_font_retains_less_than_one_atlas_of_heap() {
        let before = retained_bytes::live();
        let font = load_metrics_font(128);
        let retained = retained_bytes::live() - before;
        assert_ne!(font, 0);
        let (w, h) = atlas_extent(font);
        let atlas = w as isize * h as isize;
        assert!(
            atlas > 1 << 20,
            "a 128px atlas ({w}x{h}) dwarfs the glyph tables"
        );
        assert!(
            retained < atlas,
            "font load retained {retained} heap bytes, at least its {w}x{h} CPU atlas"
        );
    }
}

/// How many text slots this font baked: the identity block plus whatever the
/// load declared. Rock sizes its per-font glyph cache from this rather than
/// from a literal, so a declared charset reaches the batcher without a second
/// place agreeing about its size.
#[no_mangle]
pub extern "C" fn stone_get_font_slot_count(font_id: c_uint) -> c_int {
    with_font_entry(font_id, 0, |entry| entry.charset.slot_count() as c_int)
}

/// The codepoint baked at `slot`, or -1 for a slot that carries none (the
/// 0..32 hole in the identity block, or an out-of-range index). Rock walks this
/// once per load to build its codepoint -> slot map; the mapping is published
/// from the charset that baked the atlas rather than recomputed from the same
/// rules on the other side.
#[no_mangle]
pub extern "C" fn stone_get_font_slot_codepoint(font_id: c_uint, slot: c_int) -> c_int {
    if slot < 0 {
        return -1;
    }
    with_font_entry(font_id, -1, |entry| {
        entry
            .charset
            .char_at(slot as usize)
            .map_or(-1, |ch| ch as c_int)
    })
}

/// Write 9 floats to `out`: u0, v0, u1, v1, width, height, bearing_x, bearing_y, advance.
/// `face` indexes Face::ALL (0 regular, 1 bold, 2 italic, 3 bold-italic).
/// Returns false if the face was not declared at load, the glyph is invalid,
/// or either index is out of range.
#[no_mangle]
pub extern "C" fn stone_get_glyph_info(
    font_id: c_uint,
    face: c_int,
    char_code: c_int,
    out: *mut c_float,
) -> bool {
    if out.is_null() || char_code < 0 {
        return false;
    }
    // The bound is Face::ALL's length, not a literal that can drift from it.
    let face = match usize::try_from(face).ok().and_then(Face::from_index) {
        Some(f) => f,
        None => return false,
    };

    with_font_entry(font_id, false, |entry| {
        // The bound is the font's own slot count: a charset is per font, so a
        // fixed literal here would either truncate a declared set or read past
        // a smaller one.
        let Some(gi) = entry.glyphs[face.index()].get(char_code as usize) else {
            return false;
        };
        if !gi.valid {
            return false;
        }

        unsafe {
            *out.add(0) = gi.u0;
            *out.add(1) = gi.v0;
            *out.add(2) = gi.u1;
            *out.add(3) = gi.v1;
            *out.add(4) = gi.width;
            *out.add(5) = gi.height;
            *out.add(6) = gi.bearing_x;
            *out.add(7) = gi.bearing_y;
            *out.add(8) = gi.advance;
        }
        true
    })
}

fn atlas_instance_id(font_id: u32, index: usize) -> u64 {
    ((font_id as u64) << 32) | (index as u64 + 1)
}

fn decode_atlas_instance_id(instance_id: u64) -> Option<(u32, usize)> {
    let font_id = (instance_id >> 32) as u32;
    let slot = (instance_id & 0xffff_ffff) as usize;
    if font_id == 0 || slot == 0 {
        return None;
    }
    Some((font_id, slot - 1))
}

fn instance_matches(instance: &AtlasInstance, pixel_size: i32, face_revision: u64) -> bool {
    instance.pixel_size == pixel_size && instance.face_revision == face_revision
}

/// Create or reuse one independent coverage raster product. The returned handle
/// is stable until the face is unloaded; it is intentionally distinct from the
/// face's backend id.
#[no_mangle]
pub extern "C" fn stone_ensure_font_atlas_instance(font_id: c_uint, pixel_size: c_int) -> u64 {
    if pixel_size < 1 {
        return 0;
    }
    let idx = match font_index(font_id) {
        Some(idx) => idx,
        None => return 0,
    };
    let existing = FONTS.with(|fonts| {
        fonts
            .borrow()
            .get(idx)
            .and_then(|entry| entry.as_ref())
            .and_then(|entry| {
                let revision = entry.face_revision;
                entry
                    .instances
                    .iter()
                    .position(|instance| instance_matches(instance, pixel_size, revision))
            })
    });
    if let Some(instance) = existing {
        return atlas_instance_id(font_id, instance);
    }

    let source = FONTS.with(|fonts| {
        fonts
            .borrow()
            .get(idx)
            .and_then(|entry| entry.as_ref())
            .map(|entry| {
                (
                    entry.font_data.clone(),
                    entry.faces,
                    entry.charset.clone(),
                    entry.source_path.clone(),
                    entry.face_revision,
                )
            })
    });
    let Some((font_data, faces, charset, source_path, face_revision)) = source else {
        return 0;
    };

    let raster = match rasterize_glyphs(&font_data, pixel_size, faces, &charset, &source_path) {
        Some(raster) => raster,
        None => return 0,
    };
    let glyphs = raster.glyphs;
    let width = raster.atlas_width;
    let height = raster.atlas_height;
    let texture = match upload_atlas_texture(&raster.atlas_data, width, height) {
        Some(texture) => texture,
        None => return 0,
    };
    let instance = AtlasInstance {
        texture,
        glyphs,
        width: width as i32,
        height: height as i32,
        pixel_size,
        image: None,
        face_revision,
    };
    FONTS.with(|fonts| {
        let mut fonts = fonts.borrow_mut();
        let Some(entry) = fonts.get_mut(idx).and_then(|entry| entry.as_mut()) else {
            return 0;
        };
        entry.instances.push(instance);
        atlas_instance_id(font_id, entry.instances.len() - 1)
    })
}

fn with_atlas_instance<R>(instance_id: u64, default: R, f: impl FnOnce(&AtlasInstance) -> R) -> R {
    let Some((font_id, index)) = decode_atlas_instance_id(instance_id) else {
        return default;
    };
    let Some(font_idx) = font_index(font_id) else {
        return default;
    };
    FONTS.with(|fonts| {
        let fonts = fonts.borrow();
        fonts
            .get(font_idx)
            .and_then(|entry| entry.as_ref())
            .and_then(|entry| entry.instances.get(index))
            .map_or(default, f)
    })
}

#[no_mangle]
pub extern "C" fn stone_font_atlas_instance_count(font_id: c_uint) -> c_int {
    let Some(idx) = font_index(font_id) else {
        return 0;
    };
    FONTS.with(|fonts| {
        fonts
            .borrow()
            .get(idx)
            .and_then(|entry| entry.as_ref())
            .map(|entry| entry.instances.len() as c_int)
            .unwrap_or(0)
    })
}

#[no_mangle]
pub extern "C" fn stone_get_font_atlas_instance_pixel_size(
    font_id: c_uint,
    index: c_uint,
) -> c_int {
    let Some(idx) = font_index(font_id) else {
        return 0;
    };
    FONTS.with(|fonts| {
        fonts
            .borrow()
            .get(idx)
            .and_then(|entry| entry.as_ref())
            .and_then(|entry| entry.instances.get(index as usize))
            .map(|instance| instance.pixel_size)
            .unwrap_or(0)
    })
}

#[no_mangle]
pub extern "C" fn stone_get_font_atlas_instance_image(font_id: c_uint, index: c_uint) -> u64 {
    let Some(idx) = font_index(font_id) else {
        return 0;
    };
    FONTS.with(|fonts| {
        fonts
            .borrow()
            .get(idx)
            .and_then(|entry| entry.as_ref())
            .and_then(|entry| entry.instances.get(index as usize))
            .and_then(|instance| instance.image)
            .unwrap_or(0)
    })
}

#[no_mangle]
pub extern "C" fn stone_get_font_atlas_image_cached(font_id: c_uint) -> u64 {
    let Some(idx) = font_index(font_id) else {
        return 0;
    };
    FONT_ATLAS_IMAGES.with(|images| images.borrow().get(idx).copied().flatten().unwrap_or(0))
}

#[no_mangle]
pub extern "C" fn stone_get_font_atlas_instance_image_at(font_id: c_uint, index: c_int) -> u64 {
    if index < 0 {
        return 0;
    }
    stone_get_font_instance_image(atlas_instance_id(font_id, index as usize))
}

#[no_mangle]
pub extern "C" fn stone_get_font_instance_size(
    instance_id: u64,
    width: *mut c_int,
    height: *mut c_int,
) -> bool {
    if width.is_null() || height.is_null() {
        return false;
    }
    with_atlas_instance(instance_id, false, |instance| unsafe {
        *width = instance.width;
        *height = instance.height;
        instance.width > 0 && instance.height > 0
    })
}

#[no_mangle]
pub extern "C" fn stone_get_font_instance_pixel_size(instance_id: u64) -> c_int {
    with_atlas_instance(instance_id, 0, |instance| instance.pixel_size)
}

#[no_mangle]
pub extern "C" fn stone_get_font_instance_byte_len(instance_id: u64) -> u64 {
    with_atlas_instance(instance_id, 0, AtlasInstance::byte_len)
}

#[no_mangle]
pub extern "C" fn stone_get_font_instance_glyph_info(
    instance_id: u64,
    face: c_int,
    char_code: c_int,
    out: *mut c_float,
) -> bool {
    if out.is_null() || char_code < 0 {
        return false;
    }
    let Some(face) = usize::try_from(face)
        .ok()
        .and_then(|face| Face::from_index(face))
    else {
        return false;
    };
    with_atlas_instance(instance_id, false, |instance| {
        let Some(glyph) = instance.glyphs[face.index()].get(char_code as usize) else {
            return false;
        };
        if !glyph.valid {
            return false;
        }
        unsafe {
            *out.add(0) = glyph.u0;
            *out.add(1) = glyph.v0;
            *out.add(2) = glyph.u1;
            *out.add(3) = glyph.v1;
            *out.add(4) = glyph.width;
            *out.add(5) = glyph.height;
            *out.add(6) = glyph.bearing_x;
            *out.add(7) = glyph.bearing_y;
            *out.add(8) = glyph.advance;
        }
        true
    })
}

#[no_mangle]
pub extern "C" fn stone_get_font_instance_image(instance_id: u64) -> u64 {
    let Some((font_id, index)) = decode_atlas_instance_id(instance_id) else {
        return 0;
    };
    let Some(font_idx) = font_index(font_id) else {
        return 0;
    };
    let cached = FONTS.with(|fonts| {
        fonts
            .borrow()
            .get(font_idx)
            .and_then(|entry| entry.as_ref())
            .and_then(|entry| entry.instances.get(index))
            .and_then(|instance| instance.image)
    });
    if let Some(image) = cached {
        return image;
    }
    let details = FONTS.with(|fonts| {
        fonts
            .borrow()
            .get(font_idx)
            .and_then(|entry| entry.as_ref())
            .and_then(|entry| entry.instances.get(index))
            .map(|instance| (instance.texture, instance.width, instance.height))
    });
    let Some((texture, width, height)) = details else {
        return 0;
    };
    with_gl(|gl| unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_SWIZZLE_R, glow::RED as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_SWIZZLE_G, glow::RED as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_SWIZZLE_B, glow::RED as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_SWIZZLE_A, glow::RED as i32);
    });
    let image = crate::handles::mint(Class::Image);
    IMAGES.with(|images| {
        images.borrow_mut().insert(
            image,
            LoadedImage {
                texture,
                width: width as u32,
                height: height as u32,
                repeat: false,
                format: crate::image_texture::ImageFormat::R8,
                stub: false,
            },
        );
    });
    FONTS.with(|fonts| {
        if let Some(instance) = fonts
            .borrow_mut()
            .get_mut(font_idx)
            .and_then(|entry| entry.as_mut())
            .and_then(|entry| entry.instances.get_mut(index))
        {
            instance.image = Some(image);
        }
    });
    image
}

/// Return the font's pixel size (set during stone_load_font).
#[no_mangle]
pub extern "C" fn stone_get_font_pixel_size(font_id: c_uint) -> c_int {
    with_font_entry(font_id, 0, |entry| entry.font_pixel_size)
}

#[no_mangle]
pub extern "C" fn stone_get_font_unit_metrics(font_id: c_uint, out: *mut c_float) -> bool {
    if out.is_null() {
        return false;
    }
    let Some(idx) = font_index(font_id) else {
        return false;
    };
    // Prepared at raster time from the same bytes: no parse, no clone.
    let metrics = FONTS.with(|fonts| {
        fonts
            .borrow()
            .get(idx)
            .and_then(|entry| entry.as_ref())
            .map(|entry| entry.unit_metrics)
    });
    let Some(metrics) = metrics else { return false };
    unsafe {
        *out.add(0) = metrics.units_per_em;
        *out.add(1) = metrics.ascender;
        *out.add(2) = metrics.descender;
        *out.add(3) = metrics.line_gap;
        *out.add(4) = metrics.cap_height;
    }
    true
}

#[no_mangle]
pub extern "C" fn stone_get_glyph_unit_metrics(
    font_id: c_uint,
    face: c_int,
    char_code: c_int,
    out: *mut c_float,
) -> bool {
    if out.is_null() || char_code < 0 {
        return false;
    }
    let Some(face) = usize::try_from(face).ok().and_then(Face::from_index) else {
        return false;
    };
    let Some(idx) = font_index(font_id) else {
        return false;
    };
    // Prepared at raster time from the same bytes at unit scale: a slot
    // outside the table, a charset hole and an uncovered codepoint all read
    // back the `None` the per-query parse answered with. No parse, no clone.
    let metrics = FONTS.with(|fonts| {
        fonts
            .borrow()
            .get(idx)
            .and_then(|entry| entry.as_ref())
            .and_then(|entry| {
                entry
                    .glyph_unit
                    .get(face.index())?
                    .get(char_code as usize)?
                    .as_ref()
                    .copied()
            })
    });
    let Some(metrics) = metrics else {
        return false;
    };
    unsafe {
        *out.add(0) = metrics.left;
        *out.add(1) = metrics.top;
        *out.add(2) = metrics.width;
        *out.add(3) = metrics.height;
        *out.add(4) = metrics.advance;
    }
    true
}

/// Live font ids (`native_ledger`). 1-based, and monotonic because
/// `stone_load_font` only ever pushes -- so a font destroyed and reloaded is a
/// different id rather than the same slot back.
pub(crate) fn live_font_ids() -> Vec<u64> {
    FONTS.with(|fonts| {
        fonts
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.is_some())
            .map(|(idx, _)| idx as u64 + 1)
            .collect()
    })
}

/// Bytes of TTF/OTF the font retains (`FontEntry::font_data`), for the asset
/// report's CPU column. -1 for an id that names no live font.
#[no_mangle]
pub extern "C" fn stone_font_data_len(font_id: c_uint) -> i64 {
    let Some(idx) = font_index(font_id) else {
        return -1;
    };
    FONTS.with(|fonts| {
        fonts.borrow()[idx]
            .as_ref()
            .map_or(-1, |entry| entry.font_data.len() as i64)
    })
}

/// Drop an `IMAGES` entry that is a font atlas CACHE rather than an owned
/// texture, clearing the font's cache slot with it. The GL texture belongs to
/// the font and survives; the next `stone_get_font_atlas_image` mints the entry
/// again. Returns false when `image_id` is not an atlas cache, which is how the
/// ledger's image teardown tells an alias from an image it may delete.
pub(crate) fn forget_atlas_image(image_id: u64) -> bool {
    let idx = FONT_ATLAS_IMAGES.with(|imgs| {
        imgs.borrow()
            .iter()
            .position(|slot| *slot == Some(image_id))
    });
    let found = if let Some(idx) = idx {
        FONT_ATLAS_IMAGES.with(|imgs| imgs.borrow_mut()[idx] = None);
        true
    } else {
        FONTS.with(|fonts| {
            for entry in fonts
                .borrow_mut()
                .iter_mut()
                .filter_map(|entry| entry.as_mut())
            {
                for instance in &mut entry.instances {
                    if instance.image == Some(image_id) {
                        instance.image = None;
                        return true;
                    }
                }
            }
            false
        })
    };
    if !found {
        return false;
    }
    if IMAGES
        .with(|images| images.borrow_mut().remove(&image_id))
        .is_some()
    {
        crate::handles::retire(Class::Image, image_id);
    }
    true
}

/// Drop EVERY cached atlas image, through the same single-handle path that
/// gets one right.
///
/// The two whole-subsystem teardowns -- `text_init`'s hot-reload re-entry and
/// `stone_text_shutdown` -- delete every font's atlas texture, so every cached
/// `IMAGES` entry pointing at one is dangling the moment they run. Clearing
/// `FONT_ATLAS_IMAGES` alone left those entries LIVE in the registry, naming
/// deleted textures: an image leak the id census sees as `live`.
///
/// It delegates rather than restating: `forget_atlas_image` is where the
/// retire, the registry removal and the "the font owns this texture, do not
/// delete it" rule live, and a second copy of that rule is a second place for
/// it to drift.
fn forget_all_atlas_images() {
    let cached: Vec<u64> =
        FONT_ATLAS_IMAGES.with(|imgs| imgs.borrow().iter().filter_map(|slot| *slot).collect());
    for image_id in cached {
        forget_atlas_image(image_id);
    }
}

fn forget_instance_images(entries: &[FontEntry]) {
    for image_id in entries
        .iter()
        .flat_map(|entry| entry.instances.iter().filter_map(|instance| instance.image))
    {
        if IMAGES
            .with(|images| images.borrow_mut().remove(&image_id))
            .is_some()
        {
            crate::handles::retire(Class::Image, image_id);
        }
    }
}

/// Unload a previously-loaded font. Frees the GL atlas texture and
/// clears the slot. Subsequent ops against this font_id silently
/// no-op. Returns false if the font_id was already unloaded or out
/// of range — caller can ignore unless it wants to assert.
#[no_mangle]
pub extern "C" fn stone_unload_font(font_id: c_uint) -> bool {
    let idx = match font_index(font_id) {
        Some(i) => i,
        None => return false,
    };
    let entry = FONTS.with(|fonts| fonts.borrow_mut()[idx].take());
    let entry = match entry {
        Some(e) => e,
        None => return false,
    };
    // Drop the cached IMAGES handle for this font's atlas, if any.
    let image_id = FONT_ATLAS_IMAGES.with(|imgs| {
        let mut imgs = imgs.borrow_mut();
        imgs[idx].take()
    });
    if let Some(id) = image_id {
        if IMAGES
            .with(|images| images.borrow_mut().remove(&id))
            .is_some()
        {
            crate::handles::retire(Class::Image, id);
        }
    }
    for id in entry.instances.iter().filter_map(|instance| instance.image) {
        if IMAGES
            .with(|images| images.borrow_mut().remove(&id))
            .is_some()
        {
            crate::handles::retire(Class::Image, id);
        }
    }
    delete_font_textures(std::slice::from_ref(&entry));
    true
}

/// Drop one font's atlas textures and image aliases while retaining its
/// bytes, metrics and charset. Later raster demand can rebuild the atlases.
///
/// A metrics-shaped entry holds nothing native to drop and returns true
/// without recording the release, so its size and reload behavior are
/// unchanged. Idempotent otherwise. False only names an unknown font id.
#[no_mangle]
pub extern "C" fn stone_font_release_coverage(font_id: c_uint) -> bool {
    let idx = match font_index(font_id) {
        Some(i) => i,
        None => return false,
    };
    let (atlas, instances) = FONTS.with(|fonts| {
        let mut fonts = fonts.borrow_mut();
        let entry = fonts
            .get_mut(idx)
            .and_then(|entry| entry.as_mut())
            .expect("font_index returned a None slot");
        if entry.atlas_texture.is_some() || !entry.instances.is_empty() {
            entry.coverage_released = true;
        }
        (
            entry.atlas_texture.take(),
            std::mem::take(&mut entry.instances),
        )
    });
    with_gl(|gl| unsafe {
        if let Some(texture) = atlas {
            gl.delete_texture(texture);
        }
        for instance in &instances {
            // Instance 0 aliases the compat texture; deleting it twice would
            // hand a later creation a recycled name.
            if Some(instance.texture) != atlas {
                gl.delete_texture(instance.texture);
            }
        }
    });
    // Instance 0 shares the compat alias id, so the removal decides who
    // retires it, exactly as the unload path does.
    let mut retired = Vec::new();
    if let Some(id) = FONT_ATLAS_IMAGES.with(|imgs| imgs.borrow_mut()[idx].take()) {
        retired.push(id);
    }
    retired.extend(instances.iter().filter_map(|instance| instance.image));
    for id in retired {
        if IMAGES
            .with(|images| images.borrow_mut().remove(&id))
            .is_some()
        {
            crate::handles::retire(Class::Image, id);
        }
    }
    true
}

#[cfg(test)]
mod atlas_face_tests {
    use super::*;

    fn droid() -> Vec<u8> {
        FALLBACK_FONT.to_vec()
    }

    const PX: i32 = 32;

    fn bake(faces: FaceSet) -> RasterizedAtlas {
        rasterize_glyphs(&droid(), PX, faces, &Charset::default(), "DroidSans")
            .expect("DroidSans bakes")
    }

    const BOTH: FaceSet = FaceSet {
        bold: true,
        italic: true,
    };

    // Row count derived from the atlas the bake produced, not from the
    // constants it was built from: the cell pitch is the v-distance between
    // glyph 0 and glyph 16, which sit one row apart by construction.
    fn rows(a: &RasterizedAtlas) -> u32 {
        let h = a.atlas_height as f32;
        let r0 = a.glyphs[Face::Regular.index()][GLYPH_FIRST].v0 * h;
        let r1 = a.glyphs[Face::Regular.index()][GLYPH_FIRST + 16].v0 * h;
        let pitch = r1 - r0;
        assert!(pitch > 0.0, "row pitch must be positive");
        (h / pitch).round() as u32
    }

    // The default declares nothing, so every existing caller keeps the atlas it
    // had. Stated as an EQUALITY with a single-face bake rather than as "small
    // enough": a layout that reserved four blocks and left three empty would
    // pass a size bound and fail this.
    #[test]
    fn declaring_nothing_bakes_exactly_one_face() {
        let plain = bake(FaceSet::default());
        let four = bake(BOTH);

        assert_eq!(
            rows(&plain),
            6,
            "96 text slots at 16 per row is one 6-row block"
        );
        assert_eq!(rows(&four), 24, "four declared faces are four 6-row blocks");

        for face in [Face::Bold, Face::Italic, Face::BoldItalic] {
            let any_valid = (GLYPH_FIRST..=GLYPH_LAST).any(|c| plain.glyphs[face.index()][c].valid);
            assert!(
                !any_valid,
                "{face:?} must be entirely invalid when it was never declared -- \
                 a face that silently bakes is a face that silently falls back"
            );
        }
        assert!(
            plain.glyphs[Face::Regular.index()][b'n' as usize].valid,
            "regular is always baked"
        );
    }

    #[test]
    fn latin_accent_is_baked_into_the_extension_slot() {
        let plain = bake(FaceSet::default());
        let accent = plain.glyphs[Face::Regular.index()][GLYPH_ACCENT_A_ACUTE_SLOT];
        assert!(accent.valid, "DroidSans must provide the Á glyph");
        assert!(accent.advance > 0.0, "Á must carry an advance");
    }

    // Declaring one axis bakes that face and NOT the other, and not the
    // combination either: bold-italic needs both axes.
    #[test]
    fn each_axis_is_declared_independently() {
        let bold_only = bake(FaceSet {
            bold: true,
            italic: false,
        });
        let valid = |a: &RasterizedAtlas, f: Face| a.glyphs[f.index()][b'n' as usize].valid;

        assert!(valid(&bold_only, Face::Regular), "regular baked");
        assert!(valid(&bold_only, Face::Bold), "declared bold baked");
        assert!(
            !valid(&bold_only, Face::Italic),
            "undeclared italic must not bake"
        );
        assert!(
            !valid(&bold_only, Face::BoldItalic),
            "bold-italic needs BOTH axes declared -- half of it is not it"
        );

        let italic_only = bake(FaceSet {
            bold: false,
            italic: true,
        });
        assert!(valid(&italic_only, Face::Italic), "declared italic baked");
        assert!(
            !valid(&italic_only, Face::Bold),
            "undeclared bold must not bake"
        );
    }

    // Every baked glyph's UV box must stay inside its own cell. This is the
    // overhang claim: the oblique pushes ink right of the advance, and cells
    // sized from Regular alone would bleed bold-italic into its neighbour.
    // The second assertion is what makes the first non-vacuous -- it proves
    // the widest face IS wider, so cell sizing had to account for it.
    #[test]
    fn cells_hold_the_widest_face() {
        let four = bake(BOTH);
        let plain = bake(FaceSet::default());
        assert!(
            four.atlas_width > plain.atlas_width,
            "bold-italic must be wider than regular ({}px vs {}px cells); if it is not, \
             the containment check below proves nothing",
            four.atlas_width / 16,
            plain.atlas_width / 16
        );

        let cell_w = four.atlas_width as f32 / 16.0;
        for face in Face::ALL {
            for c in GLYPH_FIRST..=GLYPH_LAST {
                let gi = &four.glyphs[face.index()][c];
                if !gi.valid || gi.width == 0.0 {
                    continue;
                }
                let idx = c - GLYPH_FIRST;
                let col = (idx % 16) as f32;
                let (left, right) = (
                    gi.u0 * four.atlas_width as f32,
                    gi.u1 * four.atlas_width as f32,
                );
                assert!(
                    left >= col * cell_w && right <= (col + 1.0) * cell_w,
                    "{face:?} '{}' spans [{left:.1},{right:.1}] outside its cell \
                     [{:.1},{:.1}] -- overhang bled into the neighbouring glyph",
                    c as u8 as char,
                    col * cell_w,
                    (col + 1.0) * cell_w
                );
            }
        }
    }

    // Each face occupies its own row block, so the same character reads from
    // four different places. Without this a face index could be accepted and
    // quietly resolve to the same texels as Regular.
    #[test]
    fn faces_occupy_distinct_atlas_regions() {
        let four = bake(BOTH);
        let mut vs: Vec<f32> = Face::ALL
            .iter()
            .map(|f| four.glyphs[f.index()][b'n' as usize].v0)
            .collect();
        vs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for w in vs.windows(2) {
            assert!(w[1] > w[0], "two faces share a row block ({:?})", vs);
        }

        let g = |f: Face| four.glyphs[f.index()][b'n' as usize];
        assert!(
            g(Face::Bold).advance > g(Face::Regular).advance,
            "bold must set wider than regular"
        );
        assert!(
            (g(Face::Italic).advance - g(Face::Regular).advance).abs() < 0.01,
            "a shear must not change the advance"
        );
    }

    // The pixel-font guard. OBLIQUE is what breaks a pixel grid; synthetic
    // bold at a clean size does NOT, because the widening (px/24) is a whole
    // number of texels there and a centred stroke lands back on the grid.
    // Both halves are asserted, because the bold half is exactly why the
    // guard has to measure every declared face instead of probing one.
    #[test]
    fn oblique_not_bold_antialiases_a_pixel_font() {
        let font = pixel_font();
        let clean = grid_gray_fraction(&font, 48.0);
        assert!(
            clean <= GRID_CLEAN,
            "the pixel font at 48px is the binary control ({clean:.3})"
        );

        let f = RasterFont::new(&font, 48.0).expect("the pixel font parses");
        let bold = glyph_raster::gray_fraction(&f, GRID_PROBE, Face::Bold.synth(48.0));
        let italic = glyph_raster::gray_fraction(&f, GRID_PROBE, Face::Italic.synth(48.0));

        assert!(
            italic - clean > GRID_MARGIN,
            "synthesized oblique must visibly antialias a pixel font \
             (regular {clean:.3}, italic {italic:.3}) -- if it does not, the warning \
             this test justifies is dead"
        );
        assert!(
            bold <= GRID_CLEAN,
            "at 48px the bold widening is 2 whole texels, so it must stay binary \
             (got {bold:.3}); a guard that probed bold alone would report this font clean"
        );
    }

    fn pixel_font() -> Vec<u8> {
        crate::test_fonts::pixel_font()
    }
}

#[cfg(test)]
mod offgrid_tests {
    use super::*;

    fn pixel_font() -> Vec<u8> {
        crate::test_fonts::pixel_font()
    }

    // A pixel font has near-binary (clean) sizes and,
    // between them, antialiased (off-grid) sizes. The detector must stay
    // silent on the clean ones and fire on the dirty ones. 48 rasterizes
    // near-binary, so it must NOT warn.
    #[test]
    fn offgrid_detector_fires_only_off_grid() {
        let font = pixel_font();
        let font = font.as_slice();

        // 48 is on-grid (a clean multiple of the pixel grid).
        assert!(
            grid_gray_fraction(&font, 48.0) <= GRID_CLEAN,
            "expected 48px near-binary, got {:.3}",
            grid_gray_fraction(&font, 48.0)
        );
        assert!(
            offgrid_report(&font, 48, "pixel").is_none(),
            "48px should not warn (it is a clean multiple)"
        );

        // Somewhere between native multiples there must be an off-grid
        // size that the detector flags -- proves the fire path works, not
        // just the early-out.
        let fired: Vec<i32> = (12..=64)
            .filter(|&px| offgrid_report(&font, px, "pixel").is_some())
            .collect();
        assert!(
            !fired.is_empty(),
            "expected at least one off-grid size in 12..=64 to warn; none did"
        );
    }

    // A smooth font (DroidSans, the embedded fallback) antialiases at
    // every size, so no size is ever near-binary -> never warn.
    #[test]
    fn offgrid_detector_silent_on_smooth_font() {
        let font = FALLBACK_FONT;
        for px in (12..=64).step_by(4) {
            assert!(
                offgrid_report(&font, px, "DroidSans").is_none(),
                "smooth font should never warn (px={})",
                px
            );
        }
    }
}

#[cfg(test)]
mod atlas_image_teardown_tests {
    use super::*;

    #[test]
    fn atlas_registration_keeps_an_existing_instance_image_reachable() {
        let mut image = Some(17);
        retain_font_instance_image(&mut image, 23);
        assert_eq!(image, Some(17));

        let mut empty = None;
        retain_font_instance_image(&mut empty, 23);
        assert_eq!(empty, Some(23));
    }

    fn fake_texture(n: u32) -> glow::Texture {
        glow::NativeTexture(std::num::NonZeroU32::new(n).expect("non-zero texture name"))
    }

    /// Register an atlas image the way `stone_get_font_atlas_image` does, minus
    /// the GL work, so the teardown below has a real registry entry to find.
    fn cache_atlas_image(slot: usize, texture: u32) -> u64 {
        FONT_ATLAS_IMAGES.with(|imgs| {
            let mut imgs = imgs.borrow_mut();
            while imgs.len() <= slot {
                imgs.push(None);
            }
        });
        let id = crate::handles::mint(Class::Image);
        IMAGES.with(|images| {
            images.borrow_mut().insert(
                id,
                crate::LoadedImage {
                    texture: fake_texture(texture),
                    width: 8,
                    height: 8,
                    repeat: false,
                    format: crate::image_texture::ImageFormat::R8,
                    stub: false,
                },
            );
        });
        FONT_ATLAS_IMAGES.with(|imgs| imgs.borrow_mut()[slot] = Some(id));
        id
    }

    /// Tearing the text subsystem down takes its cached atlas images OUT of
    /// `IMAGES`, rather than clearing the cache slots and leaving live handles
    /// naming deleted textures.
    ///
    /// `text_init`'s hot-reload re-entry and `stone_text_shutdown` both delete
    /// every font's atlas texture; an `IMAGES` entry that outlives that is a
    /// dangling handle the id census counts as live.
    ///
    /// The population is asserted on BOTH sides -- present first, gone after --
    /// so a run where nothing was ever cached cannot report the same green.
    #[test]
    fn tearing_down_the_atlas_cache_retires_its_image_handles() {
        let first = cache_atlas_image(0, 4001);
        let second = cache_atlas_image(1, 4002);
        assert_ne!(first, second, "two atlas slots mint two image handles");
        assert_eq!(
            IMAGES.with(|i| {
                let i = i.borrow();
                [first, second]
                    .iter()
                    .filter(|id| i.contains_key(id))
                    .count()
            }),
            2,
            "both atlas images are live before the teardown, which is the \
             population the assertion below is over"
        );

        forget_all_atlas_images();

        assert_eq!(
            IMAGES.with(|i| {
                let i = i.borrow();
                [first, second]
                    .iter()
                    .filter(|id| i.contains_key(id))
                    .count()
            }),
            0,
            "no atlas image may outlive the texture its font owned"
        );
        assert!(
            FONT_ATLAS_IMAGES.with(|imgs| imgs.borrow().iter().take(2).all(|s| s.is_none())),
            "and the cache slots are empty, so the next draw mints them again"
        );
    }
}

#[cfg(test)]
mod charset_tests {
    use super::*;

    const PX: i32 = 32;

    fn emoji_font() -> Vec<u8> {
        crate::test_fonts::symbol_only_font()
    }

    fn bake(extra: &str) -> RasterizedAtlas {
        rasterize_glyphs(
            &emoji_font(),
            PX,
            FaceSet::default(),
            &Charset::parse(extra),
            "symbol",
        )
        .expect("the symbol font bakes")
    }

    // The whole point of the arc: a codepoint past ASCII reaches the atlas.
    #[test]
    fn a_declared_emoji_bakes_into_an_appended_slot() {
        let a = bake("\u{1F600}");
        let slot = Charset::parse("\u{1F600}")
            .slot_of('\u{1F600}')
            .expect("a declared codepoint has a slot");
        assert_eq!(slot, GLYPH_ASCII_SLOTS, "the first extra appends at 128");

        let g = &a.glyphs[Face::Regular.index()][slot];
        assert!(
            g.valid,
            "the symbol font covers U+1F600 through its format 12 cmap"
        );
        assert!(g.advance > 0.0, "an emoji glyph carries an advance");
        assert!(g.width > 0.0 && g.height > 0.0, "and has ink");
    }

    // Declaring extras must not disturb the identity block: the grid, the face
    // row-blocks and batch.rock's cap-height probe all index it positionally.
    // Measured on a font that HAS ASCII, so "unchanged" is not vacuous.
    #[test]
    fn declaring_extras_leaves_the_ascii_slots_where_they_were() {
        let droid = FALLBACK_FONT.to_vec();
        let bake_droid = |extra: &str| {
            rasterize_glyphs(
                &droid,
                PX,
                FaceSet::default(),
                &Charset::parse(extra),
                "DroidSans",
            )
            .expect("DroidSans bakes")
        };
        let plain = bake_droid("");
        let with_extras = bake_droid("\u{03A9}\u{0416}"); // omega, cyrillic zhe

        let mut compared = 0;
        for slot in GLYPH_FIRST..=GLYPH_LAST {
            let a = &plain.glyphs[Face::Regular.index()][slot];
            let b = &with_extras.glyphs[Face::Regular.index()][slot];
            assert_eq!(a.valid, b.valid, "slot {slot} validity moved");
            assert_eq!(a.advance, b.advance, "slot {slot} advance moved");
            compared += 1;
        }
        assert_eq!(compared, 95, "the whole printable-ASCII block was compared");
        assert!(
            plain.glyphs[Face::Regular.index()][b'X' as usize].valid,
            "slot 88 is 'X' -- batch.rock reads cap height from it positionally"
        );
        assert!(
            with_extras.glyphs[Face::Regular.index()][GLYPH_ASCII_SLOTS].valid,
            "and the declared extra did land, so the comparison was not against a no-op bake"
        );
    }

    // An emoji font like NotoEmoji carries no printable ASCII, so a mixed
    // sentence needs two fonts however wide the charset gets. This is why the
    // fallback arc exists rather than being an optimization, and Rock's
    // cap-height probe takes its documented pixel-size fallback for this font
    // rather than reading a valid 'X'.
    #[test]
    fn an_emoji_only_font_carries_emoji_but_no_ascii() {
        let a = bake("\u{1F600}");
        let regular = &a.glyphs[Face::Regular.index()];
        assert!(
            !regular[b'X' as usize].valid && !regular[b'a' as usize].valid,
            "the symbol font maps no ASCII codepoint"
        );
        assert!(
            regular[GLYPH_ASCII_SLOTS].valid,
            "while the declared emoji bakes -- the font is a symbol subset, not a broken parse"
        );
    }

    // A charset is a declaration, not a request: what the font does not cover
    // stays invalid and the draw skips it, exactly as an uncovered ASCII slot
    // already does. U+10FFFE is a Unicode noncharacter, permanently
    // unassigned, so no font covers it.
    #[test]
    fn an_uncovered_declared_codepoint_bakes_invalid() {
        let a = bake("\u{10FFFE}");
        let slot = Charset::parse("\u{10FFFE}").slot_of('\u{10FFFE}').unwrap();
        assert!(
            !a.glyphs[Face::Regular.index()][slot].valid,
            "the symbol font has no U+10FFFE; the slot exists and is invalid"
        );
    }

    #[test]
    fn the_charset_drops_duplicates_and_codepoints_ascii_already_reaches() {
        // 'A' and the accent both already have identity slots.
        let c = Charset::parse("A\u{00C1}\u{1F600}\u{1F600}\u{1F44D}");
        assert_eq!(c.slot_count(), GLYPH_ASCII_SLOTS + 2, "two extras survive");
        assert_eq!(c.slot_of('A'), Some(b'A' as usize));
        assert_eq!(c.slot_of('\u{00C1}'), Some(GLYPH_ACCENT_A_ACUTE_SLOT));
        assert_eq!(c.slot_of('\u{1F600}'), Some(GLYPH_ASCII_SLOTS));
        assert_eq!(c.slot_of('\u{1F44D}'), Some(GLYPH_ASCII_SLOTS + 1));
        assert_eq!(c.slot_of('\u{1F525}'), None, "undeclared has no slot");
    }
}

// The metrics-only load, exercised in the environment it exists for: a test
// process with no window, so `with_gl` answers None for every caller.
#[cfg(test)]
mod metrics_only_font_tests {
    use super::*;
    use std::ffi::CString;

    // `stone_load_font` resolves through the asset registry and falls back to the
    // embedded DroidSans for a path it cannot read, which is what an unset
    // registry does here -- so both loads below bake the SAME bytes and their
    // metrics are comparable.
    fn load(residency: AtlasResidency) -> c_uint {
        let path = CString::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../packages/stone/engine/assets/fonts/DroidSans.ttf"
        ))
        .expect("path");
        let extra = CString::new("").expect("extra");
        load_font_with_residency(path.as_ptr(), 32, 0, extra.as_ptr(), residency)
    }

    fn unit_metrics(font_id: c_uint) -> [f32; 5] {
        let mut out = [0.0f32; 5];
        assert!(
            stone_get_font_unit_metrics(font_id, out.as_mut_ptr()),
            "unit metrics must be readable from a metrics-only font"
        );
        out
    }

    // The capability itself. Without a window `stone_load_font` returns 0,
    // because `rasterize_font`'s texture upload is `with_gl(..)?`; the metrics
    // load succeeds at the same call site and hands back a usable id.
    #[test]
    fn a_metrics_load_succeeds_where_a_gpu_load_cannot() {
        assert_eq!(
            load(AtlasResidency::Gpu),
            0,
            "a GPU load has no context here, so it must fail rather than              produce a font with no texture"
        );
        let metrics = load(AtlasResidency::Metrics);
        assert_ne!(metrics, 0, "the metrics-only load does not need a context");
        assert_eq!(
            stone_get_font_pixel_size(metrics),
            32,
            "the metrics font keeps the raster size it was asked for"
        );
        assert!(
            stone_get_font_slot_count(metrics) > 0,
            "the charset is baked, so the slot count is the atlas's own"
        );
    }

    // What the Rock layout path reads: font-unit metrics and per-glyph unit
    // advances. Both are prepared from the retained font bytes at raster
    // time, so they owe nothing to GL -- this pins that they are actually
    // present, not merely that the call returned true with zeros.
    #[test]
    fn metrics_only_publishes_font_and_glyph_unit_metrics() {
        let font = load(AtlasResidency::Metrics);
        assert_ne!(font, 0, "metrics load");
        let m = unit_metrics(font);
        assert!(m[0] > 0.0, "units per em");
        assert!(m[1] > 0.0, "ascender");
        assert!(m[4] > 0.0, "cap height");

        let mut glyph = [0.0f32; 5];
        assert!(
            stone_get_glyph_unit_metrics(font, 0, b'n' as c_int, glyph.as_mut_ptr()),
            "the regular face's 'n' has unit metrics"
        );
        assert!(glyph[4] > 0.0, "'n' carries an advance");
        assert!(glyph[2] > 0.0 && glyph[3] > 0.0, "'n' has ink extent");
    }

    // The raster size a metrics font is loaded at must not change what it
    // MEASURES: the Rock layout divides every unit advance by the font's own
    // units-per-em, so two raster sizes are the same measurement. This is the
    // fact that lets a code test reproduce a native sim's number while
    // rastering at a different density.
    #[test]
    fn unit_metrics_do_not_depend_on_the_raster_size() {
        let path = CString::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../packages/stone/engine/assets/fonts/DroidSans.ttf"
        ))
        .expect("path");
        let extra = CString::new("").expect("extra");
        let small = load_font_with_residency(
            path.as_ptr(),
            16,
            0,
            extra.as_ptr(),
            AtlasResidency::Metrics,
        );
        let large = load_font_with_residency(
            path.as_ptr(),
            96,
            0,
            extra.as_ptr(),
            AtlasResidency::Metrics,
        );
        assert_ne!(small, 0);
        assert_ne!(large, 0);
        assert_eq!(
            unit_metrics(small),
            unit_metrics(large),
            "font-unit metrics are the file's, not the raster's"
        );

        let advance = |id: c_uint| {
            let mut out = [0.0f32; 5];
            assert!(stone_get_glyph_unit_metrics(
                id,
                0,
                b'W' as c_int,
                out.as_mut_ptr()
            ));
            out[4]
        };
        assert_eq!(
            advance(small),
            advance(large),
            "a glyph's unit advance is the file's, not the raster's"
        );
    }

    // A metrics font owns no texture, so every GL-bound reader must refuse it by
    // name. `stone_get_font_atlas_image` returning 0 is what keeps Rock from
    // registering an Image over texture 0 and sampling the white pixel through
    // the glyph quads.
    #[test]
    fn a_metrics_font_publishes_no_atlas_image_and_refuses_a_re_raster() {
        let font = load(AtlasResidency::Metrics);
        assert_ne!(font, 0, "metrics load");
        assert_eq!(
            stone_get_font_atlas_image(font),
            0,
            "no texture means no Image handle"
        );
        assert_eq!(
            stone_get_font_atlas_image(font),
            0,
            "the refusal is not cached as an image id, so it answers 0 again"
        );
        assert!(
            !stone_resize_font(font, 64),
            "a re-raster into a texture that does not exist is refused, not              reported as a successful resize"
        );
        assert_eq!(
            stone_get_font_pixel_size(font),
            32,
            "the refused resize left the raster size alone"
        );
    }
}

// A failed reload must leave the old font behind, not a hybrid: the commit
// order (main atlas, coverage instances) is pinned here by
// forcing uploads to succeed -- headless, `with_gl` answers None and every
// reload would bail before mutating anything -- and forcing one dependent
// rebuild to fail. Atlas pixels are not retained after upload, so the
// snapshots below compare geometry, glyphs and revisions, not rasters.
#[cfg(test)]
mod reload_atomicity_tests {
    use super::*;
    use std::ffi::CString;

    const MAIN_PX: i32 = 32;
    const EXTRA_PX: i32 = 48;

    fn fake_texture(n: u32) -> glow::Texture {
        glow::NativeTexture(std::num::NonZeroU32::new(n).expect("non-zero texture name"))
    }

    /// Preserve the caller's font state and fault settings across assertion failures.
    struct ReloadFaults {
        fonts: Vec<Option<FontEntry>>,
        atlas_images: Vec<Option<u64>>,
        uploads_ok: bool,
        fail_dependent_raster: bool,
    }
    impl ReloadFaults {
        fn set(uploads_ok: bool, fail_dependent_raster: bool) -> Self {
            Self {
                fonts: FONTS.with(|f| std::mem::take(&mut *f.borrow_mut())),
                atlas_images: FONT_ATLAS_IMAGES.with(|f| std::mem::take(&mut *f.borrow_mut())),
                uploads_ok: FORCE_UPLOADS_OK.with(|f| f.replace(uploads_ok)),
                fail_dependent_raster: FAIL_DEPENDENT_RASTER
                    .with(|f| f.replace(fail_dependent_raster)),
            }
        }
    }
    impl Drop for ReloadFaults {
        fn drop(&mut self) {
            FONTS.with(|f| *f.borrow_mut() = std::mem::take(&mut self.fonts));
            FONT_ATLAS_IMAGES.with(|f| *f.borrow_mut() = std::mem::take(&mut self.atlas_images));
            FORCE_UPLOADS_OK.with(|f| f.set(self.uploads_ok));
            FAIL_DEPENDENT_RASTER.with(|f| f.set(self.fail_dependent_raster));
        }
    }

    /// A GPU-shaped font the way `rasterize_font` lays one out, minus the GL
    /// work: the bakes are real, the texture names are fake, and uploads never
    /// see them because the fault guard forces success.
    fn install_font(extra_instance: bool) -> c_uint {
        let bytes = FALLBACK_FONT.to_vec();
        let faces = FaceSet::default();
        let charset = Charset::default();
        let main = rasterize_glyphs(&bytes, MAIN_PX, faces, &charset, "DroidSans")
            .expect("DroidSans bakes at 32px");
        let extra = extra_instance.then(|| {
            rasterize_glyphs(&bytes, EXTRA_PX, faces, &charset, "DroidSans")
                .expect("DroidSans bakes at 48px")
        });

        let glyphs = main.glyphs;
        let mut instances = vec![AtlasInstance {
            texture: fake_texture(5101),
            glyphs: glyphs.clone(),
            width: main.atlas_width as i32,
            height: main.atlas_height as i32,
            pixel_size: MAIN_PX,
            image: None,
            face_revision: 0,
        }];
        if let Some(extra) = extra {
            instances.push(AtlasInstance {
                texture: fake_texture(5102),
                glyphs: extra.glyphs,
                width: extra.atlas_width as i32,
                height: extra.atlas_height as i32,
                pixel_size: EXTRA_PX,
                image: None,
                face_revision: 0,
            });
        }
        let entry = FontEntry {
            atlas_texture: Some(fake_texture(5101)),
            glyphs,
            faces,
            charset,
            unit_metrics: main.unit_metrics,
            glyph_unit: main.glyph_unit.clone(),
            font_data: Rc::new(bytes),
            source_path: "DroidSans".to_owned(),
            font_pixel_size: MAIN_PX,
            atlas_width: main.atlas_width as i32,
            atlas_height: main.atlas_height as i32,
            instances,
            face_revision: 0,
            coverage_released: false,
        };
        // `FONT_ATLAS_IMAGES` runs parallel to `FONTS`; a load pushes both.
        let font_id = FONTS.with(|fonts| {
            let mut fonts = fonts.borrow_mut();
            fonts.push(Some(entry));
            fonts.len() as c_uint
        });
        FONT_ATLAS_IMAGES.with(|imgs| imgs.borrow_mut().push(None));
        font_id
    }

    fn pixel_font_path() -> String {
        crate::test_fonts::font_file("pixel", &crate::test_fonts::pixel_font())
            .to_str()
            .expect("the pixel font path is UTF-8")
            .to_owned()
    }

    /// Reload the font's bytes from a different font file at the same size,
    /// so any atlas change is the new file's, not a re-raster's.
    fn reload(font_id: c_uint) -> bool {
        let path = CString::new(pixel_font_path()).expect("pixel font path has no nul");
        stone_reload_font(font_id, path.as_ptr(), MAIN_PX)
    }

    struct InstanceSnapshot {
        pixel_size: i32,
        width: i32,
        height: i32,
        face_revision: u64,
        glyphs: [Vec<GlyphInfo>; 4],
    }

    struct FontSnapshot {
        font_data: Vec<u8>,
        source_path: String,
        atlas_width: i32,
        atlas_height: i32,
        pixel_size: i32,
        face_revision: u64,
        glyphs: [Vec<GlyphInfo>; 4],
        instances: Vec<InstanceSnapshot>,
    }

    fn snapshot(font_id: c_uint) -> FontSnapshot {
        FONTS.with(|fonts| {
            let fonts = fonts.borrow();
            let entry = fonts[(font_id - 1) as usize]
                .as_ref()
                .expect("snapshot reads our own font");
            FontSnapshot {
                font_data: entry.font_data.as_slice().to_vec(),
                source_path: entry.source_path.clone(),
                atlas_width: entry.atlas_width,
                atlas_height: entry.atlas_height,
                pixel_size: entry.font_pixel_size,
                face_revision: entry.face_revision,
                glyphs: entry.glyphs.clone(),
                instances: entry
                    .instances
                    .iter()
                    .map(|i| InstanceSnapshot {
                        pixel_size: i.pixel_size,
                        width: i.width,
                        height: i.height,
                        face_revision: i.face_revision,
                        glyphs: i.glyphs.clone(),
                    })
                    .collect(),
            }
        })
    }

    fn glyph_tables_equal(a: &[Vec<GlyphInfo>; 4], b: &[Vec<GlyphInfo>; 4]) -> bool {
        a.iter().zip(b.iter()).all(|(x, y)| {
            x.len() == y.len()
                && x.iter().zip(y.iter()).all(|(g, h)| {
                    g.valid == h.valid
                        && g.u0 == h.u0
                        && g.v0 == h.v0
                        && g.u1 == h.u1
                        && g.v1 == h.v1
                        && g.width == h.width
                        && g.height == h.height
                        && g.bearing_x == h.bearing_x
                        && g.bearing_y == h.bearing_y
                        && g.advance == h.advance
                })
        })
    }

    fn assert_snapshot_restored(font_id: c_uint, before: &FontSnapshot) {
        let after = snapshot(font_id);
        assert_eq!(
            after.font_data, before.font_data,
            "a failed reload keeps the old bytes"
        );
        assert_eq!(
            after.source_path, before.source_path,
            "a failed reload keeps the old source path"
        );
        assert_eq!(
            (after.atlas_width, after.atlas_height),
            (before.atlas_width, before.atlas_height),
            "a failed reload keeps the old atlas extent"
        );
        assert_eq!(after.pixel_size, before.pixel_size, "raster size untouched");
        assert_eq!(
            after.face_revision, before.face_revision,
            "face revision moves only on a committed reload"
        );
        assert!(
            glyph_tables_equal(&after.glyphs, &before.glyphs),
            "a failed reload keeps the old glyph table"
        );
        assert_eq!(
            after.instances.len(),
            before.instances.len(),
            "a failed reload adds no instances"
        );
        for (n, (a, b)) in after
            .instances
            .iter()
            .zip(before.instances.iter())
            .enumerate()
        {
            assert!(
                glyph_tables_equal(&a.glyphs, &b.glyphs),
                "instance {n} keeps its glyph table"
            );
            assert_eq!(
                (a.pixel_size, a.width, a.height, a.face_revision),
                (b.pixel_size, b.width, b.height, b.face_revision),
                "instance {n} keeps its geometry and revision"
            );
        }
    }

    #[test]
    fn failed_coverage_refresh_leaves_the_old_atlas_and_bytes_in_place() {
        let _faults = ReloadFaults::set(true, true);
        let font = install_font(true);
        let before = snapshot(font);
        assert!(
            !reload(font),
            "the forced instance failure must fail the reload"
        );
        assert_snapshot_restored(font, &before);
    }

    // The control both failures are read against: with no fault forced, the
    // same fixture reloads onto the new file completely -- bytes, atlas,
    // instances and revisions -- so a failing assertion above is the commit
    // order, not a fixture that could never reload.
    #[test]
    fn successful_reload_replaces_atlas_bytes_and_revisions() {
        let _faults = ReloadFaults::set(true, false);
        let font = install_font(true);
        assert!(reload(font), "an unforced reload must succeed");
        let new_bytes = std::fs::read(pixel_font_path()).expect("pixel font on disk");
        FONTS.with(|fonts| {
            let fonts = fonts.borrow();
            let entry = fonts[(font - 1) as usize]
                .as_ref()
                .expect("control reads our own font");
            assert_eq!(
                entry.font_data.as_slice(),
                new_bytes.as_slice(),
                "the retained bytes are the new file's"
            );
            assert_eq!(
                entry.source_path,
                pixel_font_path(),
                "the source path follows"
            );
            let fresh = rasterize_glyphs(&new_bytes, MAIN_PX, entry.faces, &entry.charset, "pixel")
                .expect("the pixel font bakes");
            assert_eq!(
                (entry.atlas_width, entry.atlas_height),
                (fresh.atlas_width as i32, fresh.atlas_height as i32),
                "the atlas extent is the new file's bake"
            );
            assert!(
                glyph_tables_equal(&entry.glyphs, &fresh.glyphs),
                "and so is the glyph table"
            );
            assert_eq!(entry.face_revision, 1, "the reload bumps face_revision");
            assert_eq!(entry.instances.len(), 2, "no instance gained or lost");
            assert!(
                entry.instances.iter().all(|i| i.face_revision == 1),
                "every coverage instance commits the new revision"
            );
        });
    }
}

#[cfg(test)]
mod logo_atlas_tests {
    use super::*;

    // Stone's display-bake probe: a large coverage bake of a real text face
    // must fit a 2048 atlas page. 48 mirrors the production logo size MAG
    // requests (MAG_LOGO_FONT_SIZE); MAG owns its settings, so this pins
    // Stone's page budget with a Stone-local size instead of parsing MAG
    // source -- if MAG changes its request, MAG pins its own.
    const STONE_DISPLAY_BAKE_PX: i32 = 48;

    // The logo's coverage draws are 48px splash lines; larger coverage comes
    // from demand-bucket instances, so the
    // base atlas has no business near the GL size limit. A logo-sized (768px)
    // bake of a display face ran to roughly 16k x 6k texels; the 2048 page
    // below still allows a 2x linear overshoot of the 48px need and turns red
    // on any return to a logo-sized bake.
    #[test]
    fn logo_coverage_atlas_fits_a_2048_page() {
        let bake = rasterize_glyphs(
            FALLBACK_FONT,
            STONE_DISPLAY_BAKE_PX,
            FaceSet::default(),
            &Charset::default(),
            "DroidSans",
        )
        .expect("the display-bake request bakes");
        assert!(
            bake.atlas_width <= 2048 && bake.atlas_height <= 2048,
            "DroidSans at the Stone display-bake size {}px baked a {}x{} atlas ({} texels); larger text uses demand-sized instances rather than a logo-sized base atlas",
            STONE_DISPLAY_BAKE_PX,
            bake.atlas_width,
            bake.atlas_height,
            bake.atlas_width as u64 * bake.atlas_height as u64,
        );
    }
}

#[cfg(test)]
mod startup_allocation_tests {
    #[test]
    fn required_text_allocation_fails_without_publishing_or_leaking() {
        let gl = crate::sprite::tests::failing_driver(1);
        let result = std::panic::catch_unwind(|| {
            assert!(!unsafe { super::init_geometry(&gl) });
            super::TEXT_PIPELINE.with(|pipeline| assert!(pipeline.borrow().is_none()));
            crate::sprite::tests::assert_no_driver_objects();
        });
        if let Some(pipeline) = super::TEXT_PIPELINE.with(|pipeline| pipeline.borrow_mut().take()) {
            unsafe {
                gl.delete_vertex_array(pipeline.vao);
                gl.delete_buffer(pipeline.vbo);
            }
        }
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
}
