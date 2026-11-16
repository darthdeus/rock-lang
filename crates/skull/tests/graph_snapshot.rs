//! The canonical-snapshot oracle the persistent graph updater is gated on
//!.
//!
//! Two properties, and the second is what makes the first worth anything: the
//! snapshot must be STABLE across builds that mean the same thing, and it must
//! CHANGE for any one-sided semantic change. A snapshot that omitted a field
//! would satisfy stability perfectly and detect nothing.
//!
//! Completeness is asserted at FIELD granularity, not section granularity. Each
//! arm below builds two graphs differing in exactly one field of one record and
//! requires the canonical form to tell them apart -- so a rendering that drops a
//! nested payload (an edge's span, a bound tree's sub-witnesses, a
//! disagreement's two substitutions) is red here rather than silently equal.

use crate::common::ws_of_files;
use std::collections::{BTreeMap, BTreeSet};

use crate::support::graph_snapshot::{CanonicalGraph, canonical};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::Span;
use skull::instance_graph::{
    self, Agreed, AlwaysEmittedFailure, AlwaysEmittedOutcome, Boundary, BoundaryClass, DeclKey,
    EdgeKind, Instance, InstanceBoundNext, InstanceBoundTree, InstanceGraph, Owner, Rejection,
    RejectionKind, RootHole, Site, SiteDomain, SiteOp, SiteOutcome, SiteRecord, Unresolved,
};
use skull_mir::monoty::{MonoSubst, MonoTy};
use ustr::Ustr;

/// One fixture, chosen so that every section the graph can populate from a
/// VALID program is non-empty: generic calls, a coroutine, a bounded-generic
/// chain (`bound_trees`), a global initializer (`always_emitted_targets`), an
/// exempt generic emission root (`uninstantiated_roots`), mangled ones
/// (`excused_generic_roots`), and polymorphic recursion (`depth_capped` plus a
/// `ChangingInstanceRecursion` rejection).
///
/// The sections it CANNOT populate are the invariant-empty ones -- see
/// [`INVARIANT_EMPTY`].
const SRC: &str = r#"
struct Holder<T> { v: T }

fn passthru<T>(x: T) -> T { return x }
fn ident<T>(x: T) -> T { return passthru(x) }

co fn ticks(n: int) -> Coroutine<int> {
    yield n
}

fn plain(a: int) -> int { return a + 1 }

@nomangle
fn shared_gen<T>(x: T) -> T { return x }

fn deep<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    let wrapped: [T] = [x]
    return 1 + deep(wrapped, n - 1)
}

fn sum_all(v: Vec<int>) -> int {
    let t = 0
    for x in v { t = t + x }
    for y in v.iter().map(_ * 2) { t = t + y }
    return t
}

global g_total: int = plain(7)

fn main() {
    let c = ticks(3)
    let v: Vec<int> = vec![1, 2, 3]
    let h = Holder { v: 1 }
    print("{ident(1)} {ident("s")} {plain(2)} {c.resume() ?? -1} {sum_all(v)} {g_total} {deep(1, 3)} {h.v}")
}
"#;

fn build(perturb: bool) -> (Workspace, InstanceGraph) {
    let ws = ws_of_files(&[("<test>", SRC)]);
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots, perturb);
    (ws, g)
}

fn snap(ws: &Workspace, g: &InstanceGraph) -> CanonicalGraph {
    canonical(ws, g)
}

/// Two builds of one program that differ only in DISCOVERY ORDER mean the same
/// thing, so the canonical form must not be able to tell them apart. This is
/// the same property `S 20g.4` asserts of the rendered manifest, at the level
/// the updater is actually compared on.
#[test]
fn discovery_order_does_not_change_the_canonical_form() {
    let (ws_a, a) = build(false);
    let (ws_b, b) = build(true);
    let (sa, sb) = (snap(&ws_a, &a), snap(&ws_b, &b));
    let d = sa.diff(&sb);
    assert!(
        d.is_empty(),
        "perturbing discovery order moved the canonical form:\n{}",
        d.join("\n")
    );
}

/// Sections no VALID program populates, each because the graph records it only
/// where an invariant it asserts has already broken. Their arms below construct
/// the record instead of asking the fixture for one -- and the list is stated
/// so "the fixture does not populate it" cannot quietly grow to cover a section
/// that simply went missing from the walk.
///
/// - `duplicate_sites`: a body is walked once per instance, so a repeat key
///   means two operations collapsed onto one.
/// - `subst_disagreements` / `bound_tree_disagreements`: a specialized body's
///   shape must be a function of its instance.
/// - `unresolved`: every remaining producer needs a binding record inference did
///   not write; `instance_graph.rs`'s own arms reach them by removing one from a
///   `Workspace`, which is a perturbation rather than a program.
const INVARIANT_EMPTY: &[&str] = &[
    "bound_tree_disagreements",
    "duplicate_sites",
    "subst_disagreements",
    "unresolved",
];

/// Every section `canonical` emits. Named here rather than read off the
/// snapshot, so a section DELETED from the renderer fails this list instead of
/// disappearing from the coverage it is measured against.
const SECTIONS: &[&str] = &[
    "always_emitted_targets",
    "bound_tree_disagreements",
    "bound_trees",
    "boundaries",
    "call_sites",
    "coroutine_helpers",
    "depth_capped",
    "duplicate_sites",
    "edges",
    "excused_generic_roots",
    "expanded",
    "inline_body_deps",
    "instances",
    "rejections",
    "subst_disagreements",
    "substitutions",
    "uninstantiated_roots",
    "unresolved",
];

/// The oracle must emit every section, and the fixture must populate every one
/// a valid program can. An arm over a section that is empty on both sides
/// proves nothing, so this is what keeps the arms below honest.
#[test]
fn the_fixture_populates_every_section_a_valid_program_can() {
    let (ws, g) = build(false);
    let s = snap(&ws, &g);
    let emitted: BTreeSet<&str> = s.0.keys().copied().collect();
    let expected: BTreeSet<&str> = SECTIONS.iter().copied().collect();
    assert_eq!(
        emitted, expected,
        "`canonical` emits a different section set than SECTIONS names"
    );

    for section in SECTIONS {
        let populated = s.0.get(section).is_some_and(|v| !v.is_empty());
        let invariant_empty = INVARIANT_EMPTY.contains(section);
        assert_eq!(
            populated, !invariant_empty,
            "section `{section}`: populated={populated}, but INVARIANT_EMPTY says \
             {invariant_empty}"
        );
    }
}

// ---------------------------------------------------------------------------
// Field-level completeness
// ---------------------------------------------------------------------------

type Mutation = Box<dyn Fn(&mut InstanceGraph)>;

/// One field probe: two graphs differing only in the named field, which the
/// canonical form must report under `section`.
struct Arm {
    name: &'static str,
    section: &'static str,
    a: Mutation,
    b: Mutation,
}

fn arm(
    name: &'static str,
    section: &'static str,
    a: impl Fn(&mut InstanceGraph) + 'static,
    b: impl Fn(&mut InstanceGraph) + 'static,
) -> Arm {
    Arm {
        name,
        section,
        a: Box::new(a),
        b: Box::new(b),
    }
}

fn noop(_: &mut InstanceGraph) {}

/// An arm whose two sides install the same base record, one side altered
/// first, so the pair differs in exactly the altered field.
fn variant<T: 'static>(
    name: &'static str,
    section: &'static str,
    base: fn(&InstanceGraph) -> T,
    alter: fn(&InstanceGraph, &mut T),
    install: fn(&mut InstanceGraph, T),
) -> Arm {
    arm(
        name,
        section,
        move |g| {
            let r = base(g);
            install(g, r)
        },
        move |g| {
            let mut r = base(g);
            alter(g, &mut r);
            install(g, r)
        },
    )
}

/// The first contribution that owns an edge. The oracle's arms mutate the
/// PRIMARY RECORD, which is where the row lives -- there is no flat copy to
/// perturb instead, and perturbing one would prove nothing about the other.
fn first_edge_owner(g: &mut InstanceGraph) -> &mut skull::instance_graph::Contribution {
    g.contributions
        .values_mut()
        .find(|c| !c.edges.is_empty())
        .expect("an edge")
}

/// Any contribution, for arms that ADD a record rather than alter one.
fn any_owner(g: &mut InstanceGraph) -> &mut skull::instance_graph::Contribution {
    g.contributions.values_mut().next().expect("a contribution")
}

/// The n-th instance in the graph's own order. Deterministic: `instances` is a
/// `BTreeSet` and the build is order-stable (the arm above proves it).
fn inst(g: &InstanceGraph, n: usize) -> Instance {
    g.instances
        .iter()
        .nth(n)
        .expect("the fixture has instances")
        .clone()
}

/// A real span from the graph, so a span arm probes a rendering rather than the
/// `<unknown>` sentinel.
fn real_span(g: &InstanceGraph) -> Span {
    g.edges()
        .map(|e| e.span)
        .find(|s| !instance_graph::is_spanless(s))
        .expect("the fixture has a spanned edge")
}

fn shifted(mut s: Span) -> Span {
    s.set_line_range((s.line_range().0, s.line_range().1 + 1));
    s
}

fn expr_idx(n: u32) -> bone_semantic::prelude::Idx<bone_semantic::hir::Expr> {
    bone_semantic::prelude::Idx::from_raw(n.into())
}

fn a_site() -> Site {
    Site {
        domain: SiteDomain::Own,
        expr: expr_idx(0),
        op: SiteOp::DirectCall,
    }
}

fn defaults_domain(args: Vec<MonoTy>) -> SiteDomain {
    SiteDomain::struct_defaults(
        bone_semantic::prelude::ModuleId::builtin(),
        bone_semantic::prelude::Idx::from_raw(0u32.into()),
        args,
    )
}

/// A substitution with one binding, taken from the graph so its `VarId` is a
/// real one, retyped so two arms can differ in the VALUE alone.
fn subst_with(g: &InstanceGraph, ty: MonoTy) -> MonoSubst {
    let var = *g
        .substitutions()
        .map(|(_, s)| s)
        .find(|s| !s.is_empty())
        .expect("the fixture has a generic instance")
        .keys()
        .next()
        .expect("non-empty");
    let mut out = MonoSubst::default();
    out.insert(var, ty);
    out
}

/// A low-sorting owner, so a value written under it is the AGREED one.
fn first_owner(g: &InstanceGraph) -> Owner {
    Owner::Instance(inst(g, 0))
}

/// A synthetic always-emitted owner. Every `Owner::AlwaysEmitted` sorts after
/// every `Owner::Instance`, so a value written under it is the DISCARDED half
/// of a disagreement rather than the kept one.
fn later_owner() -> Owner {
    Owner::AlwaysEmitted(
        bone_semantic::prelude::ModuleId::builtin(),
        bone_semantic::hir::BodyOwner::Global(bone_semantic::prelude::Idx::from_raw(0u32.into())),
    )
}

/// One instance's whole witness record: agreed value, plus an optional second
/// opinion. A disagreement IS two owners answering differently, so it is
/// constructed that way rather than as a record beside them.
fn set_trees(
    g: &mut InstanceGraph,
    i: Instance,
    agreed: InstanceBoundTree,
    other: Option<InstanceBoundTree>,
) {
    let (a, b) = (first_owner(g), later_owner());
    let mut rec = Agreed {
        owners: BTreeSet::from([a]),
        value: agreed,
        dissent: BTreeMap::new(),
        total: 1,
    };
    if let Some(o) = other {
        rec.dissent.insert(b, o);
        rec.total += 1;
    }
    g.index.trees.insert(i, rec);
}

fn set_substs(g: &mut InstanceGraph, i: Instance, agreed: MonoSubst, other: Option<MonoSubst>) {
    let (a, b) = (first_owner(g), later_owner());
    let mut rec = Agreed {
        owners: BTreeSet::from([a]),
        value: agreed,
        dissent: BTreeMap::new(),
        total: 1,
    };
    if let Some(o) = other {
        rec.dissent.insert(b, o);
        rec.total += 1;
    }
    g.index.subst.insert(i, rec);
}

fn leaf() -> InstanceBoundTree {
    InstanceBoundTree::default()
}

fn tree(callee: Instance, sub: InstanceBoundTree) -> InstanceBoundTree {
    let mut t = InstanceBoundTree::default();
    t.next.insert(0, InstanceBoundNext { callee, sub });
    t
}

fn put_site(g: &mut InstanceGraph, site: Site, record: SiteRecord) {
    let owner = Owner::Instance(inst(g, 0));
    g.contributions
        .get_mut(&owner)
        .expect("owner")
        .call_sites
        .insert(site, record);
}

fn resolved_site(g: &InstanceGraph, n: usize) -> SiteRecord {
    SiteRecord {
        outcome: SiteOutcome::Resolved(inst(g, n)),
        span: real_span(g),
    }
}

fn synthetic_unresolved(g: &InstanceGraph) -> Unresolved {
    Unresolved {
        from: inst(g, 0),
        callee: inst(g, 0).func,
        reason: "synthetic-a",
        span: real_span(g),
    }
}

fn synthetic_rejection(g: &InstanceGraph) -> Rejection {
    Rejection {
        kind: RejectionKind::GenericFnValue,
        at: inst(g, 0),
        span: real_span(g),
        message: "synthetic-a".to_string(),
    }
}

const HOLE: &str = "synthetic_root";

fn arms() -> Vec<Arm> {
    let mut v: Vec<Arm> = Vec::new();

    // --- presence of each primary population ---
    v.push(arm("instances.presence", "instances", noop, |g| {
        let victim = inst(g, 0);
        g.instances.remove(&victim);
    }));
    v.push(arm("expanded.presence", "expanded", noop, |g| {
        g.contributions
            .values_mut()
            .find(|c| c.expanded)
            .expect("an expanded body")
            .expanded = false;
    }));
    v.push(arm("edges.presence", "edges", noop, |g| {
        first_edge_owner(g).edges.pop().expect("an edge");
    }));
    v.push(arm("depth_capped.presence", "depth_capped", noop, |g| {
        g.contributions
            .values_mut()
            .find(|c| c.depth_capped)
            .expect("the fixture's capped instance")
            .depth_capped = false;
    }));
    v.push(arm("boundaries.presence", "boundaries", noop, |g| {
        let k = g.boundaries.keys().next().expect("a boundary").clone();
        g.boundaries.remove(&k);
    }));
    v.push(arm(
        "coroutine_helpers.presence",
        "coroutine_helpers",
        noop,
        |g| {
            let victim = g.coroutine_helpers.iter().next().expect("a helper").clone();
            g.coroutine_helpers.remove(&victim);
        },
    ));
    v.push(arm(
        "always_emitted_targets.presence",
        "always_emitted_targets",
        noop,
        |g| {
            let victim = g
                .always_emitted_targets()
                .next()
                .expect("a seeded target")
                .clone();
            g.index.always_emitted.remove(&victim);
        },
    ));
    v.push(arm("call_sites.presence", "call_sites", noop, |g| {
        g.contributions
            .values_mut()
            .find(|c| !c.call_sites.is_empty())
            .expect("a call-site owner")
            .call_sites
            .clear();
    }));
    v.push(arm("substitutions.presence", "substitutions", noop, |g| {
        let victim = g.index.subst.keys().next().expect("a substitution").clone();
        g.index.subst.remove(&victim);
        g.root_instances.remove(&victim);
    }));
    v.push(arm(
        "inline_body_deps.presence",
        "inline_body_deps",
        noop,
        |g| {
            let k = *g.inline_body_deps.keys().next().expect("an inline dep");
            g.inline_body_deps.remove(&k);
        },
    ));
    v.push(arm(
        "excused_generic_roots.presence",
        "excused_generic_roots",
        noop,
        |g| {
            let k = *g
                .excused_generic_roots
                .keys()
                .next()
                .expect("an excused root");
            g.excused_generic_roots.remove(&k);
        },
    ));
    v.push(arm(
        "uninstantiated_roots.presence",
        "uninstantiated_roots",
        noop,
        |g| {
            let k = g
                .uninstantiated_roots
                .keys()
                .next()
                .expect("a root hole")
                .clone();
            g.uninstantiated_roots.remove(&k);
        },
    ));
    // The fixture's own rejection is the WHOLE-GRAPH one: the recursion pass is
    // not a body's walk, so it is the graph that owns it.
    v.push(arm("rejections.presence", "rejections", noop, |g| {
        g.graph_rejections
            .pop()
            .expect("the fixture's recursion rejection");
    }));
    v.push(arm("bound_trees.presence", "bound_trees", noop, |g| {
        let k = g.index.trees.keys().next().expect("a bound tree").clone();
        g.index.trees.remove(&k);
    }));

    // --- edges: every field ---
    v.push(arm(
        "edges.kind",
        "edges",
        |g| first_edge_owner(g).edges[0].kind = EdgeKind::DirectCall,
        |g| first_edge_owner(g).edges[0].kind = EdgeKind::FnValue,
    ));
    v.push(arm("edges.from", "edges", noop, |g| {
        let to = inst(g, 1);
        first_edge_owner(g).edges[0].from = to;
    }));
    v.push(arm("edges.to", "edges", noop, |g| {
        let to = inst(g, 1);
        first_edge_owner(g).edges[0].to = to;
    }));
    for (name, mutate) in span_field_probes() {
        v.push(Arm {
            name,
            section: "edges",
            a: Box::new(|g: &mut InstanceGraph| {
                let s = real_span(g);
                first_edge_owner(g).edges[0].span = s;
            }),
            b: Box::new(move |g: &mut InstanceGraph| {
                let mut s = real_span(g);
                mutate(&mut s);
                first_edge_owner(g).edges[0].span = s;
            }),
        });
    }

    // --- unresolved: every field ---
    let push_unresolved: fn(&mut InstanceGraph, Unresolved) =
        |g, u| any_owner(g).unresolved.push(u);
    for (name, alter) in [
        (
            "unresolved.from",
            (|g, u| u.from = inst(g, 1)) as fn(&InstanceGraph, &mut Unresolved),
        ),
        ("unresolved.callee", |g, u| u.callee = inst(g, 1).func),
        ("unresolved.reason", |_, u| u.reason = "synthetic-b"),
        ("unresolved.span", |_, u| u.span = shifted(u.span)),
    ] {
        v.push(variant(
            name,
            "unresolved",
            synthetic_unresolved,
            alter,
            push_unresolved,
        ));
    }

    // --- rejections: every field ---
    let push_rejection: fn(&mut InstanceGraph, Rejection) = |g, r| any_owner(g).rejections.push(r);
    for (name, alter) in [
        (
            "rejections.kind",
            (|_, r| r.kind = RejectionKind::UnconstrainedTypeVar)
                as fn(&InstanceGraph, &mut Rejection),
        ),
        ("rejections.at", |g, r| r.at = inst(g, 1)),
        ("rejections.message", |_, r| {
            r.message = "synthetic-b".into()
        }),
        ("rejections.span", |_, r| r.span = shifted(r.span)),
    ] {
        v.push(variant(
            name,
            "rejections",
            synthetic_rejection,
            alter,
            push_rejection,
        ));
    }

    // --- call_sites: the whole Site identity, the outcome and the span ---
    type SiteArm = fn(&InstanceGraph) -> (Site, SiteRecord);
    let plain: SiteArm = |g| (a_site(), resolved_site(g, 0));
    let in_defaults: SiteArm = |g| {
        let mut s = a_site();
        s.domain = defaults_domain(vec![MonoTy::Never]);
        (s, resolved_site(g, 0))
    };
    let bound_next: SiteArm = |g| {
        let mut s = a_site();
        s.op = SiteOp::BoundNext(vec![0].into());
        (s, resolved_site(g, 0))
    };
    let failed: SiteArm = |g| {
        let record = SiteRecord {
            outcome: SiteOutcome::Failed("synthetic-a"),
            span: real_span(g),
        };
        (a_site(), record)
    };
    for (name, base, alter) in [
        (
            "call_sites.site_domain",
            plain,
            (|_, (s, _)| s.domain = defaults_domain(vec![MonoTy::Never]))
                as fn(&InstanceGraph, &mut (Site, SiteRecord)),
        ),
        ("call_sites.site_domain_args", in_defaults, |_, (s, _)| {
            s.domain = defaults_domain(vec![MonoTy::Unit])
        }),
        ("call_sites.site_expr", plain, |_, (s, _)| {
            s.expr = expr_idx(1)
        }),
        ("call_sites.site_op", bound_next, |_, (s, _)| {
            s.op = SiteOp::BoundNext(vec![1].into())
        }),
        ("call_sites.outcome_target", plain, |g, (_, r)| {
            *r = resolved_site(g, 1)
        }),
        ("call_sites.outcome_failure", failed, |_, (_, r)| {
            r.outcome = SiteOutcome::Failed("synthetic-b")
        }),
        ("call_sites.span", plain, |_, (_, r)| {
            r.span = shifted(r.span)
        }),
    ] {
        v.push(variant(name, "call_sites", base, alter, |g, (s, r)| {
            put_site(g, s, r)
        }));
    }

    // --- duplicate_sites: which instance a duplicate belongs to is the OWNER
    // of the record, so `instance` puts the same site on two owners ---
    for (name, alter) in [
        (
            "duplicate_sites.instance",
            (|_, (n, _)| *n = 1) as fn(&InstanceGraph, &mut (usize, Site)),
        ),
        ("duplicate_sites.site", |_, (_, s)| s.op = SiteOp::FnValue),
    ] {
        v.push(variant(
            name,
            "duplicate_sites",
            |_| (0, a_site()),
            alter,
            |g, (n, s)| {
                let owner = Owner::Instance(inst(g, n));
                g.contributions
                    .get_mut(&owner)
                    .expect("owner")
                    .duplicate_sites
                    .push(s);
            },
        ));
    }

    // --- bound trees, including the RECURSIVE payload, and disagreements,
    // which carry two complete payloads each ---
    type Trees = (Instance, InstanceBoundTree, Option<InstanceBoundTree>);
    let one: fn(&InstanceGraph) -> Trees = |g| (inst(g, 0), tree(inst(g, 0), leaf()), None);
    let nested: fn(&InstanceGraph) -> Trees = |g| {
        let t = tree(inst(g, 0), tree(inst(g, 0), leaf()));
        (inst(g, 0), t, None)
    };
    let disputed: fn(&InstanceGraph) -> Trees =
        |g| (inst(g, 0), tree(inst(g, 0), leaf()), Some(leaf()));
    let disputed_nested: fn(&InstanceGraph) -> Trees = |g| {
        let t = tree(inst(g, 0), tree(inst(g, 0), leaf()));
        (inst(g, 0), t, Some(leaf()))
    };
    let disputed_by_nested: fn(&InstanceGraph) -> Trees = |g| {
        let o = tree(inst(g, 1), tree(inst(g, 0), leaf()));
        (inst(g, 0), tree(inst(g, 0), leaf()), Some(o))
    };
    for (name, section, base, alter) in [
        (
            "bound_trees.callee",
            "bound_trees",
            one,
            (|g, (_, t, _)| *t = tree(inst(g, 1), leaf())) as fn(&InstanceGraph, &mut Trees),
        ),
        (
            "bound_trees.nested_callee",
            "bound_trees",
            nested,
            |g, (_, t, _)| *t = tree(inst(g, 0), tree(inst(g, 1), leaf())),
        ),
        (
            "bound_trees.witness_index",
            "bound_trees",
            one,
            |g, (_, t, _)| {
                t.next.clear();
                t.next.insert(
                    3,
                    InstanceBoundNext {
                        callee: inst(g, 0),
                        sub: leaf(),
                    },
                );
            },
        ),
        (
            "bound_tree_disagreements.instance",
            "bound_tree_disagreements",
            disputed,
            |g, (i, _, _)| *i = inst(g, 1),
        ),
        (
            "bound_tree_disagreements.recorded",
            "bound_tree_disagreements",
            disputed_nested,
            |g, (_, t, _)| *t = tree(inst(g, 0), tree(inst(g, 1), leaf())),
        ),
        (
            "bound_tree_disagreements.incoming",
            "bound_tree_disagreements",
            disputed_by_nested,
            |g, (_, _, o)| *o = Some(tree(inst(g, 1), tree(inst(g, 1), leaf()))),
        ),
    ] {
        v.push(variant(name, section, base, alter, |g, (i, t, o)| {
            set_trees(g, i, t, o)
        }));
    }

    // --- substitutions: the BINDING, not only the key ---
    type Substs = (Instance, MonoSubst, Option<MonoSubst>);
    let agreed: fn(&InstanceGraph) -> Substs = |g| (inst(g, 0), subst_with(g, MonoTy::Never), None);
    let split: fn(&InstanceGraph) -> Substs = |g| {
        let other = subst_with(g, MonoTy::Unit);
        (inst(g, 0), subst_with(g, MonoTy::Never), Some(other))
    };
    for (name, section, base, alter) in [
        (
            "substitutions.value",
            "substitutions",
            agreed,
            (|g, (_, s, _)| *s = subst_with(g, MonoTy::Unit)) as fn(&InstanceGraph, &mut Substs),
        ),
        (
            "subst_disagreements.instance",
            "subst_disagreements",
            split,
            |g, (i, _, _)| *i = inst(g, 1),
        ),
        (
            "subst_disagreements.kept",
            "subst_disagreements",
            split,
            |g, (_, s, _)| *s = subst_with(g, MonoTy::Bool),
        ),
        (
            "subst_disagreements.discarded",
            "subst_disagreements",
            split,
            |g, (_, _, o)| *o = Some(subst_with(g, MonoTy::Bool)),
        ),
    ] {
        v.push(variant(name, section, base, alter, |g, (i, s, o)| {
            set_substs(g, i, s, o)
        }));
    }

    // --- uninstantiated_roots: the hole's cause AND every failure it carries ---
    let failing: fn(&InstanceGraph) -> RootHole = |g| RootHole {
        always_emitted_sites: vec![AlwaysEmittedFailure {
            outcome: AlwaysEmittedOutcome::Unresolved("synthetic"),
            span: real_span(g),
        }],
        ..RootHole::default()
    };
    for (name, base, alter) in [
        (
            "uninstantiated_roots.generic_root",
            (|_| RootHole::default()) as fn(&InstanceGraph) -> RootHole,
            (|_, h| h.generic_root = true) as fn(&InstanceGraph, &mut RootHole),
        ),
        (
            "uninstantiated_roots.instances",
            |_| RootHole {
                instances: 1,
                ..RootHole::default()
            },
            |_, h| h.instances = 2,
        ),
        ("uninstantiated_roots.failure_outcome", failing, |_, h| {
            h.always_emitted_sites[0].outcome =
                AlwaysEmittedOutcome::Rejection(RejectionKind::GenericFnValue)
        }),
        ("uninstantiated_roots.failure_span", failing, |_, h| {
            h.always_emitted_sites[0].span = shifted(h.always_emitted_sites[0].span)
        }),
    ] {
        v.push(variant(
            name,
            "uninstantiated_roots",
            base,
            alter,
            |g, h| {
                g.uninstantiated_roots.insert(HOLE.to_string(), h);
            },
        ));
    }

    // --- the remaining per-declaration records ---
    for (name, alter) in [
        (
            "boundaries.class",
            (|_, b| b.class = BoundaryClass::Intrinsic) as fn(&InstanceGraph, &mut Boundary),
        ),
        ("boundaries.symbol", |_, b| b.symbol = "sym-b".to_string()),
    ] {
        v.push(variant(
            name,
            "boundaries",
            |_| Boundary {
                class: BoundaryClass::Extern,
                symbol: "sym-a".to_string(),
            },
            alter,
            |g, b| {
                g.boundaries.insert("synthetic".to_string(), b);
            },
        ));
    }
    type Deps = (DeclKey, BTreeSet<&'static str>);
    let first_dep: fn(&InstanceGraph) -> Deps = |g| {
        let k = *g.inline_body_deps.keys().next().expect("an inline dep");
        (k, BTreeSet::from(["__map_col_len"]))
    };
    let first_decl: fn(&InstanceGraph) -> Deps =
        |g| (DeclKey(inst(g, 0).func), BTreeSet::from(["__map_col_len"]));
    for (name, base, alter) in [
        (
            "inline_body_deps.entries",
            first_dep,
            (|_, (_, set)| *set = BTreeSet::from(["__map_col_free"]))
                as fn(&InstanceGraph, &mut Deps),
        ),
        ("inline_body_deps.declaration", first_decl, |g, (k, _)| {
            *k = DeclKey(inst(g, 1).func)
        }),
    ] {
        v.push(variant(
            name,
            "inline_body_deps",
            base,
            alter,
            |g, (k, set)| {
                g.inline_body_deps.insert(k, set);
            },
        ));
    }
    v.push(arm(
        "excused_generic_roots.reason",
        "excused_generic_roots",
        |g| {
            g.excused_generic_roots
                .entry("mangled")
                .or_default()
                .insert("syn".to_string());
        },
        |g| {
            g.excused_generic_roots
                .entry("stdlib-funcref")
                .or_default()
                .insert("syn".to_string());
        },
    ));

    v
}
/// Span perturbations, one field each. A span rendered through `{:?}` prints
/// its FILE alone (`rock_core::source_code`), so a section carrying its span
/// that way passes a presence check and loses every position -- which is what
/// these four separate.
fn span_field_probes() -> Vec<(&'static str, Box<dyn Fn(&mut Span)>)> {
    vec![
        (
            "edges.span_file",
            Box::new(|s: &mut Span| s.file = Ustr::from("<other>")),
        ),
        (
            "edges.span_lines",
            Box::new(|s: &mut Span| s.set_line_range((s.line_range().0 + 1, s.line_range().1))),
        ),
        (
            "edges.span_cols",
            Box::new(|s: &mut Span| s.set_col_range((s.col_range().0, s.col_range().1 + 1))),
        ),
        (
            "edges.span_offsets",
            Box::new(|s: &mut Span| {
                s.set_offset_range((s.offset_range().0, s.offset_range().1 + 1))
            }),
        ),
    ]
}

/// **A one-sided semantic change must fail equality, field by field.**
///
/// Each arm builds two graphs differing in exactly one field of one record and
/// requires the canonical form to notice AND to blame the right section. A
/// field omitted from `canonical` cannot fail an equality check, so this is the
/// check on the oracle itself.
#[test]
fn every_semantic_field_is_observed() {
    let mut covered: BTreeMap<&str, usize> = BTreeMap::new();
    // Every arm runs before anything is asserted: one blind field would
    // otherwise hide the rest behind it.
    let mut blind: Vec<String> = Vec::new();
    let mut misblamed: Vec<String> = Vec::new();
    let (ws, _) = build(false);
    for Arm {
        name,
        section,
        a,
        b,
    } in arms()
    {
        let (_, mut ga) = build(false);
        let (_, mut gb) = build(false);
        a(&mut ga);
        b(&mut gb);
        let (sa, sb) = (snap(&ws, &ga), snap(&ws, &gb));
        let d = sa.diff(&sb);
        if d.is_empty() {
            blind.push(name.to_string());
        } else if !d.iter().any(|l| l.starts_with(section)) {
            misblamed.push(format!("{name} -> {d:?} (expected `{section}`)"));
        }
        *covered.entry(section).or_default() += 1;
    }
    assert!(
        blind.is_empty(),
        "fields two graphs differ in that the canonical form CANNOT see -- each is missing \
         from `graph_snapshot::canonical`:\n  {}",
        blind.join("\n  ")
    );
    assert!(
        misblamed.is_empty(),
        "fields the canonical form saw but blamed on the wrong section:\n  {}",
        misblamed.join("\n  ")
    );
    let missing: Vec<&&str> = SECTIONS
        .iter()
        .filter(|s| !covered.contains_key(**s))
        .collect();
    assert!(
        missing.is_empty(),
        "sections with no field arm at all: {missing:?}"
    );
}

/// **Operational counters and arena contents stay OUT of equality.** An updater
/// that reaches the right graph by a different route differs on work counters,
/// and the arena keeps types whose instances were pruned -- neither is a
/// semantic difference and neither may fail the comparison.
#[test]
fn counters_and_arena_growth_are_not_semantic() {
    let (ws, base) = build(false);
    let before = snap(&ws, &base);

    let (_, mut g) = build(false);
    g.sites_attempted += 1234;
    g.recursion_starts += 99;
    g.always_emitted_seeds += 3;
    // The agreement counters are DERIVED from the reach index now, so moving
    // them means moving the index -- an extra reach of an instance the graph
    // already has, which is work performed and not a fact about the graph.
    if let Some(a) = g.index.subst.values_mut().next() {
        a.total += 11;
    }
    assert!(g.subst_comparisons() > 0 && g.bound_tree_comparisons() == g.bound_tree_comparisons());
    // An orphan type interned but referenced by no instance -- exactly what a
    // persisted arena accumulates across edits.
    g.interner().intern(MonoTy::Never);

    let after = snap(&ws, &g);
    let d = before.diff(&after);
    assert!(
        d.is_empty(),
        "work counters or arena growth leaked into semantic equality:\n{}",
        d.join("\n")
    );
}
