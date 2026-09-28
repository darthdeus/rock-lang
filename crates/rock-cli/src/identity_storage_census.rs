//! `rock identity-storage-census [root]`: the exact per-package, per-kind
//! count of identity structs stored in declaration positions
//! (docs/identity-storage-plan.md S0a). Packages on
//! `bone_semantic::identity_storage::IDENTITY_STORAGE_EXEMPT` report no
//! diagnostics while they migrate; this census is what keeps their debt
//! visible. `crates/rock-cli/tests/identity_storage_census.rs` pins the table.
//!
//! Scope: every package under `root` (each checked with its own dependency
//! set), and each top-level `tests/*.rock` program as an unpackaged program.
//! Programs embedded in Rust tests are unpackaged too but are not counted.
//! Sites are unique by file, line and kind, and belong to the package that
//! owns the file, so a dependency reached from several roots counts once.
//! Output is deterministic: sorted, no timings, no absolute paths.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bone_semantic::identity_storage::{
    identity_storage_exempt, identity_storage_package, identity_storage_sites, IdentityStorageKind,
    IDENTITY_STORAGE_EXEMPT,
};
use bone_semantic::inference::Workspace;

use crate::Result;

const KINDS: [IdentityStorageKind; 5] = [
    IdentityStorageKind::Field,
    IdentityStorageKind::Container,
    IdentityStorageKind::Payload,
    IdentityStorageKind::Newtype,
    IdentityStorageKind::Global,
];

#[derive(Default)]
struct Census {
    /// package -> exempt?
    exempt: BTreeMap<String, bool>,
    /// (package, kind) -> unique (file, line, identity)
    sites: BTreeMap<(String, IdentityStorageKind), BTreeSet<(String, usize, String)>>,
    /// Inputs that did not type-check for a reason other than identity storage.
    failed: Vec<(String, String)>,
}

pub fn do_identity_storage_census(root: &str) -> Result<()> {
    print!("{}", identity_storage_census(root)?);
    Ok(())
}

pub fn identity_storage_census(root: &str) -> Result<String> {
    let root = Path::new(root)
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("identity-storage-census: cannot resolve `{root}`: {e}"))?;
    let mut census = Census::default();
    for dir in packages_under(&root) {
        let label = rel(&root, &dir);
        let spec = crate::common::path_to_source_spec(&dir.display().to_string());
        census_input(&root, &label, &spec, &mut census);
    }
    let mut programs: Vec<PathBuf> = fs_err::read_dir(root.join("tests"))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "rock"))
                .collect()
        })
        .unwrap_or_default();
    programs.sort();
    for file in programs {
        let label = rel(&root, &file);
        let spec = crate::unified_compile::SourceSpec::Files(vec![file]);
        census_input(&root, &label, &spec, &mut census);
    }
    Ok(render(&census))
}

fn census_input(root: &Path, label: &str, spec: &crate::unified_compile::SourceSpec, census: &mut Census) {
    let loaded = crate::unified_compile::discover_sources(spec)
        .map_err(|e| e.to_string())
        .and_then(|sources| {
            crate::skull_backend::skull_assemble_project(&sources)
                .map_err(|errs| format!("{} parse/assemble errors", errs.len()))
        })
        .and_then(|(mut modules, _entry)| {
            crate::bone_backend::check_modules_for_tools(&mut modules)
                .map_err(|errs| format!("{} semantic errors", errs.len()))
        });
    let (ws, errs) = match loaded {
        Ok(loaded) => loaded,
        Err(why) => {
            census.failed.push((label.to_string(), why));
            return;
        }
    };
    let identity_errors = record(root, &ws, census);
    // Identity diagnostics in enforced packages show up in the table's
    // enforced rows; anything else means the counts may be incomplete.
    let other = errs.len().saturating_sub(identity_errors);
    if other > 0 {
        census.failed.push((label.to_string(), format!("{other} other semantic errors")));
    }
}

/// Records every module's sites and returns how many of them the check
/// reported as errors (sites in enforced packages).
fn record(root: &Path, ws: &Workspace, census: &mut Census) -> usize {
    let mut reported = 0;
    for module in ws.modules.iter() {
        let package = identity_storage_package(module).to_string();
        let exempt = identity_storage_exempt(ws, module);
        let sites = identity_storage_sites(ws, module);
        if sites.is_empty() {
            continue;
        }
        if !exempt {
            reported += sites.len();
        }
        census.exempt.insert(package.clone(), exempt);
        let file = rel(root, Path::new(module.file.as_str()));
        for site in sites {
            census
                .sites
                .entry((package.clone(), site.kind))
                .or_default()
                .insert((file.clone(), site.span.line_range.0, site.identity.to_string()));
        }
    }
    reported
}

fn render(census: &Census) -> String {
    let mut out = String::new();
    out.push_str("| package | status | field | container | payload | newtype | global | total |\n");
    out.push_str("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    // A listed package with no sites left is shown so its entry gets removed.
    let mut rows = census.exempt.clone();
    for package in IDENTITY_STORAGE_EXEMPT {
        rows.entry(package.to_string()).or_insert(true);
    }
    for (package, exempt) in &rows {
        let counts: Vec<usize> = KINDS
            .iter()
            .map(|kind| census.sites.get(&(package.clone(), *kind)).map_or(0, BTreeSet::len))
            .collect();
        let total: usize = counts.iter().sum();
        let status = match (*exempt, total) {
            (true, 0) => "exempt, zero: remove from the list",
            (true, _) => "exempt",
            (false, _) => "ENFORCED",
        };
        let cells = counts.iter().map(usize::to_string).collect::<Vec<_>>().join(" | ");
        out.push_str(&format!("| {package} | {status} | {cells} | {total} |\n"));
    }
    if !census.failed.is_empty() {
        out.push_str("\nNOT fully analyzed:\n");
        for (label, why) in &census.failed {
            out.push_str(&format!("- {label}: {why}\n"));
        }
    }
    out
}

/// Every package directory under `root`, outside `crates/` (whose packages
/// are Rust test inputs) and build or VCS directories.
fn packages_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir != root && dir.join("stick.toml").exists() {
            out.push(dir);
            continue;
        }
        let Ok(entries) = fs_err::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || matches!(name, "target" | "node_modules" | "tmp" | "crates") {
                continue;
            }
            stack.push(p);
        }
    }
    out.sort();
    out
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}
