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
use crate::image_texture::{insert_image, with_image_mut, ImageFormat};
use crate::IMAGES;
use crate::{track_text_draw_call, with_gl, SHADERS};

/// Glyph info for the font atlas, indexed by the Rock text slot.
#[derive(Clone, Copy, Default, PartialEq)]
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

impl GlyphInfo {
    fn floats(&self) -> [f32; 9] {
        [
            self.u0,
            self.v0,
            self.u1,
            self.v1,
            self.width,
            self.height,
            self.bearing_x,
            self.bearing_y,
            self.advance,
        ]
    }
}

fn write_floats(out: *mut c_float, values: &[f32]) -> bool {
    if out.is_null() {
        return false;
    }
    unsafe { std::ptr::copy_nonoverlapping(values.as_ptr(), out, values.len()) };
    true
}

/// The entry for `face` (an index into `Face::ALL`) and `slot` in a per-face table.
fn face_slot<T: Copy>(table: &[Vec<T>; 4], face: c_int, slot: c_int) -> Option<T> {
    let face = Face::from_index(usize::try_from(face).ok()?)?;
    table[face.index()]
        .get(usize::try_from(slot).ok()?)
        .copied()
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

/// Slots `0..GLYPH_ASCII_SLOTS` are the identity block (32..=126, U+00C1 at 127), which
/// Rock indexes positionally; declared extras append above it.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct Charset {
    extras: Vec<char>,
    extra_slots: HashMap<char, usize>,
}

impl Charset {
    /// Extras cross the FFI as UTF-8.
    fn parse(extra: &str) -> Charset {
        let mut set = Charset::default();
        for ch in extra.chars() {
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
    fn baked(&self) -> impl Iterator<Item = (usize, char)> + '_ {
        (0..self.slot_count()).filter_map(|slot| Some((slot, self.char_at(slot)?)))
    }
}

#[derive(Clone, Default, PartialEq)]
struct AtlasLayout {
    /// Per face, then per text slot. An undeclared face is entirely invalid;
    /// nothing substitutes `Regular` for it.
    glyphs: [Vec<GlyphInfo>; 4],
    width: i32,
    height: i32,
    pixel_size: i32,
}

#[derive(Clone)]
struct FontSource {
    data: Rc<Vec<u8>>,
    path: String,
    faces: FaceSet,
    charset: Charset,
}

impl FontSource {
    fn bake(&self, pixel_size: i32) -> Option<RasterizedAtlas> {
        rasterize_glyphs(
            &self.data,
            pixel_size,
            self.faces,
            &self.charset,
            &self.path,
        )
    }
}

struct FontEntry {
    /// `None` for a metrics-only font: `stone_load_font_metrics` rasterizes the
    /// atlas on the CPU but uploads nothing, so there is no texture to bind.
    /// When present, `instances[0]` is this texture with this layout.
    atlas_texture: Option<glow::Texture>,
    /// The compatibility atlas; its CPU raster is not retained once uploaded.
    layout: AtlasLayout,
    source: FontSource,
    /// Unit metrics come from the same bytes as the raster.
    unit_metrics: FontUnitMetrics,
    /// Per face and text slot; None marks a charset hole or missing codepoint.
    /// The ABI permits querying undeclared faces, so all four are baked.
    glyph_unit: [Vec<Option<GlyphUnitMetrics>>; 4],
    /// GL raster products for one face revision; demand instances own their textures.
    instances: Vec<AtlasInstance>,
    face_revision: u64,
    /// Metrics-only entries have no GL coverage to release.
    coverage_released: bool,
}

impl FontEntry {
    fn new(source: FontSource, size: i32, residency: AtlasResidency) -> Option<FontEntry> {
        let raster = source.bake(size)?;
        let atlas_texture = match residency {
            AtlasResidency::Metrics => None,
            AtlasResidency::Gpu => Some(upload_atlas_texture(&raster)?),
        };
        let instances = atlas_texture
            .map(|texture| AtlasInstance {
                texture,
                layout: raster.layout.clone(),
                image: None,
                face_revision: 0,
            })
            .into_iter()
            .collect();
        Some(FontEntry {
            atlas_texture,
            layout: raster.layout,
            source,
            unit_metrics: raster.unit_metrics,
            glyph_unit: raster.glyph_unit,
            instances,
            face_revision: 0,
            coverage_released: false,
        })
    }

    fn regular_glyph(&self, ch: char) -> Option<&GlyphInfo> {
        let slot = self.source.charset.slot_of(ch)?;
        Some(&self.layout.glyphs[Face::Regular.index()][slot])
    }

    /// The tallest valid regular-face bearing in `text`, floored at zero, or
    /// None when no glyph is valid.
    fn max_ascent(&self, text: &str, scale: f32) -> Option<f32> {
        text.chars()
            .filter_map(|ch| self.regular_glyph(ch))
            .filter(|glyph| glyph.valid)
            .fold(None, |max, glyph| {
                Some(max.unwrap_or(0.0f32).max(glyph.bearing_y * scale))
            })
    }

    fn line_height(&self) -> f32 {
        let metrics = self.unit_metrics;
        (metrics.ascender - metrics.descender + metrics.line_gap) / metrics.units_per_em
            * self.layout.pixel_size as f32
    }

    fn atlas_image(&self) -> Option<u64> {
        self.atlas_texture?;
        self.instances.first()?.image
    }
}

#[derive(Clone)]
struct AtlasInstance {
    texture: glow::Texture,
    layout: AtlasLayout,
    image: Option<u64>,
    face_revision: u64,
}

impl AtlasInstance {
    fn publish(&mut self, layout: AtlasLayout, face_revision: u64) {
        if let Some(image) = self.image {
            with_image_mut(image, |loaded| {
                loaded.width = layout.width as u32;
                loaded.height = layout.height as u32;
            });
        }
        self.layout = layout;
        self.face_revision = face_revision;
    }
}

/// Retire the instances' image aliases and delete their textures.
fn drop_coverage(instances: &[AtlasInstance]) {
    for id in instances.iter().filter_map(|instance| instance.image) {
        forget_image(id);
    }
    with_gl(|gl| unsafe {
        for instance in instances {
            gl.delete_texture(instance.texture);
        }
    });
}

thread_local! {
    static TEXT_VERTICES: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
    // font_ids are 1-based and never reuse a slot; unloaded slots become None.
    static FONTS: RefCell<Vec<Option<FontEntry>>> = const { RefCell::new(Vec::new()) };
}

pub(crate) const FALLBACK_FONT: &[u8] =
    include_bytes!("../../packages/stone/engine/assets/fonts/DroidSans.ttf");

/// Read one loaded font. The only precondition is that the font exists: no
/// text pipeline, no GL, no window.
fn font<R>(font_id: u32, f: impl FnOnce(&FontEntry) -> R) -> Option<R> {
    let idx = (font_id as usize).checked_sub(1)?;
    FONTS.with(|fonts| fonts.borrow().get(idx)?.as_ref().map(f))
}

fn font_mut<R>(font_id: u32, f: impl FnOnce(&mut FontEntry) -> R) -> Option<R> {
    let idx = (font_id as usize).checked_sub(1)?;
    FONTS.with(|fonts| fonts.borrow_mut().get_mut(idx)?.as_mut().map(f))
}

fn push_font(entry: FontEntry) -> c_uint {
    FONTS.with(|fonts| {
        let mut fonts = fonts.borrow_mut();
        fonts.push(Some(entry));
        fonts.len() as c_uint
    })
}

fn c_str<'a>(ptr: *const c_char) -> &'a str {
    if ptr.is_null() {
        return "";
    }
    unsafe { CStr::from_ptr(ptr).to_str().unwrap_or("") }
}

#[no_mangle]
pub extern "C" fn stone_track_text_draw_call() {
    crate::track_text_draw_call();
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_text_init() -> bool {
    shutdown();
    if !crate::sprite::stone_sprite_init() {
        return false;
    }
    with_gl(|gl| unsafe {
        gl.bind_vertex_array(None);
        gl.bind_buffer(glow::ARRAY_BUFFER, None);
        true
    })
    .unwrap_or(false)
}

pub(crate) fn shutdown() {
    TEXT_VERTICES.with_borrow_mut(|vertices| *vertices = Vec::new());
    let drained: Vec<FontEntry> =
        FONTS.with(|fonts| fonts.borrow_mut().drain(..).flatten().collect());
    for entry in &drained {
        drop_coverage(&entry.instances);
    }
}

const GRID_PROBE: &[u8] = b"HMoxnwgB";

fn grid_gray_fraction(font_data: &[u8], px: f32) -> f32 {
    match RasterFont::new(font_data, px) {
        Ok(f) => glyph_raster::gray_fraction(&f, GRID_PROBE, Synth::REGULAR),
        Err(_) => 0.0,
    }
}

const GRID_CLEAN: f32 = 0.05; // <=5% partial ink = effectively a pixel grid
const GRID_MARGIN: f32 = 0.15; // chosen must be this much grayer than clean

fn offgrid_report(font_data: &[u8], size: i32, path: &str) -> Option<String> {
    if size < 8 {
        return None;
    }
    let chosen = grid_gray_fraction(font_data, size as f32);
    if chosen <= GRID_CLEAN {
        return None; // already crisp (native size or a clean multiple)
    }
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

fn warn_if_offgrid(font_data: &[u8], size: i32, path: &str) {
    if let Some(msg) = offgrid_report(font_data, size, path) {
        ::log::warn!(target: "TEXT", "{}", msg);
    }
}

/// A CPU raster: the R8 pixels to upload, their layout, and the unit metrics
/// parsed from the same bytes.
struct RasterizedAtlas {
    pixels: Vec<u8>,
    layout: AtlasLayout,
    unit_metrics: FontUnitMetrics,
    glyph_unit: [Vec<Option<GlyphUnitMetrics>>; 4],
}

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

const GLYPHS_PER_ROW: usize = 16;

fn rasterize_glyphs(
    font_data: &[u8],
    size: i32,
    faces: FaceSet,
    charset: &Charset,
    path_for_log: &str,
) -> Option<RasterizedAtlas> {
    let pixel_size = size as f32;
    let font = match RasterFont::new(font_data, pixel_size) {
        Ok(f) => f,
        Err(e) => {
            ::log::error!(target: "TEXT", "rasterize_glyphs: failed to parse font '{}': {}", path_for_log, e);
            return None;
        }
    };

    warn_if_offgrid(font_data, size, path_for_log);
    warn_if_synthesizing_a_pixel_font(font_data, size, faces, path_for_log);

    // One unit-scale parse answers every per-glyph unit query the font will
    // serve. `glyph_unit_metrics` divides the pixel-space synthesis back out,
    // so the answers do not depend on the raster size.
    let unit_font = match RasterFont::new(font_data, 1.0) {
        Ok(f) => f,
        Err(e) => {
            ::log::error!(target: "TEXT", "unit metrics: failed to parse font '{}': {}", path_for_log, e);
            return None;
        }
    };
    let glyph_unit = std::array::from_fn(|face| {
        let synth = Face::ALL[face].synth(1.0);
        (0..charset.slot_count())
            .map(|slot| {
                charset
                    .char_at(slot)
                    .and_then(|ch| unit_font.glyph_unit_metrics(ch, synth))
            })
            .collect()
    });

    let baked: Vec<Face> = faces.declared().collect();
    let rendered: Vec<Vec<(usize, usize, glyph_raster::RasterGlyph)>> = baked
        .iter()
        .map(|face| {
            let synth = face.synth(pixel_size);
            charset
                .baked()
                .enumerate()
                .filter_map(|(cell, (slot, ch))| Some((cell, slot, font.rasterize(ch, synth)?)))
                .collect()
        })
        .collect();

    let glyph_extent = |extent: fn(&glyph_raster::RasterGlyph) -> usize| {
        let cells = rendered.iter().flatten();
        cells.map(|(_, _, glyph)| extent(glyph)).max().unwrap_or(0) + 2 // +2 for padding
    };
    let cell_width = glyph_extent(|glyph| glyph.width);
    let cell_height = glyph_extent(|glyph| glyph.height);
    let rows_per_face = charset.baked().count().div_ceil(GLYPHS_PER_ROW);
    let atlas_width = cell_width * GLYPHS_PER_ROW;
    let atlas_height = cell_height * rows_per_face * baked.len();
    if !atlas_dimensions_supported(atlas_width as u32, atlas_height as u32) {
        return None;
    }

    let mut pixels = vec![0u8; atlas_width * atlas_height];
    let mut glyphs: [Vec<GlyphInfo>; 4] =
        std::array::from_fn(|_| vec![GlyphInfo::default(); charset.slot_count()]);
    for ((block, face), cells) in baked.iter().enumerate().zip(&rendered) {
        // Each face occupies whole row-blocks in declaration order.
        for (cell, slot, g) in cells {
            let x = cell % GLYPHS_PER_ROW * cell_width + 1;
            let y = (block * rows_per_face + cell / GLYPHS_PER_ROW) * cell_height + 1;
            for row in 0..g.height {
                let dst = (y + row) * atlas_width + x;
                pixels[dst..dst + g.width].copy_from_slice(&g.data[row * g.width..][..g.width]);
            }
            glyphs[face.index()][*slot] = GlyphInfo {
                u0: x as f32 / atlas_width as f32,
                v0: y as f32 / atlas_height as f32,
                u1: (x + g.width) as f32 / atlas_width as f32,
                v1: (y + g.height) as f32 / atlas_height as f32,
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
        pixels,
        layout: AtlasLayout {
            glyphs,
            width: atlas_width as i32,
            height: atlas_height as i32,
            pixel_size: size,
        },
        unit_metrics: unit_font.unit_metrics(),
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
        return true;
    };
    if atlas_dimensions_fit(width, height, limit) {
        return true;
    }
    ::log::error!(target: "TEXT", "font atlas {}x{} exceeds GL texture limit {}; refusing allocation/upload", width, height, limit);
    false
}

fn upload_atlas_texture(raster: &RasterizedAtlas) -> Option<glow::Texture> {
    with_gl(|gl| unsafe {
        let tex = gl.create_texture().ok()?;
        define_r8_atlas(gl, tex, raster);
        for axis in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, axis, glow::CLAMP_TO_EDGE as i32);
        }
        for filter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, filter, glow::NEAREST as i32);
        }
        Some(tex)
    })
    .flatten()
}

fn replace_atlas_texture(texture: glow::Texture, raster: &RasterizedAtlas) -> bool {
    #[cfg(test)]
    if FORCE_UPLOADS_OK.with(Cell::get) {
        return true;
    }
    with_gl(|gl| unsafe { define_r8_atlas(gl, texture, raster) }).is_some()
}

unsafe fn define_r8_atlas(gl: &crate::DebugGl, texture: glow::Texture, raster: &RasterizedAtlas) {
    gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        glow::R8 as i32,
        raster.layout.width,
        raster.layout.height,
        0,
        glow::RED,
        glow::UNSIGNED_BYTE,
        glow::PixelUnpackData::Slice(Some(&raster.pixels)),
    );
}

/// Metrics-only loads retain the CPU raster but own no GL texture or atlas.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AtlasResidency {
    /// Upload the atlas and keep the compatibility instance. Drawing works.
    Gpu,
    /// Keep the raster on the CPU only. Measurement and layout work; every
    /// GL-bound reader refuses this entry by name.
    Metrics,
}

#[cfg(test)]
thread_local! {
    static FORCE_UPLOADS_OK: Cell<bool> = const { Cell::new(false) };
    /// Headless tests force a dependent rebuild failure to cover reload atomicity.
    static FAIL_DEPENDENT_RASTER: Cell<bool> = const { Cell::new(false) };
}

/// Re-rasterize a font from `source` at `pixel_size` into the textures it
/// already owns, so the font id and every image handle stay valid. A reload
/// also rebuilds each demand instance at its own size and bumps the face
/// revision. Nothing is published unless every bake succeeded; the uploads
/// share one GL context, so they cannot fail piecemeal.
fn rebake(font_id: u32, source: FontSource, pixel_size: i32, reload: bool) -> bool {
    let Some((atlas, released, revision, demand)) = font(font_id, |entry| {
        let compat = entry.atlas_texture.is_some() as usize;
        let demand: Vec<(usize, glow::Texture, i32)> = (entry.instances.iter().enumerate())
            .skip(compat)
            .filter(|_| reload)
            .map(|(index, instance)| (index, instance.texture, instance.layout.pixel_size))
            .collect();
        let revision = entry.face_revision.saturating_add(reload as u64);
        (
            entry.atlas_texture,
            entry.coverage_released,
            revision,
            demand,
        )
    }) else {
        return false;
    };
    let Some(main) = source.bake(pixel_size) else {
        return false;
    };
    let mut rebuilt = Vec::with_capacity(demand.len());
    for (index, texture, size) in demand {
        let raster = source.bake(size);
        #[cfg(test)]
        let raster = raster.filter(|_| !FAIL_DEPENDENT_RASTER.with(Cell::get));
        let Some(raster) = raster else {
            ::log::warn!(target: "TEXT", "stone_reload_font: failed to refresh raster instances for '{}'", source.path);
            return false;
        };
        rebuilt.push((index, texture, raster));
    }
    if atlas.is_none() && !released {
        ::log::error!(target: "TEXT",
            "re-raster refused: font {} is metrics-only and owns no atlas texture",
            font_id);
        return false;
    }

    let mut uploads = (atlas.map(|texture| (texture, &main)).into_iter()).chain(
        rebuilt
            .iter()
            .map(|(_, texture, raster)| (*texture, raster)),
    );
    if !uploads.all(|(texture, raster)| replace_atlas_texture(texture, raster)) {
        return false;
    }
    font_mut(font_id, |entry| {
        if atlas.is_some() {
            entry.instances[0].publish(main.layout.clone(), revision);
        }
        for (index, _, raster) in rebuilt {
            entry.instances[index].publish(raster.layout, revision);
        }
        entry.layout = main.layout;
        entry.unit_metrics = main.unit_metrics;
        entry.glyph_unit = main.glyph_unit;
        entry.face_revision = revision;
        if reload {
            entry.source = source;
        }
    });
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

/// Load matching font metrics without a GL context or drawable atlas.
/// Drawing with the returned id is refused.
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
    let path = c_str(path);
    if path.is_empty() {
        ::log::error!(target: "TEXT", "stone_load_font: empty path");
        return 0;
    }
    let data = crate::asset_registry::resolve_asset(path).unwrap_or_else(|| {
        crate::log_once!("TEXT",
            "stone_load_font: failed to read font file '{}' — using embedded fallback (DroidSans.ttf)",
            path);
        FALLBACK_FONT.to_vec()
    });
    let source = FontSource {
        data: Rc::new(data),
        path: path.to_owned(),
        faces: FaceSet::from_mask(face_mask),
        charset: Charset::parse(c_str(extra_chars)),
    };
    FontEntry::new(source, size, residency).map_or(0, push_font)
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
    let path = c_str(path);
    if pixel_size < 1 || path.is_empty() {
        return false;
    }
    let Some(mut source) = font(font_id, |entry| entry.source.clone()) else {
        return false;
    };
    let Some(data) = crate::asset_registry::resolve_asset(path) else {
        ::log::warn!(target: "TEXT", "stone_reload_font: failed to read '{}'", path);
        return false;
    };
    source.data = Rc::new(data);
    source.path = path.to_owned();
    rebake(font_id, source, pixel_size, true)
}

/// Re-rasterize a font from its retained source bytes at a new physical em.
/// The font and atlas texture handles remain stable.
#[no_mangle]
pub extern "C" fn stone_resize_font(font_id: c_uint, pixel_size: c_int) -> bool {
    if pixel_size < 1 {
        return false;
    }
    font(font_id, |entry| entry.source.clone())
        .is_some_and(|source| rebake(font_id, source, pixel_size, false))
}

fn text_line_vertices(
    entry: &FontEntry,
    vertices: &mut Vec<f32>,
    line: &str,
    x: f32,
    y: f32,
    sx: f32,
    sy: f32,
    baseline: f32,
) {
    let abs_sy = sy.abs();
    let ascent = if baseline >= 0.0 {
        baseline
    } else {
        entry.max_ascent(line, abs_sy).unwrap_or(0.0)
    };

    vertices.clear();
    let mut pen_x = x;
    for gi in line.chars().filter_map(|c| entry.regular_glyph(c)) {
        if gi.valid && gi.width != 0.0 && gi.height != 0.0 {
            let x0 = pen_x + gi.bearing_x * sx;
            let y0 = if sy < 0.0 {
                y - gi.bearing_y * sy
            } else {
                y + (ascent - gi.bearing_y * abs_sy)
            };
            let (x1, y1) = (x0 + gi.width * sx, y0 + gi.height * sy);
            let (u0, v0, u1, v1) = (gi.u0, gi.v0, gi.u1, gi.v1);
            vertices.extend_from_slice(&[
                x0, y0, u0, v0, x0, y1, u0, v1, x1, y1, u1, v1, x0, y0, u0, v0, x1, y1, u1, v1, x1,
                y0, u1, v0,
            ]);
        }
        pen_x += gi.advance * sx;
    }
}

#[no_mangle]
pub extern "C" fn stone_stream_text(
    shader: c_uint,
    font_id: c_uint,
    text: *const c_char,
    x: c_float,
    y: c_float,
    sx: c_float,
    sy: c_float,
    baseline: c_float,
    multiline: bool,
) {
    let text = c_str(text);
    if text.is_empty() || (multiline && text.bytes().all(|byte| byte == b'\n')) {
        return;
    }
    let Some(vao) = crate::sprite::mesh_vao() else {
        return;
    };
    let shader = u64::from(shader);
    font(font_id, |entry| {
        let Some(program) =
            SHADERS.with_borrow(|shaders| shaders.get(&shader).map(|sh| sh.program))
        else {
            crate::log_once!(
                "TEXT",
                "text shader program (handle {}) not in registry; skipping text draw",
                shader
            );
            return;
        };
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
            crate::uniform_block::bind_block_for_program(gl, shader);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(atlas));
            gl.bind_vertex_array(Some(vao));
            gl.enable(glow::BLEND);
            gl.blend_func_separate(
                glow::SRC_ALPHA,
                glow::ONE_MINUS_SRC_ALPHA,
                glow::ONE,
                glow::ONE_MINUS_SRC_ALPHA,
            );

            TEXT_VERTICES.with_borrow_mut(|vertices| {
                let mut line_y = y;
                let line_height = entry.line_height() * sy.abs();
                for line in text.splitn(if multiline { usize::MAX } else { 1 }, '\n') {
                    if !line.is_empty() {
                        text_line_vertices(entry, vertices, line, x, line_y, sx, sy, baseline);
                        if !vertices.is_empty() {
                            let _ = crate::sprite::stream_triangles(
                                gl,
                                bytemuck::cast_slice(vertices),
                                vertices.len() / 4,
                                4,
                                &[2, 2],
                                |_, _| {
                                    crate::uniform_block::mark_program_drawn(program);
                                    track_text_draw_call();
                                },
                            );
                            gl.bind_buffer(glow::ARRAY_BUFFER, None);
                        }
                    }
                    line_y += line_height;
                }
            });
            gl.bind_vertex_array(None);
        });
    });
}

/// LuaJIT cannot trace C struct-by-value returns; use reusable out-pointers.
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
    let text = c_str(text);
    let (width, height) = font(font_id, |entry| {
        let mut max_line_width = 0.0f32;
        let mut max_glyph_height = 0.0f32;
        let mut line_count = 0;
        for line in text.split('\n') {
            let mut line_width = 0.0f32;
            for gi in line.chars().filter_map(|c| entry.regular_glyph(c)) {
                line_width += gi.advance * scale;
                max_glyph_height = max_glyph_height.max(gi.height * scale);
            }
            max_line_width = max_line_width.max(line_width);
            line_count += 1;
        }
        let height = if line_count <= 1 {
            max_glyph_height
        } else {
            (line_count - 1) as f32 * entry.line_height() * scale + max_glyph_height
        };
        (max_line_width, height)
    })
    .unwrap_or((0.0, 0.0));
    unsafe {
        if !out_width.is_null() {
            *out_width = width;
        }
        if !out_height.is_null() {
            *out_height = height;
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
    let text = c_str(text);
    font(font_id, |entry| entry.max_ascent(text, scale.abs()))
        .flatten()
        .unwrap_or(-1.0)
}

fn register_r8_alias(texture: glow::Texture, layout: &AtlasLayout) -> u64 {
    with_gl(|gl| unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        for swizzle in [
            glow::TEXTURE_SWIZZLE_R,
            glow::TEXTURE_SWIZZLE_G,
            glow::TEXTURE_SWIZZLE_B,
            glow::TEXTURE_SWIZZLE_A,
        ] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, swizzle, glow::RED as i32);
        }
    });
    let id = crate::handles::mint(Class::Image);
    let size = (layout.width as u32, layout.height as u32);
    insert_image(id, texture, size, ImageFormat::R8, false);
    id
}

fn forget_image(id: u64) {
    if IMAGES
        .with(|images| images.borrow_mut().remove(&id))
        .is_some()
    {
        crate::handles::retire(Class::Image, id);
    }
}

#[no_mangle]
pub extern "C" fn stone_get_font_atlas_image(font_id: c_uint) -> u64 {
    let Some((has_atlas, released)) = font(font_id, |entry| {
        (entry.atlas_texture.is_some(), entry.coverage_released)
    }) else {
        return 0;
    };
    if has_atlas {
        stone_get_font_instance_image(atlas_instance_id(font_id, 0))
    } else if released {
        crate::log_once!(
            "TEXT",
            "font {} released its coverage and has no atlas image; the caller must not ask a released font to draw coverage",
            font_id
        );
        0
    } else {
        crate::log_once!(
            "TEXT",
            "font {} is metrics-only and has no atlas image; the caller must not ask a metrics font to draw",
            font_id
        );
        0
    }
}

/// Return the generated atlas extent without exposing the GL image handle.
/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_get_font_atlas_size(
    font_id: c_uint,
    width: *mut c_int,
    height: *mut c_int,
) -> bool {
    font(font_id, |entry| {
        let layout = &entry.layout;
        if entry.coverage_released || layout.width <= 0 || layout.height <= 0 {
            return false;
        }
        if width.is_null() || height.is_null() {
            return false;
        }
        unsafe {
            *width = layout.width;
            *height = layout.height;
        }
        true
    })
    .unwrap_or(false)
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_get_font_slot_count(font_id: c_uint) -> c_int {
    font(font_id, |entry| entry.source.charset.slot_count() as c_int).unwrap_or(0)
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_get_font_slot_codepoint(font_id: c_uint, slot: c_int) -> c_int {
    let slot = usize::try_from(slot).ok();
    font(font_id, |entry| entry.source.charset.char_at(slot?))
        .flatten()
        .map_or(-1, |ch| ch as c_int)
}

fn write_glyph(glyphs: &[Vec<GlyphInfo>; 4], face: c_int, slot: c_int, out: *mut c_float) -> bool {
    face_slot(glyphs, face, slot)
        .filter(|glyph| glyph.valid)
        .is_some_and(|glyph| write_floats(out, &glyph.floats()))
}

/// Writes 9 floats: u0, v0, u1, v1, width, height, bearing_x, bearing_y, advance.
/// `face` indexes Face::ALL.
#[no_mangle]
pub extern "C" fn stone_get_glyph_info(
    font_id: c_uint,
    face: c_int,
    char_code: c_int,
    out: *mut c_float,
) -> bool {
    font(font_id, |entry| {
        write_glyph(&entry.layout.glyphs, face, char_code, out)
    })
    .unwrap_or(false)
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

fn instance<R>(instance_id: u64, f: impl FnOnce(&AtlasInstance) -> R) -> Option<R> {
    let (font_id, index) = decode_atlas_instance_id(instance_id)?;
    font(font_id, |entry| entry.instances.get(index).map(f)).flatten()
}

#[no_mangle]
pub extern "C" fn stone_ensure_font_atlas_instance(font_id: c_uint, pixel_size: c_int) -> u64 {
    if pixel_size < 1 {
        return 0;
    }
    let found = font(font_id, |entry| {
        let revision = entry.face_revision;
        let matches = |instance: &AtlasInstance| {
            instance.layout.pixel_size == pixel_size && instance.face_revision == revision
        };
        match entry.instances.iter().position(matches) {
            Some(index) => Ok(index),
            None => Err((entry.source.clone(), revision)),
        }
    });
    let (source, face_revision) = match found {
        None => return 0,
        Some(Ok(index)) => return atlas_instance_id(font_id, index),
        Some(Err(missing)) => missing,
    };
    let Some(raster) = source.bake(pixel_size) else {
        return 0;
    };
    let Some(texture) = upload_atlas_texture(&raster) else {
        return 0;
    };
    let instance = AtlasInstance {
        texture,
        layout: raster.layout,
        image: None,
        face_revision,
    };
    font_mut(font_id, |entry| {
        entry.instances.push(instance);
        atlas_instance_id(font_id, entry.instances.len() - 1)
    })
    .unwrap_or(0)
}

/// The same lookup as a Lua number: LuaJIT boxes a `u64` result in a cdata
/// whenever the call runs in the interpreter, and instance ids are far below 2^53.
#[no_mangle]
pub extern "C" fn stone_ensure_font_atlas_instance_number(
    font_id: c_uint,
    pixel_size: c_int,
) -> f64 {
    stone_ensure_font_atlas_instance(font_id, pixel_size) as f64
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_font_atlas_instance_count(font_id: c_uint) -> c_int {
    font(font_id, |entry| entry.instances.len() as c_int).unwrap_or(0)
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_get_font_atlas_instance_pixel_size(
    font_id: c_uint,
    index: c_uint,
) -> c_int {
    stone_get_font_instance_pixel_size(atlas_instance_id(font_id, index as usize))
}

#[no_mangle]
pub extern "C" fn stone_get_font_atlas_image_cached(font_id: c_uint) -> u64 {
    font(font_id, FontEntry::atlas_image).flatten().unwrap_or(0)
}

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_get_font_atlas_instance_image_at(font_id: c_uint, index: c_int) -> u64 {
    if index < 0 {
        return 0;
    }
    stone_get_font_instance_image(atlas_instance_id(font_id, index as usize))
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_get_font_instance_size(
    instance_id: u64,
    width: *mut c_int,
    height: *mut c_int,
) -> bool {
    if width.is_null() || height.is_null() {
        return false;
    }
    instance(instance_id, |instance| unsafe {
        *width = instance.layout.width;
        *height = instance.layout.height;
        instance.layout.width > 0 && instance.layout.height > 0
    })
    .unwrap_or(false)
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_get_font_instance_pixel_size(instance_id: u64) -> c_int {
    instance(instance_id, |instance| instance.layout.pixel_size).unwrap_or(0)
}

/// Bytes of the R8 raster an instance's texture holds; no CPU copy is retained.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_get_font_instance_byte_len(instance_id: u64) -> c_int {
    instance(instance_id, |instance| {
        let layout = &instance.layout;
        layout.width.max(0).saturating_mul(layout.height.max(0))
    })
    .unwrap_or(0)
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_get_font_instance_glyph_info(
    instance_id: u64,
    face: c_int,
    char_code: c_int,
    out: *mut c_float,
) -> bool {
    instance(instance_id, |instance| {
        write_glyph(&instance.layout.glyphs, face, char_code, out)
    })
    .unwrap_or(false)
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_get_font_instance_image(instance_id: u64) -> u64 {
    let Some((font_id, index)) = decode_atlas_instance_id(instance_id) else {
        return 0;
    };
    font_mut(font_id, |entry| {
        let AtlasInstance {
            texture,
            layout,
            image,
            ..
        } = entry.instances.get_mut(index)?;
        Some(*image.get_or_insert_with(|| register_r8_alias(*texture, layout)))
    })
    .flatten()
    .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn stone_get_font_pixel_size(font_id: c_uint) -> c_int {
    font(font_id, |entry| entry.layout.pixel_size).unwrap_or(0)
}

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_get_font_unit_metrics(font_id: c_uint, out: *mut c_float) -> bool {
    font(font_id, |entry| {
        let m = entry.unit_metrics;
        let values = [
            m.units_per_em,
            m.ascender,
            m.descender,
            m.line_gap,
            m.cap_height,
        ];
        write_floats(out, &values)
    })
    .unwrap_or(false)
}

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_get_glyph_unit_metrics(
    font_id: c_uint,
    face: c_int,
    char_code: c_int,
    out: *mut c_float,
) -> bool {
    let metrics = font(font_id, |entry| {
        face_slot(&entry.glyph_unit, face, char_code)
    });
    let Some(m) = metrics.flatten().flatten() else {
        return false;
    };
    write_floats(out, &[m.left, m.top, m.width, m.height, m.advance])
}

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

/// Bytes of TTF/OTF the font retains, for the asset report's CPU column. -1
/// for an id that names no live font.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_font_data_len(font_id: c_uint) -> i64 {
    font(font_id, |entry| entry.source.data.len() as i64).unwrap_or(-1)
}

/// Drop an `IMAGES` entry that is a font atlas alias rather than an owned
/// texture, clearing the instance's cache slot with it. The GL texture belongs
/// to the font and survives; the next image request mints the entry again.
/// Returns false when `image_id` is not an atlas alias, which is how the
/// ledger's image teardown tells an alias from an image it may delete.
pub(crate) fn forget_atlas_image(image_id: u64) -> bool {
    let found = FONTS.with(|fonts| {
        let mut fonts = fonts.borrow_mut();
        let mut instances = fonts
            .iter_mut()
            .flatten()
            .flat_map(|entry| &mut entry.instances);
        instances
            .find(|instance| instance.image == Some(image_id))
            .map(|instance| instance.image = None)
            .is_some()
    });
    if found {
        forget_image(image_id);
    }
    found
}

/// Release the atlas. Later operations on this id no-op; returns false for an invalid id.
#[no_mangle]
pub extern "C" fn stone_unload_font(font_id: c_uint) -> bool {
    let Some(idx) = (font_id as usize).checked_sub(1) else {
        return false;
    };
    let Some(entry) = FONTS.with(|fonts| fonts.borrow_mut().get_mut(idx)?.take()) else {
        return false;
    };
    drop_coverage(&entry.instances);
    true
}

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_font_release_coverage(font_id: c_uint) -> bool {
    let Some(instances) = font_mut(font_id, |entry| {
        if entry.atlas_texture.take().is_some() || !entry.instances.is_empty() {
            entry.coverage_released = true;
        }
        std::mem::take(&mut entry.instances)
    }) else {
        return false;
    };
    drop_coverage(&instances);
    true
}

#[cfg(test)]
mod test_fixtures {
    use super::*;
    use std::ffi::CString;

    pub(super) const MAIN_PX: i32 = 32;
    pub(super) const EXTRA_PX: i32 = 48;

    pub(super) fn fake_texture(n: u32) -> glow::Texture {
        glow::NativeTexture(std::num::NonZeroU32::new(n).expect("non-zero texture name"))
    }

    pub(super) fn load_droid(size: c_int, residency: AtlasResidency) -> c_uint {
        let path = CString::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../packages/stone/engine/assets/fonts/DroidSans.ttf"
        ))
        .expect("path");
        let extra = CString::new("").expect("extra");
        load_font_with_residency(path.as_ptr(), size, 0, extra.as_ptr(), residency)
    }

    pub(super) fn droid_source() -> FontSource {
        FontSource {
            data: Rc::new(FALLBACK_FONT.to_vec()),
            path: "DroidSans".to_owned(),
            faces: FaceSet::default(),
            charset: Charset::default(),
        }
    }

    /// Real bakes with fake texture names, without GL uploads.
    pub(super) fn install_fake_gpu_font(first_texture: u32, extra_instance: bool) -> c_uint {
        let source = droid_source();
        let main = source.bake(MAIN_PX).expect("DroidSans bakes at 32px");
        let instance = |n: u32, layout: AtlasLayout| AtlasInstance {
            texture: fake_texture(first_texture + n),
            layout,
            image: None,
            face_revision: 0,
        };
        let mut instances = vec![instance(0, main.layout.clone())];
        if extra_instance {
            let extra = source.bake(EXTRA_PX).expect("DroidSans bakes at 48px");
            instances.push(instance(1, extra.layout));
        }
        push_font(FontEntry {
            atlas_texture: Some(fake_texture(first_texture)),
            layout: main.layout,
            source,
            unit_metrics: main.unit_metrics,
            glyph_unit: main.glyph_unit,
            instances,
            face_revision: 0,
            coverage_released: false,
        })
    }
}

#[cfg(test)]
mod font_census_tests {
    use super::test_fixtures::{install_fake_gpu_font, EXTRA_PX, MAIN_PX};
    use super::*;
    use crate::image_texture::{live_texture_bytes, stone_get_image_byte_len};

    struct CensusFonts {
        fonts_len: usize,
        images: Vec<u64>,
    }

    impl CensusFonts {
        fn capture() -> Self {
            CensusFonts {
                fonts_len: FONTS.with(|f| f.borrow().len()),
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
        }
    }

    /// Re-bake so the rasterizer, not the fixture, supplies the expected texel counts.
    fn oracle_texels(pixel_size: i32) -> u64 {
        let bake = rasterize_glyphs(
            FALLBACK_FONT,
            pixel_size,
            FaceSet::default(),
            &Charset::default(),
            "DroidSans",
        )
        .expect("the oracle bake matches the installer");
        bake.layout.width as u64 * bake.layout.height as u64
    }

    /// Coverage atlases upload R8, so the live-image census counts each alias
    /// at one byte per texel, not the RGBA8 four. Returns the texels registered.
    fn register_aliases(guard: &mut CensusFonts, font: c_uint) -> u64 {
        let mut total = 0;
        let mut previous = 0;
        for (instance, pixel_size) in [(0, MAIN_PX), (1, EXTRA_PX)] {
            let texels = oracle_texels(pixel_size);
            assert!(texels > 0, "the oracle bake is a real atlas");
            let before = live_texture_bytes();
            let image = if instance == 0 {
                stone_get_font_atlas_image(font)
            } else {
                stone_get_font_instance_image(atlas_instance_id(font, instance))
            };
            assert_ne!(image, 0, "instance {instance} alias registers headless");
            assert_ne!(image, previous, "a later bucket owns its texture");
            guard.note_image(image);
            assert_eq!(
                live_texture_bytes() - before,
                texels,
                "instance {instance}: one R8 texel is one byte in the live total"
            );
            assert_eq!(
                stone_get_image_byte_len(image),
                texels,
                "instance {instance}: the per-image census agrees with the live total"
            );
            previous = image;
            total += texels;
        }
        total
    }

    fn has_atlas(font: c_uint) -> bool {
        let (mut w, mut h) = (0 as c_int, 0 as c_int);
        stone_get_font_atlas_size(font, &mut w, &mut h)
    }

    fn glyph_a(font: c_uint) -> [c_float; 9] {
        let mut glyph = [0.0 as c_float; 9];
        assert!(
            stone_get_glyph_info(font, 0, b'A' as c_int, glyph.as_mut_ptr()),
            "the installed 'A' measures"
        );
        glyph
    }

    #[test]
    fn release_coverage_drops_aliases_and_keeps_the_face() {
        let mut guard = CensusFonts::capture();
        let font = install_fake_gpu_font(5201, true);
        let alias_texels = register_aliases(&mut guard, font);

        let data_len = stone_font_data_len(font);
        assert!(data_len > 1000, "the installed face retains its TTF bytes");
        let glyph_before = glyph_a(font);
        assert!(glyph_before[8] > 0.0, "the installed advance is positive");

        let before = live_texture_bytes();
        assert!(
            stone_font_release_coverage(font),
            "a live font releases its coverage"
        );
        assert_eq!(
            before - live_texture_bytes(),
            alias_texels,
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
        assert!(!has_atlas(font), "a released atlas publishes no extent");
        assert_eq!(
            stone_font_data_len(font),
            data_len,
            "the TTF bytes stay resident"
        );
        assert_eq!(
            glyph_a(font),
            glyph_before,
            "metrics answer after the release and no metric moves"
        );
        assert!(
            stone_font_release_coverage(font),
            "releasing twice is idempotent"
        );
        assert!(
            !stone_font_release_coverage(999999),
            "an unknown font releases nothing"
        );
    }

    /// Reload after release must not re-upload coverage.
    #[test]
    fn reload_after_release_refreshes_metrics_and_keeps_coverage_down() {
        let mut guard = CensusFonts::capture();
        let font = install_fake_gpu_font(5201, true);
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
        assert!(!has_atlas(font), "coverage stays down across the reload");
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
        assert!(glyph_a(font)[8] > 0.0, "the reloaded advance is positive");
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
    use super::test_fixtures::load_droid;
    use super::*;

    fn atlas_extent(font_id: c_uint) -> (i32, i32) {
        let (mut w, mut h) = (0, 0);
        assert!(stone_get_font_atlas_size(font_id, &mut w, &mut h));
        (w, h)
    }

    #[test]
    fn a_loaded_font_retains_less_than_one_atlas_of_heap() {
        let before = retained_bytes::live();
        let font = load_droid(128, AtlasResidency::Metrics);
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

#[cfg(test)]
mod atlas_face_tests {
    use super::*;

    const PX: i32 = 32;

    fn bake(bold: bool, italic: bool) -> RasterizedAtlas {
        let faces = FaceSet { bold, italic };
        rasterize_glyphs(FALLBACK_FONT, PX, faces, &Charset::default(), "DroidSans")
            .expect("DroidSans bakes")
    }

    // Stated as an equality with the declared block count rather than as "small
    // enough": a layout that reserved four blocks and left three empty would
    // pass a size bound and fail this.
    #[test]
    fn declared_faces_take_distinct_blocks_and_cells_hold_the_widest() {
        let four = bake(true, true);
        let plain = bake(false, false);
        assert!(
            four.layout.width > plain.layout.width,
            "bold-italic must be wider than regular ({}px vs {}px cells); if it is not, \
             the containment check below proves nothing",
            four.layout.width / 16,
            plain.layout.width / 16
        );

        let cell_w = four.layout.width as f32 / 16.0;
        for face in Face::ALL {
            for c in GLYPH_FIRST..=GLYPH_LAST {
                let gi = &four.layout.glyphs[face.index()][c];
                if !gi.valid || gi.width == 0.0 {
                    continue;
                }
                let col = ((c - GLYPH_FIRST) % 16) as f32;
                let (left, right) = (
                    gi.u0 * four.layout.width as f32,
                    gi.u1 * four.layout.width as f32,
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

        let g = |f: Face| four.layout.glyphs[f.index()][b'n' as usize];
        let mut vs: Vec<f32> = Face::ALL.iter().map(|&f| g(f).v0).collect();
        vs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for w in vs.windows(2) {
            assert!(w[1] > w[0], "two faces share a row block ({:?})", vs);
        }
        assert!(
            g(Face::Bold).advance > g(Face::Regular).advance,
            "bold must set wider than regular"
        );
        assert!(
            (g(Face::Italic).advance - g(Face::Regular).advance).abs() < 0.01,
            "a shear must not change the advance"
        );
    }
}

#[cfg(test)]
mod offgrid_tests {
    use super::*;

    #[test]
    fn offgrid_detector_fires_only_for_a_pixel_font_off_grid() {
        let pixel = crate::test_fonts::pixel_font();
        assert!(
            offgrid_report(&pixel, 48, "pixel").is_none(),
            "48px should not warn (it is a clean multiple)"
        );
        assert!(
            (12..=64).any(|px| offgrid_report(&pixel, px, "pixel").is_some()),
            "expected at least one off-grid size in 12..=64 to warn; none did"
        );
        for px in (12..=64).step_by(4) {
            assert!(
                offgrid_report(FALLBACK_FONT, px, "DroidSans").is_none(),
                "smooth font should never warn (px={px})"
            );
        }
    }
}

#[cfg(test)]
mod atlas_image_teardown_tests {
    use super::test_fixtures::install_fake_gpu_font;
    use super::*;

    fn live_count(ids: &[u64]) -> usize {
        IMAGES.with(|images| {
            let images = images.borrow();
            ids.iter().filter(|id| images.contains_key(id)).count()
        })
    }

    #[test]
    fn tearing_down_the_atlas_cache_retires_its_image_handles() {
        let fonts = [
            install_fake_gpu_font(4001, false),
            install_fake_gpu_font(4003, false),
        ];
        let images = fonts.map(|font| stone_get_font_atlas_image(font));
        assert_ne!(images[0], images[1], "two fonts mint two image handles");
        assert_eq!(
            stone_get_font_atlas_instance_image_at(fonts[0], 0),
            images[0],
            "instance 0 is the compatibility atlas and shares its image"
        );
        assert_eq!(live_count(&images), 2, "both atlas images are live");

        assert!(
            forget_atlas_image(images[0]),
            "the ledger recognizes an atlas alias"
        );
        assert_eq!(
            stone_get_font_atlas_image_cached(fonts[0]),
            0,
            "the cache slot is empty, so the next draw mints it again"
        );
        let reminted = stone_get_font_atlas_image(fonts[0]);
        assert_ne!(reminted, 0);
        assert!(
            !forget_atlas_image(u64::MAX),
            "an unknown image is not an alias"
        );

        shutdown();
        assert_eq!(
            live_count(&[reminted, images[1]]),
            0,
            "no atlas image may outlive the texture its font owned"
        );
    }
}

#[cfg(test)]
mod charset_tests {
    use super::*;

    const PX: i32 = 32;

    #[test]
    fn declared_codepoints_bake_into_appended_slots_when_the_font_covers_them() {
        let extra = "\u{1F600}\u{10FFFE}";
        let charset = Charset::parse(extra);
        let atlas = rasterize_glyphs(
            &crate::test_fonts::symbol_only_font(),
            PX,
            FaceSet::default(),
            &charset,
            "symbol",
        )
        .expect("the symbol font bakes");
        let regular = &atlas.layout.glyphs[Face::Regular.index()];

        let emoji = charset.slot_of('\u{1F600}');
        assert_eq!(
            emoji,
            Some(GLYPH_ASCII_SLOTS),
            "the first extra appends at 128"
        );
        let g = &regular[GLYPH_ASCII_SLOTS];
        assert!(
            g.valid,
            "the symbol font covers U+1F600 through its format 12 cmap"
        );
        assert!(g.advance > 0.0, "an emoji glyph carries an advance");
        assert!(g.width > 0.0 && g.height > 0.0, "and has ink");

        let uncovered = charset.slot_of('\u{10FFFE}').expect("declared");
        assert!(
            !regular[uncovered].valid,
            "the symbol font has no U+10FFFE; the slot exists and is invalid"
        );
        assert!(
            !regular[b'X' as usize].valid && !regular[b'a' as usize].valid,
            "the symbol font maps no ASCII codepoint"
        );
    }

    #[test]
    fn the_charset_drops_duplicates_and_codepoints_ascii_already_reaches() {
        let c = Charset::parse("A\u{00C1}\u{1F600}\u{1F600}\u{1F44D}");
        assert_eq!(c.slot_count(), GLYPH_ASCII_SLOTS + 2, "two extras survive");
        assert_eq!(c.slot_of('A'), Some(b'A' as usize));
        assert_eq!(c.slot_of('\u{00C1}'), Some(GLYPH_ACCENT_A_ACUTE_SLOT));
        assert_eq!(c.slot_of('\u{1F600}'), Some(GLYPH_ASCII_SLOTS));
        assert_eq!(c.slot_of('\u{1F44D}'), Some(GLYPH_ASCII_SLOTS + 1));
        assert_eq!(c.slot_of('\u{1F525}'), None, "undeclared has no slot");
    }
}

#[cfg(test)]
mod metrics_only_font_tests {
    use super::test_fixtures::{droid_source, load_droid};
    use super::*;

    #[test]
    fn multiline_measurement_uses_font_vertical_metrics() {
        let fonts: [(&str, &[u8]); 2] = [
            ("DroidSans", FALLBACK_FONT),
            (
                "pixel",
                include_bytes!("../../packages/stone/tests/fixtures/pixel.ttf"),
            ),
        ];
        for (name, bytes) in fonts {
            for size in [8, 16, 32, 48] {
                let source = FontSource {
                    data: Rc::new(bytes.to_vec()),
                    path: name.to_owned(),
                    ..droid_source()
                };
                let entry =
                    FontEntry::new(source, size, AtlasResidency::Metrics).expect("metrics font");
                let font = push_font(entry);
                let raster = RasterFont::new(bytes, size as f32).unwrap();
                let (ascent, descent, gap) = raster.vertical_metrics();
                let line_height = ascent - descent + gap;
                for scale in [0.5, 1.0, 2.0] {
                    let measure = |text: &CStr| {
                        let (mut width, mut height) = (0.0, 0.0);
                        stone_measure_text(font, text.as_ptr(), scale, &mut width, &mut height);
                        (width, height)
                    };
                    let single = measure(c"A");
                    for (text, gaps) in [(c"A\nA", 1), (c"A\n\nA", 2), (c"A\n", 1)] {
                        let multiline = measure(text);
                        assert_eq!(multiline.0, single.0);
                        let expected = single.1 + gaps as f32 * line_height * scale;
                        assert!(
                            (multiline.1 - expected).abs() < 0.0001,
                            "{name} at {size}px, scale {scale}: got {}, expected {expected}",
                            multiline.1
                        );
                    }
                }
                FONTS.with(|fonts| fonts.borrow_mut().pop());
            }
        }
    }

    fn unit_metrics(font_id: c_uint) -> [f32; 5] {
        let mut out = [0.0f32; 5];
        assert!(
            stone_get_font_unit_metrics(font_id, out.as_mut_ptr()),
            "unit metrics must be readable from a metrics-only font"
        );
        out
    }

    // What the Rock layout path reads, font-unit metrics and per-glyph unit
    // advances, is prepared from the retained font bytes and owes nothing to
    // GL. A metrics font owns no texture, so every GL-bound reader must refuse
    // it: an atlas image of 0 keeps Rock from sampling texture 0 through the
    // glyph quads.
    #[test]
    fn a_metrics_load_needs_no_context_publishes_metrics_and_refuses_gl_readers() {
        assert_eq!(
            load_droid(32, AtlasResidency::Gpu),
            0,
            "a GPU load has no context here, so it must fail rather than \
             produce a font with no texture"
        );
        let font = load_droid(32, AtlasResidency::Metrics);
        assert_ne!(font, 0, "the metrics-only load does not need a context");
        assert_eq!(stone_get_font_pixel_size(font), 32);
        assert!(
            stone_get_font_slot_count(font) > 0,
            "the charset is baked, so the slot count is the atlas's own"
        );

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

        for attempt in ["first", "second"] {
            assert_eq!(
                stone_get_font_atlas_image(font),
                0,
                "{attempt} ask: no texture means no Image handle, and the refusal is not cached"
            );
        }
        assert!(
            !stone_resize_font(font, 64),
            "a re-raster into a texture that does not exist is refused"
        );
        assert_eq!(
            stone_get_font_pixel_size(font),
            32,
            "the refused resize left the raster size alone"
        );
    }

    // Unit advances are independent of raster size.
}

#[cfg(test)]
mod reload_atomicity_tests {
    use super::test_fixtures::{install_fake_gpu_font, MAIN_PX};
    use super::*;
    use std::ffi::CString;

    /// Preserve the caller's font state and fault settings across assertion failures.
    struct ReloadFaults {
        fonts: Vec<Option<FontEntry>>,
        uploads_ok: bool,
        fail_dependent_raster: bool,
    }
    impl ReloadFaults {
        fn set(uploads_ok: bool, fail_dependent_raster: bool) -> Self {
            Self {
                fonts: FONTS.with(|f| std::mem::take(&mut *f.borrow_mut())),
                uploads_ok: FORCE_UPLOADS_OK.with(|f| f.replace(uploads_ok)),
                fail_dependent_raster: FAIL_DEPENDENT_RASTER
                    .with(|f| f.replace(fail_dependent_raster)),
            }
        }
    }
    impl Drop for ReloadFaults {
        fn drop(&mut self) {
            FONTS.with(|f| *f.borrow_mut() = std::mem::take(&mut self.fonts));
            FORCE_UPLOADS_OK.with(|f| f.set(self.uploads_ok));
            FAIL_DEPENDENT_RASTER.with(|f| f.set(self.fail_dependent_raster));
        }
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

    struct FontSnapshot {
        font_data: Vec<u8>,
        source_path: String,
        layout: AtlasLayout,
        face_revision: u64,
        instances: Vec<(AtlasLayout, u64)>,
    }

    fn snapshot(font_id: c_uint) -> FontSnapshot {
        font(font_id, |entry| FontSnapshot {
            font_data: entry.source.data.as_slice().to_vec(),
            source_path: entry.source.path.clone(),
            layout: entry.layout.clone(),
            face_revision: entry.face_revision,
            instances: (entry.instances.iter())
                .map(|i| (i.layout.clone(), i.face_revision))
                .collect(),
        })
        .expect("snapshot reads our own font")
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
        assert!(
            after.layout == before.layout,
            "a failed reload keeps the old atlas extent, raster size and glyph table"
        );
        assert_eq!(
            after.face_revision, before.face_revision,
            "face revision moves only on a committed reload"
        );
        assert!(
            after.instances == before.instances,
            "a failed reload keeps every instance's geometry, glyphs and revision"
        );
    }

    #[test]
    fn failed_coverage_refresh_leaves_the_old_atlas_and_bytes_in_place() {
        let _faults = ReloadFaults::set(true, true);
        let font = install_fake_gpu_font(5101, true);
        let before = snapshot(font);
        assert!(
            !reload(font),
            "the forced instance failure must fail the reload"
        );
        assert_snapshot_restored(font, &before);
    }

    #[test]
    fn successful_reload_replaces_atlas_bytes_and_revisions() {
        let _faults = ReloadFaults::set(true, false);
        let font = install_fake_gpu_font(5101, true);
        assert!(reload(font), "an unforced reload must succeed");
        let new_bytes = std::fs::read(pixel_font_path()).expect("pixel font on disk");
        super::font(font, |entry| {
            assert_eq!(
                entry.source.data.as_slice(),
                new_bytes.as_slice(),
                "the retained bytes are the new file's"
            );
            assert_eq!(
                entry.source.path,
                pixel_font_path(),
                "the source path follows"
            );
            let fresh = entry.source.bake(MAIN_PX).expect("the pixel font bakes");
            assert!(
                entry.layout == fresh.layout,
                "the atlas extent and glyph table are the new file's bake"
            );
            assert_eq!(entry.face_revision, 1, "the reload bumps face_revision");
            assert_eq!(entry.instances.len(), 2, "no instance gained or lost");
            assert!(
                entry.instances.iter().all(|i| i.face_revision == 1),
                "every coverage instance commits the new revision"
            );
        })
        .expect("control reads our own font");
    }
}
