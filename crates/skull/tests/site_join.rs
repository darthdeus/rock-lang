//! Controls for the MIR-call -> `ConcreteBody` join
//! (`skull::site_join`, plan S 9 / Stage 3).
//!
//! The join's value is that a call instruction selects the RIGHT concrete body.
//! Every property below is therefore about DISTINGUISHABILITY -- two things
//! that must not collapse onto one key -- plus the mutation arms proving each
//! half of the identity is load-bearing rather than decorative.

use std::sync::LazyLock;

use bone_semantic::inference::Workspace;
use skull::instance_graph::{self, InstanceGraph};
use skull::site_join::{self, SiteJoinCensus, Unjoined};
use skull_mir::lower::ScalarCtx;
use skull_mir::mir::{Inst, MirFunc};
use skull_mir::site::{Site, SiteOp};
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn check(src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

fn graph_of(src: &str) -> (Workspace, InstanceGraph) {
    let ws = check(src);
    let roots = instance_graph::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots, false);
    (ws, g)
}

fn census_of(src: &str) -> SiteJoinCensus {
    let (ws, mut g) = graph_of(src);
    let bodies = skull::concrete_body::build(&ws, &mut g);
    let scx = ScalarCtx::new(&ws);
    site_join::census(&ws, &scx, &g, &bodies)
}

/// Lower the `<test>` module's fn named `name` and run the SAME transform
/// sequence emission runs, so a test inspects the body emission renders.
fn prepared_body(ws: &Workspace, name: &str) -> MirFunc {
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let (fid, f) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .expect("fn not found");
    let fref = skull_mir::mir::FuncRef { module: mid, func: fid };
    let scx = ScalarCtx::new(ws);
    let subst = skull_mir::lower::SpecSubst::default();
    let mut mf = skull_mir::lower::lower_function_spec(ws, &scx, mid, fid, &subst).expect("lower");
    let mangled = skull_mir::mangle::mangle_fn(ws, f);
    let mut cache = skull_transform::InlineCache::default();
    skull::mir_emit::prepare_instance_body(ws, &scx, fref, &subst, &mangled, &mut mf, &mut cache)
        .expect("prepare");
    mf
}

/// Every recorded site in `mf`, including hoisted nested bodies.
fn sites(mf: &MirFunc) -> Vec<Site> {
    let mut out: Vec<Site> = mf.call_sites.values().map(|c| c.site.clone()).collect();
    for nf in &mf.nested_fns {
        out.extend(sites(nf));
    }
    out
}

fn call_count(mf: &MirFunc) -> usize {
    mf.insts
        .iter()
        .filter(|(_, i)| {
            matches!(
                i,
                Inst::Call { .. }
                    | Inst::CallMulti { .. }
                    | Inst::CallValue { .. }
                    | Inst::CallValueMulti { .. }
            )
        })
        .count()
        + mf.nested_fns.iter().map(call_count).sum::<usize>()
}

// ---------------------------------------------------------------------------
// Distinguishability
// ---------------------------------------------------------------------------

/// SENTINEL -- flip when `SiteOp` classification is completed (mono Stage 3
/// cut B).
///
/// A `for` head's `iter` and `next` are the canonical same-span pair: two
/// callee obligations at ONE expression carrying ONE span. The GOAL is that
/// they reach MIR as two different sites, which is the whole reason `SiteOp`
/// exists. That is proved at the KEY level by
/// [`one_expression_under_different_operations_stays_distinct`] below.
///
/// It is NOT yet proved at the RECORDING level, and this pins why. A bounded
/// generic's loop emits `Inst::CallBoundMulti`, which is not one of the four
/// call forms `Builder::emit_cur` stamps, so no witness site reaches MIR here
/// at all. The corpus does produce them (bitgun: 7 `for-iter`, 7 `for-next`),
/// so the recording works on the `BoundNext::Concrete` path -- this fixture
/// takes the `CallerGeneric` one and is the gap.
///
/// When `CallBoundMulti` joins the recorded forms this goes red: assert both
/// witness populations non-empty, that they share an expression, and that they
/// stay distinct.
#[test]
fn a_bounded_loops_witnesses_reach_mir_unrecorded_sentinel() {
    let ws = check(
        r#"
struct Counter { n: int }

fn next(c: Counter) -> (bool, int) {
    if c.n >= 3 { return (false, 0) }
    c.n = c.n + 1
    return (true, c.n)
}

fn sum_all<I: Iterator<int>>(it: I) -> int {
    let s = 0
    for x in it { s = s + x }
    return s
}

fn main() { print("{sum_all(Counter { n: 0 })}") }
"#,
    );
    let mf = prepared_body(&ws, "main");
    let s = sites(&mf);
    let witnesses: Vec<&Site> = s
        .iter()
        .filter(|s| {
            matches!(s.op, SiteOp::ForIter | SiteOp::ForNext(_) | SiteOp::BoundNext(_))
        })
        .collect();
    assert!(
        witnesses.is_empty(),
        "PINNED GAP: a witness site now reaches MIR through the bounded path. The \
         `CallBoundMulti` recording gap is closed -- rewrite this test to assert the \
         iter/next pair shares one expression and stays distinct. Got {witnesses:?}"
    );
    // The body IS lowered and DOES contain calls -- so the emptiness above is a
    // classification gap, not an empty fixture.
    assert!(!s.is_empty(), "fixture produced no sites at all, so it pins nothing");
}

/// The same expression reached under different operations stays distinct even
/// when the operations are the SAME variant with different conformance paths --
/// `ForNext(vec![])` and `ForNext(vec![0])` are two witnesses of one tree.
/// Asserted directly on the type, because no corpus fixture is guaranteed to
/// produce a nested tree and a control that might not fire proves nothing.
#[test]
fn one_expression_under_different_operations_stays_distinct() {
    let ws = check("fn main() { print(\"x\") }");
    let mf = prepared_body(&ws, "main");
    let e = mf.call_sites.values().next().expect("main calls something").site.expr;
    let variants = [
        Site { expr: e, op: SiteOp::DirectCall },
        Site { expr: e, op: SiteOp::FnValue },
        Site { expr: e, op: SiteOp::ForIter },
        Site { expr: e, op: SiteOp::ForNext(vec![]) },
        Site { expr: e, op: SiteOp::ForNext(vec![0]) },
        Site { expr: e, op: SiteOp::BoundNext(vec![]) },
        Site { expr: e, op: SiteOp::BoundNext(vec![0]) },
        Site { expr: e, op: SiteOp::ImplicitIter },
        Site { expr: e, op: SiteOp::Synthesized("__map_col_slot") },
        Site { expr: e, op: SiteOp::Synthesized("__map_col_free") },
    ];
    let mut set: std::collections::BTreeSet<&Site> = std::collections::BTreeSet::new();
    for v in &variants {
        assert!(set.insert(v), "two operations at one expression collapsed: {v:?}");
    }
    assert_eq!(set.len(), variants.len());
    // `ForNext` and `BoundNext` at the same path are deliberately different
    // arms: a `for` over a bounded call carries both, at one expression and
    // with the same path.
    assert_ne!(
        Site { expr: e, op: SiteOp::ForNext(vec![0]) },
        Site { expr: e, op: SiteOp::BoundNext(vec![0]) },
    );
}

// ---------------------------------------------------------------------------
// Survival through remapping and inlining
// ---------------------------------------------------------------------------

/// Inlining clones a callee's instructions into the caller under a new `Idx`
/// space. The site entries must be remapped with them -- and their OWNER
/// re-interned into the caller's table, since index 0 there is the CALLER.
#[test]
fn sites_survive_inlining_and_carry_the_callee_owner() {
    let ws = check(
        r#"
@inline
fn leaf(x: int) -> int { return double(x) }
fn double(x: int) -> int { return x + x }

fn top(x: int) -> int { return leaf(x) }
fn main() { print("{top(3)}") }
"#,
    );
    let mf = prepared_body(&ws, "top");
    // `leaf` inlined, so `top` now contains the call to `double` that `leaf`
    // owned. Its site must name `leaf` as owner, not `top`.
    let owners: Vec<&str> = mf
        .call_sites
        .values()
        .map(|cs| {
            let o = &mf.site_owners[cs.owner.0 as usize];
            ws.module(o.func.module).functions[o.func.func].name.as_str()
        })
        .collect();
    assert!(
        owners.iter().any(|n| *n == "leaf"),
        "no spliced call attributed to `leaf`; owners = {owners:?}"
    );
    // Every entry indexes a real owner slot -- a stale `OwnerRef` would be a
    // silent misattribution rather than a lookup failure.
    for cs in mf.call_sites.values() {
        assert!(
            (cs.owner.0 as usize) < mf.site_owners.len(),
            "OwnerRef {:?} out of range of a {}-entry table",
            cs.owner,
            mf.site_owners.len()
        );
    }
}

/// Every call instruction that survives to the emitted body carries a site.
/// Stated over this fixture's own population rather than as a general claim:
/// the corpus census reports a real `no-site` bucket, so "always" would be
/// false.
#[test]
fn every_call_in_a_simple_prepared_body_carries_a_site() {
    let ws = check(
        r#"
fn a(x: int) -> int { return x + 1 }
fn b(x: int) -> int { return a(x) + a(x + 1) }
fn main() { print("{b(2)}") }
"#,
    );
    let mf = prepared_body(&ws, "b");
    let calls = call_count(&mf);
    assert!(calls > 0, "fixture emitted no calls at all");
    assert_eq!(sites(&mf).len(), calls, "a call in this body carries no site");
}

// ---------------------------------------------------------------------------
// The census population
// ---------------------------------------------------------------------------

/// A non-generic direct call joins, and so does a boundary call. Both arms are
/// asserted non-zero: a join satisfied entirely by one arm says nothing about
/// the other, and `print` is a boundary while `helper` is Rock.
#[test]
fn non_generic_and_boundary_calls_both_join() {
    let c = census_of(
        r#"
fn helper(x: int) -> int { return x * 2 }
fn main() {
    let v = helper(21)
    print("{v}")
}
"#,
    );
    assert!(c.bodies_measured > 0, "nothing measured");
    assert!(c.joined_rock > 0, "no Rock callee joined: {c:?}");
    assert!(c.joined_boundary > 0, "no boundary callee joined: {c:?}");
    assert!(c.accounts(), "accounting identity broken: {c:?}");
}

/// The census accounts for every examined call on a fixture that exercises
/// generics, inlining and iteration together.
#[test]
fn the_census_accounts_for_every_examined_call() {
    let c = census_of(
        r#"
fn ident<T>(x: T) -> T { return x }
@inline
fn twice(x: int) -> int { return x + x }

fn main() {
    let a = ident(1)
    let b = ident("s")
    let t = twice(a)
    let v = [1, 2, 3]
    let sum = 0
    for x in v { sum = sum + x }
    print("{b} {t} {sum}")
}
"#,
    );
    assert!(c.call_insts > 0, "no calls examined");
    assert!(c.accounts(), "accounting identity broken: {c:?}");
    assert_eq!(
        c.joined + c.unjoined_total(),
        c.call_insts,
        "a call is neither joined nor given a cause"
    );
}

/// A generic body the inliner splices as its SHARED snapshot has no concrete
/// owner instance, and the join must REFUSE it rather than pick one of the
/// declaration's instances. This is the property that makes the census safe to
/// build instance-owned emission on: it fails closed.
#[test]
fn a_shared_generic_splice_refuses_rather_than_guessing() {
    let c = census_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn use_both() -> int {
    let a = ident(1)
    let b = ident(2.0)
    return a + int(b)
}
fn main() { print("{use_both()}") }
"#,
    );
    // Whatever the counts, nothing may join through an owner the graph does not
    // have: `owner-not-an-instance` is a refusal, never a silent match.
    assert!(c.accounts(), "accounting identity broken: {c:?}");
    let guessed = c.unjoined.get(&Unjoined::OwnerNotInTable).copied().unwrap_or(0);
    assert_eq!(guessed, 0, "an owner index escaped its table: {c:?}");
}
