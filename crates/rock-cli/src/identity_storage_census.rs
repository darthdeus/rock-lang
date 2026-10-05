//! `rock identity-storage-census [root]`: the exact per-package, per-kind
//! count of identity structs stored in declaration positions (S0a) and in
//! function bodies (S0b; docs/identity-storage-plan.md). Packages on
//! `bone_semantic::identity_storage::IDENTITY_STORAGE_EXEMPT`, and modules
//! under a prefix on `IDENTITY_STORAGE_EXEMPT_MODULES`, report no diagnostics
//! while they migrate; this census is what keeps their debt visible. A
//! module prefix is its own row, and its sites are also listed one by one.
//! `crates/rock-cli/tests/identity_storage_census.rs` pins the table.
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
    identity_module_body_sites, identity_storage_exempt, identity_storage_package, identity_storage_sites,
    IdentityStorageExemption, IdentityStorageExemptions, IdentityStorageKind, IDENTITY_STORAGE_EXEMPTIONS,
};
use bone_semantic::inference::Workspace;

use crate::Result;

#[derive(Default)]
struct Census {
    /// row -> its exemption. A row is a package, or `package::prefix` for
    /// the modules under a listed prefix.
    rows: BTreeMap<String, Option<IdentityStorageExemption>>,
    /// (row, kind) -> unique (file, line, identity)
    sites: BTreeMap<(String, IdentityStorageKind), BTreeSet<(String, usize, String)>>,
    /// Inputs that did not type-check for a reason other than identity storage.
    failed: Vec<(String, String)>,
    /// (file, line, kind, identity, binding) of every site, for
    /// `ROCK_IDENTITY_CENSUS_SITES`.
    site_rows: BTreeSet<(String, usize, IdentityStorageKind, String, String)>,
}

/// `ROCK_IDENTITY_CENSUS_BY_FILE=<kind label>` (e.g. `across-yield`) also
/// prints that kind's counts per file and identity, which is how a migration
/// shard checks that each file it rewrote dropped.
/// `ROCK_IDENTITY_CENSUS_SITES=<kind label>|all` prints each site as
/// `file<TAB>line<TAB>kind<TAB>identity<TAB>binding` (1-based line; the
/// binding is the local, parameter, global or field the diagnostic names).
pub fn do_identity_storage_census(root: &str) -> Result<()> {
    let census = collect(root, &IDENTITY_STORAGE_EXEMPTIONS)?;
    print!("{}", render(&census, &IDENTITY_STORAGE_EXEMPTIONS));
    if let Ok(label) = std::env::var("ROCK_IDENTITY_CENSUS_BY_FILE") {
        let kind = IdentityStorageKind::ALL
            .iter()
            .find(|kind| kind.label() == label)
            .ok_or_else(|| anyhow::anyhow!("ROCK_IDENTITY_CENSUS_BY_FILE: unknown kind `{label}`"))?;
        print!("{}", render_by_file(&census, *kind));
    }
    if let Ok(label) = std::env::var("ROCK_IDENTITY_CENSUS_SITES") {
        let kind = match label.as_str() {
            "all" => None,
            _ => Some(
                *IdentityStorageKind::ALL
                    .iter()
                    .find(|kind| kind.label() == label)
                    .ok_or_else(|| anyhow::anyhow!("ROCK_IDENTITY_CENSUS_SITES: unknown kind `{label}`"))?,
            ),
        };
        print!("{}", render_sites(&census, kind));
    }
    Ok(())
}

fn collect(root: &str, exemptions: &IdentityStorageExemptions) -> Result<Census> {
    let root = Path::new(root)
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("identity-storage-census: cannot resolve `{root}`: {e}"))?;
    let mut census = Census::default();
    for dir in packages_under(&root) {
        let label = rel(&root, &dir);
        let spec = crate::common::path_to_source_spec(&dir.display().to_string());
        census_input(&root, &label, &spec, exemptions, &mut census);
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
        census_input(&root, &label, &spec, exemptions, &mut census);
    }
    Ok(census)
}

fn render_by_file(census: &Census, kind: IdentityStorageKind) -> String {
    let mut per_file: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for ((_, site_kind), sites) in &census.sites {
        if *site_kind != kind {
            continue;
        }
        for (file, _, identity) in sites {
            *per_file.entry((file.as_str(), identity.as_str())).or_default() += 1;
        }
    }
    let mut out = format!("\n{} by file and identity:\n", kind.label());
    for ((file, identity), count) in per_file {
        out.push_str(&format!("{count}\t{file}\t{identity}\n"));
    }
    out
}

fn render_sites(census: &Census, kind: Option<IdentityStorageKind>) -> String {
    let mut out = String::from("\nsites (file, line, kind, identity, binding):\n");
    for (file, line, site_kind, identity, binding) in &census.site_rows {
        if kind.is_none_or(|k| k == *site_kind) {
            out.push_str(&format!("{file}\t{line}\t{}\t{identity}\t{binding}\n", site_kind.label()));
        }
    }
    out
}

/// The first backticked name in a site message other than its identity.
fn site_binding(message: &str, identity: &str) -> String {
    message
        .split('`')
        .skip(1)
        .step_by(2)
        .find(|name| *name != identity && !name.starts_with("Entity<"))
        .unwrap_or("-")
        .to_string()
}

fn census_input(
    root: &Path,
    label: &str,
    spec: &crate::unified_compile::SourceSpec,
    exemptions: &IdentityStorageExemptions,
    census: &mut Census,
) {
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
    let identity_errors = record(root, &ws, exemptions, census);
    // Identity diagnostics in enforced packages show up in the table's
    // enforced rows; anything else means the counts may be incomplete.
    let other = errs.len().saturating_sub(identity_errors);
    if other > 0 {
        census.failed.push((label.to_string(), format!("{other} other semantic errors")));
    }
}

/// Records every module's sites and returns how many of them the check
/// reported as errors: sites the production lists enforce, whatever lists the
/// rows follow.
fn record(root: &Path, ws: &Workspace, exemptions: &IdentityStorageExemptions, census: &mut Census) -> usize {
    let mut reported = 0;
    for module in ws.modules.iter() {
        let package = identity_storage_package(module);
        let exemption = exemptions.exemption(ws, module);
        let mut sites = identity_storage_sites(ws, module);
        sites.extend(identity_module_body_sites(ws, module));
        if sites.is_empty() {
            continue;
        }
        if !identity_storage_exempt(ws, module) {
            reported += sites.len();
        }
        let row = row_label(package, exemption);
        census.rows.insert(row.clone(), exemption);
        let file = rel(root, Path::new(module.file.as_str()));
        for site in sites {
            census.site_rows.insert((
                file.clone(),
                site.span.line_range.0 + 1,
                site.kind,
                site.identity.to_string(),
                site_binding(&site.message, &site.identity),
            ));
            census
                .sites
                .entry((row.clone(), site.kind))
                .or_default()
                .insert((file.clone(), site.span.line_range.0, site.identity.to_string()));
        }
    }
    reported
}

fn row_label(package: &str, exemption: Option<IdentityStorageExemption>) -> String {
    match exemption {
        Some(IdentityStorageExemption::Module(prefix)) => format!("{package}::{prefix}"),
        _ => package.to_string(),
    }
}

fn render(census: &Census, exemptions: &IdentityStorageExemptions) -> String {
    let mut out = String::new();
    let labels: Vec<&str> = IdentityStorageKind::ALL.iter().map(|kind| kind.label()).collect();
    out.push_str(&format!("| package | status | {} | total |\n", labels.join(" | ")));
    out.push_str(&format!("| --- | --- |{} ---: |\n", " ---: |".repeat(labels.len())));
    // A listed entry with no sites left is shown so it gets removed.
    let mut rows = census.rows.clone();
    for package in exemptions.packages {
        rows.entry(package.to_string()).or_insert(Some(IdentityStorageExemption::Package));
    }
    for (package, prefix) in exemptions.modules {
        let exemption = Some(IdentityStorageExemption::Module(prefix));
        rows.entry(row_label(package, exemption)).or_insert(exemption);
    }
    for (row, exemption) in &rows {
        let counts: Vec<usize> = IdentityStorageKind::ALL
            .iter()
            .map(|kind| census.sites.get(&(row.clone(), *kind)).map_or(0, BTreeSet::len))
            .collect();
        let total: usize = counts.iter().sum();
        let status = match (exemption, total) {
            (Some(IdentityStorageExemption::Package), 0) => "exempt, zero: remove from the list",
            (Some(IdentityStorageExemption::Package), _) => "exempt",
            (Some(IdentityStorageExemption::Module(_)), 0) => "exempt module, zero: remove from the list",
            (Some(IdentityStorageExemption::Module(_)), _) => "exempt module",
            (None, _) => "ENFORCED",
        };
        let cells = counts.iter().map(usize::to_string).collect::<Vec<_>>().join(" | ");
        out.push_str(&format!("| {row} | {status} | {cells} | {total} |\n"));
    }
    let mut module_sites = BTreeSet::new();
    for ((row, kind), sites) in &census.sites {
        if matches!(rows.get(row), Some(Some(IdentityStorageExemption::Module(_)))) {
            // Spans count lines from zero; `file:line` counts from one.
            module_sites.extend(
                sites
                    .iter()
                    .map(|(file, line, identity)| (row, file, line + 1, kind.label(), identity)),
            );
        }
    }
    if !module_sites.is_empty() {
        out.push_str("\nExempt module sites:\n");
        for (row, file, line, kind, identity) in module_sites {
            out.push_str(&format!("- {row}: {file}:{line} {kind} {identity}\n"));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use bone_semantic::identity_storage::IdentityStorageExemptions;

    // One enforced package with a listed module prefix: the enforced part and
    // the prefix are separate rows, and the prefix's sites are listed one by
    // one so a site that moves inside it changes the table.
    #[test]
    fn a_partly_exempt_package_has_a_row_per_status() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/identity-storage-scope");
        let exemptions = IdentityStorageExemptions {
            packages: &[bone_semantic::identity_storage::STDLIB_PACKAGE],
            modules: &[("scope_fixture", "nav"), ("scope_fixture", "empty")],
        };
        let census = collect(root.to_str().expect("UTF-8 fixture path"), &exemptions).expect("census");
        assert_eq!(
            render(&census, &exemptions),
            "\
| package | status | field | container | payload | newtype | local-storage | capture | co-param | across-yield | any-erasure | total |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| scope_fixture | ENFORCED | 0 | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 |
| scope_fixture::empty | exempt module, zero: remove from the list | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| scope_fixture::nav | exempt module | 0 | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 |
| std | exempt, zero: remove from the list | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |

Exempt module sites:
- scope_fixture::nav: scope-fixture/nav.rock:4 container scope_fixture::nav::NavMob
- scope_fixture::nav: scope-fixture/nav/graph.rock:4 container scope_fixture::nav::graph::GraphMob
"
        );
    }
}
