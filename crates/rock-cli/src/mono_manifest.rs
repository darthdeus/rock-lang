//! `rock mono-manifest [root]` -- Stage 0d evidence (`docs/monomorphization-plan.md`
//! S 0d). DIAGNOSTIC: observes what a monomorphizing compiler would instantiate
//! and changes no emission.
//!
//! Shares the closed-world enumeration with `mono-ty-census` (Stage 0c) so the
//! two measurements describe the same world, and seeds from
//! `skull::instance_graph::emission_roots`, which is production's own root
//! predicate rather than a copy of it.
//!
//! Deterministic: instances are rendered sorted by symbol, and the manifest
//! digest is computed over that text.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use skull::instance_graph::{self, InstanceGraph};

use crate::Result;

struct ProjectRow {
    name: String,
    instances: usize,
    /// Distinct FUNCTIONS behind those instances. The difference is the
    /// duplication monomorphization would introduce.
    funcs: usize,
    /// Instances with a non-empty argument list.
    generic_instances: usize,
    edges: usize,
    unresolved: usize,
    uninstantiated_roots: usize,
    digest: u64,
    stable: bool,
}

pub fn do_mono_manifest(root: Option<&str>, dump: Option<&str>) -> Result<()> {
    let root = Path::new(root.unwrap_or(".")).canonicalize().map_err(|e| {
        anyhow::anyhow!("mono-manifest: cannot resolve `{}`: {e}", root.unwrap_or("."))
    })?;
    let projects = crate::lint_union::bin_projects_under(&root);
    if projects.is_empty() {
        anyhow::bail!("mono-manifest: no `bin` project found under `{}`", root.display());
    }

    let mut rows: Vec<ProjectRow> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut hist: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut unresolved_reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut direct_rec: std::collections::BTreeSet<String> = Default::default();
    let mut mutual_rec: std::collections::BTreeSet<String> = Default::default();
    let mut changing_rec: std::collections::BTreeSet<String> = Default::default();
    let mut rec_today = 0usize;
    let mut rec_matched = 0usize;
    let mut rec_missing: std::collections::BTreeSet<String> = Default::default();
    let mut rec_rejected = 0usize;
    let mut rec_shadow_only = 0usize;
    let mut rec_unreached = 0usize;

    for dir in &projects {
        let name = dir.strip_prefix(&root).unwrap_or(dir).display().to_string();
        match one_project(dir, &name, dump.map(|d| (d, name.as_str()))) {
            Ok(row) => {
                for (k, v) in &row.1 {
                    *hist.entry(k).or_insert(0) += v;
                }
                for (k, v) in row.2 {
                    *unresolved_reasons.entry(k).or_insert(0) += v;
                }
                direct_rec.extend(row.3.direct_same_instance);
                mutual_rec.extend(row.3.mutual_same_instance);
                changing_rec.extend(row.3.changing_instance);
                rec_today += row.4.today;
                rec_matched += row.4.matched;
                rec_rejected += row.4.rejected_sites;
                rec_shadow_only += row.4.shadow_only;
                rec_unreached += row.4.missing_unreached_caller;
                rec_missing.extend(row.4.missing);
                rows.push(row.0);
            }
            Err(e) => skipped.push((name, e.to_string())),
        }
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    skipped.sort();

    println!("# Shadow instance manifest");
    println!();
    println!("projects: {} analyzed, {} skipped", rows.len(), skipped.len());
    println!();
    println!(
        "| project | instances | fns | generic inst. | extra bodies | edges | unresolved | order-stable |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|:--:|");
    for r in &rows {
        println!(
            "| {} | {} | {} | {} | +{} | {} | {} | {} |",
            r.name,
            r.instances,
            r.funcs,
            r.generic_instances,
            r.instances - r.funcs,
            r.edges,
            r.unresolved,
            if r.stable { "yes" } else { "**NO**" }
        );
    }
    let uninst: usize = rows.iter().map(|r| r.uninstantiated_roots).max().unwrap_or(0);
    println!();
    println!(
        "`uninst. generic roots` (max over projects): **{uninst}** -- generic fns that are \
         emission roots but that no call instantiates."
    );
    println!();
    println!("digests (manifest text, FNV-1a):");
    for r in &rows {
        println!("- `{}`: `{:016x}`", r.name, r.digest);
    }
    println!();

    println!("## edge-kind histogram (all projects)");
    println!();
    println!("| kind | count |");
    println!("|---|---:|");
    for (k, v) in &hist {
        println!("| `{k}` | {v} |");
    }
    println!();

    println!("## unresolved call sites by reason");
    println!();
    if unresolved_reasons.is_empty() {
        println!("none.");
    } else {
        println!("| reason | count |");
        println!("|---|---:|");
        for (k, v) in &unresolved_reasons {
            println!("| `{k}` | {v} |");
        }
    }
    println!();

    println!("## recursion census");
    println!();
    println!("- direct, same-instance (finite): **{}**", direct_rec.len());
    println!("- mutual, same-instance (finite): **{}**", mutual_rec.len());
    println!(
        "- changing-instance (polymorphic, expansion capped at depth {}): **{}**",
        instance_graph::MAX_INSTANTIATION_DEPTH,
        changing_rec.len()
    );
    for s in changing_rec.iter().take(10) {
        println!("  - `{s}`");
    }
    println!();

    println!("## shadow vs. today's conditional specializer");
    println!();
    println!("- specializations the CURRENT machinery produces: **{rec_today}**");
    println!("- ...present in the shadow graph as a complete instance: **{rec_matched}**");
    println!("- ...MISSING from the shadow graph: **{}**", rec_missing.len());
    for m in rec_missing.iter().take(10) {
        println!("  - `{m}`");
    }
    println!(
        "- ...absent because the shadow walk never reached the CALLING body \
         (a reachability difference, not a missing instance): **{rec_unreached}**"
    );
    println!("- shadow instances with no counterpart today: **{rec_shadow_only}**");
    println!("- call sites whose spec decision REJECTED (Stage 0b): **{rec_rejected}**");
    println!();

    if !skipped.is_empty() {
        println!("## NOT analyzed");
        println!();
        for (name, why) in &skipped {
            println!("- `{name}`: {}", why.lines().next().unwrap_or("?"));
        }
        println!();
    }
    Ok(())
}

type OneProject = (
    ProjectRow,
    BTreeMap<&'static str, usize>,
    BTreeMap<String, usize>,
    instance_graph::RecursionCensus,
    instance_graph::Reconciliation,
);

fn one_project(dir: &PathBuf, name: &str, dump: Option<(&str, &str)>) -> Result<OneProject> {
    let spec = crate::common::path_to_source_spec(&dir.display().to_string());
    let sources = crate::unified_compile::discover_sources(&spec)?;
    let (mut modules, _entry) = crate::skull_backend::skull_assemble_project(&sources)
        .map_err(|errs| anyhow::anyhow!("{} parse/assemble errors", errs.len()))?;
    let ws = crate::bone_backend::check_modules(&mut modules)
        .map_err(|errs| anyhow::anyhow!("{} type errors", errs.len()))?;

    let project_root = dir.display().to_string();
    let roots = instance_graph::emission_roots(&ws, Some(&project_root));

    let g = instance_graph::build(&ws, roots.clone(), false);
    // S 20g.4: perturbing discovery order may not change the manifest.
    let g2 = instance_graph::build(&ws, roots, true);
    let text = instance_graph::render_manifest(&ws, &g);
    let stable = text == instance_graph::render_manifest(&ws, &g2);

    if let Some((path, name)) = dump {
        let file = format!("{path}/{}.manifest", name.replace('/', "_"));
        std::fs::write(&file, &text)
            .map_err(|e| anyhow::anyhow!("writing {file}: {e}"))?;
    }

    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    for u in &g.unresolved {
        *reasons.entry(u.reason.to_string()).or_insert(0) += 1;
    }
    let census = g.recursion_census(&ws);
    let row = ProjectRow {
        // Relative: an absolute path varies by checkout and would make the
        // report non-comparable across machines.
        name: name.to_string(),
        instances: instance_graph::instance_count(&g),
        funcs: g
            .instances
            .iter()
            .map(|i| (i.func.module.raw(), i.func.func.into_raw().into_u32()))
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        generic_instances: g.instances.iter().filter(|i| !i.args.is_empty()).count(),
        edges: g.edges.len(),
        unresolved: g.unresolved.len(),
        uninstantiated_roots: g.uninstantiated_roots.len(),
        digest: instance_graph::manifest_digest(&ws, &g),
        stable,
    };
    let rec = instance_graph::reconcile(&ws, &g, bone_semantic::hir::FnBackend::Lua);
    Ok((row, hist_of(&g), reasons, census, rec))
}

fn hist_of(g: &InstanceGraph) -> BTreeMap<&'static str, usize> {
    g.edge_histogram()
}
