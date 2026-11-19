//! `rock lint --union <lib>` intersects dead-code findings across the bin
//! projects under a selected root. Each consumer is checked with its own
//! dependencies, so a library API used by any consumer remains live.
//!
//! The search root must contain every consumer. A library with callers outside
//! the selected tree cannot be linted as a closed world this way.

use crate::Result;
use crate::lint_pass;
use bone_core::prelude::Ustr;
use rock_core::SourceFiles;
use rock_host::stick::{PackageType, StickConfig};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

type UnreachedKeys = HashSet<(Ustr, usize)>;

pub(crate) fn do_lint_union(lib: &str, consumers_root: Option<&str>) -> Result<()> {
    let lib_root = Path::new(lib)
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("lint --union: cannot resolve `{lib}`: {e}"))?;
    if !lib_root.is_dir() {
        anyhow::bail!("lint --union: `{lib}` is not a directory (a library's root)");
    }
    let search_root = match consumers_root {
        Some(d) => Path::new(d)
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("lint --union: cannot resolve `{d}`: {e}"))?,
        None => lib_root.parent().unwrap_or(&lib_root).to_path_buf(),
    };
    let candidates = bin_projects_under(&search_root)?;

    let mut consumers: Vec<PathBuf> = Vec::new();
    let mut skipped: Vec<(PathBuf, String)> = Vec::new();
    let mut intersection: Option<UnreachedKeys> = None;
    // Findings render against a consumer's sources; any consumer that reached
    // the lib has its files, so the first success supplies them.
    let mut rendered: HashMap<(Ustr, usize), rock_core::CompilerError> = HashMap::new();
    let mut sources_for_render: Option<SourceFiles> = None;

    for dir in &candidates {
        match unreached_in(dir, &lib_root) {
            Ok(None) => {} // not a consumer of this lib
            Ok(Some((keys, findings, sources))) => {
                consumers.push(dir.clone());
                for f in findings {
                    rendered.entry((f.file(), f.line)).or_insert(f.error);
                }
                if sources_for_render.is_none() {
                    sources_for_render = Some(sources);
                }
                intersection = Some(match intersection {
                    None => keys,
                    Some(acc) => acc.intersection(&keys).copied().collect(),
                });
            }
            Err(e) => skipped.push((dir.clone(), e.to_string())),
        }
    }

    if consumers.is_empty() {
        anyhow::bail!(
            "lint --union: no consumer of `{}` found under `{}` -- nothing to intersect",
            lib_root.display(),
            search_root.display()
        );
    }

    // Loud, never a footnote: the intersection is only as sound as its inputs,
    // and a dropped consumer may have been a function's sole caller.
    for (dir, why) in &skipped {
        log::warn!(
            target: "LINT",
            "union: could not analyze consumer `{}` ({why}) -- its calls are NOT counted, so a finding below may be live code it alone reaches",
            dir.display()
        );
    }

    let dead = intersection.unwrap_or_default();
    let mut findings: Vec<rock_core::CompilerError> = dead
        .iter()
        .filter_map(|k| rendered.get(k).cloned())
        .collect();
    findings.sort_by_key(|e| (e.location.file, e.location.line));

    if findings.is_empty() {
        log::info!(
            target: "LINT",
            "{}: no findings across {} consumer(s)",
            lib_root.display(),
            consumers.len()
        );
        return Ok(());
    }
    if let Some(sources) = &sources_for_render {
        eprintln!("{}", rock_core::errors::render_all(&findings, sources));
    }
    log::info!(
        target: "LINT",
        "{}: {} finding(s) unreached by all {} consumer(s){}",
        lib_root.display(),
        findings.len(),
        consumers.len(),
        if skipped.is_empty() {
            String::new()
        } else {
            format!(" ({} consumer(s) NOT analyzed -- see warnings)", skipped.len())
        }
    );
    Ok(())
}

/// Analyze one candidate consumer. `Ok(None)` means it doesn't depend on the
/// lib at all (not a consumer, not a skip). `Err` means it IS a consumer but
/// could not be typechecked -- the caller must surface that, never absorb it.
#[allow(clippy::type_complexity)]
fn unreached_in(
    consumer: &Path,
    lib_root: &Path,
) -> Result<Option<(UnreachedKeys, Vec<bone_lints::Finding>, SourceFiles)>> {
    // Whole-project scope on purpose: `tests/` holds `@sim`/`@test` roots, and a
    // production-only source set would report every sim-only engine helper as
    // dead.
    let sources = rock_host::compilation_units::load_sources_for_project(consumer)?;
    let reaches_lib = sources.files.iter().any(|f| {
        let f = f.borrow();
        Path::new(&f.file.to_id_path()).starts_with(lib_root)
    });
    if !reaches_lib {
        return Ok(None);
    }

    let mut modules = crate::skull_backend::skull_assemble_project(&sources)
        .map_err(|errs| anyhow::anyhow!("{} parse error(s)", errs.len()))?
        .0;
    let ws = crate::bone_backend::check_modules(&mut modules)
        .map_err(|errs| anyhow::anyhow!("{} type error(s)", errs.len()))?;

    let findings = lint_pass::unused_fns_under(&ws, &sources, consumer, lib_root);
    let keys = findings.iter().map(|f| (f.file(), f.line)).collect();
    Ok(Some((keys, findings, sources)))
}

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
        if let Some(config) = StickConfig::from_path_optional(&manifest).map_err(|error| {
            anyhow::anyhow!(
                "lint --union: cannot load `{}`: {error:#}",
                manifest.display()
            )
        })? {
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
