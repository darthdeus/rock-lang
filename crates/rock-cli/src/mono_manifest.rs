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

#[derive(Default)]
struct FindingSummary {
    total: usize,
    examples: Vec<String>,
}

impl FindingSummary {
    fn record(&mut self, example: String) {
        self.total += 1;
        if self.examples.len() < 10 {
            self.examples.push(example);
        }
    }

    fn absorb(&mut self, other: Self) {
        self.total += other.total;
        for example in other.examples {
            if self.examples.len() < 10 {
                self.examples.push(example);
            }
        }
    }
}

/// Per-artifact totals across the whole corpus. The deferred list is a property
/// of the TARGET, not of a project, so it is copied rather than summed.
struct ArtifactTotals {
    /// Rock instances the artifact lowers, boundaries it names, their sum, and
    /// the SHARED population they were taken from -- a retained count with no
    /// denominator cannot say whether an artifact dropped one declaration or
    /// all of them.
    instances: usize,
    boundaries: usize,
    retained: usize,
    shared: usize,
    excluded: usize,
    excluded_by_cause: BTreeMap<&'static str, usize>,
    realized_by_kind: BTreeMap<&'static str, usize>,
    symbol_collisions: usize,
    inline_deps_examined: usize,
    inline_deps_unresolved: usize,
    inline_deps_boundary: usize,
    inline_deps_rock: usize,
    roots: usize,
    sites_walked: usize,
    severed: usize,
    severed_by_cause: BTreeMap<&'static str, usize>,
    /// Foreign bodies the artifact emits, how many publish more than one
    /// result lane, and the widest wire its scratch bank must cover.
    foreign_bodies: usize,
    multi_lane_publishers: usize,
    widest_result_lanes: usize,
    /// Retained instances with NO substitution record in the shared graph. An
    /// invariant finding, and the ONLY way a retained foreign body goes without
    /// a stated wire: the width is built from the instance's substitution, so
    /// being generic is not a reason to be unanswered.
    missing_substitutions: usize,
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
    let mut excused_roots: BTreeMap<&'static str, std::collections::BTreeSet<String>> =
        BTreeMap::new();
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
    let mut erased_entries: std::collections::BTreeSet<String> = Default::default();
    let mut coroutine_helpers = 0usize;
    let mut coroutine_collisions = 0usize;
    let mut always_emitted_seeds = 0usize;
    let mut subst_disagreements = 0usize;
    let mut subst_disagreeing_instances = 0usize;
    let mut no_subst: std::collections::BTreeSet<String> = Default::default();
    let mut subst_examples: Vec<String> = Vec::new();
    let mut substitutions_recorded = 0usize;
    let mut subst_comparisons = 0usize;
    let mut bound_tree_disagreements = 0usize;
    let mut bound_tree_comparisons = 0usize;
    let mut bound_trees_recorded = 0usize;
    let mut site_join = skull::site_join::SiteJoinCensus::default();
    let mut body_count = 0usize;
    let mut body_expr_examined = 0usize;
    let mut body_binding_examined = 0usize;
    let mut body_free: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut body_free_by_slot: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut body_free_spanless = 0usize;
    let mut body_gaps: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut body_free_examples: Vec<String> = Vec::new();
    let mut body_unexpanded = 0usize;
    let mut body_generics_bound = 0usize;
    let mut body_generic_bodies = 0usize;
    let mut body_untyped_by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut body_untyped_examples: Vec<String> = Vec::new();
    let mut body_untyped_exprs = 0usize;
    let mut body_untyped_bindings = 0usize;
    let mut body_expr_slots = 0usize;
    let mut body_binding_slots = 0usize;
    let mut sites_examined = 0usize;
    let mut sites_edges = 0usize;
    let mut sites_resolved = 0usize;
    let mut callee_direct = 0usize;
    let mut callee_fn_values = 0usize;
    let mut callee_synthesized = 0usize;
    let mut callee_iterator_ops: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut callee_rock = 0usize;
    let mut callee_boundary = 0usize;
    let mut callee_holes: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    let mut callee_hole_examples: Vec<String> = Vec::new();
    let mut callee_unknown_rock = 0usize;
    let mut callee_duplicate_sites = 0usize;
    let mut untyped_as_callee_key: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut synthesized_approximated = 0usize;
    let mut span_edges: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    let mut span_rejections: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    let mut span_unresolved: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    let mut span_always_emitted: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    let mut shader_packaging_pairs = 0usize;
    let mut shader_packaging_disagreements = FindingSummary::default();
    let mut sibling_selections = 0usize;
    let mut boundary_pairs = 0usize;
    let mut boundary_divergences = 0usize;
    let mut divergent_groups: std::collections::BTreeSet<String> = Default::default();
    let mut art_rows: BTreeMap<&'static str, ArtifactTotals> = BTreeMap::new();
    // Instances one artifact retained that the shared graph does not hold. A
    // view may drop and reject; it may never discover.
    let mut art_invented = 0usize;
    let mut art_symbol_examples: Vec<String> = Vec::new();
    let mut art_dep_examples: Vec<String> = Vec::new();
    let mut art_shared_pairs = 0usize;
    let mut art_spelling_conflicts = 0usize;
    let mut art_spelling_agreements = 0usize;
    let mut art_shared_collisions = 0usize;
    let mut art_id_collisions = 0usize;
    let mut art_id_collisions_by_name = 0usize;
    let mut art_severed_examples: Vec<String> = Vec::new();
    let mut art_severed_groups: BTreeMap<(&'static str, String, &'static str), (usize, usize)> =
        BTreeMap::new();
    let mut shader_totals = ShaderStats::default();

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
                for (reason, names) in p.excused_generic_roots {
                    excused_roots.entry(reason).or_default().extend(names);
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
                erased_entries.extend(p.erased_entries);
                coroutine_helpers += p.coroutine_helpers;
                coroutine_collisions += p.coroutine_helper_collisions;
                always_emitted_seeds += p.always_emitted_seeds;
                subst_disagreements += p.subst_disagreements;
                subst_disagreeing_instances += p.subst_disagreeing_instances;
                no_subst.extend(p.instances_without_substitution);
                substitutions_recorded += p.substitutions_recorded;
                subst_comparisons += p.subst_comparisons;
                bound_tree_disagreements += p.bound_tree_disagreements;
                bound_tree_comparisons += p.bound_tree_comparisons;
                bound_trees_recorded += p.bound_trees_recorded;
                for x in p.subst_examples {
                    if subst_examples.len() < 10 {
                        subst_examples.push(x);
                    }
                }
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
                site_join.merge(&p.site_join);
                body_count += p.body_census.bodies;
                body_expr_examined += p.body_census.expr_examined;
                body_binding_examined += p.body_census.binding_examined;
                body_free_spanless += p.body_census.spanless;
                for (k, v) in &p.body_census.free_by_reason {
                    *body_free.entry(*k).or_insert(0) += v;
                }
                for (k, v) in &p.body_census.free_by_slot {
                    *body_free_by_slot.entry(*k).or_insert(0) += v;
                }
                for (k, v) in &p.body_census.gaps {
                    *body_gaps.entry(*k).or_insert(0) += v;
                }
                body_unexpanded += p.bodies_missing_for_expanded;
                body_generics_bound += p.body_census.generics_bound;
                body_generic_bodies += p.body_census.generic_bodies;
                body_untyped_exprs += p.body_census.untyped_exprs;
                for (k, v) in &p.body_census.untyped_by_kind {
                    *body_untyped_by_kind.entry(*k).or_insert(0) += v;
                }
                for x in p.body_untyped_examples {
                    if body_untyped_examples.len() < 10 {
                        body_untyped_examples.push(x);
                    }
                }
                body_untyped_bindings += p.body_census.untyped_bindings;
                body_expr_slots += p.body_census.expr_slots;
                body_binding_slots += p.body_census.binding_slots;
                sites_examined += p.body_census.sites_examined;
                sites_edges += p.sites_edges;
                sites_resolved += p.sites_resolved;
                callee_direct += p.body_census.direct_calls;
                callee_fn_values += p.body_census.fn_values;
                callee_synthesized += p.body_census.synthesized_calls;
                for (k, v) in &p.body_census.iterator_ops {
                    *callee_iterator_ops.entry(*k).or_insert(0) += v;
                }
                callee_rock += p.body_census.rock_callees;
                callee_boundary += p.body_census.boundary_callees;
                for (k, (n, spanless)) in &p.body_census.callee_holes {
                    let e = callee_holes.entry(*k).or_insert((0, 0));
                    e.0 += n;
                    e.1 += spanless;
                }
                for x in p.callee_hole_examples {
                    if callee_hole_examples.len() < 10 {
                        callee_hole_examples.push(x);
                    }
                }
                callee_unknown_rock += p.body_census.unknown_rock_callees;
                callee_duplicate_sites += p.duplicate_sites;
                for (k, v) in &p.body_census.untyped_as_callee_key {
                    *untyped_as_callee_key.entry(*k).or_insert(0) += v;
                }
                synthesized_approximated += p.synthesized_approximated;
                for x in p.body_free_examples {
                    if body_free_examples.len() < 10 {
                        body_free_examples.push(x);
                    }
                }
                shader_packaging_pairs += p.shader_packaging_pairs;
                shader_packaging_disagreements.absorb(p.shader_packaging_disagreements);
                sibling_selections += p.artifact_sibling_selections;
                boundary_pairs += p.artifact_boundary_pairs;
                boundary_divergences += p.artifact_boundary_divergences;
                divergent_groups.extend(p.divergent_variant_groups);
                art_invented += p.artifact_invented;
                art_shared_pairs += p.artifact_shared_pairs;
                art_spelling_conflicts += p.artifact_spelling_conflicts;
                art_spelling_agreements += p.artifact_spelling_agreements;
                art_shared_collisions += p.shared_symbol_collisions;
                art_id_collisions += p.identity_collisions.0;
                art_id_collisions_by_name += p.identity_collisions.1;
                shader_totals.merge(&p.shader_stats);
                for (k, (sites, insts)) in p.artifact_severed_groups {
                    let e = art_severed_groups.entry(k).or_insert((0, 0));
                    e.0 += sites;
                    e.1 += insts;
                }
                for x in p.artifact_severed_examples {
                    if art_severed_examples.len() < 10 {
                        art_severed_examples.push(x);
                    }
                }
                for x in p.artifact_symbol_examples {
                    if art_symbol_examples.len() < 10 {
                        art_symbol_examples.push(x);
                    }
                }
                for x in p.artifact_dep_examples {
                    if art_dep_examples.len() < 10 {
                        art_dep_examples.push(x);
                    }
                }
                for (target, c) in p.artifacts {
                    // SUMMED across projects. `insert` here reported the last
                    // project's numbers under a whole-corpus heading.
                    let e = art_rows.entry(target).or_insert_with(|| ArtifactTotals {
                        instances: 0,
                        boundaries: 0,
                        retained: 0,
                        shared: 0,
                        excluded: 0,
                        excluded_by_cause: BTreeMap::new(),
                        realized_by_kind: BTreeMap::new(),
                        symbol_collisions: 0,
                        inline_deps_examined: 0,
                        inline_deps_unresolved: 0,
                        inline_deps_boundary: 0,
                        inline_deps_rock: 0,
                        roots: 0,
                        sites_walked: 0,
                        severed: 0,
                        severed_by_cause: BTreeMap::new(),
                        foreign_bodies: 0,
                        multi_lane_publishers: 0,
                        widest_result_lanes: 0,
                        missing_substitutions: 0,
                        deferred: c.deferred.clone(),
                    });
                    e.instances += c.instances;
                    e.boundaries += c.boundaries;
                    e.retained += c.retained;
                    e.shared += c.shared_instances;
                    e.excluded += c.excluded;
                    e.symbol_collisions += c.symbol_collisions;
                    e.inline_deps_examined += c.inline_deps_examined;
                    e.inline_deps_unresolved += c.inline_deps_unresolved;
                    e.inline_deps_boundary += c.inline_deps_boundary;
                    e.inline_deps_rock += c.inline_deps_rock;
                    e.roots += c.roots;
                    e.sites_walked += c.sites_walked;
                    e.severed += c.severed;
                    e.foreign_bodies += c.foreign_bodies;
                    e.multi_lane_publishers += c.multi_lane_publishers;
                    e.missing_substitutions += c.missing_substitutions;
                    e.widest_result_lanes = e.widest_result_lanes.max(c.widest_result_lanes);
                    for (k, v) in &c.severed_by_cause {
                        *e.severed_by_cause.entry(*k).or_insert(0) += v;
                    }
                    for (k, v) in &c.excluded_by_cause {
                        *e.excluded_by_cause.entry(*k).or_insert(0) += v;
                    }
                    for (k, v) in &c.realized_by_kind {
                        *e.realized_by_kind.entry(*k).or_insert(0) += v;
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
        "`uncovered bare-name obligations` (max over projects): **{uninst}**. Not \"no call \
         instantiates them\": a root listed above can have concrete instances from Rock call \
         sites, which discharge those call sites and not the bare-name entry opaque Lua text \
         can call."
    );
    // The EXCLUDED population, by reason. A narrowing whose rejects are not
    // reported is indistinguishable from a check that never fires.
    println!();
    let excused_total: usize = excused_roots.values().map(|n| n.len()).sum();
    println!(
        "generic emission roots carrying NO bare-name obligation ({excused_total}), by what \
         discharges them:"
    );
    println!();
    for (reason, names) in &excused_roots {
        let what = match *reason {
            "mangled" => "emitted symbol is mangled -- opaque text cannot spell it",
            "stdlib-funcref" => {
                "`stdlib_funcref` catalogue entry -- existence per target is rejected at \
                 compile time, not promised"
            }
            other => other,
        };
        println!("- **{reason}** ({}): {what}", names.len());
        for n in names.iter().take(8) {
            println!("  - `{n}`");
        }
        if names.len() > 8 {
            println!("  - ...and {} more", names.len() - 8);
        }
    }
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

    // The join instance-driven emission will consume: does every lowered call
    // instruction resolve, through its recorded site, to exactly one callee the
    // graph already decided? Reported with its denominator and with every
    // exclusion named -- an unstated exclusion is how a broken join looks green.
    {
        println!("## MIR call -> `ConcreteBody` join");
        println!();
        println!(
            "Phase: POST-TRANSFORM (`mir_emit::prepare_instance_body`), reachable blocks only \
             -- the body emission renders."
        );
        println!();
        println!("- instance bodies measured (the SPACE): **{}**", site_join.bodies_measured);
        if !site_join.bodies_skipped.is_empty() {
            println!();
            println!("| expanded instance not measured | count |");
            println!("|---|---:|");
            for (k, v) in &site_join.bodies_skipped {
                println!("| `{}` | {v} |", k.label());
            }
            println!();
        }
        println!(
            "- reachable call instructions examined (the DENOMINATOR): **{}**",
            site_join.call_insts
        );
        println!(
            "- joined to exactly one callee: **{}** ({} Rock, {} boundary); of those, **{}** are \
             SPLICED calls joined through the callee instance's own body",
            site_join.joined,
            site_join.joined_rock,
            site_join.joined_boundary,
            site_join.joined_spliced
        );
        println!(
            "- of the joined, **{}** had the resolved callee CHECKED against the FuncRef the call \
             targets; **{}** are indirect and name no declaration to check",
            site_join.joined_callee_verified, site_join.joined_callee_unverifiable
        );
        let unjoined_total = site_join.unjoined_total();
        println!("- unjoined: **{unjoined_total}**");
        if unjoined_total > 0 {
            println!();
            println!("| cause | count |");
            println!("|---|---:|");
            for (k, v) in &site_join.unjoined {
                println!("| `{}` | {v} |", k.label());
            }
            println!();
            println!("| unjoined by operation | count |");
            println!("|---|---:|");
            for (k, v) in &site_join.unjoined_by_op {
                println!("| `{k}` | {v} |");
            }
        }
        println!();
        println!(
            "accounting: {} joined + {unjoined_total} unjoined == {} examined: **{}**",
            site_join.joined,
            site_join.call_insts,
            if site_join.accounts() { "yes" } else { "NO" }
        );
        if !site_join.examples.is_empty() {
            println!();
            for e in &site_join.examples {
                println!("- {e}");
            }
        }
        println!();
    }

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

    println!("## foreign boundaries (shared graph)");
    println!();
    println!("| class | total across projects | max in one project |");
    println!("|---|---:|---:|");
    for (k, v) in &boundary_classes {
        println!("| `{k}` | {v} | {} |", boundary_max.get(k).unwrap_or(&0));
    }
    println!();
    println!(
        "- whether a target can SERVE one is an ARTIFACT question, reported per CALL as a \
         severed call below (plan S 10: a spanned rejection, never a stub)"
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

    println!("## instance substitutions (plan S 9 precondition)");
    println!();
    println!(
        "A `ConcreteBody`'s type tables are built under the instance's substitution, so"
    );
    println!("that substitution must be a FUNCTION OF THE INSTANCE. The `VarId` keys come");
    println!("from the CALLER's inference, so nothing structural forces two callers of one");
    println!("instance to agree.");
    println!();
    println!("- substitutions recorded: **{substitutions_recorded}**");
    println!(
        "- times the walk COMPARED a re-reached instance's substitution (the \
         DENOMINATOR -- an instance reached once never tests agreement): \
         **{subst_comparisons}**"
    );
    println!(
        "- DISAGREEING COMPARISONS (must be 0): **{subst_disagreements}**, over \
         **{subst_disagreeing_instances}** distinct instances"
    );
    println!(
        "- instances with NO recorded substitution (must be 0 -- a non-generic \
         instance's substitution is EMPTY, not absent): **{}**",
        no_subst.len()
    );
    for n in no_subst.iter().take(5) {
        println!("  - `{n}`");
    }
    for x in &subst_examples {
        println!("  - `{x}`");
    }
    println!("- concrete bounded-witness trees recorded: **{bound_trees_recorded}**");
    println!("- re-reached trees compared: **{bound_tree_comparisons}**");
    println!(
        "- bounded-witness tree disagreements (must be 0): **{bound_tree_disagreements}**"
    );
    println!();

    println!("## ConcreteBody type tables (plan S 9, S 15a)");
    println!();
    println!(
        "Every expanded instance's `expr_types`/`binding_types`, converted from the"
    );
    println!("inferred `TyId`s under THAT INSTANCE's substitution. \"No free generic types\"");
    println!("is a TOTALITY claim, not a structural one -- `MonoTy` has no generic");
    println!("constructor, so a slot that would hold one is a recorded failure instead.");
    println!();
    println!("- bodies built: **{body_count}**");
    println!(
        "- expanded instances with NO body built (must be 0): **{}**",
        body_unexpanded
    );
    for (k, v) in &body_gaps {
        println!("  - `{k}`: {v}");
    }
    println!(
        "- slots converted (the DENOMINATOR): **{body_expr_examined}** expr + \
         **{body_binding_examined}** binding"
    );
    println!(
        "- ...of those bodies, built under a NON-EMPTY substitution: **{body_generic_bodies}**"
    );
    println!(
        "- declared generics resolved THROUGH the substitution (the space the zero \
         below is measured over -- a body that never consults `subst` reports no \
         free types for the trivial reason): **{body_generics_bound}**"
    );
    println!(
        "- slots that stayed NON-CONCRETE (must be 0): **{}**",
        body_free.values().sum::<usize>()
    );
    for (k, v) in &body_free {
        println!("  - `{k}`: {v}");
    }
    for (k, v) in &body_free_by_slot {
        println!("  - slot `{k}`: {v}");
    }
    println!("- ...of those, with no span: **{body_free_spanless}**");
    println!(
        "- slots in the bodies' own arenas with NO inferred type, so the tables \
         never saw them (a free type cannot be reported for a slot that was never \
         examined): **{body_untyped_exprs}** expr of **{body_expr_slots}**, \
         **{body_untyped_bindings}** binding of **{body_binding_slots}**"
    );
    for (k, v) in &body_untyped_by_kind {
        println!("  - `{k}`: {v}");
    }
    for x in &body_untyped_examples {
        println!("  - {x}");
    }
    for x in &body_free_examples {
        println!("  - {x}");
    }
    println!();

    println!("## ConcreteBody callee tables (plan S 9)");
    println!();
    println!("`direct_calls` / `fn_values` / `iterator_ops` / `synthesized_calls`, each");
    println!("naming a `Callee::Rock(instance)` or a `Callee::Boundary(id)`. These are the");
    println!("WALK's own resolutions projected per table -- a second resolver could");
    println!("disagree with the graph, and correlating the two by span could not detect it.");
    println!();
    println!("- callee sites recorded (the DENOMINATOR): **{sites_examined}**");
    println!(
        "- ...accounted for as entries + holes: **{}** + **{}** = **{}**",
        callee_direct + callee_fn_values + callee_synthesized
            + callee_iterator_ops.values().sum::<usize>(),
        callee_holes.values().map(|(n, _)| n).sum::<usize>(),
        callee_direct
            + callee_fn_values
            + callee_synthesized
            + callee_iterator_ops.values().sum::<usize>()
            + callee_holes.values().map(|(n, _)| n).sum::<usize>()
    );
    println!(
        "- resolved sites vs EDGES (separate structures, one edge per resolved callee): \
         **{sites_resolved}** vs **{sites_edges}**"
    );
    println!("- sites recorded TWICE for one instance (must be 0): **{callee_duplicate_sites}**");
    println!("- `direct_calls`: **{callee_direct}**");
    println!("- `fn_values`: **{callee_fn_values}**");
    println!("- `synthesized_calls`: **{callee_synthesized}**");
    println!(
        "- `iterator_ops`: **{}**",
        callee_iterator_ops.values().sum::<usize>()
    );
    for (k, v) in &callee_iterator_ops {
        println!("  - `{k}`: {v}");
    }
    println!("- `Callee::Rock`: **{callee_rock}**, `Callee::Boundary`: **{callee_boundary}**");
    println!(
        "- a Rock callee naming an instance the graph does not have (must be 0): \
         **{callee_unknown_rock}**"
    );
    println!(
        "- sites with no callee, by reason (`n` / of those spanless): **{}**",
        callee_holes.values().map(|(n, _)| n).sum::<usize>()
    );
    for (k, (n, spanless)) in &callee_holes {
        println!("  - `{k}`: {n} / {spanless}");
    }
    for x in &callee_hole_examples {
        println!("  - {x}");
    }
    println!(
        "- synthesized sites that could NOT decide their exact entry, so more than one \
         name was kept: **{synthesized_approximated}**"
    );
    println!(
        "- UNTYPED expression slots that are also a callee-table key: **{}**",
        untyped_as_callee_key.values().sum::<usize>()
    );
    for (k, v) in &untyped_as_callee_key {
        println!("  - `{k}`: {v}");
    }
    println!(
        "  (zero means no table consumes an expression's own type: a direct call reads"
    );
    println!(
        "  `resolved_overload_calls`, an iterator op reads `IterWitness` /"
    );
    println!(
        "  `for_iter_protocol` / `bound_call_trees`, and a synthesized call reads its"
    );
    println!("  RECEIVER's type, whose failures are the storage failures above.)");
    println!();

    println!("## always-emitted body seeds");
    println!();
    println!(
        "- callees seeded from a decorator definition or a global initializer: \
         **{always_emitted_seeds}**"
    );
    println!();

    println!("## one graph, many realizations");
    println!();
    println!("There is ONE target-neutral graph. Sibling selection happens in");
    println!("`artifact::realize_decl`, so a shared `Instance` realizes to a different");
    println!("DECLARATION -- and a different `BoundaryId` -- per artifact while remaining");
    println!("one logical identity. Identity is that shared `Instance`, not a comparison");
    println!("between two graphs that happened to agree.");
    println!();
    println!(
        "- retained instances whose artifact declaration is NOT the shared representative \
         (every agreement below is vacuous if this is 0): **{sibling_selections}**"
    );
    println!(
        "- instances two artifacts both retain as a BOUNDARY: **{boundary_pairs}**, of which \
         **{boundary_divergences}** carry different `BoundaryId`s -- one logical identity, \
         two target contracts"
    );
    println!(
        "- shader-packaging pairs sharing one platform + execution target: \
         **{shader_packaging_pairs}**, of which \
         **{}** decide anything differently -- plan S 20a's separation, measured \
         rather than declared. The pair count is the denominator; shader \
         packaging cannot reach CPU realization.",
        shader_packaging_disagreements.total
    );
    for d in &shader_packaging_disagreements.examples {
        println!("  - {d}");
    }
    println!(
        "- names whose same-signature backend variants do NOT share a mangling: **{}**",
        divergent_groups.len()
    );
    for d in divergent_groups.iter().take(10) {
        println!("  - `{d}`");
    }
    println!();

    println!("## artifact views (plan S 9, S 11)");
    println!();
    println!("Each target's REALIZATION of the ONE shared graph: which instances it reaches");
    println!("from its own roots, how each is supplied, and what it spells them. Retention is");
    println!("a WALK, not a declaration filter -- `unreachable` and the unrealizable causes");
    println!("are separate populations, and a retained caller whose callee is unrealizable is");
    println!("a SEVERED call, which a filter cannot produce because it never reads an edge.");
    println!();
    println!(
        "| artifact | rock + boundary = retained / shared | roots | sites walked | severed | excluded | symbol collisions | deps unres./exam. |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    for (name, a) in &art_rows {
        println!(
            "| `{name}` | {} + {} = {}/{} | {} | {} | {} | {} | {} | {}/{} |",
            a.instances,
            a.boundaries,
            a.retained,
            a.shared,
            a.roots,
            a.sites_walked,
            a.severed,
            a.excluded,
            a.symbol_collisions,
            a.inline_deps_unresolved,
            a.inline_deps_examined
        );
    }
    println!();
    println!(
        "- instances an artifact realized that the SHARED graph does NOT hold \
         (must be 0): **{art_invented}**"
    );
    println!(
        "- instances the shared graph's IDENTITY merges (must be 0 -- every consumer \
         keys, sorts and deduplicates on it): **{art_id_collisions}**, against \
         **{art_id_collisions_by_name}** that the NAME-keyed identity it replaced \
         merged. The first is 0 by construction now; the second is what that \
         construction bought."
    );
    println!(
        "- inline-body dependencies terminating in a `RealizedCallee::Boundary` / \
         `RealizedCallee::Rock`: **{}** / **{}**",
        art_rows.values().map(|a| a.inline_deps_boundary).sum::<usize>(),
        art_rows.values().map(|a| a.inline_deps_rock).sum::<usize>()
    );
    println!();
    println!("**Identity across artifacts** (plan S 20g.1) is the shared `Instance`, NOT the");
    println!("spelling: plan S 0h puts the emitted spelling under the ARTIFACT, one symbol per");
    println!("`(artifact, instance)`, and two artifacts may legally spell one instance");
    println!("differently. Totality and injectivity are asserted INSIDE each artifact; what");
    println!("is compared across them is the instance.");
    println!();
    println!(
        "- `(artifact, artifact, instance)` triples where BOTH artifacts retain the \
         instance: **{art_shared_pairs}**"
    );
    println!(
        "- ...of those, spelled the same by both / spelled DIFFERENTLY: **{}** / \
         **{art_spelling_conflicts}**",
        art_spelling_agreements
    );
    println!(
        "  Neither number is a pass or a fail. The default planner is target-neutral, so"
    );
    println!(
        "  these artifacts agree; a renaming planner would make the second number the"
    );
    println!(
        "  whole population and change nothing about identity."
    );
    println!(
        "  `artifact_view::cross_artifact_identity_is_the_instance_and_not_the_spelling`"
    );
    println!("  uses one, so this agreement cannot be mistaken for the property.");
    println!();
    for (name, a) in &art_rows {
        println!("`{name}` realizes:");
        for (k, v) in &a.realized_by_kind {
            println!("- `{k}`: {v}");
        }
        if !a.excluded_by_cause.is_empty() {
            println!("...and excludes:");
            for (k, v) in &a.excluded_by_cause {
                println!("- `{k}`: {v}");
            }
        }
        for d in &a.deferred {
            println!("- DEFERRED: {d}");
        }
        println!();
    }
    // An empty axis NAMES its fixture rather than claiming coverage. Two of the
    // four discriminants have no corpus population, and a zero over zero is not
    // evidence that the arm works.
    let empty_axes: Vec<(&str, usize, &str)> = vec![
        (
            "native-mapping-missing",
            art_rows
                .values()
                .map(|a| a.excluded_by_cause.get("native-mapping-missing").copied().unwrap_or(0))
                .sum(),
            "`@native_required` intrinsics in this workspace all carry an `inline(lua)` \
             body, so on luajit they MAP and on every other artifact they are \
             foreign-to-artifact before the mapping is asked. Fixture: \
             `native_required_availability_comes_from_the_artifact_mapping`",
        ),
        (
            "symbol collisions",
            art_rows.values().map(|a| a.symbol_collisions).sum(),
            "the SHARED graph no longer holds two instances that encode alike either \
             (see the shared-collision line above), so this is not being bought by \
             retention dropping a duplicate. Fixture: \
             `an_artifact_symbol_is_total_and_injective_over_what_it_retains`",
        ),
        (
            "unreachable",
            art_rows
                .values()
                .map(|a| a.excluded_by_cause.get("unreachable").copied().unwrap_or(0))
                .sum(),
            "production's root predicate makes every non-library declaration an \
             emission root, so nothing REALIZABLE is unreachable under it -- the \
             population exists only when an artifact narrows its roots, which plan \
             S 11 permits. Fixture: \
             `retention_is_reachability_not_a_declaration_filter`",
        ),
        (
            "unresolved inline deps",
            art_rows.values().map(|a| a.inline_deps_unresolved).sum(),
            "measured over 26 examined dependencies on luajit and 0 on the other \
             three, whose opaque OWNERS are themselves excluded. Fixture: \
             `inline_body_dependencies_resolve_to_targets_in_the_artifact`",
        ),
    ];
    println!(
        "- ...measured against **{art_shared_collisions}** symbols the SHARED graph \
         spells for more than one instance. Retention is what removes them, so this \
         is the space the zero above is measured over."
    );
    for (axis, n, fixture) in &empty_axes {
        if *n == 0 {
            println!("- `{axis}`: **0** corpus-wide -- {fixture}");
        } else {
            println!("- `{axis}`: **{n}**");
        }
    }
    println!();
    for x in &art_symbol_examples {
        println!("- symbol collision: {x}");
    }
    for x in &art_dep_examples {
        println!("- unresolved dependency: {x}");
    }
    for x in &art_severed_examples {
        println!("- severed call: {x}");
    }
    println!();
    println!("### foreign bodies and their PUBLISHER LANES (plan S 13b)");
    println!();
    println!(
        "The result wire of every foreign body an artifact emits, from the concrete result ABI \
         (`skull_mir::lower::BackendFnAbiPlan`, built under THAT INSTANCE's substitution) rather \
         than from the body text or from the declaration. On JS a multi-lane body publishes \
         through one final `return __rock_publish(..)` whose arity is this number; luajit carries \
         the same lanes on native multi-return and needs no publisher. Emission still asks the \
         DECLARATION -- it writes one shared body per declaration until the Stage-3 cutover -- so \
         this per-instance reading is the artifact view's, and the two agree wherever the \
         substitution is empty."
    );
    println!();
    for (name, a) in &art_rows {
        println!(
            "- `{name}`: **{}** foreign bodies with a stated wire, **{}** multi-lane publishers, \
             widest wire **{}**, **{}** retained instances with no substitution record at all \
             (must be 0 -- and the only way a wire goes unstated)",
            a.foreign_bodies,
            a.multi_lane_publishers,
            a.widest_result_lanes,
            a.missing_substitutions
        );
    }
    println!();
    println!("## shader artifact views (plan S 9, S 20a)");
    println!();
    println!(
        "The GPU half of the same artifacts, over the SAME graph: the `rock_shader` entries \
         production registers, the helpers they reach, and GLSL's own rejections over exactly \
         that retained set. The profile is the TYPED value on the shader axis; a target with \
         no shader axis packages none."
    );
    println!();
    println!(
        "- target/project pairs packaging a shader artifact: **{}**, without one: **{}**",
        shader_totals.targets_with_shader, shader_totals.targets_without_shader
    );
    println!(
        "- shader entries **{}**, helpers **{}**, inline-rendered builtins **{}**, over \
         **{}** call sites walked",
        shader_totals.entries,
        shader_totals.helpers,
        shader_totals.builtins,
        shader_totals.sites_walked
    );
    println!(
        "- shader PROGRAMS the GLSL pass (emission's own `check_entry_profile` / \
         `check_pipeline_profile`) actually ran over: \
         **{}** of **{}** registered, **{}** excluded; rejections: recursion **{}**, profile \
         **{}**; structural-vs-emitter disagreements: **{}**",
        shader_totals.entries_checked,
        shader_totals.programs,
        shader_totals.programs_unchecked,
        shader_totals.recursion_rejections,
        shader_totals.profile_rejections,
        shader_totals.disagreements
    );
    println!(
        "- retained shader instances with NO substitution record at all (an invariant finding, \
         must be 0 -- and the only thing that can leave a program unchecked, since a generic \
         instance is checked and emitted under its own instantiation): **{}**; artifacts \
         packaging a profile other than `Core330`: **{}**",
        shader_totals.missing_substitutions,
        shader_totals.non_core330
    );
    for x in &shader_totals.examples {
        println!("- shader rejection: {x}");
    }
    println!();
    println!("### severed calls, by callee and cause");
    println!();
    println!(
        "The per-SITE severed total is the COST of an artifact's missing bodies; grouping by \
         `(callee declaration, cause)` is the obligation list. Both denominators are reported \
         because either alone reads as the other."
    );
    println!();
    for target in
        art_rows.keys().copied().filter(|t| art_severed_groups.keys().any(|(a, ..)| a == t))
    {
        let mut rows: Vec<(&String, &'static str, usize, usize)> = art_severed_groups
            .iter()
            .filter(|((a, ..), _)| *a == target)
            .map(|((_, callee, cause), (sites, insts))| (callee, *cause, *sites, *insts))
            .collect();
        rows.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(b.0)));
        let sites: usize = rows.iter().map(|r| r.2).sum();
        let mut by_cause: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
        for r in &rows {
            let e = by_cause.entry(r.1).or_insert((0, 0));
            e.0 += 1;
            e.1 += r.2;
        }
        println!(
            "`{target}`: **{}** severed call sites over **{}** `(callee, cause)` groups",
            sites,
            rows.len()
        );
        for (cause, (decls, n)) in &by_cause {
            println!("  - `{cause}`: {decls} declarations, {n} sites");
        }
        for (callee, cause, n, insts) in rows.iter().take(15) {
            println!("  - {callee} `{cause}`: {n} sites, {insts} instances");
        }
        if rows.len() > 15 {
            println!("  - ...and {} further groups", rows.len() - 15);
        }
    }
    println!();
    if !art_symbol_examples.is_empty()
        || !art_dep_examples.is_empty()
        || !art_severed_examples.is_empty()
    {
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
    excused_generic_roots: BTreeMap<&'static str, std::collections::BTreeSet<String>>,
    closure: skull::instance_graph::Closure,
    synthesized_examined: usize,
    /// Storage-query failures by `MonoTyReason`, kept apart from the rejection
    /// KIND histogram: a query that failed on an unbound generic is filed under
    /// `unconstrained-type-var` there, so counting only
    /// `concrete-type-unavailable` would under-report the storage population.
    storage_failures: BTreeMap<&'static str, usize>,
    inline_dep_names: std::collections::BTreeSet<String>,
    span_census: instance_graph::SpanCensus,
    /// The MIR-call -> `ConcreteBody` join over this project's expanded set.
    site_join: skull::site_join::SiteJoinCensus,
    rejections: BTreeMap<&'static str, usize>,
    rejection_examples: Vec<String>,
    reconciliation: instance_graph::Reconciliation,
    /// Boundary counts by class, for the graph's own (Lua) target.
    boundaries: BTreeMap<&'static str, usize>,
    /// Boundaries the target cannot serve -- plan S 10's `Invalid boundary`.
    erased_entries: std::collections::BTreeSet<String>,
    coroutine_helpers: usize,
    coroutine_helper_collisions: usize,
    always_emitted_seeds: usize,
    subst_disagreements: usize,
    subst_disagreeing_instances: usize,
    instances_without_substitution: std::collections::BTreeSet<String>,
    subst_examples: Vec<String>,
    substitutions_recorded: usize,
    subst_comparisons: usize,
    bound_tree_disagreements: usize,
    bound_tree_comparisons: usize,
    bound_trees_recorded: usize,
    /// Instances both target views reached, and how many of those took a
    /// different SYMBOL on the two targets. The identity property.
    body_census: skull::concrete_body::BodyCensus,
    /// Expanded instances the table builder produced nothing for. Counted from
    /// the graph's own `expanded` set rather than from the gap list, so a body
    /// that was silently never attempted is still a miss.
    bodies_missing_for_expanded: usize,
    body_free_examples: Vec<String>,
    body_untyped_examples: Vec<String>,
    /// Resolved sites and edges, reported side by side. The walk pushes one
    /// edge per resolved callee, so these must agree; they are separate
    /// structures written by the same recorders, which is why the equality is
    /// evidence rather than a tautology.
    sites_resolved: usize,
    sites_edges: usize,
    duplicate_sites: usize,
    callee_hole_examples: Vec<String>,
    synthesized_approximated: usize,
    /// Retained instances whose ARTIFACT declaration is not the shared
    /// representative -- sibling selection actually happened. Every
    /// cross-artifact agreement below is vacuous if this is 0.
    artifact_sibling_selections: usize,
    /// Instances two artifacts BOTH retain as a boundary, and how many of those
    /// pairs carry different `BoundaryId`s. One shared logical identity
    /// realizing to two target contracts is the property; the pair count is the
    /// population it was measured over.
    artifact_boundary_pairs: usize,
    artifact_boundary_divergences: usize,
    /// Names whose same-signature backend variants do not share a mangling.
    divergent_variant_groups: std::collections::BTreeSet<String>,
    /// One census per artifact, keyed by target name.
    artifacts: Vec<(&'static str, skull::artifact::ArtifactCensus)>,
    /// Instances an artifact realized that the shared graph does not hold.
    artifact_invented: usize,
    /// Pairs that differ only in shader packaging, and any CPU decisions that
    /// nevertheless differ. The pair count is the denominator.
    shader_packaging_pairs: usize,
    shader_packaging_disagreements: FindingSummary,
    /// `(artifact, artifact, instance)` triples both artifacts retain, and how
    /// many of those the two spell differently. The identity property, over the
    /// population that can actually witness it.
    artifact_shared_pairs: usize,
    artifact_spelling_conflicts: usize,
    artifact_spelling_agreements: usize,
    artifact_symbol_examples: Vec<String>,
    artifact_dep_examples: Vec<String>,
    /// Symbols the SHARED graph spells for more than one instance -- the space
    /// each artifact's zero is measured against.
    shared_symbol_collisions: usize,
    /// `(instances merged by the declaration-keyed identity, by the name-keyed
    /// one it replaced)`. The first must be 0; the second is the magnitude of
    /// what the old key was hiding.
    identity_collisions: (usize, usize),
    artifact_severed_examples: Vec<String>,
    /// Severed calls grouped by `(artifact, callee declaration, cause)`. The
    /// per-site total is the COST of the artifact's missing bodies; this is the
    /// obligation list, which is what a worklist actually needs.
    artifact_severed_groups: Vec<((&'static str, String, &'static str), (usize, usize))>,
    shader_stats: ShaderStats,
}

/// The shader artifact's census, summed over targets and projects. Entries and
/// helpers are reported apart from the walk's site denominator: zero rejections
/// over zero sites walked is not a clean shader artifact, it is an absent one.
#[derive(Default, Clone)]
struct ShaderStats {
    targets_with_shader: usize,
    targets_without_shader: usize,
    entries: usize,
    helpers: usize,
    builtins: usize,
    sites_walked: usize,
    recursion_rejections: usize,
    profile_rejections: usize,
    entries_checked: usize,
    programs_unchecked: usize,
    programs: usize,
    missing_substitutions: usize,
    disagreements: usize,
    non_core330: usize,
    examples: Vec<String>,
}

impl ShaderStats {
    fn merge(&mut self, o: &ShaderStats) {
        self.targets_with_shader += o.targets_with_shader;
        self.targets_without_shader += o.targets_without_shader;
        self.entries += o.entries;
        self.helpers += o.helpers;
        self.builtins += o.builtins;
        self.sites_walked += o.sites_walked;
        self.recursion_rejections += o.recursion_rejections;
        self.profile_rejections += o.profile_rejections;
        self.entries_checked += o.entries_checked;
        self.programs_unchecked += o.programs_unchecked;
        self.programs += o.programs;
        self.missing_substitutions += o.missing_substitutions;
        self.disagreements += o.disagreements;
        self.non_core330 += o.non_core330;
        for x in &o.examples {
            if self.examples.len() < 10 {
                self.examples.push(x.clone());
            }
        }
    }
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

    let mut g = instance_graph::build(&ws, roots.clone(), false);
    // S 20g.4: perturbing discovery order may not change the manifest.
    let g2 = instance_graph::build(&ws, roots.clone(), true);
    let text = instance_graph::render_manifest(&ws, &g);
    let stable = text == instance_graph::render_manifest(&ws, &g2);

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

    // Artifact views. Every one realizes THE SAME `g`: the graph records bone's
    // collapsed representative, and each artifact selects the backend sibling
    // that serves it. There is no per-target graph to mismatch and no second
    // arena to compare across.
    //
    // The JS artifact gets the REAL backend oracle -- ONE seam, carrying
    // `skull_codegen_js::emit::js_supply` itself. Without it a
    // `@native_required` intrinsic would read as unmapped on JS for the
    // trivial reason that nothing was wired, and the corpus number would be
    // measuring this driver rather than the artifact.
    let js_supply = skull_codegen_js::JsBackendRealization;
    let realize = |spec: skull::target_projection::TargetSpec| {
            let req = skull::artifact::ArtifactRequest::new(spec, &roots);
            let req = match spec.execution.backend {
                Some(b @ bone_semantic::hir::FnBackend::Js) => req.with_mappings(
                    skull::artifact::ArtifactMappings::for_backend(b).with_supply(&js_supply),
                ),
                _ => req,
            };
            skull::artifact::realize(&ws, &g, &req)
        };
    let views: Vec<skull::artifact::ArtifactView> = skull::target_projection::ALL
        .iter()
        .map(|spec| realize(*spec))
        .collect();

    // The comparison changes only shader packaging. Platform capabilities may
    // legitimately change roots or boundary admission; the CPU ABI's shader
    // independence says nothing about that axis.
    let mut shader_packaging_pairs = 0usize;
    let mut shader_packaging_disagreements = FindingSummary::default();
    for a in &views {
        let paired_spec = skull::target_projection::TargetSpec {
            name: "shader-packaging-probe",
            shader: if a.spec.shader.is_some() {
                None
            } else {
                Some(skull::target_projection::GLSL_330)
            },
            ..a.spec
        };
        let b = realize(paired_spec);
        shader_packaging_pairs += 1;
        if !a.same_cpu_decisions(&b) {
            shader_packaging_disagreements.record(format!("`{}`", a.spec.name));
        }
    }
    let artifact_invented: usize =
        views.iter().map(|v| v.retained().filter(|i| !g.instances.contains(i)).count()).sum();
    // The NON-VACUITY number for every cross-artifact agreement below: retained
    // instances whose artifact declaration is not the shared representative, so
    // sibling selection actually happened somewhere. Zero here would mean the
    // agreements hold because nothing forked.
    let artifact_sibling_selections: usize = views
        .iter()
        .map(|v| v.realization.iter().filter(|(i, r)| r.decl != i.func).count())
        .sum();
    // One shared `Instance` realizing to DIFFERENT boundary identities. The
    // pair count is the denominator: a divergence count alone cannot say
    // whether any instance was retained as a boundary by two artifacts at all.
    let mut artifact_boundary_pairs = 0usize;
    let mut artifact_boundary_divergences = 0usize;
    for (ai, a) in views.iter().enumerate() {
        for b in views.iter().skip(ai + 1) {
            for (inst, ida) in &a.boundaries {
                let Some(idb) = b.boundaries.get(inst) else { continue };
                artifact_boundary_pairs += 1;
                artifact_boundary_divergences += usize::from(ida != idb);
            }
        }
    }
    let by_artifact: Vec<BTreeMap<String, String>> = views
        .iter()
        .map(|v| {
            v.retained()
                .map(|i| (g.identity(&ws, i), v.symbols[i].clone()))
                .collect::<BTreeMap<String, String>>()
        })
        .collect();
    let mut artifact_shared_pairs = 0usize;
    let mut artifact_spelling_conflicts = 0usize;
    for (ai, a) in by_artifact.iter().enumerate() {
        for b in by_artifact.iter().skip(ai + 1) {
            for id in a.keys() {
                if b.contains_key(id) {
                    artifact_shared_pairs += 1;
                }
            }
        }
    }
    // The IDENTITY property: a shared identity string is one instance. That is
    // an equality by construction here -- the map is keyed on it -- so what is
    // actually reported is the population, and the spelling comparison below is
    // kept SEPARATE because plan S 0h permits it to differ.
    let mut artifact_spelling_agreements = 0usize;
    for (ai, a) in by_artifact.iter().enumerate() {
        for b in by_artifact.iter().skip(ai + 1) {
            for (id, sa) in a {
                if let Some(sb) = b.get(id) {
                    if sa == sb {
                        artifact_spelling_agreements += 1;
                    } else {
                        artifact_spelling_conflicts += 1;
                    }
                }
            }
        }
    }
    let mut artifact_symbol_examples: Vec<String> = Vec::new();
    let mut artifact_dep_examples: Vec<String> = Vec::new();
    let mut artifact_severed_examples: Vec<String> = Vec::new();
    let mut artifact_severed_groups: Vec<((&'static str, String, &'static str), (usize, usize))> =
        Vec::new();
    for v in &views {
        for x in v.render_symbol_collisions(&ws).into_iter().take(2) {
            artifact_symbol_examples.push(format!("`{}` {x}", v.spec.name));
        }
        for x in v.render_unresolved_inline_deps(&ws).into_iter().take(2) {
            artifact_dep_examples.push(format!("`{}` {x}", v.spec.name));
        }
        for x in v.render_severed(&ws, &g).into_iter().take(2) {
            artifact_severed_examples.push(format!("`{}` {x}", v.spec.name));
        }
        // Keyed by the callee's NAME and backend rather than by its `FuncRef`:
        // the corpus is 26 separately typechecked workspaces, so an arena index
        // is not comparable across them and the same stdlib declaration must
        // aggregate.
        for grp in v.severed_groups() {
            let f = &ws.module(grp.callee.module).functions[grp.callee.func];
            artifact_severed_groups.push((
                (v.spec.name, format!("{} [{:?}]", f.name, f.backend), grp.cause.label()),
                (grp.sites, grp.instances.len()),
            ));
        }
    }
    // The CONTRAST for the artifacts' zero: how many symbols the SHARED graph
    // spells for more than one instance. A per-artifact zero measured against
    // an unstated space cannot say whether retention did anything.
    let mut shared_by_symbol: BTreeMap<String, usize> = BTreeMap::new();
    for i in &g.instances {
        *shared_by_symbol.entry(g.logical_symbol(&ws, i)).or_insert(0) += 1;
    }
    let shared_symbol_collisions =
        shared_by_symbol.values().filter(|n| **n > 1).count();
    let artifacts: Vec<(&'static str, skull::artifact::ArtifactCensus)> =
        views.iter().map(|v| (v.spec.name, v.census())).collect();

    // The SHADER half of the same artifacts, over the same graph: the
    // `rock_shader` entries production registers plus the helpers they reach.
    // Roots come from `skull_mir`'s own collector -- an input, exactly like the
    // CPU emission roots.
    let shader_programs: Vec<skull::shader_artifact::ShaderProgram> =
        skull_mir::lower::collect_shader_collection(&ws)
            .map(|c| {
                c.pairs
                    .into_iter()
                    .map(|p| skull::shader_artifact::ShaderProgram { vert: p.vert, frag: p.frag })
                    .collect()
            })
            .unwrap_or_default();
    let mut shader_stats = ShaderStats::default();
    for spec in skull::target_projection::ALL {
        let Some(sv) = skull::shader_artifact::realize_shader(
            &ws,
            &g,
            &skull::shader_artifact::ShaderArtifactRequest {
                spec: *spec,
                programs: &shader_programs,
            },
        ) else {
            shader_stats.targets_without_shader += 1;
            continue;
        };
        shader_stats.targets_with_shader += 1;
        shader_stats.entries += sv.roots.len();
        shader_stats.helpers += sv.helpers.len();
        shader_stats.builtins += sv.builtins.len();
        shader_stats.sites_walked += sv.sites_walked;
        shader_stats.entries_checked += sv.entries_checked;
        shader_stats.programs_unchecked += sv.programs_unchecked;
        shader_stats.programs += sv.programs;
        shader_stats.missing_substitutions += sv.missing_substitutions.len();
        shader_stats.disagreements += sv.disagreements.len();
        for x in sv.disagreements.iter().take(2) {
            shader_stats.examples.push(format!("`{}` DISAGREEMENT {x}", spec.name));
        }
        for r in &sv.rejections {
            match r.kind {
                skull::shader_artifact::ShaderRejectionKind::Recursion => {
                    shader_stats.recursion_rejections += 1
                }
                skull::shader_artifact::ShaderRejectionKind::Profile => {
                    shader_stats.profile_rejections += 1
                }
            }
        }
        for x in sv.render_rejections(&ws).into_iter().take(2) {
            shader_stats.examples.push(format!("`{}` {x}", spec.name));
        }
        if sv.profile != skull_codegen_glsl::GlslProfile::Core330 {
            shader_stats.non_core330 += 1;
        }
    }

    if let Some((path, name)) = dump {
        let stem = format!("{path}/{}", name.replace('/', "_"));
        std::fs::write(format!("{stem}.manifest"), &text)
            .map_err(|e| anyhow::anyhow!("writing {stem}.manifest: {e}"))?;
        // The synthesized-call name histogram. This is the artifact the
        // "exactly the entries lowering emits" claim is checked against: the
        // same names counted in the project's emitted Lua must agree, and a
        // hand count of either side is not reproducible.
        // Counted per SOURCE SITE -- `(function, expression, name)` -- not per
        // instance. Emission today produces one body per function, so two
        // instances of one generic body share one emitted call; counting
        // instances would report a difference that is the conditional
        // specializer's, not the attribution's.
        let mut seen: std::collections::BTreeSet<(u32, u32, u32, &'static str)> =
            Default::default();
        let mut synth: BTreeMap<&'static str, usize> = BTreeMap::new();
        for (inst, sites) in &g.call_sites {
            for site in sites.keys() {
                let skull::instance_graph::SiteOp::Synthesized(n) = site.op else { continue };
                let key = (
                    inst.func.module.raw(),
                    inst.func.func.into_raw().into_u32(),
                    site.expr.into_raw().into(),
                    n,
                );
                if seen.insert(key) {
                    *synth.entry(n).or_insert(0) += 1;
                }
            }
        }
        let mut synth_sites: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
        for sites in g.call_sites.values() {
            for (site, rec) in sites {
                let skull::instance_graph::SiteOp::Synthesized(n) = site.op else { continue };
                synth_sites.entry(n).or_default().push(format!(
                    "\t\t{}:{}",
                    relativize(rec.span.file.as_str(), root),
                    rec.span.line_range.0 + 1
                ));
            }
        }
        let synth_text: String = synth
            .iter()
            .map(|(n, c)| {
                let mut lines = synth_sites.remove(*n).unwrap_or_default();
                lines.sort();
                lines.dedup();
                format!("{c}\t{n}\n{}\n", lines.join("\n"))
            })
            .collect();
        std::fs::write(format!("{stem}.synth"), synth_text)
            .map_err(|e| anyhow::anyhow!("writing {stem}.synth: {e}"))?;
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

    // The ConcreteBody type tables, over the walk's OWN record of which bodies
    // it entered. Interned into `g.arena`, so a slot's type id means the same
    // thing as an instance argument's.
    let bodies = skull::concrete_body::build(&ws, &mut g);
    let body_census = bodies.census();
    // The MIR-call -> ConcreteBody join, over production's own expanded set.
    let site_join = {
        let scx = skull_mir::lower::ScalarCtx::new(&ws);
        skull::site_join::census(&ws, &scx, &g, &bodies)
    };
    let bodies_missing_for_expanded =
        g.expanded.iter().filter(|i| !bodies.bodies.contains_key(*i)).count();
    let body_free_examples: Vec<String> = bodies
        .free_types
        .iter()
        .take(3)
        .map(|(i, f)| {
            format!(
                "`{}` {} slot, `{}` at {}:{}",
                g.identity(&ws, i),
                f.slot.kind(),
                f.reason.bucket(),
                relativize(f.span.file.as_str(), root),
                f.span.line_range.0 + 1
            )
        })
        .collect();

    // One example per KIND, not the first three overall: the population is
    // dominated by one kind, so a flat head repeats it and says nothing about
    // the others.
    let mut seen_kinds: std::collections::BTreeSet<&'static str> = Default::default();
    let body_untyped_examples: Vec<String> = bodies
        .untyped
        .iter()
        .filter(|(_, u)| seen_kinds.insert(u.kind))
        .map(|(i, u)| {
            format!(
                "`{}` `{}` at {}:{}",
                g.identity(&ws, i),
                u.kind,
                relativize(u.span.file.as_str(), root),
                u.span.line_range.0 + 1
            )
        })
        .collect();

    let sites_resolved = g
        .call_sites
        .values()
        .flat_map(|m| m.values())
        .filter(|r| matches!(r.outcome, skull::instance_graph::SiteOutcome::Resolved(_)))
        .count();
    let callee_hole_examples: Vec<String> = bodies
        .callee_holes
        .iter()
        .take(3)
        .map(|h| {
            format!(
                "`{}` {} `{}` at {}:{}",
                g.identity(&ws, &h.instance),
                h.site.op.name(),
                h.reason,
                relativize(h.span.file.as_str(), root),
                h.span.line_range.0 + 1
            )
        })
        .collect();
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
        excused_generic_roots: g.excused_generic_roots.clone(),
        closure: g.closure(&ws),
        synthesized_examined: g.synthesized_examined,
        storage_failures: {
            let mut m: BTreeMap<&'static str, usize> = BTreeMap::new();
            for r in &g.storage_failures {
                *m.entry(r.bucket()).or_insert(0) += 1;
            }
            m
        },
        inline_dep_names: g.inline_body_deps.values().flatten().map(|s| (*s).to_string()).collect(),
        span_census: g.span_census(),
        rejections,
        rejection_examples,
        reconciliation: rec,
        boundaries,
        erased_entries: g.erased_entries.clone(),
        coroutine_helpers: g.coroutine_helpers.len(),
        coroutine_helper_collisions: g.colliding_coroutine_helpers().len(),
        always_emitted_seeds: g.always_emitted_seeds,
        subst_disagreements: g.subst_disagreements.len(),
        subst_disagreeing_instances: g
            .subst_disagreements
            .iter()
            .map(|d| g.identity(&ws, &d.instance))
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        instances_without_substitution: g.instances_without_substitution(&ws),
        substitutions_recorded: g.substitutions.len(),
        subst_comparisons: g.subst_comparisons,
        bound_tree_disagreements: g.bound_tree_disagreements.len(),
        bound_tree_comparisons: g.bound_tree_comparisons,
        bound_trees_recorded: g.bound_trees.len(),
        subst_examples: g
            .subst_disagreements
            .iter()
            .take(3)
            .map(|d| g.identity(&ws, &d.instance))
            .collect(),
        sites_resolved,
        sites_edges: g.edges.len(),
        duplicate_sites: g.duplicate_sites.len(),
        callee_hole_examples,
        synthesized_approximated: g.synthesized_approximated,
        body_census,
        site_join,
        bodies_missing_for_expanded,
        body_free_examples,
        body_untyped_examples,
        artifact_sibling_selections,
        artifact_boundary_pairs,
        artifact_boundary_divergences,
        divergent_variant_groups,
        artifacts,
        artifact_invented,
        shader_packaging_pairs,
        shader_packaging_disagreements,
        artifact_shared_pairs,
        artifact_spelling_conflicts,
        artifact_spelling_agreements,
        artifact_symbol_examples,
        artifact_dep_examples,
        shared_symbol_collisions,
        identity_collisions: g.colliding_identities(&ws),
        artifact_severed_examples,
        artifact_severed_groups,
        shader_stats,
    })
}

fn hist_of(g: &InstanceGraph) -> BTreeMap<&'static str, usize> {
    g.edge_histogram()
}

#[cfg(test)]
mod tests {
    use super::FindingSummary;

    #[test]
    fn finding_summary_counts_past_its_example_limit() {
        let mut findings = FindingSummary::default();
        for i in 0..6 {
            findings.record(format!("finding-{i}"));
        }
        let mut more = FindingSummary::default();
        for i in 6..12 {
            more.record(format!("finding-{i}"));
        }
        findings.absorb(more);
        assert_eq!(findings.total, 12);
        assert_eq!(findings.examples.len(), 10);
    }
}
