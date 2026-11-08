//! Glyph outline rasterization with synthetic bold and oblique.
//!
//! Outlines come from ttf-parser, rasterization and stroking from zeno.
//! Synthesis is applied to the OUTLINE before rasterizing, so stems thicken
//! and diagonals stay antialiased; a bitmap-level dilate or shear cannot do
//! either.

/// Which face a bake is producing. `Regular` reads the font's own outlines;
/// the rest are synthesized from those same outlines.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Face {
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

impl Face {
    pub const ALL: [Face; 4] = [Face::Regular, Face::Bold, Face::Italic, Face::BoldItalic];

    pub fn index(self) -> usize {
        match self {
            Face::Regular => 0,
            Face::Bold => 1,
            Face::Italic => 2,
            Face::BoldItalic => 3,
        }
    }

    pub fn from_index(i: usize) -> Option<Face> {
        Face::ALL.get(i).copied()
    }

    pub fn synth(self, px: f32) -> Synth {
        let bold = if self.is_bold() {
            px * BOLD_STRENGTH_PER_PX
        } else {
            0.0
        };
        let skew = if self.is_italic() {
            OBLIQUE_DEGREES
        } else {
            0.0
        };
        Synth {
            embolden: bold,
            skew_deg: skew,
        }
    }

    pub fn is_bold(self) -> bool {
        matches!(self, Face::Bold | Face::BoldItalic)
    }

    pub fn is_italic(self) -> bool {
        matches!(self, Face::Italic | Face::BoldItalic)
    }
}

/// Which synthesized faces a family DECLARES. `Regular` is always baked; the
/// rest exist only when asked for, so the default bakes exactly the outlines
/// the font shipped. A caller that requests an undeclared face is rejected
/// upstream rather than served `Regular` -- synthesis is a declaration, not a
/// fallback, and a pixel family declaring nothing is what keeps its atlas
/// binary (`oblique_not_bold_antialiases_a_pixel_font`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FaceSet {
    pub bold: bool,
    pub italic: bool,
}

impl FaceSet {
    pub const BOLD_BIT: i32 = 1;
    pub const ITALIC_BIT: i32 = 2;

    pub fn from_mask(mask: i32) -> Self {
        FaceSet {
            bold: mask & Self::BOLD_BIT != 0,
            italic: mask & Self::ITALIC_BIT != 0,
        }
    }

    #[cfg(test)]
    pub fn mask(self) -> i32 {
        (if self.bold { Self::BOLD_BIT } else { 0 })
            | (if self.italic { Self::ITALIC_BIT } else { 0 })
    }

    /// Bold-italic needs both axes declared; there is no half-synthesized face.
    pub fn declares(self, face: Face) -> bool {
        (!face.is_bold() || self.bold) && (!face.is_italic() || self.italic)
    }

    pub fn declared(self) -> impl Iterator<Item = Face> {
        Face::ALL.into_iter().filter(move |&f| self.declares(f))
    }

    pub fn synthesizes_anything(self) -> bool {
        self.bold || self.italic
    }
}

/// Total stem widening, as a fraction of the em. Matches the weight step a
/// real bold cut adds at UI sizes without closing counters.
pub const BOLD_STRENGTH_PER_PX: f32 = 1.0 / 24.0;

/// Oblique lean. Real italic faces report an `italicAngle` clustering at
/// 11-12 degrees; CSS synthesizes at 14. The lower value reads as type
/// rather than as a transform, and the web side must be told this angle
/// explicitly rather than left to synthesize its own.
pub const OBLIQUE_DEGREES: f32 = 12.0;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Synth {
    /// Total stem widening in pixels.
    pub embolden: f32,
    /// Oblique angle in degrees; positive leans right.
    pub skew_deg: f32,
}

impl Synth {
    pub const REGULAR: Synth = Synth {
        embolden: 0.0,
        skew_deg: 0.0,
    };
}

/// One rasterized glyph in the same units `GlyphInfo` records.
pub struct RasterGlyph {
    pub width: usize,
    pub height: usize,
    /// Left bearing in pixels, relative to the pen.
    pub left: f32,
    /// Distance from the baseline UP to the top row.
    pub top: f32,
    pub advance: f32,
    /// Coverage, row-major, `width * height`.
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct FontUnitMetrics {
    pub units_per_em: f32,
    pub ascender: f32,
    pub descender: f32,
    pub line_gap: f32,
    pub cap_height: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct GlyphUnitMetrics {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    pub advance: f32,
}

impl RasterGlyph {
    fn blank(advance: f32) -> Self {
        RasterGlyph {
            width: 0,
            height: 0,
            left: 0.0,
            top: 0.0,
            advance,
            data: Vec::new(),
        }
    }

    /// Sum of coverage / 255.
    #[cfg(test)]
    pub fn ink(&self) -> f64 {
        self.data.iter().map(|&v| v as f64).sum::<f64>() / 255.0
    }

    /// Ink extending right of the advance. A synthetic oblique pushes the top
    /// of the glyph past its own advance; the atlas cell must allow for it.
    #[cfg(test)]
    pub fn right_overhang(&self) -> f32 {
        if self.width == 0 {
            return 0.0;
        }
        self.left + self.width as f32 - self.advance
    }
}

/// A parsed face, kept so one bake resolves outlines once per font.
pub struct RasterFont<'a> {
    face: ttf_parser::Face<'a>,
    scale: f32,
}

impl<'a> RasterFont<'a> {
    pub fn new(data: &'a [u8], px: f32) -> Result<Self, String> {
        let face = ttf_parser::Face::parse(data, 0).map_err(|e| format!("{e:?}"))?;
        let upem = face.units_per_em();
        if upem == 0 {
            return Err("units_per_em is 0".into());
        }
        Ok(RasterFont {
            face,
            scale: px / upem as f32,
        })
    }

    pub fn unit_metrics(&self) -> FontUnitMetrics {
        let cap_height = self
            .face
            .glyph_index('X')
            .and_then(|glyph| self.face.glyph_bounding_box(glyph))
            .map(|bounds| (bounds.y_max - bounds.y_min) as f32)
            .unwrap_or(self.face.units_per_em() as f32);
        FontUnitMetrics {
            units_per_em: self.face.units_per_em() as f32,
            ascender: self.face.ascender() as f32,
            descender: self.face.descender() as f32,
            line_gap: self.face.line_gap() as f32,
            cap_height,
        }
    }

    pub fn glyph_unit_metrics(&self, ch: char, synth: Synth) -> Option<GlyphUnitMetrics> {
        let glyph = self.face.glyph_index(ch)?;
        let bounds = self.face.glyph_bounding_box(glyph);
        let mut left = bounds.map(|b| b.x_min as f32).unwrap_or(0.0);
        let top = bounds.map(|b| b.y_max as f32).unwrap_or(0.0);
        let mut width = bounds.map(|b| (b.x_max - b.x_min) as f32).unwrap_or(0.0);
        let height = bounds.map(|b| (b.y_max - b.y_min) as f32).unwrap_or(0.0);
        if synth.embolden > 0.0 {
            left -= synth.embolden / self.scale;
            width += synth.embolden / self.scale;
        }
        if synth.skew_deg != 0.0 {
            let shear = synth.skew_deg.to_radians().tan();
            left += (top * shear).min((top - height) * shear);
            width += height.abs() * shear.abs();
        }
        let advance = self.face.glyph_hor_advance(glyph).unwrap_or(0) as f32
            + if synth.embolden > 0.0 {
                synth.embolden / self.scale
            } else {
                0.0
            };
        if !left.is_finite() || !width.is_finite() || !height.is_finite() {
            return None;
        }
        Some(GlyphUnitMetrics {
            left,
            top,
            width: width.max(0.0),
            height: height.max(0.0),
            advance,
        })
    }

    /// Ascent, descent and line gap in pixels at this size. Backend-owned:
    /// callers must not re-derive vertical metrics from glyph boxes. Nothing
    /// reads these yet -- synthesis leaves a face's vertical metrics alone, so
    /// mixed faces already share a baseline; the consumer is the multiline
    /// layout arc, which owes a real line height in place of the em * 1.2 that
    /// `stone_measure_text` fabricates today.
    #[cfg(test)]
    pub fn vertical_metrics(&self) -> (f32, f32, f32) {
        (
            self.face.ascender() as f32 * self.scale,
            self.face.descender() as f32 * self.scale,
            self.face.line_gap() as f32 * self.scale,
        )
    }

    pub fn rasterize(&self, ch: char, synth: Synth) -> Option<RasterGlyph> {
        let gid = self.face.glyph_index(ch)?;
        let advance = self.face.glyph_hor_advance(gid).unwrap_or(0) as f32 * self.scale;

        let mut sink = OutlineSink { cmds: Vec::new() };
        self.face.outline_glyph(gid, &mut sink);
        if sink.cmds.is_empty() {
            return Some(RasterGlyph::blank(advance));
        }

        Some(render_outline(&sink.cmds, self.scale, advance, synth))
    }
}

struct OutlineSink {
    cmds: Vec<zeno::Command>,
}

impl ttf_parser::OutlineBuilder for OutlineSink {
    fn move_to(&mut self, x: f32, y: f32) {
        self.cmds
            .push(zeno::Command::MoveTo(zeno::Point::new(x, y)));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.cmds
            .push(zeno::Command::LineTo(zeno::Point::new(x, y)));
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.cmds.push(zeno::Command::QuadTo(
            zeno::Point::new(x1, y1),
            zeno::Point::new(x, y),
        ));
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.cmds.push(zeno::Command::CurveTo(
            zeno::Point::new(x1, y1),
            zeno::Point::new(x2, y2),
            zeno::Point::new(x, y),
        ));
    }
    fn close(&mut self) {
        self.cmds.push(zeno::Command::Close);
    }
}

fn render_outline(cmds: &[zeno::Command], scale: f32, advance: f32, synth: Synth) -> RasterGlyph {
    use zeno::{Fill, Format, Mask, Stroke, Style, Transform};

    // Scale, y-flip and oblique in ONE matrix, built directly rather than
    // composed: zeno's `Transform::then` and `transform_point` do not share a
    // row/column convention, so composing silently transposes the shear.
    // `transform_point` is x' = px*xx + py*yx, y' = px*xy + py*yy, and a right
    // lean wants x' = s*(px + py*tan), y' = -s*py.
    let shear = synth.skew_deg.to_radians().tan();
    let t = Transform::new(scale, 0.0, scale * shear, -scale, 0.0, 0.0);

    let render = |style: Style| -> (Vec<u8>, zeno::Placement) {
        Mask::new(cmds)
            .style(style)
            .transform(Some(t))
            .format(Format::Alpha)
            .origin(zeno::Origin::TopLeft)
            .render()
    };

    // Under Origin::TopLeft, `Placement.top` is the y-DOWN coordinate of the
    // top edge, not a baseline-up bearing. The path is already y-flipped, so
    // the glyph sits at negative y; RasterGlyph::top wants the distance UP
    // from the baseline, hence the negation.
    let (fill_data, fill) = render(Style::Fill(Fill::NonZero));

    if synth.embolden <= 0.0 {
        return RasterGlyph {
            width: fill.width as usize,
            height: fill.height as usize,
            left: fill.left as f32,
            top: -(fill.top as f32),
            advance,
            data: fill_data,
        };
    }

    // Embolden by unioning the fill with a centred stroke of the target
    // width, which thickens stems outward without closing counters.
    // `scale = false` puts the width in POST-transform pixels; zeno's default
    // strokes in font units, where a pixel-sized width is ~1/2000 em and
    // vanishes silently.
    let mut stroke = Stroke::new(synth.embolden);
    stroke.scale = false;
    let (stroke_data, stroked) = render(Style::Stroke(stroke));

    // Union both bitmaps in the shared y-down placement space.
    let left = fill.left.min(stroked.left);
    let top = fill.top.min(stroked.top);
    let right = (fill.left + fill.width as i32).max(stroked.left + stroked.width as i32);
    let bottom = (fill.top + fill.height as i32).max(stroked.top + stroked.height as i32);
    let w = (right - left).max(0) as usize;
    let h = (bottom - top).max(0) as usize;

    let mut data = vec![0u8; w * h];
    for (src, place) in [(&fill_data, fill), (&stroke_data, stroked)] {
        let dx = (place.left - left) as usize;
        let dy = (place.top - top) as usize;
        for y in 0..place.height as usize {
            for x in 0..place.width as usize {
                let v = src[y * place.width as usize + x];
                let di = (y + dy) * w + (x + dx);
                if di < data.len() && v > data[di] {
                    data[di] = v;
                }
            }
        }
    }

    RasterGlyph {
        width: w,
        height: h,
        left: left as f32,
        top: -(top as f32),
        // The stroke widens the glyph on both sides, so the advance has to
        // grow with it or bold text sets tighter than it draws.
        advance: advance + synth.embolden,
        data,
    }
}

/// Fraction of the probe glyphs' ink texels that are only partially covered
/// (1..=254). A pixel font rasterized on its native grid is near-binary (~0);
/// off the grid -- or any smooth font, or a synthesized face -- antialiases
/// edges into many partial texels. Returns 0 if nothing rasterized.
pub fn gray_fraction(font: &RasterFont, probe: &[u8], synth: Synth) -> f32 {
    let mut ink = 0u64;
    let mut gray = 0u64;
    for &c in probe {
        let g = match font.rasterize(c as char, synth) {
            Some(g) => g,
            None => continue,
        };
        for &v in &g.data {
            if v == 0 {
                continue;
            }
            ink += 1;
            if v < 255 {
                gray += 1;
            }
        }
    }
    if ink == 0 {
        0.0
    } else {
        gray as f32 / ink as f32
    }
}

#[cfg(test)]
mod synth_tests {
    use super::*;

    fn droid() -> Vec<u8> {
        crate::text::FALLBACK_FONT.to_vec()
    }

    // The generated pixel font has a native grid; 48 rasterizes
    // near-binary on it, the size the pixel-grid tests load.
    fn pixel_font() -> Vec<u8> {
        crate::test_fonts::pixel_font()
    }

    const PX: f32 = 32.0;

    // Emboldening must add ink. A shear alone must not: it moves ink without
    // creating it, so an italic that changes ink is transforming wrongly.
    #[test]
    fn bold_adds_ink_and_oblique_does_not() {
        let data = droid();
        let f = RasterFont::new(&data, PX).expect("DroidSans parses");

        let base: f64 = ('a'..='z')
            .filter_map(|c| f.rasterize(c, Synth::REGULAR))
            .map(|g| g.ink())
            .sum();
        let bold: f64 = ('a'..='z')
            .filter_map(|c| f.rasterize(c, Face::Bold.synth(PX)))
            .map(|g| g.ink())
            .sum();
        let italic: f64 = ('a'..='z')
            .filter_map(|c| f.rasterize(c, Face::Italic.synth(PX)))
            .map(|g| g.ink())
            .sum();

        assert!(base > 0.0, "regular face produced no ink");
        let bold_ratio = bold / base;
        assert!(
            bold_ratio > 1.15 && bold_ratio < 1.75,
            "bold should widen stems (expected ink ratio 1.15..1.75, got {bold_ratio:.3})"
        );
        let italic_ratio = italic / base;
        assert!(
            (italic_ratio - 1.0).abs() < 0.05,
            "oblique should move ink, not create it (expected ratio ~1.0, got {italic_ratio:.3})"
        );
    }

    // The lean must be to the RIGHT. A sign error still produces a slanted
    // glyph with a plausible ink total, so ink alone cannot catch it; only
    // the direction of the overhang can.
    #[test]
    fn oblique_leans_right() {
        let data = droid();
        let f = RasterFont::new(&data, PX).expect("DroidSans parses");

        let upright = f.rasterize('l', Synth::REGULAR).expect("l rasterizes");
        let leaned = f
            .rasterize('l', Face::Italic.synth(PX))
            .expect("l rasterizes italic");

        assert!(
            leaned.right_overhang() > upright.right_overhang() + 1.0,
            "oblique must extend ink right of the advance (upright {:.2}, leaned {:.2}) \
             -- a negative skew leans left and looks correct in every scalar but this",
            upright.right_overhang(),
            leaned.right_overhang()
        );
    }

    // A pixel font on its native grid rasterizes binary. Regular must stay
    // that way through the new rasterizer: this is what `offgrid` protects
    // and what CrispArt depends on.
    #[test]
    fn pixel_font_regular_stays_binary() {
        let data = pixel_font();
        let f = RasterFont::new(&data, 48.0).expect("the pixel font parses");
        let gray = gray_fraction(&f, b"HMoxnwgB", Synth::REGULAR);
        assert!(
            gray <= 0.02,
            "the pixel font at 48px must rasterize near-binary, got gray fraction {gray:.3}"
        );
    }

    // Cross-check against an independent rasterizer: on unmodified glyphs the
    // ttf-parser/zeno path must agree with fontdue. Without this the synthesis
    // numbers above would have no control.
    #[test]
    fn regular_face_agrees_with_fontdue() {
        let data = droid();
        let f = RasterFont::new(&data, PX).expect("DroidSans parses");
        let fd = fontdue::Font::from_bytes(data.as_slice(), fontdue::FontSettings::default())
            .expect("fontdue parses DroidSans");

        let mut ours = 0.0f64;
        let mut theirs = 0.0f64;
        for c in 'a'..='z' {
            ours += f.rasterize(c, Synth::REGULAR).expect("rasterizes").ink();
            let (m, bitmap) = fd.rasterize(c, PX);
            let _ = m;
            theirs += bitmap.iter().map(|&v| v as f64).sum::<f64>() / 255.0;
        }
        let ratio = ours / theirs;
        assert!(
            (ratio - 1.0).abs() < 0.02,
            "regular ink must match fontdue within 2% (ratio {ratio:.4})"
        );
    }

    // The FFI carries a face as a mask (load) and as an index (glyph lookup),
    // and Rock encodes the same bits on its side. Both encodings must round
    // trip, or a family declares one face and reads another.
    #[test]
    fn face_encodings_round_trip() {
        for bold in [false, true] {
            for italic in [false, true] {
                let set = FaceSet { bold, italic };
                assert_eq!(FaceSet::from_mask(set.mask()), set, "mask round trip");
            }
        }
        for face in Face::ALL {
            assert_eq!(
                Face::from_index(face.index()),
                Some(face),
                "index round trip"
            );
        }
        assert_eq!(Face::from_index(Face::ALL.len()), None, "no fifth face");
    }

    // Synthesis leaves the source font's vertical metrics alone, which is why
    // mixed faces share a baseline for free. Units are pixels at this size:
    // ascent up (positive), descent down (negative), both scaling with px.
    #[test]
    fn vertical_metrics_are_pixels_at_this_size() {
        let data = droid();
        let a = RasterFont::new(&data, PX).expect("DroidSans parses");
        let b = RasterFont::new(&data, PX * 2.0).expect("DroidSans parses");

        let (asc, desc, gap) = a.vertical_metrics();
        assert!(asc > 0.0, "ascent is up (got {asc})");
        assert!(desc < 0.0, "descent is down (got {desc})");
        assert!(gap >= 0.0, "line gap is non-negative (got {gap})");
        assert!(
            asc - desc > PX * 0.8 && asc - desc < PX * 2.0,
            "ascent-to-descent must be em-scaled at {PX}px (got {})",
            asc - desc
        );

        let (asc2, desc2, _) = b.vertical_metrics();
        assert!(
            (asc2 / asc - 2.0).abs() < 0.001 && (desc2 / desc - 2.0).abs() < 0.001,
            "doubling px must double the metrics -- these are pixels, not font units"
        );
    }

    // Advances must come from the font's own hmtx, not from the ink box.
    // Emboldening widens the glyph, so the advance has to grow with it or
    // bold text sets tighter than it draws.
    #[test]
    fn bold_advance_grows_with_the_stems() {
        let data = droid();
        let f = RasterFont::new(&data, PX).expect("DroidSans parses");
        let base = f.rasterize('n', Synth::REGULAR).expect("n rasterizes");
        let bold = f
            .rasterize('n', Face::Bold.synth(PX))
            .expect("n rasterizes bold");
        assert!(
            bold.advance > base.advance,
            "bold advance must exceed regular ({:.2} vs {:.2})",
            bold.advance,
            base.advance
        );
    }
}
