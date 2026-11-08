//! The visual bank's line schema: one compact JSON line per character,
//! animation, prop, icon, texture and architectural set. Eat builds and
//! publishes these lines; packaging and `eat verify-bank` read them back
//! through `parse_bank` and check them against each other with `resolve`. Every line that owns a sheet names its directory
//! `assets/.eat/<dir>/`, whose files have fixed names per kind (`sheet_files`).

mod verify;
pub use verify::{SheetSpec, check_sheet, sheet_spec};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

macro_rules! bank_file_name {
    () => {
        "visual_bank.jsonl"
    };
}
pub const BANK_FILE_NAME: &str = bank_file_name!();
/// The bank's path inside a game project.
pub const BANK_PATH: &str = concat!("assets/", bank_file_name!());
/// A sheet file is `<asset root>/.eat/<dir>/<name>`.
pub const SHEET_ROOT: &str = ".eat";
pub const SHEET_GEOMETRY: &str = "sheet.json";
/// The relight channels of an animation sheet, in the order the bake writes them.
pub const RELIGHT_CHANNELS: [&str; 4] = ["albedo", "normal", "depth", "material"];
/// The number of bake directions (Eat's `bake::DIRECTIONS`); a character
/// line's camera holds one basis per direction.
pub const DIRECTION_COUNT: usize = 8;

pub type JointTrackSet = BTreeMap<String, Vec<Vec<[f32; 3]>>>;

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Line {
    Animation(AnimationLine),
    Architecture(ArchitectureLine),
    Character(CharacterLine),
    Icon(IconLine),
    Prop(PropLine),
    Surface(SurfaceLine),
    Texture(TextureLine),
}

/// A static four-channel bake, independent of character animation and world placement.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceLine {
    pub id: String,
    pub dir: String,
    pub size: [u32; 2],
    pub cell: [u32; 2],
    pub pivot: [f32; 2],
    pub camera: Camera,
    #[serde(default)]
    pub skin_mask: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub studio: Option<Studio>,
}

/// Authored lighting paired with a relightable surface. The view LUT is shared by the consumer.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Studio {
    pub radiance_range: f32,
    pub skin: bool,
}

/// One camera and presentation scale per character; layers retain their body id.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CharacterLine {
    pub id: String,
    pub draw_size: f32,
    pub padding_px: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_of: Option<String>,
    pub camera: Camera,
}

/// The relight camera: one basis per Eat `bake::DIRECTIONS` entry in that order,
/// mirrored directions included, each `origin, dx, dy, dz` (`RelightDirection`).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Camera {
    pub ground_y: f32,
    pub bases: Vec<[f32; 12]>,
}

impl Camera {
    /// Admission checks reconstruction data. Intersecting `ground_y` is a
    /// world-placement operation; independent views can look along the floor.
    pub fn validate(&self) -> Result<()> {
        self.validate_views(DIRECTION_COUNT)
    }

    fn validate_views(&self, count: usize) -> Result<()> {
        anyhow::ensure!(self.ground_y.is_finite(), "camera ground_y must be finite");
        anyhow::ensure!(
            self.bases.len() == count,
            "camera has {} bases, expected {}",
            self.bases.len(),
            count
        );
        anyhow::ensure!(
            self.bases.iter().all(
                |basis| finite(basis) && basis[9..12].iter().any(|component| *component != 0.0)
            ),
            "camera bases must be finite with nonzero depth rays"
        );
        Ok(())
    }
}

/// One complete timeline per animation sheet, including layers.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AnimationLine {
    pub character: String,
    pub label: String,
    pub dir: String,
    pub size: [u32; 2],
    pub cell: [u32; 2],
    pub pivot: [f32; 2],
    pub frames: u32,
    pub frame_time: f64,
    pub skin_mask: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub travel: Option<Travel>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Travel {
    pub px: f32,
    pub curve: Vec<f32>,
}

/// A world prop: three relight channels plus one mask per `masks` entry, on an
/// atlas of `frame_size.w` x `frame_size.h * rows`. `rows` is every baked
/// azimuth row times `orientations` (a projectile bakes one row per flight
/// direction at one orientation).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PropLine {
    pub id: String,
    pub dir: String,
    pub frame_size: [u32; 2],
    pub pivot: [f32; 2],
    pub orientations: u32,
    pub rows: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub masks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth_planes: Option<Vec<[f32; 2]>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_height: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_offset: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fire_size: Option<[f32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fire_loop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light_origin: Option<[f32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light_source: Option<[f32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light_ground: Option<[f32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub walk_ramp: Option<WalkRamp>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub pixel_depth: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light: Option<String>,
    pub spin: bool,
}

/// A UI icon: one lit colour sheet and optional skin lighting response. It is
/// never a world prop and carries no world role.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IconLine {
    pub id: String,
    pub dir: String,
    pub frame_size: [u32; 2],
    pub pivot: [f32; 2],
    pub skin: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialSampling {
    #[default]
    Nearest,
    Linear,
    MipStable,
}

/// A standalone texture or tile atlas (`floor`, `wall`).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TextureLine {
    pub id: String,
    pub dir: String,
    pub size: [u32; 2],
    pub normal: bool,
    pub material: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<Floor>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Floor {
    #[serde(default)]
    pub sampling: [MaterialSampling; 3],
    pub repeat: [u32; 2],
    pub region: [u32; 4],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wang: Option<[u32; 2]>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchitectureLine {
    pub id: String,
    pub dir: String,
    pub size: [u32; 2],
    pub tile_size: [u32; 2],
    pub sprite_scale: [f32; 2],
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub masks: Vec<String>,
    pub pieces: Vec<ArchitecturalPiece>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchitecturalFrame {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub orientation: u32,
    #[serde(default)]
    pub anchor_point: [f32; 2],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pivot: Option<[f32; 2]>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchitecturalVariant {
    pub index: u32,
    pub frames: Vec<ArchitecturalFrame>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchitecturalPiece {
    pub id: String,
    pub role: String,
    pub anchor: String,
    pub anchor_point: [f32; 2],
    pub pivot: [f32; 2],
    pub sprite_scale: [f32; 2],
    #[serde(default)]
    pub orientation_map: Vec<u32>,
    pub variants: Vec<ArchitecturalVariant>,
}

/// An animation directory's `sheet.json`: the packed cell rects
/// `[x, y, w, h, origin_x, origin_y, mirrored]`, direction-major over all
/// eight directions, and optional joint tracks `[direction][frame] = [x, y, z]`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SheetGeometry {
    pub rects: Vec<[u32; 7]>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub joints: BTreeMap<String, Vec<Vec<[f64; 3]>>>,
}

impl SheetGeometry {
    pub fn validate(&self, animation: &AnimationLine) -> Result<()> {
        self.validate_layout(
            animation.size,
            animation.cell,
            animation.frames as usize,
            DIRECTION_COUNT,
        )
    }

    pub fn validate_layout(
        &self,
        size: [u32; 2],
        cell: [u32; 2],
        frames: usize,
        directions: usize,
    ) -> Result<()> {
        anyhow::ensure!(
            self.rects.len() == frames * directions,
            "{SHEET_GEOMETRY} has {} rects, expected {} frames x {directions} directions",
            self.rects.len(),
            frames
        );
        for (index, &[x, y, w, h, ox, oy, mirrored]) in self.rects.iter().enumerate() {
            let inside = |offset: u32, extent: u32, limit: u32| {
                offset.checked_add(extent).is_some_and(|end| end <= limit)
            };
            anyhow::ensure!(
                w > 0
                    && h > 0
                    && mirrored <= 1
                    && inside(x, w, size[0])
                    && inside(y, h, size[1])
                    && inside(ox, w, cell[0])
                    && inside(oy, h, cell[1]),
                "{SHEET_GEOMETRY} rect {index} is empty or exceeds its sheet or logical cell"
            );
        }
        for (name, tracks) in &self.joints {
            anyhow::ensure!(
                tracks.len() == directions && tracks.iter().all(|row| row.len() == frames),
                "{SHEET_GEOMETRY} joint {name:?} is not {directions} directions of {frames} frames"
            );
            anyhow::ensure!(
                tracks
                    .iter()
                    .flatten()
                    .flatten()
                    .all(|value| value.is_finite()),
                "{SHEET_GEOMETRY} joint {name:?} has a non-finite coordinate"
            );
        }
        Ok(())
    }
}

impl Line {
    /// The bank's order: kind, then id, then label.
    pub fn key(&self) -> (&'static str, &str, &str) {
        match self {
            Line::Animation(line) => ("animation", &line.character, &line.label),
            Line::Architecture(line) => ("architecture", &line.id, ""),
            Line::Character(line) => ("character", &line.id, ""),
            Line::Icon(line) => ("icon", &line.id, ""),
            Line::Prop(line) => ("prop", &line.id, ""),
            Line::Surface(line) => ("surface", &line.id, ""),
            Line::Texture(line) => ("texture", &line.id, ""),
        }
    }

    pub fn dir(&self) -> Option<&str> {
        match self {
            Line::Character(_) => None,
            Line::Animation(line) => Some(&line.dir),
            Line::Architecture(line) => Some(&line.dir),
            Line::Icon(line) => Some(&line.dir),
            Line::Prop(line) => Some(&line.dir),
            Line::Surface(line) => Some(&line.dir),
            Line::Texture(line) => Some(&line.dir),
        }
    }

    pub fn dir_mut(&mut self) -> Option<&mut String> {
        match self {
            Line::Character(_) => None,
            Line::Animation(line) => Some(&mut line.dir),
            Line::Architecture(line) => Some(&mut line.dir),
            Line::Icon(line) => Some(&mut line.dir),
            Line::Prop(line) => Some(&mut line.dir),
            Line::Surface(line) => Some(&mut line.dir),
            Line::Texture(line) => Some(&mut line.dir),
        }
    }

    pub fn describe(&self) -> String {
        match self.key() {
            (kind, id, "") => format!("{kind} {id:?}"),
            (kind, id, label) => format!("{kind} {id:?}/{label:?}"),
        }
    }
}

pub fn mask_file(mask: &str) -> String {
    format!("mask-{mask}.png")
}

/// The file names a sheet line's directory holds, sorted.
pub fn sheet_files(line: &Line) -> Vec<String> {
    let mut files: Vec<String> = match line {
        Line::Character(_) => Vec::new(),
        Line::Animation(_) => RELIGHT_CHANNELS
            .iter()
            .map(|channel| format!("{channel}.png"))
            .chain([SHEET_GEOMETRY.to_string()])
            .collect(),
        Line::Surface(surface) => RELIGHT_CHANNELS.iter()
            .map(|channel| format!("{channel}.png"))
            .chain([SHEET_GEOMETRY.to_string()])
            .chain(surface.studio.as_ref().into_iter().flat_map(|studio| {
                ["painted.png", "radiance.png"].into_iter()
                    .chain(studio.skin.then_some(["skin-albedo.png", "skin-diffuse.png", "skin-specular.png"]).into_iter().flatten())
                    .map(str::to_string)
            })).collect(),
        Line::Prop(prop) => ["albedo.png", "normal.png", "material.png"]
            .into_iter()
            .map(str::to_string)
            .chain(prop.masks.iter().map(|mask| mask_file(mask)))
            .collect(),
        Line::Icon(icon) => std::iter::once("color.png")
            .chain(
                icon.skin
                    .then_some(["skin-albedo.png", "skin-diffuse.png", "skin-specular.png"])
                    .into_iter()
                    .flatten(),
            )
            .map(str::to_string)
            .collect(),
        Line::Texture(texture) => std::iter::once("color.png")
            .chain(texture.normal.then_some("normal.png"))
            .chain(texture.material.then_some("material.png"))
            .map(str::to_string)
            .collect(),
        Line::Architecture(set) => std::iter::once("color.png".to_string())
            .chain(set.masks.iter().map(|mask| mask_file(mask)))
            .collect(),
    };
    files.sort();
    files
}

fn finite(values: &[f32]) -> bool {
    values.iter().all(|value| value.is_finite())
}

fn positive(size: [u32; 2]) -> bool {
    size[0] > 0 && size[1] > 0
}

pub fn is_sheet_dir(dir: &str) -> bool {
    dir.len() == 64
        && dir
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn check_masks(masks: &[String]) -> Result<()> {
    anyhow::ensure!(
        masks.windows(2).all(|pair| pair[0] < pair[1]),
        "masks {masks:?} are not sorted and unique"
    );
    for mask in masks {
        anyhow::ensure!(
            !mask.is_empty()
                && mask
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "mask name {mask:?} is not a file name component"
        );
    }
    Ok(())
}

/// The checks one line carries on its own; `resolve` checks lines against
/// each other.
pub fn check_line(line: &Line) -> Result<()> {
    let (_, id, label) = line.key();
    anyhow::ensure!(
        !id.is_empty() && (!matches!(line, Line::Animation(_)) || !label.is_empty()),
        "bank identifiers must be non-empty"
    );
    if let Some(dir) = line.dir() {
        anyhow::ensure!(
            is_sheet_dir(dir),
            "dir {dir:?} is not 64 lowercase hex digits"
        );
    }
    match line {
        Line::Character(c) => {
            anyhow::ensure!(
                c.layer_of.as_ref().is_none_or(|id| !id.is_empty()),
                "layer_of must be non-empty"
            );
            anyhow::ensure!(
                c.draw_size.is_finite() && c.draw_size > 0.0,
                "draw_size must be finite and positive"
            );
            anyhow::ensure!(
                c.padding_px.is_finite() && c.padding_px >= 0.0,
                "padding_px must be finite and non-negative"
            );
            c.camera.validate()?;
        }
        Line::Surface(surface) => {
            anyhow::ensure!(
                positive(surface.size) && positive(surface.cell),
                "size and cell must be non-zero"
            );
            anyhow::ensure!(finite(&surface.pivot), "pivot must be finite");
            let views = surface.camera.bases.len();
            anyhow::ensure!(views == 1 || views == DIRECTION_COUNT, "surface requires one or eight camera bases");
            surface.camera.validate_views(views)?;
            if let Some(studio) = &surface.studio {
                anyhow::ensure!(studio.radiance_range.is_finite() && studio.radiance_range > 0.0,
                    "studio radiance range must be finite and positive");
            }
        }
        Line::Animation(a) => {
            anyhow::ensure!(
                positive(a.size) && positive(a.cell),
                "size and cell must be non-zero"
            );
            anyhow::ensure!(finite(&a.pivot), "pivot must be finite");
            anyhow::ensure!(a.frames > 0, "frames must be non-zero");
            anyhow::ensure!(
                a.frame_time.is_finite() && a.frame_time > 0.0,
                "frame_time must be finite and positive"
            );
            if let Some(travel) = &a.travel {
                anyhow::ensure!(
                    travel.px.is_finite() && finite(&travel.curve),
                    "travel must be finite"
                );
                anyhow::ensure!(
                    travel.curve.len() == a.frames as usize,
                    "travel curve has {} frames, expected {}",
                    travel.curve.len(),
                    a.frames
                );
            }
        }
        Line::Prop(p) => {
            anyhow::ensure!(positive(p.frame_size), "frame_size must be non-zero");
            anyhow::ensure!(
                p.orientations > 0 && p.rows > 0 && p.rows % p.orientations == 0,
                "rows {} must be a non-zero multiple of orientations {}",
                p.rows,
                p.orientations
            );
            anyhow::ensure!(finite(&p.pivot), "pivot must be finite");
            check_masks(&p.masks)?;
            anyhow::ensure!(
                !(p.pixel_depth && p.depth_planes.is_some()),
                "pixel_depth and depth_planes are exclusive"
            );
            anyhow::ensure!(
                !(p.pixel_depth
                    && p.masks
                        .iter()
                        .any(|mask| mask == "water" || mask == "emissive")),
                "pixel_depth props cannot carry a water or emissive mask"
            );
            anyhow::ensure!(
                [&p.light, &p.fire_loop]
                    .into_iter()
                    .flatten()
                    .all(|text| !text.is_empty()),
                "prop presentation names must be non-empty"
            );
            if let Some(planes) = &p.depth_planes {
                anyhow::ensure!(
                    planes.len() == p.orientations as usize
                        && planes.iter().all(|plane| finite(plane)),
                    "depth_planes must contain one finite plane per orientation"
                );
            }
            anyhow::ensure!(
                [p.mount_height, p.mount_offset]
                    .into_iter()
                    .flatten()
                    .all(f32::is_finite)
                    && [p.fire_size, p.light_origin, p.light_source, p.light_ground]
                        .into_iter()
                        .flatten()
                        .all(|point| finite(&point)),
                "prop presentation coordinates must be finite"
            );
            if let Some(ramp) = &p.walk_ramp {
                anyhow::ensure!(
                    finite(&ramp.area[0])
                        && finite(&ramp.area[1])
                        && finite(&ramp.run)
                        && ramp.rise.is_finite(),
                    "walk_ramp coordinates must be finite"
                );
            }
            if let Some(placement) = &p.placement {
                placement.validate(p.orientations)?;
            }
        }
        Line::Icon(i) => {
            anyhow::ensure!(positive(i.frame_size), "frame_size must be non-zero");
            anyhow::ensure!(finite(&i.pivot), "pivot must be finite");
        }
        Line::Texture(t) => {
            anyhow::ensure!(positive(t.size), "size must be non-zero");
            if let Some(floor) = &t.floor {
                validate_floor_texture(
                    Some(floor.repeat),
                    Some(floor.region),
                    floor.wang,
                    (t.size[0], t.size[1]),
                )?;
            }
        }
        Line::Architecture(a) => {
            anyhow::ensure!(
                positive(a.size) && positive(a.tile_size),
                "size and tile_size must be non-zero"
            );
            anyhow::ensure!(
                finite(&a.sprite_scale) && a.sprite_scale.iter().all(|v| *v > 0.),
                "sprite_scale must be finite and positive"
            );
            check_masks(&a.masks)?;
            anyhow::ensure!(!a.pieces.is_empty(), "pieces must be non-empty");
            let mut ids = std::collections::BTreeSet::new();
            for piece in &a.pieces {
                anyhow::ensure!(
                    !piece.id.is_empty() && !piece.role.is_empty() && ids.insert(&piece.id),
                    "empty or duplicate architectural piece id or role"
                );
                anyhow::ensure!(
                    ["cell_center", "edge_center", "vertex", "doorway_center"]
                        .contains(&piece.anchor.as_str()),
                    "invalid architectural anchor"
                );
                anyhow::ensure!(
                    finite(&piece.anchor_point)
                        && finite(&piece.pivot)
                        && finite(&piece.sprite_scale)
                        && piece.sprite_scale.iter().all(|v| *v > 0.),
                    "invalid architectural piece coordinates"
                );
                anyhow::ensure!(!piece.variants.is_empty(), "variants must be non-empty");
                for (index, variant) in piece.variants.iter().enumerate() {
                    anyhow::ensure!(
                        variant.index as usize == index && !variant.frames.is_empty(),
                        "variant indices must be contiguous with non-empty frames"
                    );
                    for (orientation, frame) in variant.frames.iter().enumerate() {
                        anyhow::ensure!(
                            frame.orientation as usize == orientation
                                && frame.w > 0
                                && frame.h > 0
                                && u64::from(frame.x) + u64::from(frame.w) <= u64::from(a.size[0])
                                && u64::from(frame.y) + u64::from(frame.h) <= u64::from(a.size[1])
                                && finite(&frame.anchor_point)
                                && frame.pivot.is_none_or(|p| finite(&p)),
                            "invalid architectural frame"
                        );
                    }
                }
                let count = piece.variants[0].frames.len();
                let mut orientations = piece.orientation_map.clone();
                orientations.sort_unstable();
                anyhow::ensure!(
                    orientations == (0..count as u32).collect::<Vec<_>>(),
                    "orientation_map must select every authored orientation exactly once"
                );
            }
        }
    }
    Ok(())
}

pub fn parse_line(raw: &str) -> Result<Line> {
    let line: Line = serde_json::from_str(raw)?;
    check_line(&line)?;
    Ok(line)
}

/// Parses a bank file into its lines, with every line that does not parse or
/// check reported by number.
pub fn parse_bank(text: &str) -> (Vec<Line>, Vec<String>) {
    let mut problems = Vec::new();
    if !text.is_empty() && !text.ends_with('\n') {
        problems.push("the bank does not end with a newline".to_string());
    }
    let mut lines = Vec::new();
    for (number, raw) in text
        .lines()
        .enumerate()
        .map(|(index, raw)| (index + 1, raw))
    {
        match parse_line(raw) {
            Ok(line) => lines.push(line),
            Err(error) => problems.push(format!("line {number}: {error:#}")),
        }
    }
    (lines, problems)
}

/// The characters and animations of a bank, by id and by (character, label).
pub struct BankIndex<'a> {
    pub characters: BTreeMap<&'a str, &'a CharacterLine>,
    pub animations: BTreeMap<(&'a str, &'a str), &'a AnimationLine>,
}

/// Indexes `lines` and returns every problem between them: order and
/// uniqueness, orphan animations, and layers that do not match their body.
pub fn resolve(lines: &[Line]) -> (BankIndex<'_>, Vec<String>) {
    let mut problems = Vec::new();
    for pair in lines.windows(2) {
        if pair[0].key() >= pair[1].key() {
            problems.push(format!(
                "{} is out of order or repeated after {}",
                pair[1].describe(),
                pair[0].describe()
            ));
        }
    }
    let mut index = BankIndex {
        characters: BTreeMap::new(),
        animations: BTreeMap::new(),
    };
    for line in lines {
        match line {
            Line::Character(c) => {
                index.characters.insert(&c.id, c);
            }
            Line::Animation(a) => {
                index.animations.insert((&a.character, &a.label), a);
            }
            _ => {}
        }
    }
    for (&id, character) in &index.characters {
        let labels: Vec<&str> = index
            .animations
            .range((id, "")..)
            .take_while(|((c, _), _)| *c == id)
            .map(|((_, l), _)| *l)
            .collect();
        if labels.is_empty() {
            problems.push(format!("character {id:?} has no animations"));
        }
        let Some(body_id) = character.layer_of.as_deref() else {
            continue;
        };
        let Some(body) = index.characters.get(body_id) else {
            problems.push(format!("layer {id:?} names missing body {body_id:?}"));
            continue;
        };
        if body.layer_of.is_some() {
            problems.push(format!(
                "layer {id:?} names {body_id:?}, which is itself a layer"
            ));
        }
        for label in labels {
            match index.animations.get(&(body_id, label)) {
                None => problems.push(format!(
                    "layer {id:?} animation {label:?} has no body animation on {body_id:?}"
                )),
                Some(body_animation) => {
                    let layer = index.animations[&(id, label)];
                    if layer.frames != body_animation.frames
                        || layer.frame_time != body_animation.frame_time
                    {
                        problems.push(format!(
                            "layer {id:?} animation {label:?}: layer timeline differs from its body"
                        ));
                    }
                }
            }
        }
    }
    for &(id, label) in index.animations.keys() {
        if !index.characters.contains_key(id) {
            problems.push(format!("animation {id:?}/{label:?} has no character line"));
        }
    }
    (index, problems)
}

pub fn validate_floor_texture(
    repeat: Option<[u32; 2]>,
    region: Option<[u32; 4]>,
    wang: Option<[u32; 2]>,
    dimensions: (u32, u32),
) -> anyhow::Result<()> {
    match (repeat, region) {
        (None, None) => {
            anyhow::ensure!(
                wang.is_none(),
                "floor_wang requires floor_repeat and floor_region"
            );
            Ok(())
        }
        (Some([rx, ry]), Some([x, y, w, h])) => {
            anyhow::ensure!(
                rx > 0 && ry > 0 && w > 0 && h > 0,
                "floor repeat and region must be positive"
            );
            anyhow::ensure!(
                u64::from(x) + u64::from(w) <= u64::from(dimensions.0)
                    && u64::from(y) + u64::from(h) <= u64::from(dimensions.1),
                "floor region exceeds texture"
            );
            anyhow::ensure!(
                w % rx == 0 && h % ry == 0,
                "floor region must divide into whole-pixel cells"
            );
            if let Some([sx, sy]) = wang {
                anyhow::ensure!(sx > 0 && sy > 0, "floor_wang set counts must be positive");
                let cells_x = rx.checked_mul(4).and_then(|value| value.checked_mul(sx));
                let cells_y = ry.checked_mul(4).and_then(|value| value.checked_mul(sy));
                anyhow::ensure!(
                    cells_x.is_some_and(|cells| w % cells == 0)
                        && cells_y.is_some_and(|cells| h % cells == 0),
                    "a floor_wang region must divide into whole-pixel tiles of 4x4 sets"
                );
            }
            Ok(())
        }
        _ => anyhow::bail!("floor_repeat and floor_region must be declared together"),
    }
}

/// A walkable slope on a prop, in the prop's unturned tile coordinates (the
/// collision_footprint frame, placement point at [0.5, 0.5]). Inside `area`
/// ([min, max] corners) the ground rises from 0 at `run[0]` to `rise` metres
/// at `run[1]` along +x and stays at `rise` past it. A negative rise descends.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WalkRamp {
    pub area: [[f32; 2]; 2],
    pub run: [f32; 2],
    pub rise: f32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Composition {
    Single,
    Composed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementFrame {
    pub visual: Vec<Vec<[f32; 2]>>,
    pub front: Option<[f32; 2]>,
    pub clearance: Option<Vec<[f32; 2]>>,
    pub wall_contact: Option<[[f32; 2]; 2]>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub source_sha256: String,
    pub composition: Composition,
    pub frames: Vec<PlacementFrame>,
}

impl Placement {
    pub fn validate(&self, orientations: u32) -> Result<()> {
        anyhow::ensure!(
            self.source_sha256.len() == 64
                && self.source_sha256.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid placement source digest"
        );
        anyhow::ensure!(
            orientations > 0 && self.frames.len() == orientations as usize,
            "incomplete placement frames"
        );
        for frame in &self.frames {
            anyhow::ensure!(
                !frame.visual.is_empty() && frame.visual.len() <= 256,
                "invalid visual envelope population"
            );
            for points in &frame.visual {
                check_polygon(points)?;
            }
            if let Some(points) = &frame.clearance {
                check_polygon(points)?;
            }
            if let Some(front) = frame.front {
                anyhow::ensure!(
                    finite_point(front) && front[0] * front[0] + front[1] * front[1] > 1e-6,
                    "invalid projected front"
                );
            }
            if let Some(edge) = frame.wall_contact {
                anyhow::ensure!(
                    edge.iter().all(|p| finite_point(*p)) && edge[0] != edge[1],
                    "invalid projected wall contact"
                );
            }
        }
        Ok(())
    }
}

pub fn finite_point(point: [f32; 2]) -> bool {
    point.iter().all(|v| v.is_finite() && v.abs() <= 65536.)
}

pub fn check_polygon(points: &[[f32; 2]]) -> Result<()> {
    anyhow::ensure!(
        (3..=128).contains(&points.len()) && points.iter().all(|p| finite_point(*p)),
        "invalid placement polygon"
    );
    let area: f32 = points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .map(|(a, b)| a[0] * b[1] - a[1] * b[0])
        .sum();
    anyhow::ensure!(area.abs() > 1e-6, "placement polygon has no area");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn relight_camera_admission_does_not_require_a_floor_intersection() {
        let mut line = Line::Character(CharacterLine {
            id: "surface".into(),
            draw_size: 1.0,
            padding_px: 0.0,
            layer_of: None,
            camera: Camera {
                ground_y: 0.0,
                bases: vec![[0., 0., 0., 1., 0., 0., 0., 1., 0., 0., 0., -2.]; DIRECTION_COUNT],
            },
        });
        assert!(
            check_line(&line).is_ok(),
            "horizontal cameras reconstruct surfaces without a floor"
        );
        let Line::Character(character) = &mut line else {
            unreachable!()
        };
        character.camera.bases[0][11] = 0.0;
        assert!(
            check_line(&line).is_err(),
            "a zero depth ray cannot reconstruct a surface"
        );
        let Line::Character(character) = &mut line else {
            unreachable!()
        };
        character.camera.bases[0][10] = -1.0;
        assert!(
            check_line(&line).is_ok(),
            "world-facing cameras remain admitted"
        );
        let Line::Character(character) = &mut line else {
            unreachable!()
        };
        character.camera.bases[0][0] = f32::NAN;
        assert!(
            check_line(&line).is_err(),
            "non-finite origins remain invalid"
        );
    }

    #[test]
    fn independent_surface_uses_relight_channels_without_character_roles() {
        let raw = json!({"kind":"surface", "id":"sanctuary", "dir":"0".repeat(64),
            "size":[16,16], "cell":[8,8], "pivot":[0.5,0.5],
            "camera":{"ground_y":0.0,"bases":vec![
                [0.,0.,0.,1.,0.,0.,0.,1.,0.,0.,0.,-1.];DIRECTION_COUNT]}});
        let line: Line =
            serde_json::from_value(raw).expect("an independent surface is a bank asset");
        assert!(check_line(&line).is_ok());
        assert_eq!(
            sheet_files(&line),
            vec![
                "albedo.png",
                "depth.png",
                "material.png",
                "normal.png",
                "sheet.json"
            ]
        );
        let lines = [line];
        let (_, problems) = resolve(&lines);
        assert!(
            problems.is_empty(),
            "surface admission must not require character animations: {problems:?}"
        );
    }

    #[test]
    fn single_view_studio_surface_contract() {
        let raw = json!({"kind":"surface", "id":"relief", "dir":"0".repeat(64),
            "size":[2,2], "cell":[2,2], "pivot":[0.5,0.5], "skin_mask":true,
            "studio":{"skin":true,"radiance_range":16.0},
            "camera":{"ground_y":0.0,"bases":[
                [0.,0.,0.,1.,0.,0.,0.,1.,0.,0.,0.,-1.]]}});
        let line: Line = serde_json::from_value(raw.clone()).unwrap();
        check_line(&line).unwrap();
        let files = sheet_files(&line);
        for name in ["painted.png", "radiance.png", "skin-albedo.png", "skin-diffuse.png", "skin-specular.png"] {
            assert!(files.contains(&name.to_string()), "missing {name}");
        }
        let root = std::env::temp_dir().join(format!("surface-bank-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
        let _cleanup = Cleanup(root.clone());
        for name in &files {
            if name.ends_with(".png") { image::RgbaImage::new(2, 2).save(root.join(name)).unwrap(); }
        }
        std::fs::write(root.join(SHEET_GEOMETRY), serde_json::to_vec(&json!({
            "rects":[[0,0,2,2,0,0,0]], "joints":{}
        })).unwrap()).unwrap();
        let spec = sheet_spec(&line).unwrap().unwrap();
        check_sheet(&root, &spec).unwrap();
        std::fs::remove_file(root.join("radiance.png")).unwrap();
        assert!(check_sheet(&root, &spec).unwrap_err().iter().any(|p| p.contains("radiance.png")));
        image::RgbaImage::new(1, 2).save(root.join("radiance.png")).unwrap();
        assert!(check_sheet(&root, &spec).unwrap_err().iter().any(|p| p.contains("expected 2x2")));
        image::RgbaImage::new(2, 2).save(root.join("radiance.png")).unwrap();
        std::fs::write(root.join(SHEET_GEOMETRY), serde_json::to_vec(&json!({
            "rects":[[0,0,2,2,0,0,0],[0,0,2,2,0,0,0]], "joints":{}
        })).unwrap()).unwrap();
        assert!(check_sheet(&root, &spec).is_err());
        let mut bad = raw.clone();
        bad["camera"]["bases"] = json!([]);
        assert!(check_line(&serde_json::from_value(bad).unwrap()).is_err());
        let mut bad = raw;
        bad["studio"]["radiance_range"] = json!(0.0);
        assert!(check_line(&serde_json::from_value(bad).unwrap()).is_err());
        let mut character: Line = serde_json::from_value(json!({
            "kind":"character", "id":"actor", "draw_size":1.0,"padding_px":0.0,
            "camera":{"ground_y":0.0,"bases":[[0.,0.,0.,1.,0.,0.,0.,1.,0.,0.,0.,-1.]]}
        })).unwrap();
        assert!(check_line(&character).is_err());
        if let Line::Character(c) = &mut character { c.camera.bases = vec![c.camera.bases[0]; DIRECTION_COUNT]; }
        check_line(&character).unwrap();
    }

    #[test]
    fn floor_texture_region_admission() {
        assert!(validate_floor_texture(None, None, None, (256, 256)).is_ok());
        assert!(
            validate_floor_texture(Some([8, 4]), Some([0, 0, 1024, 448]), None, (1024, 1024))
                .is_ok()
        );
        for (repeat, region) in [
            (Some([0, 4]), Some([0, 0, 256, 256])),
            (Some([3, 4]), Some([0, 0, 256, 256])),
            (Some([4, 4]), Some([240, 0, 256, 256])),
            (Some([4, 4]), None),
            (None, Some([0, 0, 256, 256])),
        ] {
            assert!(validate_floor_texture(repeat, region, None, (256, 256)).is_err());
        }
    }

    #[test]
    fn floor_wang_admission() {
        let region = Some([0, 0, 2048, 2048]);
        assert!(validate_floor_texture(Some([2, 2]), region, Some([2, 2]), (2048, 2048)).is_ok());
        // Two sets of four tiles of two cells need a region divisible by sixteen cells.
        assert!(
            validate_floor_texture(
                Some([2, 2]),
                Some([0, 0, 2040, 2048]),
                Some([2, 2]),
                (2048, 2048)
            )
            .is_err()
        );
        assert!(validate_floor_texture(Some([2, 2]), region, Some([0, 1]), (2048, 2048)).is_err());
        assert!(validate_floor_texture(None, None, Some([1, 1]), (2048, 2048)).is_err());
        assert!(
            validate_floor_texture(Some([2, 2]), region, Some([u32::MAX, 1]), (2048, 2048))
                .is_err()
        );
        let line = |floor: serde_json::Value| -> Result<Line> {
            let line = serde_json::from_value(
                json!({"kind": "texture", "id": "cobble", "dir": "0".repeat(64),
                "size": [2048, 2048], "normal": false, "material": false, "floor": floor}),
            )?;
            check_line(&line)?;
            Ok(line)
        };
        let wang =
            line(json!({"repeat": [2, 2], "region": [0, 0, 2048, 2048], "wang": [2, 2]})).unwrap();
        assert_eq!(
            serde_json::to_value(&wang).unwrap()["floor"]["wang"],
            json!([2, 2])
        );
        assert!(
            line(json!({"repeat": [2, 2], "region": [0, 0, 2040, 2048], "wang": [2, 2]})).is_err()
        );
        let plain = line(json!({"repeat": [2, 2], "region": [0, 0, 2048, 2048]})).unwrap();
        assert!(
            serde_json::to_value(plain).unwrap()["floor"]
                .get("wang")
                .is_none()
        );
    }

    #[test]
    fn placement_polygons_need_area() {
        assert!(check_polygon(&[[0., 0.], [1., 0.], [0., 1.]]).is_ok());
        assert!(check_polygon(&[[0., 0.], [1., 0.], [2., 0.]]).is_err());
    }

    #[test]
    fn parse_bank_reports_each_bad_line_by_number_and_keeps_the_rest() {
        let good = "a".repeat(64);
        let icon = |id: &str, dir: &str| {
            format!(
                r#"{{"kind":"icon","id":"{id}","dir":"{dir}","frame_size":[8,8],"pivot":[0.5,0.5],"skin":false}}"#
            )
        };
        let text = [
            icon("ui/ring", &good),
            r#"{"kind":"icon","id":"ui/cut""#.to_owned(),
            r#"{"kind":"sound","id":"bell"}"#.to_owned(),
            format!(r#"{{"kind":"texture","id":"cobble","dir":"{good}"}}"#),
            icon("ui/ring", "assets/x"),
            format!(r#"{{"kind":"prop","id":"world/well","dir":"{good}","frame_size":[8,8],"pivot":[0.5,0.5],"orientations":1,"rows":1,"masks":["../up"],"spin":false}}"#),
            String::new(),
        ]
        .map(|line| line + "\n")
        .concat();
        let bad_depth = format!(
            r#"{{"kind":"prop","id":"bad","dir":"{good}","frame_size":[8,8],"pivot":[0.5,0.5],"orientations":1,"rows":1,"pixel_depth":true,"depth_planes":[[0,1]],"spin":false}}"#
        );
        assert!(
            !parse_bank(&(bad_depth + "\n")).1.is_empty(),
            "incompatible depth sources must reject the bank"
        );
        let (lines, errors) = parse_bank(&text);
        assert_eq!(lines.len(), 1);
        let numbers: Vec<&str> = errors
            .iter()
            .map(|error| error.split(':').next().unwrap())
            .collect();
        assert_eq!(
            numbers,
            ["line 2", "line 3", "line 4", "line 5", "line 6", "line 7"]
        );
    }
}
