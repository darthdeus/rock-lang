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
use bone_semantic::prelude::Span;
use skull_mir::mangle::mangle_fn;
use skull_mir::monoty::MonoSubst;

use crate::instance_graph::{
    Instance, InstanceBoundTree, InstanceGraph, Site, SiteDomain, SiteOp,
};

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

/// A span's COMPLETE position, not its `Debug` -- which prints the file alone
/// (`rock_core::source_code`), so two diagnostics at different lines of one file
/// render identically through it. A stale record that survived an edit usually
/// differs from the fresh one in position and nothing else, which is exactly
/// what that would hide.
fn span_key(s: &Span) -> String {
    format!(
        "{}:{}:{}-{}:{}@{}..{}",
        s.file,
        s.line_range.0,
        s.col_range.0,
        s.line_range.1,
        s.col_range.1,
        s.offset_range.0,
        s.offset_range.1,
    )
}

/// A site's whole identity: the arena its expression indexes, the expression,
/// and the operation. The struct-defaults domain carries the ARGUMENTS that
/// selected the callee, rendered canonically rather than through `MonoTy`'s
/// `Debug`.
fn site_key(s: &Site) -> String {
    let domain = match &s.domain {
        SiteDomain::Own => "own".to_string(),
        SiteDomain::StructDefaults(m, d, args) => format!(
            "defaults({}.{}<{}>)",
            m.index(),
            d.into_raw().into_u32(),
            args.iter().map(|a| a.canonical()).collect::<Vec<_>>().join(","),
        ),
    };
    let op = match &s.op {
        SiteOp::DirectCall => "direct-call".to_string(),
        SiteOp::FnValue => "fn-value".to_string(),
        SiteOp::ForIter => "for-iter".to_string(),
        SiteOp::ImplicitIter => "implicit-iter".to_string(),
        SiteOp::ForNext(path) => format!("for-next{}", path_key(path)),
        SiteOp::BoundNext(path) => format!("bound-next{}", path_key(path)),
        SiteOp::Synthesized(name) => format!("synthesized({name})"),
        SiteOp::Dispatched { name, path } => format!("dispatched({name}){}", path_key(path)),
    };
    format!("{domain}/{}/{op}", s.expr.into_raw().into_u32())
}

fn path_key<T: std::fmt::Display>(path: &[T]) -> String {
    format!("[{}]", path.iter().map(|p| p.to_string()).collect::<Vec<_>>().join("."))
}

/// A conformance tree RECURSIVELY. Rendering only the outermost witnesses
/// leaves a chained iterator's inner `next` unobserved, which is the half a
/// re-expansion is most likely to get wrong.
fn bound_tree_key(ws: &Workspace, g: &InstanceGraph, t: &InstanceBoundTree) -> String {
    let parts: Vec<String> = t
        .next
        .iter()
        .map(|(idx, n)| {
            format!("{idx}->{}{}", instance_key(ws, g, &n.callee), bound_tree_key(ws, g, &n.sub))
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

/// A substitution's bindings, sorted so the map's iteration order cannot leak
/// into the comparison.
fn subst_key(s: &MonoSubst) -> String {
    let mut binds: Vec<String> = s.iter().map(|(v, t)| format!("{v:?}={}", t.canonical())).collect();
    binds.sort();
    binds.join(",")
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
        g.edges
            .iter()
            .map(|e| {
                format!(
                    "{} -{:?}-> {} @{}",
                    key(&e.from),
                    e.kind,
                    key(&e.to),
                    span_key(&e.span)
                )
            })
            .collect(),
    );
    put(
        "unresolved",
        g.unresolved
            .iter()
            .map(|u| {
                format!(
                    "{} -> {} [{}] @{}",
                    key(&u.from),
                    decl_key(ws, u.callee),
                    u.reason,
                    span_key(&u.span)
                )
            })
            .collect(),
    );
    put(
        "rejections",
        g.rejections
            .iter()
            .map(|r| {
                format!("{:?} at {} @{}: {}", r.kind, key(&r.at), span_key(&r.span), r.message)
            })
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
        g.substitutions.iter().map(|(i, s)| format!("{} :: {}", key(i), subst_key(s))).collect(),
    );
    put(
        "bound_trees",
        g.bound_trees
            .iter()
            .map(|(i, t)| format!("{} :: {}", key(i), bound_tree_key(ws, g, t)))
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
                        format!("{} @{} {outcome} @{}", key(i), site_key(s), span_key(&r.span))
                    })
                    .collect::<Vec<_>>()
            })
            .collect(),
    );
    put(
        "duplicate_sites",
        g.duplicate_sites.iter().map(|(i, s)| format!("{} @{}", key(i), site_key(s))).collect(),
    );
    // Each always-emitted failure by OUTCOME and SPAN, not as a count: two
    // failures of one callee are two diagnostics at two places, and a count
    // reports them as one fact with no location -- which is also what makes a
    // stale one invisible after the call it belonged to is gone.
    put(
        "uninstantiated_roots",
        g.uninstantiated_roots
            .iter()
            .map(|(n, h)| {
                let mut sites: Vec<String> = h
                    .always_emitted_sites
                    .iter()
                    .map(|s| {
                        let outcome = match &s.outcome {
                            crate::instance_graph::AlwaysEmittedOutcome::Unresolved(r) => {
                                format!("unresolved({r})")
                            }
                            crate::instance_graph::AlwaysEmittedOutcome::Rejection(k) => {
                                format!("rejection({})", k.name())
                            }
                        };
                        format!("{outcome}@{}", span_key(&s.span))
                    })
                    .collect();
                sites.sort();
                format!(
                    "{n} generic_root={} instances={} sites=[{}]",
                    h.generic_root,
                    h.instances,
                    sites.join(","),
                )
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
    // Both sides of each disagreement. The instance alone says only THAT one
    // exists; what makes the record actionable -- and what an updater can get
    // wrong on its own -- is which two answers disagreed.
    put(
        "subst_disagreements",
        g.subst_disagreements
            .iter()
            .map(|d| {
                format!(
                    "{} kept={{{}}} discarded={{{}}}",
                    key(&d.instance),
                    subst_key(&d.kept),
                    subst_key(&d.discarded)
                )
            })
            .collect(),
    );
    put(
        "bound_tree_disagreements",
        g.bound_tree_disagreements
            .iter()
            .map(|d| {
                format!(
                    "{} recorded={} incoming={}",
                    key(&d.instance),
                    bound_tree_key(ws, g, &d.recorded),
                    bound_tree_key(ws, g, &d.incoming)
                )
            })
            .collect(),
    );
    CanonicalGraph(out)
}
