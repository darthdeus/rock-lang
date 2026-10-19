//! `rock lint --union <lib>` -- closed-world reachability for a LIBRARY.
//!
//! `unused_fn` asks "is this function reachable", which is only answerable
//! against a closed world. A `bin` has one: itself plus its dependencies. A
//! `lib` does not -- its callers live in consumers it cannot see -- so the
//! ordinary pass skips libs entirely (`StickConfig::is_bin_at`), and engine gets
//! no coverage from `rock lint` or the LSP.
//!
//! This recovers it by supplying the missing world. Every consumer in the repo
//! is analyzed on its own, and a function is reported only when it is unreached
//! in ALL of them -- the intersection. Per-consumer absence proves nothing: half
//! of engine's API is called by another project and not project, and reporting either half
//! from one consumer's view is how the LSP came to flag live engine code.
//!
//! Sound here because the repo is the whole world: engine has no external users.
//! A lib with consumers outside the tree cannot be linted this way.

use crate::Result;
use crate::lint_pass;
use bone_core::prelude::Ustr;
use rock_core::SourceFiles;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One consumer's verdict on the library: the key of every lib function it does
/// NOT reach. Keyed by (file, line) -- the same source read by every consumer,
/// so the keys line up for intersection.
type UnreachedKeys = HashSet<(Ustr, usize)>;

pub fn do_lint_union(lib: &str, consumers_root: Option<&str>) -> Result<()> {
    let lib_root = Path::new(lib)
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("lint --union: cannot resolve `{lib}`: {e}"))?;
    if !lib_root.is_dir() {
        anyhow::bail!("lint --union: `{lib}` is not a directory (a library's root)");
    }
    // The lib's parent by default -- `engine/` sits beside every game that uses
    // it, so the common case needs no flag. Deriving this by walking up to a
    // `.git` was tried and is a trap: a repo can sit anywhere above (`/tmp` is
    // one on this machine), and the scan then walks a filesystem.
    let search_root = match consumers_root {
        Some(d) => Path::new(d)
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("lint --union: cannot resolve `{d}`: {e}"))?,
        None => lib_root.parent().unwrap_or(&lib_root).to_path_buf(),
    };
    let candidates = bin_projects_under(&search_root);

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
    let mut findings: Vec<rock_core::CompilerError> =
        dead.iter().filter_map(|k| rendered.get(k).cloned()).collect();
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
    let sources = crate::unified_compile::discover_sources(
        &crate::unified_compile::SourceSpec::Directory(consumer.to_path_buf()),
    )?;
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

/// Every `bin` package under `root`, skipping build/VCS directories and
/// registered exclusions. A selected root is always scanned. Package sources
/// are not searched for nested packages.
pub fn bin_projects_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir.join("stick.toml").exists() {
            if rock_host::stick::StickConfig::is_bin_at(&dir) {
                out.push(dir);
            }
            continue;
        }
        let Ok(entries) = fs_err::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || matches!(name, "target" | "node_modules" | "tmp") {
                continue;
            }
            if (crate::registry::registered().scan_excluded)(&p) {
                continue;
            }
            stack.push(p);
        }
    }
    out.sort();
    out
}
