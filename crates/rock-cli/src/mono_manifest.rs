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

/// Per-target projection totals across the whole corpus. The deferred list is
/// a property of the TARGET, not of a project, so it is copied rather than
/// summed.
struct ProjTotals {
    instances: usize,
    boundaries_available: usize,
    boundaries_total: usize,
    unsupported_by_class: BTreeMap<&'static str, usize>,
    deferred: Vec<&'static str>,
}

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
    let mut unresolved_examples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut rejection_kinds: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut rejection_examples: Vec<String> = Vec::new();
    let mut uninst_names: BTreeMap<String, instance_graph::RootHole> = BTreeMap::new();
    let mut closure_unexpanded: std::collections::BTreeSet<String> = Default::default();
    let mut closure_capped: std::collections::BTreeSet<String> = Default::default();
    let mut synth_examined = 0usize;
    let mut storage_failures: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut inline_dep_names: std::collections::BTreeSet<String> = Default::default();
    let mut closure_expanded = 0usize;
    let mut closure_instances = 0usize;
    let mut closure_closed = true;
    let mut closure_collisions: BTreeMap<String, std::collections::BTreeSet<String>> =
        BTreeMap::new();
    let mut direct_rec: std::collections::BTreeSet<String> = Default::default();
    let mut mutual_rec: std::collections::BTreeSet<String> = Default::default();
    let mut changing_rec: std::collections::BTreeSet<String> = Default::default();
    let mut rec_today = 0usize;
    let mut rec_matched = 0usize;
    let mut rec_missing: std::collections::BTreeSet<String> = Default::default();
    let mut rec_rejected = 0usize;
    let mut rec_shadow_only = 0usize;
    let mut rec_unreached = 0usize;
    let mut boundary_classes: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut boundary_max: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut boundaries_invalid = 0usize;
    let mut erased_entries: std::collections::BTreeSet<String> = Default::default();
    let mut coroutine_helpers = 0usize;
    let mut coroutine_collisions = 0usize;
    let mut always_emitted_seeds = 0usize;
    let mut span_edges: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    let mut span_rejections: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    let mut span_unresolved: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    let mut span_always_emitted: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    let mut identity_disagreements: Vec<String> = Vec::new();
    let mut shared_instances = 0usize;
    let mut identity_conflicts = 0usize;
    let mut variant_switches = 0usize;
    let mut js_invalid = 0usize;
    let mut lua_only_total = 0usize;
    let mut js_only_total = 0usize;
    let mut divergent_groups: std::collections::BTreeSet<String> = Default::default();
    let mut proj_digests: BTreeMap<&'static str, Vec<u64>> = BTreeMap::new();
    let mut proj_rows: BTreeMap<&'static str, ProjTotals> = BTreeMap::new();

    for dir in &projects {
        let name = dir.strip_prefix(&root).unwrap_or(dir).display().to_string();
        match one_project(dir, &name, &root, dump.map(|d| (d, name.as_str()))) {
            Ok(p) => {
                for (k, v) in &p.hist {
                    *hist.entry(k).or_insert(0) += v;
                }
                for (k, v) in p.reasons {
                    *unresolved_reasons.entry(k).or_insert(0) += v;
                }
                for (name, hole) in p.uninstantiated_root_names {
                    let e = uninst_names.entry(name).or_insert_with(
                        instance_graph::RootHole::default,
                    );
                    e.generic_root |= hole.generic_root;
                    e.always_emitted_sites.extend(hole.always_emitted_sites);
                    e.instances += hole.instances;
                }
                synth_examined += p.synthesized_examined;
                for (k, v) in p.storage_failures {
                    *storage_failures.entry(k).or_insert(0) += v;
                }
                inline_dep_names.extend(p.inline_dep_names);
                closure_expanded += p.closure.expanded;
                closure_instances += p.closure.instances;
                closure_closed &= p.closure.is_closed();
                closure_unexpanded.extend(p.closure.unexpanded);
                closure_capped.extend(p.closure.depth_capped);
                for (k, v) in p.closure.colliding_symbols {
                    closure_collisions.entry(k).or_default().extend(v);
                }
                direct_rec.extend(p.census.direct_same_instance);
                mutual_rec.extend(p.census.mutual_same_instance);
                changing_rec.extend(p.census.changing_instance);
                rec_today += p.reconciliation.today;
                rec_matched += p.reconciliation.matched;
                rec_rejected += p.reconciliation.rejected_sites;
                rec_shadow_only += p.reconciliation.shadow_only;
                rec_unreached += p.reconciliation.missing_unreached_caller;
                rec_missing.extend(p.reconciliation.missing);
                for (k, v) in &p.rejections {
                    *rejection_kinds.entry(k).or_insert(0) += v;
                }
                for x in p.rejection_examples {
                    if rejection_examples.len() < 10 {
                        rejection_examples.push(x);
                    }
                }
                for (k, v) in p.examples {
                    let e = unresolved_examples.entry(k).or_default();
                    for x in v {
                        if e.len() < 3 && !e.contains(&x) {
                            e.push(x);
                        }
                    }
                }
                for (k, v) in &p.boundaries {
                    *boundary_classes.entry(*k).or_insert(0) += v;
                    boundary_max.insert(*k, (*boundary_max.get(k).unwrap_or(&0)).max(*v));
                }
                boundaries_invalid += p.boundaries_invalid;
                erased_entries.extend(p.erased_entries);
                coroutine_helpers += p.coroutine_helpers;
                coroutine_collisions += p.coroutine_helper_collisions;
                always_emitted_seeds += p.always_emitted_seeds;
                for (map, from) in [
                    (&mut span_edges, &p.span_census.edges),
                    (&mut span_rejections, &p.span_census.rejections),
                    (&mut span_unresolved, &p.span_census.unresolved),
                    (&mut span_always_emitted, &p.span_census.always_emitted),
                ] {
                    for (k, c) in from {
                        let e = map.entry(k).or_insert((0, 0));
                        e.0 += c.examined;
                        e.1 += c.spanless;
                    }
                }
                shared_instances += p.shared_instances;
                identity_conflicts += p.identity_conflicts;
                if p.identity_conflicts > 0 {
                    identity_disagreements.push(p.row.name.clone());
                }
                variant_switches += p.target_variant_switches;
                lua_only_total += p.lua_only;
                js_only_total += p.js_only;
                divergent_groups.extend(p.divergent_variant_groups);
                js_invalid += p.js_boundaries_invalid;
                for pr in p.projections {
                    proj_digests.entry(pr.target).or_default().push(pr.manifest_digest);
                    // SUMMED across projects. `insert` here reported the last
                    // project's numbers under a whole-corpus heading.
                    let e = proj_rows.entry(pr.target).or_insert_with(|| ProjTotals {
                        instances: 0,
                        boundaries_available: 0,
                        boundaries_total: 0,
                        unsupported_by_class: BTreeMap::new(),
                        deferred: pr.deferred.clone(),
                    });
                    e.instances += pr.instances;
                    e.boundaries_available += pr.boundaries_available;
                    e.boundaries_total += pr.boundaries_total;
                    for (k, v) in &pr.unsupported_by_class {
                        *e.unsupported_by_class.entry(*k).or_insert(0) += v;
                    }
                }
                rows.push(p.row);
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
    if !uninst_names.is_empty() {
        println!();
        println!(
            "uncovered bare-name obligations, union over projects ({}):",
            uninst_names.len()
        );
        println!();
        println!("| name | cause | instances elsewhere |");
        println!("|---|---|---:|");
        for (n, h) in &uninst_names {
            let mut cause: Vec<String> = Vec::new();
            if h.generic_root {
                cause.push("generic emission root".to_string());
            }
            // Split by what the failure WOULD have been recorded as, not by
            // reason alone: a rejection is the stronger classification and
            // becomes a compile error at the cutover.
            let mut by_outcome: BTreeMap<(&'static str, &'static str), usize> = BTreeMap::new();
            for s in &h.always_emitted_sites {
                let what = match s.outcome {
                    instance_graph::AlwaysEmittedOutcome::Rejection(_) => "rejection",
                    instance_graph::AlwaysEmittedOutcome::Unresolved(_) => "unresolved",
                };
                *by_outcome.entry((what, s.outcome.label())).or_insert(0) += 1;
            }
            for ((what, label), n) in &by_outcome {
                cause.push(format!("always-emitted {what}: `{label}` x{n}"));
            }
            println!(
                "| `{n}` | {} | {} |",
                cause.join("; "),
                if h.instances == 0 { "0".to_string() } else { format!("**{}**", h.instances) }
            );
        }
        println!();
        println!(
            "`instances elsewhere` is NOT a closed hole. A generic emission root's \
             obligation is to exist under a BARE name that opaque Lua text can call, and \
             concrete instances from Rock call sites do not discharge it -- a boundary \
             contract has to. The column separates \"no concrete body of this function \
             exists at all\" from \"the Rock calls are covered and only the opaque-caller \
             entry is not\"."
        );
    }
    println!();
    println!(
        "`uncovered bare-name obligations` (max over projects): **{uninst}**. Not \"no call instantiates them\": six of the \
         corpus's have concrete instances from Rock call sites, which discharge those \
         call sites and not the bare-name entry opaque Lua text can call."
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
        println!("| reason | count | examples |");
        println!("|---|---:|---|");
        for (k, v) in &unresolved_reasons {
            let ex = unresolved_examples
                .get(k)
                .map(|e| e.join(", "))
                .unwrap_or_else(|| "--".to_string());
            println!("| `{k}` | {v} | {ex} |");
        }
    }
    println!();

    // Plan S 0h's three decided rejections. Zero over the corpus is the
    // measurement the decisions were taken on; a non-zero count is a real
    // program the concrete-program model refuses, not a diagnostic detail.
    println!("## synthesized column intercepts");
    println!();
    println!(
        "- storage-condition queries run (the DENOMINATOR): **{synth_examined}**; edges: **{}**",
        hist.get("compiler-generated").copied().unwrap_or(0)
    );
    println!(
        "- storage queries whose receiver had no concrete type (reported, never boxed): **{}**",
        storage_failures.values().sum::<usize>()
    );
    for (reason, n) in &storage_failures {
        println!("  - `{reason}`: {n}");
    }
    println!(
        "- runtime entries an opaque `inline(lua)` body depends on: **{}**",
        inline_dep_names.len()
    );
    for n in inline_dep_names.iter().take(10) {
        println!("  - `{n}`");
    }
    println!();
    println!(
        "Still an OVER-approximation by two named arms: a Map INDEX site emits ONE of"
    );
    println!("`__map_col_slot`/`_slot_checked`/`_free` and all three are counted, and");
    println!("`__vec_data_sort` keeps both of its nobox alternatives. Exact per-operation");
    println!("attribution waits on `ConcreteBody` recording the operation.");
    println!();

    println!("## graph closure (plan S 2 gate)");
    println!();
    println!("- instances expanded / reached: **{closure_expanded}** / **{closure_instances}**");
    println!("- instances with no body and no boundary class (callees MISSING): **{}**", closure_unexpanded.len());
    for n in closure_unexpanded.iter().take(10) {
        println!("  - `{n}`");
    }
    println!("- expansions cut by the depth cap: **{}**", closure_capped.len());
    println!(
        "- uncovered bare-name obligations (production emits these as shared \
         generic bodies): **{}**",
        uninst_names.len()
    );
    println!(
        "- LOGICAL symbols claimed by MORE THAN ONE instance: **{}**",
        closure_collisions.len()
    );
    println!("- **closed: {}**", if closure_closed { "YES" } else { "NO" });
    for (sym, ids) in closure_collisions.iter().take(10) {
        println!("  - `{sym}` <- {}", ids.iter().map(|i| format!("`{i}`")).collect::<Vec<_>>().join(", "));
    }
    println!();

    println!("## source-span coverage (plan S 2)");
    println!();
    println!(
        "A spanless fact cannot produce a located error at the Stage-3 cutover. Reported"
    );
    println!("with its denominator per kind: a zero over a zero is not coverage.");
    println!();
    println!("| what | kind | examined | spanless |");
    println!("|---|---|---:|---:|");
    for (what, m, evidence) in [
        ("edge", &span_edges, ""),
        ("rejection", &span_rejections, "`rejections_of` asserts it per fixture"),
        (
            "unresolved",
            &span_unresolved,
            "`an_unresolved_call_site_keeps_its_span` is the only evidence",
        ),
        (
            "always-emitted",
            &span_always_emitted,
            "`an_always_emitted_call_hole_keeps_its_reason_and_span`",
        ),
    ] {
        // An EMPTY population is stated, never left as an absent row: zero
        // spanless out of zero facts is not span coverage. Which fixture stands
        // in for the missing population is NAMED -- "the fixtures are the
        // evidence" asserted that a fixture existed without saying which, and
        // for the unresolved axis none did.
        if m.is_empty() {
            println!("| {what} | -- | 0 | (no corpus population; {evidence}) |");
            continue;
        }
        for (k, (examined, spanless)) in m {
            println!(
                "| {what} | `{k}` | {examined} | {} |",
                if *spanless == 0 { "0".to_string() } else { format!("**{spanless}**") }
            );
        }
    }
    let spanless_total: usize = span_edges
        .values()
        .chain(span_rejections.values())
        .chain(span_unresolved.values())
        .chain(span_always_emitted.values())
        .map(|(_, s)| s)
        .sum();
    println!();
    println!("- spanless facts across the corpus: **{spanless_total}**");
    println!();

    println!("## rejected constructions (plan S 0h)");
    println!();
    if rejection_kinds.is_empty() {
        println!("none.");
    } else {
        println!("| kind | count |");
        println!("|---|---:|");
        for (k, v) in &rejection_kinds {
            println!("| `{k}` | {v} |");
        }
        println!();
        for x in &rejection_examples {
            println!("- {x}");
        }
    }
    println!();

    println!("## recursion census");
    println!();
    println!("- direct, same-instance (finite): **{}**", direct_rec.len());
    println!("- mutual, same-instance (finite): **{}**", mutual_rec.len());
    println!(
        "- changing-instance (polymorphic, rejected; walk capped at type depth {}): **{}**",
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

    println!("## foreign boundaries (Lua view)");
    println!();
    println!("| class | total across projects | max in one project |");
    println!("|---|---:|---:|");
    for (k, v) in &boundary_classes {
        println!("| `{k}` | {v} | {} |", boundary_max.get(k).unwrap_or(&0));
    }
    println!();
    println!(
        "- boundaries INVALID on the target (plan S 10: a spanned rejection, never a stub): \
         **{boundaries_invalid}**"
    );
    println!(
        "- erased runtime entries reached, of the 15 catalogued in \
         `docs/boundary-contracts.md` S 1.4: **{}**",
        erased_entries.len()
    );
    if !erased_entries.is_empty() {
        println!(
            "  - {}",
            erased_entries.iter().map(|e| format!("`{e}`")).collect::<Vec<_>>().join(", ")
        );
    }
    println!();

    println!("## coroutine helper symbols");
    println!();
    println!("- helper symbols owned by coroutine instances: **{coroutine_helpers}**");
    println!(
        "- helper symbols claimed by MORE THAN ONE instance (a Stage-3 collision): \
         **{coroutine_collisions}**"
    );
    println!();

    println!("## always-emitted body seeds");
    println!();
    println!(
        "- callees seeded from a decorator definition or a global initializer: \
         **{always_emitted_seeds}**"
    );
    println!();

    println!("## Lua vs JS target views");
    println!();
    println!("**IDENTITY** -- an instance reached by both targets keys on");
    println!("`(FuncRef, args)`; its symbol must not depend on the target.");
    println!();
    println!("- instances reached by BOTH views: **{shared_instances}**");
    println!(
        "- ...whose symbol DIFFERS between the two (the failure): **{identity_conflicts}**"
    );
    for d in &identity_disagreements {
        println!("  - `{d}`");
    }
    println!();
    println!("**REACHABILITY** -- which bodies each target reaches. Expected to");
    println!("differ (plan S 0d: \"roots and foreign boundaries may differ\").");
    println!();
    println!(
        "- symbols reached only on Lua: **{lua_only_total}** -- a call resolving to an \
         `inline(js)` sibling has no Rock body, so the whole subtree below it \
         disappears from the JS walk"
    );
    println!("- symbols reached only on JS: **{js_only_total}**");
    println!(
        "- instances whose callee resolved to a different `FuncRef` on JS \
         (the agreement is vacuous if this is 0): **{variant_switches}**"
    );
    println!("- boundaries invalid on the JS target: **{js_invalid}**");
    println!(
        "- names whose same-signature backend variants do NOT share a mangling: **{}**",
        divergent_groups.len()
    );
    for d in divergent_groups.iter().take(10) {
        println!("  - `{d}`");
    }
    println!();

    println!("## capability projections");
    println!();
    println!(
        "| target | instances | boundaries avail./total | unsupported | deferred obligations |"
    );
    println!("|---|---:|---:|---|---:|");
    for (name, p) in &proj_rows {
        let unsup = if p.unsupported_by_class.is_empty() {
            "--".to_string()
        } else {
            p.unsupported_by_class
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!(
            "| `{name}` | {} | {}/{} | {unsup} | {} |",
            p.instances, p.boundaries_available, p.boundaries_total, p.deferred.len()
        );
    }
    println!();
    let identity_stable = proj_digests.values().all(|v| !v.is_empty())
        && {
            let first = proj_digests.values().next().cloned().unwrap_or_default();
            proj_digests.values().all(|v| *v == first)
        };
    println!(
        "**logical instance identity byte-stable across all {} projections: {}** \
         (plan S 20g.1) -- the per-project manifest digests are equal for every target.",
        proj_rows.len(),
        if identity_stable { "YES" } else { "**NO**" }
    );
    println!();
    for (name, p) in &proj_rows {
        if p.deferred.is_empty() {
            continue;
        }
        println!("`{name}` defers:");
        for d in &p.deferred {
            println!("- {d}");
        }
        println!();
    }

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

/// Everything one project contributes. A named struct rather than the tuple
/// this grew out of: at six members `row.4.missing` had stopped saying anything.
struct OneProject {
    row: ProjectRow,
    hist: BTreeMap<&'static str, usize>,
    reasons: BTreeMap<String, usize>,
    examples: BTreeMap<String, Vec<String>>,
    census: instance_graph::RecursionCensus,
    uninstantiated_root_names: BTreeMap<String, instance_graph::RootHole>,
    closure: skull::instance_graph::Closure,
    synthesized_examined: usize,
    /// Storage-query failures by `MonoTyReason`, kept apart from the rejection
    /// KIND histogram: a query that failed on an unbound generic is filed under
    /// `unconstrained-type-var` there, so counting only
    /// `concrete-type-unavailable` would under-report the storage population.
    storage_failures: BTreeMap<&'static str, usize>,
    inline_dep_names: std::collections::BTreeSet<String>,
    span_census: instance_graph::SpanCensus,
    rejections: BTreeMap<&'static str, usize>,
    rejection_examples: Vec<String>,
    reconciliation: instance_graph::Reconciliation,
    /// Boundary counts by class, for the graph's own (Lua) target.
    boundaries: BTreeMap<&'static str, usize>,
    /// Boundaries the target cannot serve -- plan S 10's `Invalid boundary`.
    boundaries_invalid: usize,
    erased_entries: std::collections::BTreeSet<String>,
    coroutine_helpers: usize,
    coroutine_helper_collisions: usize,
    always_emitted_seeds: usize,
    /// Instances both target views reached, and how many of those took a
    /// different SYMBOL on the two targets. The identity property.
    shared_instances: usize,
    identity_conflicts: usize,
    /// Callees that resolved to a DIFFERENT `FuncRef` on JS than on Lua. If
    /// this is 0 the agreement above is vacuous, so it is reported beside it.
    target_variant_switches: usize,
    /// Symbols present on Lua and absent on JS. Non-zero is the FINDING: a
    /// logical instance was renamed or lost by changing target.
    lua_only: usize,
    /// Symbols present only on JS -- availability, not identity.
    js_only: usize,
    /// Names whose same-signature backend variants do not share a mangling.
    divergent_variant_groups: std::collections::BTreeSet<String>,
    js_boundaries_invalid: usize,
    projections: Vec<skull::target_projection::Projection>,
}

/// A span's file, relative to the analyzed root. An absolute path varies by
/// checkout and would make the report non-comparable across machines; the root
/// is the one the census was invoked on, never a checkout-directory name.
/// Pseudo-files (`<bone-stdlib:...>`) have no prefix to strip and pass through.
fn relativize(file: &str, root: &Path) -> String {
    Path::new(file).strip_prefix(root).map(|r| r.display().to_string()).unwrap_or_else(|_| file.to_string())
}

fn one_project(
    dir: &PathBuf,
    name: &str,
    root: &Path,
    dump: Option<(&str, &str)>,
) -> Result<OneProject> {
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
    let g2 = instance_graph::build(&ws, roots.clone(), true);
    let text = instance_graph::render_manifest(&ws, &g);
    let stable = text == instance_graph::render_manifest(&ws, &g2);

    // The JS view. Roots are the same predicate, but every callee runs through
    // `backend_variant`, so a same-name `inline(lua)`/`inline(js)` pair resolves
    // to a DIFFERENT `FuncRef` here. Logical identity must survive that; the
    // switch count is reported beside the verdict so an agreement that holds
    // only because nothing differed cannot read as the property holding.
    let js = instance_graph::build_for_target(
        &ws,
        roots,
        false,
        bone_semantic::hir::FnBackend::Js,
    );
    // Set difference, not text equality: the question is whether a logical
    // instance KEEPS its symbol, and `lua_only` is the direction that answers
    // it. A symbol present on Lua and absent on JS means the same logical
    // function was renamed or lost; extra JS symbols are availability.
    let js_text = instance_graph::render_manifest(&ws, &js);
    let lua_syms: std::collections::BTreeSet<&str> = text.lines().collect();
    let js_syms: std::collections::BTreeSet<&str> = js_text.lines().collect();
    let lua_only = lua_syms.difference(&js_syms).count();
    let js_only = js_syms.difference(&lua_syms).count();
    // The IDENTITY property, stated directly: an instance reached by both
    // targets keys on `(FuncRef, args)`, and its identity must not depend on
    // the target. Set difference alone cannot test this -- a symbol missing on
    // JS is usually an unreached body, which is availability.
    //
    // Keyed and compared on the CANONICAL encoding throughout. Keying on the
    // display rendering would let two different instances share a key, and a
    // conflict between them would then be reported as agreement.
    let mut lua_by_key: BTreeMap<(u32, u32, Vec<String>), String> = BTreeMap::new();
    for i in &g.instances {
        let args: Vec<String> = i.args.iter().map(|a| g.arena.canonical(*a)).collect();
        let key = (i.func.module.raw(), i.func.func.into_raw().into_u32(), args);
        lua_by_key.insert(key, g.identity(&ws, i));
    }
    let mut identity_conflicts = 0usize;
    let mut shared_instances = 0usize;
    for i in &js.instances {
        let args: Vec<String> = i.args.iter().map(|a| js.arena.canonical(*a)).collect();
        let key = (i.func.module.raw(), i.func.func.into_raw().into_u32(), args);
        if let Some(lua_ident) = lua_by_key.get(&key) {
            shared_instances += 1;
            if *lua_ident != js.identity(&ws, i) {
                identity_conflicts += 1;
            }
        }
    }

    // Variant groups whose members do NOT share a mangling. `@nomangle` and
    // `extern` are exemptions carried by a DECLARATION, and the backend
    // variants are different declarations -- so a group can hold one bare and
    // one mangled symbol for what is logically one function.
    // Grouped by PARAM SIGNATURE first -- the key `collapse_backend_variants`
    // uses. Two genuine overloads mangle differently by design; only a
    // same-signature pair is a backend-variant group, and only there is a
    // mangling difference a symbol divergence.
    let mut divergent_variant_groups: std::collections::BTreeSet<String> = Default::default();
    for (name, cands) in ws.functions_by_name.iter() {
        if cands.len() < 2 {
            continue;
        }
        // Keyed on the param signature AND requiring distinct BACKENDS. Without
        // the backend condition this fires on same-backend overloads that
        // happen to render one signature (`__op_eq` in the bone stdlib has six
        // candidates, all Lua) -- which is a grouping artifact, not a target
        // divergence, and would have been reported as one.
        type SigGroup = (std::collections::BTreeSet<String>, std::collections::BTreeSet<String>);
        let mut by_sig: BTreeMap<Vec<String>, SigGroup> = BTreeMap::new();
        for &(m, fi) in cands.iter() {
            let f = &ws.module(m).functions[fi];
            let sig: Vec<String> =
                f.param_decls.iter().map(|p| ws.render(None, p.ty)).collect();
            let e = by_sig.entry(sig).or_default();
            e.0.insert(skull_mir::mangle::mangle_fn(&ws, f));
            e.1.insert(format!("{:?}", f.backend));
        }
        for (_, (manglings, _)) in
            by_sig.iter().filter(|(_, (m, backends))| m.len() > 1 && backends.len() > 1)
        {
            let where_: Vec<String> = cands
                .iter()
                .map(|&(m, fi)| {
                    let f = &ws.module(m).functions[fi];
                    format!("{}[{:?}]", ws.module(m).file, f.backend)
                })
                .collect();
            divergent_variant_groups.insert(format!(
                "{name}: {} @ {}",
                manglings.iter().cloned().collect::<Vec<_>>().join(" vs "),
                where_.join(", ")
            ));
        }
    }
    let lua_refs: std::collections::BTreeSet<(u32, u32)> = g
        .instances
        .iter()
        .map(|i| (i.func.module.raw(), i.func.func.into_raw().into_u32()))
        .collect();
    let target_variant_switches = js
        .instances
        .iter()
        .filter(|i| !lua_refs.contains(&(i.func.module.raw(), i.func.func.into_raw().into_u32())))
        .count();
    let js_boundaries_invalid =
        js.boundaries.values().filter(|b| b.invalid_on_target).count();

    // Capability projections. They read `g` -- one built graph, four views --
    // which is what makes "the projection did not change the key" checkable.
    let projections: Vec<skull::target_projection::Projection> =
        skull::target_projection::ALL
            .iter()
            .map(|spec| skull::target_projection::project(&ws, &g, *spec))
            .collect();

    if let Some((path, name)) = dump {
        let stem = format!("{path}/{}", name.replace('/', "_"));
        std::fs::write(format!("{stem}.manifest"), &text)
            .map_err(|e| anyhow::anyhow!("writing {stem}.manifest: {e}"))?;
        // The JS view beside it: a target disagreement is only diagnosable by
        // diffing the two manifests, and re-deriving one by hand is how a
        // difference gets attributed to the wrong cause.
        std::fs::write(
            format!("{stem}.js.manifest"),
            instance_graph::render_manifest(&ws, &js),
        )
        .map_err(|e| anyhow::anyhow!("writing {stem}.js.manifest: {e}"))?;
    }

    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    for u in &g.unresolved {
        *reasons.entry(u.reason.to_string()).or_insert(0) += 1;
    }
    // A few examples per reason: one can be an accident, three show a pattern.
    let mut examples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for u in &g.unresolved {
        let e = examples.entry(u.reason.to_string()).or_default();
        let f = &ws.module(u.callee.module).functions[u.callee.func];
        let file = relativize(u.span.file.as_str(), root);
        let at = format!("`{}` at {}:{}", f.name, file, u.span.line_range.0 + 1);
        if e.len() < 3 && !e.contains(&at) {
            e.push(at);
        }
    }
    let mut boundaries: BTreeMap<&'static str, usize> = BTreeMap::new();
    for b in g.boundaries.values() {
        *boundaries.entry(b.class.name()).or_insert(0) += 1;
    }
    let mut rejections: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut rejection_examples: Vec<String> = Vec::new();
    for r in &g.rejections {
        *rejections.entry(r.kind.name()).or_insert(0) += 1;
        let file = relativize(r.span.file.as_str(), root);
        if rejection_examples.len() < 3 {
            rejection_examples.push(format!(
                "`{}` at {}:{} -- {}",
                r.kind.name(),
                file,
                r.span.line_range.0 + 1,
                r.message
            ));
        }
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
    Ok(OneProject {
        row,
        hist: hist_of(&g),
        reasons,
        examples,
        census,
        uninstantiated_root_names: g.uninstantiated_roots.clone(),
        closure: g.closure(&ws),
        synthesized_examined: g.synthesized_examined,
        storage_failures: {
            let mut m: BTreeMap<&'static str, usize> = BTreeMap::new();
            for r in &g.storage_failures {
                *m.entry(r.bucket()).or_insert(0) += 1;
            }
            m
        },
        inline_dep_names: g.inline_body_deps.values().flatten().cloned().collect(),
        span_census: g.span_census(),
        rejections,
        rejection_examples,
        reconciliation: rec,
        boundaries,
        boundaries_invalid: g.boundaries.values().filter(|b| b.invalid_on_target).count(),
        erased_entries: g.erased_entries.clone(),
        coroutine_helpers: g.coroutine_helpers.len(),
        coroutine_helper_collisions: g.colliding_coroutine_helpers().len(),
        always_emitted_seeds: g.always_emitted_seeds,
        shared_instances,
        identity_conflicts,
        target_variant_switches,
        lua_only,
        js_only,
        divergent_variant_groups,
        js_boundaries_invalid,
        projections,
    })
}

fn hist_of(g: &InstanceGraph) -> BTreeMap<&'static str, usize> {
    g.edge_histogram()
}
