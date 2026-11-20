//! Bin project discovery. The module keeps its old name because downstream
//! tests import `bin_projects_under` by this path.

use crate::Result;
use rock_host::stick::{PackageType, StickConfig};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Every bin project under `root`, sorted. A manifest ends the descent; hidden,
/// `target`, `node_modules` and `tmp` directories are skipped.
pub fn bin_projects_under(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        // Directory links can revisit an ancestor or another consumer path.
        if !seen.insert(fs_err::canonicalize(&dir)?) {
            continue;
        }
        let manifest = dir.join("stick.toml");
        if let Some(config) = StickConfig::from_path_optional(&manifest)
            .map_err(|error| anyhow::anyhow!("cannot load `{}`: {error:#}", manifest.display()))?
        {
            if config.package.package_type.unwrap_or(PackageType::Bin) == PackageType::Bin {
                out.push(dir);
            }
            continue;
        }
        for entry in fs_err::read_dir(&dir)? {
            let entry = entry?;
            let p = entry.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || matches!(name, "target" | "node_modules" | "tmp") {
                continue;
            }
            let file_type = entry.file_type()?;
            let is_dir = if file_type.is_symlink() {
                match fs_err::metadata(&p) {
                    Ok(metadata) => metadata.is_dir(),
                    // A dangling link contains no consumer to include.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => return Err(error.into()),
                }
            } else {
                file_type.is_dir()
            };
            if !is_dir {
                continue;
            }
            stack.push(p);
        }
    }
    out.sort();
    Ok(out)
}
