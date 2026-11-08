use crate::{DIRECTION_COUNT, Line, SHEET_GEOMETRY, SheetGeometry, sheet_files};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

/// What a sheet directory must hold.
pub struct SheetSpec {
    files: Vec<String>,
    png_size: [u32; 2],
    geometry: Option<([u32; 2], usize, usize)>,
}

pub fn sheet_spec(line: &Line) -> Result<Option<SheetSpec>> {
    let png_size = match line {
        Line::Character(_) => return Ok(None),
        Line::Animation(a) => a.size,
        Line::Surface(s) => s.size,
        Line::Prop(p) => [
            p.frame_size[0],
            p.frame_size[1]
                .checked_mul(p.rows)
                .context("prop atlas height overflows")?,
        ],
        Line::Icon(i) => i.frame_size,
        Line::Texture(t) => t.size,
        Line::Architecture(a) => a.size,
    };
    let geometry = match line {
        Line::Animation(a) => Some((a.cell, a.frames as usize, DIRECTION_COUNT)),
        Line::Surface(s) => Some((s.cell, 1, s.camera.bases.len())),
        _ => None,
    };
    Ok(Some(SheetSpec {
        files: sheet_files(line),
        png_size,
        geometry,
    }))
}

/// Reads every file of a sheet directory, decodes every PNG and checks it
/// against `spec`; returns the directory's hash or every problem found.
pub fn check_sheet(dir: &Path, spec: &SheetSpec) -> std::result::Result<String, Vec<String>> {
    let mut present = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|error| vec![format!("listing {}: {error}", dir.display())])?,
        Err(error) => return Err(vec![format!("{} is missing: {error}", dir.display())]),
    };
    present.sort();
    if present != spec.files {
        return Err(vec![format!(
            "{} holds {present:?}, expected {:?}",
            dir.display(),
            spec.files
        )]);
    }
    let mut problems = Vec::new();
    let mut listing = String::new();
    for name in &present {
        let path = dir.join(name);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                problems.push(format!("reading {}: {error}", path.display()));
                continue;
            }
        };
        listing.push_str(&format!("{:x}  {name}\n", Sha256::digest(&bytes)));
        if name.ends_with(".png") {
            match image::load_from_memory_with_format(&bytes, image::ImageFormat::Png) {
                Ok(image) if [image.width(), image.height()] == spec.png_size => {}
                Ok(image) => problems.push(format!(
                    "{} is {}x{}, expected {}x{}",
                    path.display(),
                    image.width(),
                    image.height(),
                    spec.png_size[0],
                    spec.png_size[1]
                )),
                Err(error) => problems.push(format!("decoding {}: {error}", path.display())),
            }
        } else if name == SHEET_GEOMETRY {
            let geometry = spec
                .geometry
                .as_ref()
                .expect("relight sheets hold sheet.json");
            match serde_json::from_slice::<SheetGeometry>(&bytes) {
                Ok(parsed) => {
                    if let Err(error) = parsed.validate_layout(
                        spec.png_size,
                        geometry.0,
                        geometry.1,
                        geometry.2,
                    ) {
                        problems.push(format!("{}: {error:#}", dir.display()));
                    }
                }
                Err(error) => problems.push(format!("parsing {}: {error}", path.display())),
            }
        }
    }
    if problems.is_empty() {
        Ok(format!("{:x}", Sha256::digest(listing.as_bytes())))
    } else {
        Err(problems)
    }
}
