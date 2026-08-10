//! **The arena-independent canonical form of an [`InstanceGraph`]'s semantic
//! state**, and the only sound way to ask whether two graphs mean the same
//! thing.
//!
//! Field-by-field equality is INVALID the moment one graph came from a
//! persisted [`MonoArena`]. A persisted arena keeps types whose instances were
//! pruned and interns new ones in a different ORDER than a fresh build, so two
//! logically identical instances carry different [`MonoTyId`]s and every
//! id-bearing field compares unequal while nothing at all is wrong. Every id
//! here is rendered through [`MonoArena::canonical`], which is an identity
//! string rather than the pretty syntax [`InstanceGraph::symbol`] produces.
//!
//! **Two things are deliberately OUTSIDE equality.**
//!
//! - The arena's CONTENTS. It is monotonic by design and is not a function of
//!   the live graph: an entry whose last instance was pruned stays interned,
//!   which is exactly what keeps ids stable across edits.
//! - The OPERATIONAL counters -- `sites_attempted`, `subst_comparisons`,
//!   `bound_tree_comparisons`, `subst_compared`, `recursion_starts`,
//!   `synthesized_*`, `always_emitted_seeds`. They describe work PERFORMED, not
//!   the graph produced, and an updater that reaches the right graph by a
//!   different route legitimately differs on all of them. Assert those as their
//!   own census, never through this.
//!
//! Sections are named so a failure says WHICH fact moved. A section that is
//! absent here cannot fail an equality check, so
//! `crates/skull/tests/graph_snapshot.rs` mutates each one in turn and requires
//! the snapshot to notice -- an omitted field shows up as a mutation nothing
//! detects.

use std::collections::BTreeMap;

use bone_semantic::inference::Workspace;
use skull_mir::mangle::mangle_fn;

use crate::instance_graph::{Instance, InstanceGraph};

/// One graph's semantic state: section name -> sorted canonical lines.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CanonicalGraph(pub BTreeMap<&'static str, Vec<String>>);

impl CanonicalGraph {
    /// The sections that differ, each with its own line-level delta. Empty
    /// means the two graphs mean the same thing.
    pub fn diff(&self, other: &Self) -> Vec<String> {
        let mut out = Vec::new();
        let names: std::collections::BTreeSet<&'static str> =
            self.0.keys().chain(other.0.keys()).copied().collect();
        for name in names {
            let empty = Vec::new();
            let a = self.0.get(name).unwrap_or(&empty);
            let b = other.0.get(name).unwrap_or(&empty);
            if a == b {
                continue;
            }
            let only_a: Vec<&String> = a.iter().filter(|l| !b.contains(l)).collect();
            let only_b: Vec<&String> = b.iter().filter(|l| !a.contains(l)).collect();
            out.push(format!(
                "{name}: {} vs {} lines; -{} +{}\n  - {}\n  + {}",
                a.len(),
                b.len(),
                only_a.len(),
                only_b.len(),
                only_a.iter().take(3).map(|s| s.as_str()).collect::<Vec<_>>().join("\n  - "),
                only_b.iter().take(3).map(|s| s.as_str()).collect::<Vec<_>>().join("\n  + "),
            ));
        }
        out
    }
}

/// An instance's IDENTITY string: the mangled declaration plus its arguments'
/// canonical spellings. Not [`InstanceGraph::symbol`], which renders pretty
/// type syntax and is explicitly not injective.
pub fn instance_key(ws: &Workspace, g: &InstanceGraph, i: &Instance) -> String {
    let f = &ws.module(i.func.module).functions[i.func.func];
    let mut out = mangle_fn(ws, f);
    // The raw indices disambiguate two declarations that mangle alike; they are
    // deterministic for one source, which is all a build-to-build comparison
    // needs.
    out.push_str(&format!("#{}.{}", i.func.module.raw(), i.func.func.into_raw().into_u32()));
    if !i.args.is_empty() {
        let args: Vec<String> = i.args.iter().map(|a| g.arena.canonical(*a)).collect();
        out.push('<');
        out.push_str(&args.join(","));
        out.push('>');
    }
    out
}

fn decl_key(ws: &Workspace, d: skull_mir::mir::FuncRef) -> String {
    let f = &ws.module(d.module).functions[d.func];
    format!("{}#{}.{}", mangle_fn(ws, f), d.module.raw(), d.func.into_raw().into_u32())
}

/// [`CanonicalGraph`] for `g`.
pub fn canonical(ws: &Workspace, g: &InstanceGraph) -> CanonicalGraph {
    let mut out: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    let key = |i: &Instance| instance_key(ws, g, i);
    let mut put = |name: &'static str, mut lines: Vec<String>| {
        lines.sort();
        out.insert(name, lines);
    };

    put("instances", g.instances.iter().map(&key).collect());
    put("expanded", g.expanded.iter().map(&key).collect());
    put(
        "edges",
        g.edges.iter().map(|e| format!("{} -{:?}-> {}", key(&e.from), e.kind, key(&e.to))).collect(),
    );
    put(
        "unresolved",
        g.unresolved
            .iter()
            .map(|u| format!("{} -> {} [{}]", key(&u.from), decl_key(ws, u.callee), u.reason))
            .collect(),
    );
    put(
        "rejections",
        g.rejections
            .iter()
            .map(|r| format!("{:?} at {}: {}", r.kind, key(&r.at), r.message))
            .collect(),
    );
    put("depth_capped", g.depth_capped.iter().map(&key).collect());
    put("storage_failures", g.storage_failures.iter().map(|r| format!("{r:?}")).collect());
    put(
        "boundaries",
        g.boundaries.iter().map(|(k, b)| format!("{k} {:?} {}", b.class, b.symbol)).collect(),
    );
    put(
        "coroutine_helpers",
        g.coroutine_helpers
            .iter()
            .map(|c| format!("{} {} {}", c.owner_identity, c.suffix, c.symbol_today))
            .collect(),
    );
    put("erased_entries", g.erased_entries.iter().cloned().collect());
    put(
        "inline_body_deps",
        g.inline_body_deps
            .iter()
            .map(|(d, set)| {
                format!("{} -> {:?}", decl_key(ws, d.0), set.iter().collect::<Vec<_>>())
            })
            .collect(),
    );
    put(
        "substitutions",
        g.substitutions
            .iter()
            .map(|(i, s)| {
                let mut binds: Vec<String> =
                    s.iter().map(|(v, t)| format!("{v:?}={}", t.canonical())).collect();
                binds.sort();
                format!("{} :: {}", key(i), binds.join(","))
            })
            .collect(),
    );
    put(
        "bound_trees",
        g.bound_trees
            .iter()
            .map(|(i, t)| {
                let ns: Vec<String> = t
                    .next
                    .iter()
                    .map(|(idx, n)| format!("{idx}->{}", key(&n.callee)))
                    .collect();
                format!("{} :: {}", key(i), ns.join(","))
            })
            .collect(),
    );
    // The outcome carries an `Instance`, so it is rendered through `key` rather
    // than `Debug` -- a `{:?}` of it prints raw `MonoTyId`s and makes the whole
    // section arena-dependent, which is the exact failure this module exists to
    // prevent. The stability arm caught it.
    put(
        "call_sites",
        g.call_sites
            .iter()
            .flat_map(|(i, sites)| {
                sites
                    .iter()
                    .map(|(s, r)| {
                        let outcome = match &r.outcome {
                            crate::instance_graph::SiteOutcome::Resolved(t) => {
                                format!("resolved({})", key(t))
                            }
                            crate::instance_graph::SiteOutcome::Failed(why) => {
                                format!("failed({why})")
                            }
                        };
                        format!("{} @{:?} {outcome}", key(i), s)
                    })
                    .collect::<Vec<_>>()
            })
            .collect(),
    );
    put(
        "duplicate_sites",
        g.duplicate_sites.iter().map(|(i, s)| format!("{} @{s:?}", key(i))).collect(),
    );
    put(
        "uninstantiated_roots",
        g.uninstantiated_roots
            .iter()
            .map(|(n, h)| {
                format!("{n} generic_root={} sites={} instances={}", h.generic_root, h.always_emitted_sites.len(), h.instances)
            })
            .collect(),
    );
    put(
        "excused_generic_roots",
        g.excused_generic_roots
            .iter()
            .flat_map(|(reason, names)| {
                names.iter().map(move |n| format!("{reason}: {n}")).collect::<Vec<_>>()
            })
            .collect(),
    );
    put("always_emitted_targets", g.always_emitted_targets.iter().map(&key).collect());
    put(
        "subst_disagreements",
        g.subst_disagreements.iter().map(|d| format!("{}", key(&d.instance))).collect(),
    );
    put(
        "bound_tree_disagreements",
        g.bound_tree_disagreements.iter().map(|d| format!("{}", key(&d.instance))).collect(),
    );
    CanonicalGraph(out)
}
