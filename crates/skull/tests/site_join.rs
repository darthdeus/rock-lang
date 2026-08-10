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
use skull_mir::monoty::{MonoCtor, MonoTy};
use skull_mir::site::{Site, SiteDomain, SiteOp};
use ustr::Ustr;

/// The backend the census lowers for; `check_callee` asks the emitter's own
/// declaration selection, so the comparison needs to know which target.
const LUA: bone_semantic::hir::FnBackend = bone_semantic::hir::FnBackend::Lua;

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
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots, false);
    (ws, g)
}

fn census_of(src: &str) -> SiteJoinCensus {
    let (ws, mut g) = graph_of(src);
    let bodies = skull::concrete_body::build(&ws, &mut g);
    let scx = ScalarCtx::new(&ws).sharing(g.interner().clone());
    site_join::census(&ws, &scx, &g, &bodies)
}

fn instance_claim(
    _g: &InstanceGraph,
    inst: &skull::instance_graph::Instance,
) -> skull::instance_graph::Instance {
    inst.clone()
}

/// A `Callee::Rock` over `inst`, the arm an ordinary Rock call resolves to.
fn rock(inst: &skull::instance_graph::Instance) -> skull::concrete_body::Callee {
    skull::concrete_body::Callee::Rock(inst.clone())
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
    let mut mf =
        skull_mir::lower::lower_instance(ws, &scx, &skull_mir::instance::Instance::base(fref))
            .expect("lower");
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

/// Reachable `CallBoundMulti`s -- the call form `emit_cur` does not stamp.
fn count_bound_multi(mf: &MirFunc) -> usize {
    mf.insts.iter().filter(|(_, i)| matches!(i, Inst::CallBoundMulti { .. })).count()
        + mf.nested_fns.iter().map(count_bound_multi).sum::<usize>()
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

/// A bounded loop's `next` witness reaches MIR RECORDED, on the caller-generic
/// path as well as the concrete one.
///
/// A `for` head's `iter` and `next` are the canonical same-span pair: two callee
/// obligations at ONE expression carrying ONE span. That they stay distinct is
/// proved at the KEY level by
/// [`one_expression_under_different_operations_stays_distinct`]. This is the
/// RECORDING half.
///
/// It used to be a pinned gap. `Inst::CallBoundMulti` was not one of the call
/// forms `Builder::emit_cur` stamps, so the SAME source construct was recorded
/// or invisible depending on which arm of `BoundNext` it took -- concrete
/// (recorded) or caller-generic (not). The form is stamped now.
///
/// The placeholder names no callee by construction: the concrete `next` is known
/// only per instantiation and the inliner substitutes it from the site's
/// `BoundTree` at the splice. So it carries a SITE and no TARGET, and the census
/// admits it under `bound-placeholder`.
#[test]
fn a_bounded_loops_witness_reaches_mir_recorded() {
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
    let mf = prepared_body(&ws, "sum_all");
    // The fixture must actually CONTAIN the form. Without this the assertion
    // below is satisfied by any body with no bounded loop at all.
    let bound_calls = count_bound_multi(&mf);
    assert!(
        bound_calls > 0,
        "fixture emitted no `CallBoundMulti`, so it does not exercise the recording it proves"
    );
    let s = sites(&mf);
    let witnesses: Vec<&Site> = s
        .iter()
        .filter(|s| matches!(s.op, SiteOp::ForNext(_)))
        .collect();
    assert_eq!(
        witnesses.len(),
        bound_calls,
        "every `CallBoundMulti` must carry a `for-next` site: {witnesses:?}"
    );
    // And it is the `for` HEAD's expression -- the same one an `iter` witness
    // would key on -- rather than an expression of its own.
    let heads: std::collections::BTreeSet<_> = witnesses.iter().map(|w| w.expr).collect();
    assert_eq!(heads.len(), 1, "one bounded loop, one head expression: {witnesses:?}");
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
        Site { domain: SiteDomain::Own, expr: e, op: SiteOp::DirectCall },
        Site { domain: SiteDomain::Own, expr: e, op: SiteOp::FnValue },
        Site { domain: SiteDomain::Own, expr: e, op: SiteOp::ForIter },
        Site { domain: SiteDomain::Own, expr: e, op: SiteOp::ForNext(vec![]) },
        Site { domain: SiteDomain::Own, expr: e, op: SiteOp::ForNext(vec![0]) },
        Site { domain: SiteDomain::Own, expr: e, op: SiteOp::BoundNext(vec![]) },
        Site { domain: SiteDomain::Own, expr: e, op: SiteOp::BoundNext(vec![0]) },
        Site { domain: SiteDomain::Own, expr: e, op: SiteOp::ImplicitIter },
        Site { domain: SiteDomain::Own, expr: e, op: SiteOp::Synthesized("__map_col_slot") },
        Site { domain: SiteDomain::Own, expr: e, op: SiteOp::Synthesized("__map_col_free") },
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
        Site { domain: SiteDomain::Own, expr: e, op: SiteOp::ForNext(vec![0]) },
        Site { domain: SiteDomain::Own, expr: e, op: SiteOp::BoundNext(vec![0]) },
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

#[test]
fn cloned_iterator_headers_are_reinterned_for_forl_recovery() {
    let ws = check(
        r#"
@inline
fn sum_inner(xs: [int]) -> int {
    let total = 0
    for x in xs { total = total + x }
    return total
}

fn top(xs: [int]) -> int { return sum_inner(xs) }
fn main() { print("{top([1, 2, 3])}") }
"#,
    );
    let mf = prepared_body(&ws, "top");
    assert!(
        !mf.for_iter_headers.is_empty(),
        "the inline callee's iterator header was not carried into the caller"
    );
    for &header in &mf.for_iter_headers {
        assert!(
            mf.blocks.iter().any(|(block, _)| block == header),
            "for_iter_headers contains a block outside the caller CFG: {header:?}"
        );
    }
}

fn count_bound_splits(mf: &MirFunc) -> usize {
    mf.bound_next_split.len() + mf.nested_fns.iter().map(count_bound_splits).sum::<usize>()
}

#[test]
fn cloned_bounded_value_calls_consume_split_placeholders() {
    let ws = check(
        r#"
nobox value Pair { x: int, y: int }

fn sum_all<I: Iterator<Pair>>(it: I) -> int {
    let total = 0
    for v in it { total = total + v.x }
    return total
}

fn main() {
    let xs = [Pair { x: 1, y: 2 }, Pair { x: 3, y: 4 }]
    print("{sum_all(xs.iter())}")
}
        "#,
    );
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
    let (sum_fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "sum_all")
        .expect("sum_all");
    let scx = ScalarCtx::new(&ws);
    let raw = skull_mir::lower::lower_instance(
        &ws,
        &scx,
        &skull_mir::instance::Instance::base(skull_mir::mir::FuncRef {
            module: mid,
            func: sum_fid,
        }),
    )
    .expect("lower sum_all");
    assert!(count_bound_multi(&raw) > 0, "fixture has no bounded placeholder");
    assert!(!raw.bound_next_split.is_empty(), "fixture has no split placeholder");
    let mf = prepared_body(&ws, "main");
    let bound_calls = count_bound_multi(&mf);
    assert_eq!(bound_calls, 0, "a concrete bounded call survived splicing: {mf:?}");
    assert_eq!(
        count_bound_splits(&mf),
        0,
        "a split marker survived after its bounded placeholder was consumed"
    );
}

#[test]
fn standalone_bounded_resolution_records_a_nonempty_resolved_tree() {
    let src = r#"
struct Item { id: int, p: float }

fn main() {
    let xs = [Item { id: 1, p: 3.0 }, Item { id: 2, p: 2.0 }]
    let ids = xs.map(_.id).collect()
    print("{ids[0]}")
}
"#;
    let (ws, g) = graph_of(src);
    let scx = ScalarCtx::new(&ws).sharing(g.interner().clone());
    let mut picked = None;
    for (candidate, candidate_tree) in g.bound_trees() {
        if candidate_tree.next.is_empty()
            || !candidate_tree.next.values().any(|w| !w.sub.next.is_empty())
            || !ws.module(candidate.func.module).functions[candidate.func.func]
                .generic_bounds
                .iter()
                .any(Option::is_some)
        {
            continue;
        }
        let Ok(mf) = skull_mir::lower::lower_instance(&ws, &scx, candidate) else { continue };
        if count_bound_multi(&mf) > 0 {
            picked = Some((candidate.clone(), candidate_tree.to_resolved()));
            break;
        }
    }
    let (inst, tree) = picked.expect("the fixture has no prepared bounded body with a tree");
    let mut mf = skull_mir::lower::lower_instance(&ws, &scx, &inst).expect("lower bounded instance");
    assert!(!tree.is_empty(), "the prepared bounded fixture has no incoming tree");
    let placeholder_ids: Vec<_> = mf
        .insts
        .iter()
        .filter_map(|(id, inst)| matches!(inst, Inst::CallBoundMulti { .. }).then_some(id))
        .collect();
    assert!(!placeholder_ids.is_empty(), "the standalone fixture has no placeholder");
    skull_transform::bound::resolve_standalone_bound_calls(
        &ws,
        &scx,
        scx.interner(),
        &mut mf,
        &tree,
    )
    .expect("resolve bounded instance");
    assert!(
        !mf.resolved_bound_trees.is_empty(),
        "standalone substitution did not retain the witness's resolved tree"
    );
    for id in placeholder_ids {
        assert!(
            mf.callee_instances.contains_key(&id),
            "the concrete bounded callee lost its owner identity at {id:?}"
        );
    }
}

#[test]
fn named_nested_functions_in_a_spliced_body_are_preserved() {
    let ws = check(
        r#"
@inline
fn outer(x: int) -> int {
    fn inner(y: int) -> int {
        fn child(z: int) -> int { return z + 1 }
        return child(y)
    }
    return inner(x)
}

fn top(x: int) -> int { return outer(x) + outer(x) }
fn main() { print("{top(2)}") }
"#,
    );
    let mf = prepared_body(&ws, "top");
    assert!(!mf.nested_fns.is_empty(), "the spliced named nested body was dropped");
    let names: Vec<_> = mf.nested_fns.iter().map(|nf| nf.name).collect();
    assert_eq!(names.len(), names.iter().collect::<std::collections::BTreeSet<_>>().len());
    assert!(!mf.nested_fns[0].nested_fns.is_empty(), "nested child body was dropped");
    let child_name = mf.nested_fns[0].nested_fns[0].name;
    assert!(
        mf.nested_fns[0].hoisted_fn_globals.contains(&child_name),
        "the nested parent kept no reference to its preserved child"
    );
    assert!(
        mf.hoisted_fn_globals.iter().any(|name| names.contains(name)),
        "the spliced body kept no reference to its preserved nested symbol"
    );
    let lua = crate::common::try_emit_module(
        r#"
@inline
fn outer(x: int) -> int {
    fn inner(y: int) -> int {
        fn child(z: int) -> int { return z + 1 }
        return child(y)
    }
    return inner(x)
}

fn top(x: int) -> int { return outer(x) + outer(x) }
fn main() { print("{top(2)} {top(3)}") }
"#,
    )
    .expect("a spliced named nested body must emit");
    assert_eq!(
        crate::common::run_luajit(&format!("{lua}\nmain()\n"), "spliced_nested"),
        "6 8"
    );
}

/// A synthesized column-storage call joins ONLY under its own operation.
///
/// A column-stored `Map` rewrites `m[k] = v` and `table_unset` into runtime
/// entries (`__map_col_slot`, `__map_col_free`). Those calls live at the SAME
/// expression as the source operation, so stamping them `DirectCall` makes them
/// collide with that expression's real callee -- and the callee check then
/// reports a mismatch rather than a join.
///
/// This is the control that dies when `SiteOp` is discarded: without the
/// operation half these sites resolve to the source call's callee and disagree
/// with the runtime entry they actually target.
#[test]
fn a_synthesized_column_call_joins_under_its_own_operation() {
    let c = census_of(
        r#"
value Tile { a: float, b: float }

fn build() -> int {
    let m: Map<string, Tile> = map_new()
    m["x"] = Tile { a: 1, b: 2 }
    m["y"] = Tile { a: 3, b: 4 }
    let n = len(m)
    clear(m)
    return n
}
fn main() { print("{build()}") }
"#,
    );
    assert!(c.accounts(), "accounting broken: {:?}", c.accounting_failures());
    // THE DENOMINATOR: the fixture must actually produce synthesized sites, and
    // they must actually JOIN. Without both, `mismatches == 0` below is
    // satisfied by a body with no column operation in it at all.
    let synth_examined = c.examined_by_op.get("synthesized").copied().unwrap_or(0);
    let synth_joined = c.joined_by_op.get("synthesized").copied().unwrap_or(0);
    assert!(
        synth_examined > 0,
        "no synthesized site examined, so this proves nothing about the operation half: {c:?}"
    );
    assert!(
        synth_joined > 0,
        "synthesized sites were examined ({synth_examined}) but none joined: {c:?}"
    );
    // No call may resolve to a callee it does not target. That is the exact
    // symptom of the operation being dropped: a runtime entry stamped
    // `DirectCall` matches its source expression's callee instead.
    let decl_mismatch = c.unjoined.get(&Unjoined::CalleeMismatch).copied().unwrap_or(0);
    assert_eq!(
        decl_mismatch, 0,
        "a call resolved to a callee it does not target -- the site operation is not \
         distinguishing synthesized runtime entries from their source call \
         (synthesized examined = {synth_examined}, joined = {synth_joined}): {c:?}"
    );
}

/// Pointwise operation accounting: a call filed under the WRONG operation must
/// be caught, even though every global total still balances.
///
/// Built by hand rather than from a fixture, because the census is correct on
/// real input -- the defect being guarded against is a future miscount, and the
/// only way to test the guard is to hand it one.
#[test]
fn a_call_filed_under_the_wrong_operation_fails_accounting() {
    use skull::site_join::{SiteJoinCensus, Unjoined};

    let mut c = SiteJoinCensus::default();
    c.obligations_examined = 2;
    c.joined = 1;
    c.joined_non_generic = 1;
    c.joined_rock = 1;
    c.unjoined.insert(Unjoined::NoSite, 1);
    c.examined_by_op.insert("direct-call", 1);
    c.examined_by_op.insert("synthesized", 1);
    c.joined_by_op.insert("direct-call", 1);
    c.unjoined_by_op.insert("synthesized", 1);
    assert!(
        c.accounting_failures().is_empty(),
        "the consistent census must pass, or the failure below proves nothing: {:?}",
        c.accounting_failures()
    );

    // Now MISFILE one call, on ONE side only: the join is recorded under
    // `synthesized` though it was examined under `direct-call`. Every global
    // total is unchanged -- examined 2, joined 1, unjoined 1, and all three
    // histograms still sum to their populations -- so only a pointwise check
    // can see it. (Swapping BOTH sides is a consistent relabel, not a
    // miscount, and correctly passes.)
    let mut bad = c.clone();
    bad.joined_by_op.clear();
    bad.joined_by_op.insert("synthesized", 1);
    assert_eq!(bad.examined_by_op.values().sum::<usize>(), bad.obligations_examined);
    assert_eq!(bad.joined_by_op.values().sum::<usize>(), bad.joined);
    assert_eq!(bad.unjoined_by_op.values().sum::<usize>(), bad.unjoined_total());
    let failures = bad.accounting_failures();
    assert!(
        failures.iter().any(|f| f.contains("operation `direct-call`")),
        "a call filed under the wrong operation passed accounting: {failures:?}"
    );
    assert!(
        failures.iter().any(|f| f.contains("operation `synthesized`")),
        "only one side of the misfiling was reported: {failures:?}"
    );

    // An operation that DISAPPEARS from every histogram leaves the pointwise
    // union with no key for it, so pointwise alone cannot see it. The global
    // sums are what catch it, which is why both are kept.
    let mut vanished = c.clone();
    vanished.examined_by_op.remove("synthesized");
    vanished.joined_by_op.remove("synthesized");
    vanished.unjoined_by_op.remove("synthesized");
    vanished.admitted_indirect_by_op.remove("synthesized");
    let vf = vanished.accounting_failures();
    assert!(
        vf.iter().any(|f| f.contains("examined-by-op")),
        "an operation vanished from every histogram and accounting passed: {vf:?}"
    );
    assert!(
        vf.iter().any(|f| f.contains("unjoined-by-op")),
        "only the examined side of the vanishing was reported: {vf:?}"
    );

    // And the CONSISTENT relabel (both sides moved) is not a miscount, so it
    // must pass -- otherwise this arm would fire on any renaming.
    let mut relabelled = c.clone();
    relabelled.joined_by_op.clear();
    relabelled.joined_by_op.insert("synthesized", 1);
    relabelled.unjoined_by_op.clear();
    relabelled.unjoined_by_op.insert("direct-call", 1);
    assert!(
        relabelled.accounting_failures().is_empty(),
        "a consistent relabel was reported as a miscount: {:?}",
        relabelled.accounting_failures()
    );
}

/// PREMISE TEST for the callee comparison, and the control that DIES if the
/// comparison is weakened back to declaration equality.
///
/// One generic declaration instantiated at two concrete types gives two
/// instances with the SAME `FuncRef`. Feeding each instance the OTHER's key --
/// FuncRef unchanged, generic arguments swapped -- must be an
/// `InstanceMismatch`. Under a declaration-only comparison both would be
/// accepted, which is exactly the defect this arm exists to catch.
#[test]
fn swapping_two_generic_keys_is_an_instance_mismatch() {
    let (ws, g) = graph_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() {
    let a = ident(1)
    let b = ident("s")
    print("{a} {b}")
}
"#,
    );
    // The two `ident` instances, and the key that names each.
    let insts: Vec<&skull::instance_graph::Instance> = g
        .substitutions()
        .map(|(i, _)| i)
        .filter(|i| ws.module(i.func.module).functions[i.func.func].name.as_str() == "ident")
        .collect();
    assert_eq!(insts.len(), 2, "fixture must produce TWO instances of one declaration: {insts:?}");
    let fref = insts[0].func;
    assert_eq!(insts[1].func, fref, "the two instances must share a FuncRef to be a control");

    // Each claim comes from its instance's OWN recorded substitution, never from
    // asking the oracle under test which candidate it likes.
    let c0 = instance_claim(&g, insts[0]);
    let c1 = instance_claim(&g, insts[1]);
    assert_eq!(c0.args.len(), 1, "fixture has one generic: {c0:?}");
    assert_eq!(c1.args.len(), 1, "fixture has one generic: {c1:?}");
    assert_ne!(c0.args, c1.args, "the two claims must differ, or the swap below is a no-op");

    // Matched: the instance and its OWN claim.
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, &rock(insts[0]), fref, Some(&c0), LUA),
        skull::site_join::CalleeCheck::Instance,
        "an instance did not verify against its own claim"
    );
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, &rock(insts[1]), fref, Some(&c1), LUA),
        skull::site_join::CalleeCheck::Instance
    );
    // SWAPPED: same FuncRef, other instantiation. A declaration-only comparison
    // returns agreement here; the instance comparison must not.
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, &rock(insts[0]), fref, Some(&c1), LUA),
        skull::site_join::CalleeCheck::InstanceMismatch,
        "two instantiations of one declaration compared equal -- the comparison is \
         declaration-level, not instance-level"
    );
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, &rock(insts[1]), fref, Some(&c0), LUA),
        skull::site_join::CalleeCheck::InstanceMismatch
    );
}

/// The DECLARATION half of the comparison, with the same shape: a callee
/// instance whose FuncRef is not the call's target must be a `DeclMismatch`.
/// Deleting the declaration check leaves this red.
#[test]
fn a_different_declaration_is_a_decl_mismatch() {
    let (ws, g) = graph_of(
        r#"
fn one(x: int) -> int { return x }
fn two(x: int) -> int { return x + 1 }
fn main() { print("{one(1)} {two(2)}") }
"#,
    );
    let find = |name: &str| -> skull::instance_graph::Instance {
        g.substitutions()
            .map(|(i, _)| i)
            .find(|i| ws.module(i.func.module).functions[i.func.func].name.as_str() == name)
            .expect("instance")
            .clone()
    };
    let one = find("one");
    let two = find("two");
    assert_ne!(one.func, two.func, "the fixture must give two declarations");
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, &rock(&one), one.func, None, LUA),
        skull::site_join::CalleeCheck::NonGeneric,
        "a non-generic callee against its own declaration must verify"
    );
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, &rock(&one), two.func, None, LUA),
        skull::site_join::CalleeCheck::DeclMismatch,
        "a callee resolved to another declaration was accepted"
    );
}

/// A generic callee whose call recorded NO concrete instance is UNVERIFIED, and
/// one whose record names another declaration is an invariant break -- kept
/// apart because only the first can be a consequence of policy.
///
/// The admission control is built INDEPENDENTLY, from the graph's own recorded
/// substitution, so the rejections are not circular.
#[test]
fn a_generic_callee_without_a_recorded_instance_is_unverified() {
    let (ws, g) = graph_of(
        r#"
fn pair<A, B>(a: A, b: B) -> A { return a }
fn other<A, B>(a: A, b: B) -> B { return b }
fn main() { print("{pair(1, 2.0)} {other(1, 2.0)}") }
"#,
    );
    let find = |n: &str| {
        g.substitutions()
            .map(|(i, _)| i)
            .find(|i| ws.module(i.func.module).functions[i.func.func].name.as_str() == n)
            .unwrap_or_else(|| panic!("no `{n}` instance"))
            .clone()
    };
    let inst = find("pair");
    let alien = find("other");
    assert_ne!(inst.func, alien.func, "the two declarations must differ");
    assert_eq!(
        ws.module(inst.func.module).functions[inst.func.func].generics.len(),
        2,
        "fixture must declare two generics"
    );

    let check = |c: Option<&skull::instance_graph::Instance>| {
        skull::site_join::check_callee(&ws, &g, &rock(&inst), inst.func, c, LUA)
    };

    // ADMISSION CONTROL: the instance's own recorded substitution verifies.
    let good = instance_claim(&g, &inst);
    assert_eq!(good.args.len(), 2, "one entry per generic: {good:?}");
    assert_eq!(
        check(Some(&good)),
        skull::site_join::CalleeCheck::Instance,
        "the independently-built correct claim did not verify, so the rejections below \
         prove nothing"
    );

    // No record at all -- the population conditional specialization used to own.
    assert_eq!(check(None), skull::site_join::CalleeCheck::NoInstance);

    // A record naming ANOTHER declaration. Not a policy outcome: the record is
    // written beside the call it belongs to, so this can only be a recorder bug.
    let wrong_decl = skull::instance_graph::Instance {
        func: alien.func,
        args: good.args.clone(),
    };
    assert_eq!(check(Some(&wrong_decl)), skull::site_join::CalleeCheck::ClaimDeclMismatch);

    // Right declaration, WRONG substitution: a partial one names no instance.
    let mut partial = good.clone();
    partial.args.pop();
    assert_eq!(check(Some(&partial)), skull::site_join::CalleeCheck::InstanceMismatch);
}

/// A BOUNDARY callee is verified as a boundary, not by reconstructing a Rock
/// instantiation it does not have. An opaque body is emitted once per
/// declaration and target, so its generic arguments do not participate in the
/// symbol emission spells -- and demanding a Rock instance record for one
/// reported the absence as an unverified call (bitgun: 215 sites).
///
/// The control is the Rock arm over the SAME instance: it still demands the
/// record, so this is not a blanket exemption for generic callees.
#[test]
fn a_generic_boundary_callee_verifies_as_a_boundary() {
    let (ws, g) = graph_of(
        r#"
fn main() {
    let v: [int] = []
    v.push(1)
    print("{v.len()}")
}
"#,
    );
    let mut g = g;
    let bodies = skull::concrete_body::build(&ws, &mut g);
    // Taken from the CALLEE TABLES rather than from a name: what makes this a
    // boundary is how the walk resolved it, and a generic declaration is what
    // makes the fixture non-vacuous.
    let (inst, class) = bodies
        .bodies
        .values()
        .flat_map(|b| b.callees())
        .find_map(|(_, c)| match c {
            skull::concrete_body::Callee::Boundary(i, cl)
                if !ws.module(i.func.module).functions[i.func.func].generics.is_empty() =>
            {
                Some((i.clone(), *cl))
            }
            _ => None,
        })
        .expect("the fixture must reach a GENERIC boundary, or it proves nothing");

    let boundary = skull::concrete_body::Callee::Boundary(inst.clone(), class);
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, &boundary, inst.func, None, LUA),
        skull::site_join::CalleeCheck::Boundary(class),
        "a generic boundary was asked for a Rock instantiation it cannot have"
    );
    // CONTROL: the same generic instance as a ROCK callee still demands the
    // record. Without this the arm above would read as "generic callees are
    // exempt", which is the opposite of what it says.
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, &rock(&inst), inst.func, None, LUA),
        skull::site_join::CalleeCheck::NoInstance,
        "the boundary exemption leaked to the Rock arm"
    );
}

/// `callee-resolved-unrecorded` is 0 over the corpus, and a zero is only
/// evidence if the bucket CAN become non-zero. Drop the resolved instance's
/// substitution from a cloned graph and the correct key must stop verifying --
/// there is nothing left to compare it against.
#[test]
fn a_resolved_instance_without_a_substitution_is_unrecorded() {
    let (ws, mut g) = graph_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() { print("{ident(1)}") }
"#,
    );
    let inst = g
        .substitutions()
        .map(|(i, _)| i)
        .find(|i| ws.module(i.func.module).functions[i.func.func].name.as_str() == "ident")
        .expect("instance")
        .clone();
    let claim = instance_claim(&g, &inst);
    // Control: with the substitution present, the claim verifies.
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, &rock(&inst), inst.func, Some(&claim), LUA),
        skull::site_join::CalleeCheck::Instance
    );
    // Remove ONLY the resolved instance's record -- the call still carries the
    // same complete claim, so nothing about the CALL changed.
    // Through the INDEX, which is where the record lives: removing the
    // instance's entry is removing every owner's opinion of it.
    let removed = g.index.subst.remove(&inst);
    g.root_instances.remove(&inst);
    assert!(removed.is_some(), "nothing was removed, so the arm below is untested");
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, &rock(&inst), inst.func, Some(&claim), LUA),
        skull::site_join::CalleeCheck::ResolvedUnrecorded,
        "a resolved instance with no recorded substitution was treated as agreement"
    );
}

#[test]
fn generic_calls_join_two_concrete_instances() {
    let src = r#"
@inline
fn wrap<T>(x: T) -> T { return id2(x) }
fn id2<T>(x: T) -> T { return x }

fn top() -> int {
    let a = wrap(1)
    let b = wrap(Vec2 { x: 1, y: 2 })
    return a + int(b.x)
}
fn main() { print("{top()}") }
"#;
    let (ws, g) = graph_of(src);
    let wraps: Vec<_> = g
        .substitutions()
        .filter(|(i, _)| ws.module(i.func.module).functions[i.func.func].name.as_str() == "wrap")
        .collect();
    assert_eq!(
        wraps.len(),
        2,
        "the fixture must reach both concrete `wrap` instances: {wraps:?}"
    );
    assert_eq!(
        wraps.iter().filter(|(_, subst)| !subst.is_empty()).count(),
        2,
        "a generic declaration must not use an empty specialization identity: {wraps:?}"
    );
    let c = {
        let mut graph = g;
        let bodies = skull::concrete_body::build(&ws, &mut graph);
        let scx = ScalarCtx::new(&ws).sharing(graph.interner().clone());
        site_join::census(&ws, &scx, &graph, &bodies)
    };
    assert!(c.joined_instance_verified >= 2, "{c:?}");
    assert_eq!(c.unjoined.get(&Unjoined::OwnerNotAnInstance), None, "{c:?}");
    assert_eq!(c.unjoined.get(&Unjoined::OwnerNotInTable), None, "{c:?}");
}

/// Candidate identity comes from `callee_instances`, not from the optional
/// standalone-delivery entry. Removing delivery from a concrete value call must
/// still splice that instance, and the spliced site's owner must remain the
/// callee declaration rather than the caller's slot.
#[test]
fn concrete_inline_uses_owner_when_delivery_is_absent() {
    let ws = check(
        r#"
@inline
fn wrap<T>(x: T) -> T { return id2(x) }
fn id2<T>(x: T) -> T { return x }
fn top() -> float {
    let b = wrap(Vec2 { x: 1, y: 2 })
    return b.x
}
fn main() { print("{top()}") }
"#,
    );
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
    let (fid, f) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "top")
        .expect("top");
    let fref = skull_mir::mir::FuncRef { module: mid, func: fid };
    let scx = ScalarCtx::new(&ws);
    let mut mf = skull_mir::lower::lower_instance(
        &ws,
        &scx,
        &skull_mir::instance::Instance::base(fref),
    )
    .expect("lower top");
    let (call, owner): (_, skull::instance_graph::Instance) = mf
        .insts
        .iter()
        .find_map(|(id, inst)| match inst {
            Inst::Call { func, .. } | Inst::CallMulti { func, .. }
                if ws.module(func.module).functions[func.func].name.as_str() == "wrap" =>
            {
                Some((id, mf.callee_instances.get(&id).cloned().expect("wrap owner")))
            }
            _ => None,
        })
        .expect("concrete wrap call");
    assert!(!owner.args.is_empty(), "the fixture must have a concrete wrap owner: {owner:?}");
    assert!(mf.spec_bindings.remove(&call).is_some(), "the fixture must have delivery to remove");

    let mangled = skull_mir::mangle::mangle_fn(&ws, f);
    let mut cache = skull_transform::InlineCache::default();
    skull::mir_emit::prepare_instance_body(
        &ws,
        &scx,
        fref,
        &Default::default(),
        &mangled,
        &mut mf,
        &mut cache,
    )
    .expect("prepare");

    let residual_wrap = mf.blocks.iter().flat_map(|(_, block)| block.insts.iter()).filter(|id| {
        matches!(&mf.insts[**id], Inst::Call { func, .. } | Inst::CallMulti { func, .. }
            if ws.module(func.module).functions[func.func].name.as_str() == "wrap")
    }).count();
    assert_eq!(residual_wrap, 0, "the concrete owner did not admit the splice");
    assert!(
        mf.call_sites.values().any(|cs| {
            let owner = &mf.site_owners[cs.owner.0 as usize];
            ws.module(owner.func.module).functions[owner.func.func].name.as_str() == "wrap"
        }),
        "the spliced body lost its concrete callee owner"
    );
}

/// Same-width value instances are still different ABIs. Repointing the call's
/// owner from `Vec2` to a two-int `Pair` must decline the splice even though the
/// result lane count and call form match; otherwise a plausible scalar shape
/// hides a semantically different concrete body.
#[test]
fn concrete_inline_declines_same_width_incompatible_abi() {
    let ws = check(
        r#"
value Pair { x: int, y: int }
@inline
fn wrap<T>(x: T) -> T { return x }
fn top() -> float {
    let b = wrap(Vec2 { x: 1, y: 2 })
    return b.x
}
fn main() { print("{top()}") }
"#,
    );
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
    let (fid, f) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "top")
        .expect("top");
    let fref = skull_mir::mir::FuncRef { module: mid, func: fid };
    let scx = ScalarCtx::new(&ws);
    let mut mf = skull_mir::lower::lower_instance(
        &ws,
        &scx,
        &skull_mir::instance::Instance::base(fref),
    )
    .expect("lower top");
    let call = mf
        .insts
        .iter()
        .find_map(|(id, inst)| match inst {
            Inst::CallMulti { func, .. }
                if ws.module(func.module).functions[func.func].name.as_str() == "wrap" =>
            {
                Some(id)
            }
            _ => None,
        })
        .expect("value call must use the multi-lane ABI");
    let wrap = mf.callee_instances[&call].func;
    let pair = skull_mir::instance::Instance::intern(
        scx.interner(),
        wrap,
        vec![MonoTy::App { ctor: MonoCtor::Nominal(Ustr::from("Pair")), params: Vec::new() }],
    );
    mf.callee_instances.insert(call, pair);
    assert!(mf.spec_bindings.remove(&call).is_some(), "the negative arm must remove delivery");

    let mangled = skull_mir::mangle::mangle_fn(&ws, f);
    let mut cache = skull_transform::InlineCache::default();
    skull::mir_emit::prepare_instance_body(
        &ws,
        &scx,
        fref,
        &Default::default(),
        &mangled,
        &mut mf,
        &mut cache,
    )
    .expect("an ABI mismatch declines rather than corrupting MIR");
    let residual_wrap = mf.blocks.iter().flat_map(|(_, block)| block.insts.iter()).filter(|id| {
        matches!(&mf.insts[**id], Inst::CallMulti { func, .. }
            if ws.module(func.module).functions[func.func].name.as_str() == "wrap")
    }).count();
    assert_eq!(residual_wrap, 1, "an incompatible same-width owner was spliced");
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
    assert!(c.obligations_examined > 0, "no calls examined");
    assert!(c.accounts(), "accounting identity broken: {c:?}");
    assert_eq!(
        c.joined + c.unjoined_total(),
        c.obligations_examined,
        "a call is neither joined nor given a cause"
    );
}

/// The join FAILS CLOSED on an owner the graph does not have, proved WITHOUT
/// depending on a defect being present.
///
/// Its sibling below witnesses the population today's conditional specializer
/// actually produces. That one silently disarms the day specialization goes
/// total -- the fixture stops producing a shared splice and the assertion has
/// nothing to fire on. This one removes an instance's substitution from a cloned
/// graph instead, so the arm can still be shown to fire once the defect is gone.
#[test]
fn an_owner_the_graph_does_not_have_is_refused() {
    // The callee must be one that SPLICES with a concrete owner today, or there
    // is no resolvable owner for the mutation to take away. `wrap<Vec2>` is
    // that: `@inline` plus a value-struct instantiation, which specializes.
    let src = r#"
@inline
fn wrap<T>(x: T) -> T { return id2(x) }
fn id2<T>(x: T) -> T { return x }
fn top() -> float {
    let b = wrap(Vec2 { x: 1, y: 2 })
    return b.x
}
fn main() { print("{top()}") }
"#;
    let (ws, mut g) = graph_of(src);
    let bodies = skull::concrete_body::build(&ws, &mut g);
    let scx = ScalarCtx::new(&ws).sharing(g.interner().clone());
    // CONTROL: with every substitution present, nothing is refused for want of
    // an owner. Without this the refusal below could be any other failure.
    // CONTROL: the calls `main` owns all join before the mutation. The fixture's
    // baseline is NOT zero -- the stdlib splices its own shared generics -- so
    // the claim is about the DELTA, measured on one instance, not about the
    // total.
    let before = site_join::census(&ws, &scx, &g, &bodies);
    assert!(before.joined > 0, "the control fixture joined nothing: {before:?}");
    let refused_before =
        before.unjoined.get(&Unjoined::OwnerNotAnInstance).copied().unwrap_or(0);

    assert!(before.joined_spliced > 0, "nothing spliced, so no owner is at stake: {before:?}");

    // Remove the SPLICED CALLEE's substitution -- one side only. `top`'s body is
    // still lowered under its own unchanged substitution and still stamps the
    // same owner on the spliced sites; all that is gone is the graph's ability
    // to name that owner. Perturbing the MEASURED instance instead would move
    // the recorded owner too, and a mutation applied to both sides of a
    // comparison cannot fail it.
    let victim = g
        .substitutions()
        .map(|(i, _)| i)
        .find(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str() == "wrap"
                && g.substitution(i).is_some_and(|s| !s.is_empty())
        })
        .expect("`wrap` instance with a concrete substitution")
        .clone();
    let removed = g.index.subst.remove(&victim);
    g.root_instances.remove(&victim);
    assert!(removed.is_some(), "nothing was removed, so the arm below is untested");
    let after = site_join::census(&ws, &scx, &g, &bodies);
    let refused_after =
        after.unjoined.get(&Unjoined::OwnerNotAnInstance).copied().unwrap_or(0);
    assert!(
        refused_after > refused_before,
        "removing the spliced callee's substitution refused nothing new ({refused_before} -> \
         {refused_after}), so its calls were matched to something the graph cannot name"
    );
    assert!(after.joined < before.joined, "nothing stopped joining: {after:?}");
    // The denominator DOES shrink -- the victim's own body can no longer be
    // lowered -- and that shrink is REPORTED rather than silent, which is the
    // property that keeps a falling denominator from flattering the ratio.
    assert!(
        after.bodies_skipped.contains_key(&skull::site_join::BodySkip::NoSubstitution),
        "the victim's body vanished from the space without a recorded cause: {after:?}"
    );
    assert!(after.accounts(), "accounting identity broken under the mutation: {after:?}");
    // And it REFUSED rather than reaching past the owner table.
    assert_eq!(
        after.unjoined.get(&Unjoined::OwnerNotInTable).copied().unwrap_or(0),
        0,
        "an owner index escaped its table: {after:?}"
    );
}

#[test]
fn generic_calls_join_distinct_concrete_instances() {
    let src = r#"
fn ident<T>(x: T) -> T { return x }
fn use_both() -> int {
    let a = ident(1)
    let b = ident(2.0)
    return a + int(b)
}
fn main() { print("{use_both()}") }
"#;
    let (ws, g) = graph_of(src);
    let idents: Vec<_> = g
        .substitutions()
        .filter(|(i, _)| ws.module(i.func.module).functions[i.func.func].name.as_str() == "ident")
        .collect();
    assert_eq!(idents.len(), 2, "expected two `ident` instances: {idents:?}");
    assert!(idents.iter().all(|(_, subst)| !subst.is_empty()), "{idents:?}");

    let c = {
        let mut graph = g;
        let bodies = skull::concrete_body::build(&ws, &mut graph);
        let scx = ScalarCtx::new(&ws).sharing(graph.interner().clone());
        site_join::census(&ws, &scx, &graph, &bodies)
    };
    assert!(c.obligations_examined > 0, "fixture examined no calls: {c:?}");
    assert!(
        c.joined_instance_verified >= 2,
        "the two generic calls did not verify concrete instances: {c:?}"
    );
    assert!(c.accounts(), "accounting identity broken: {c:?}");
    assert_eq!(c.unjoined.get(&Unjoined::OwnerNotAnInstance), None, "{c:?}");
    assert_eq!(c.unjoined.get(&Unjoined::OwnerNotInTable), None, "{c:?}");
}

// ---------------------------------------------------------------------------
// The site cursor, at both entries into expression lowering
// ---------------------------------------------------------------------------

/// A call in a SCALARIZED argument position carries its OWN expression.
///
/// `lower_expr_repr` is a second entry into expression lowering -- the value
/// route the scalar ABI takes -- and it did not narrow the site cursor. Every
/// call under an unboxed argument was therefore stamped with the ENCLOSING
/// expression, so it selected the enclosing expression's callee: bitgun filed
/// 138 calls as `callee-decl-mismatch` this way, `outer(inner(..))` naming
/// `outer` from the `inner` call and the reverse.
///
/// The fixture uses a value struct so the argument takes the unboxed route --
/// the boxed route already went through `lower_expr` and would pass either way.
/// The control is the boxed sibling below.
#[test]
fn a_call_under_a_scalarized_argument_carries_its_own_expression() {
    let c = census_of(
        r#"
value Pt { x: float, y: float }

fn pt(x: float, y: float) -> Pt { return Pt { x: x, y: y } }
fn len2(p: Pt) -> float { return p.x * p.x + p.y * p.y }

fn main() {
    print("{len2(pt(1.0, 2.0))}")
}
"#,
    );
    assert!(c.accounts(), "accounting identity broken: {c:?}");
    // Both calls exist and both joined: `pt` must not be filed under `len2`'s
    // expression, nor the reverse.
    assert_eq!(
        c.unjoined.get(&Unjoined::CalleeMismatch).copied().unwrap_or(0),
        0,
        "a nested call selected the enclosing expression's callee: {c:?}"
    );
    // A declaration mismatch of ZERO proves nothing if no nested call was
    // examined at all, so the population is asserted in the same run.
    assert!(
        c.joined >= 2,
        "the fixture examined too few joined calls to have exercised nesting: {c:?}"
    );
}

/// The nearest valid admission control: the same nesting through a BOXED
/// argument, which reaches lowering through `lower_expr` and always carried its
/// own expression. Without this, the test above could pass because nesting
/// stopped being lowered at all.
#[test]
fn a_call_under_a_boxed_argument_also_carries_its_own_expression() {
    let c = census_of(
        r#"
fn inner(x: int) -> int { return x + 1 }
fn outer(x: int) -> int { return x * 2 }

fn main() {
    print("{outer(inner(3))}")
}
"#,
    );
    assert!(c.accounts(), "accounting identity broken: {c:?}");
    assert_eq!(
        c.unjoined.get(&Unjoined::CalleeMismatch).copied().unwrap_or(0),
        0,
        "a nested call selected the enclosing expression's callee: {c:?}"
    );
    assert!(c.joined >= 2, "too few joined calls to have exercised nesting: {c:?}");
}

// ---------------------------------------------------------------------------
// The indirect class
// ---------------------------------------------------------------------------

/// An indirect call through a RUNTIME VALUE is the permanent non-direct class,
/// and it is admitted for a structural reason rather than by exhaustion:
/// emission renders the callee OPERAND (`__rock_call(f, ..)` on JS, `f(..)` on
/// Lua), so the call spells no symbol and has no target it could name wrongly.
///
/// The arm that keeps this from being a licence is
/// `indirect_unrecorded_symbol`: it reads the emitted NAME rather than the
/// records, so an operand that DOES spell a function is a finding here even
/// though the classifier would have called it a runtime value.
#[test]
fn an_indirect_call_through_a_value_is_the_permanent_non_direct_class() {
    let c = census_of(
        r#"
fn add_one(x: int) -> int { return x + 1 }

fn apply(f: fn(int) -> int, x: int) -> int { return f(x) }

fn main() { print("{apply(add_one, 3)}") }
"#,
    );
    assert!(c.accounts(), "accounting identity broken: {c:?}");
    let runtime = c
        .indirect_forms_by_producer
        .get(&skull::site_join::IndirectProducer::RuntimeValue)
        .copied()
        .unwrap_or(0);
    assert!(runtime > 0, "no indirect call through a value was examined: {c:?}");
    assert_eq!(
        c.indirect_unrecorded_symbol, 0,
        "an admitted indirect operand spells a workspace function symbol: {c:?}"
    );
}

/// A fn-value SYMBOL MINT joins against `ConcreteBody::fn_values`, including
/// when it is PASSED or STORED rather than called in place.
///
/// The mint is the one place a callee declaration becomes a spelled value, so it
/// is the only thing an indirect call through it could name wrongly. Verifying
/// it only where the minted `Inst::Global` is directly the operand of a
/// `CallValue` leaves every passed or stored mint verified NOWHERE -- it is
/// classified as a runtime value at the call and never compared with the graph's
/// `FnValue` obligation at the mint.
///
/// Three shapes in one fixture, each reaching the mint by a different route:
/// called in place, passed as an argument, stored in a local.
#[test]
fn a_fn_value_symbol_mint_joins_against_the_graphs_obligation() {
    let c = census_of(
        r#"
fn add_one(x: int) -> int { return x + 1 }
fn add_two(x: int) -> int { return x + 2 }
fn add_three(x: int) -> int { return x + 3 }

fn apply(f: fn(int) -> int, x: int) -> int { return f(x) }

fn main() {
    // PASSED as an argument: the mint never becomes a CallValue operand here.
    let a = apply(add_one, 1)
    // STORED in a local first, then called through the cell.
    let g = add_two
    let b = g(2)
    // Called through a local bound directly from the mint.
    let h = add_three
    let d = apply(h, 3)
    print("{a} {b} {d}")
}
"#,
    );
    assert!(c.accounts(), "accounting identity broken: {c:?}");
    // The DENOMINATOR must contain them, or every assertion below is vacuous.
    assert!(
        c.symbol_mints >= 3,
        "the fixture minted fewer than its three fn values, so the join is untested: {c:?}"
    );
    // Every mint joined: none fell into an unjoined cause.
    let mint_op_examined = c.examined_by_op.get("fn-value").copied().unwrap_or(0);
    let mint_op_joined = c.joined_by_op.get("fn-value").copied().unwrap_or(0);
    assert_eq!(
        mint_op_examined, c.symbol_mints,
        "a mint was examined under an operation other than `fn-value`: {c:?}"
    );
    assert_eq!(
        mint_op_joined, mint_op_examined,
        "a fn-value mint did not join against the graph's `FnValue` obligation: {c:?}"
    );
}

/// The mint's RECORD is compared BY THE CENSUS, not merely present: a record
/// naming a different function becomes a `callee-decl-mismatch` in the bucket,
/// while the mint stays in the denominator.
///
/// Asserted through `measure_prepared_body`, the same classifier production
/// runs, rather than by calling `check_callee` directly -- that would prove the
/// declaration comparison (already covered by
/// [`a_different_declaration_is_a_decl_mismatch`]) and NOT that a corrupted
/// record travels through `reachable_call_rows` into a census bucket.
///
/// One side only: the recorded declaration is perturbed while the emitted
/// instruction, its site and the graph are untouched, so the graph still
/// resolves the mint to the function the source named. Perturbing the source
/// instead would move both sides and could not fail.
#[test]
fn a_mint_recording_the_wrong_function_is_a_census_mismatch() {
    let src = r#"
fn add_one(x: int) -> int { return x + 1 }
fn add_two(x: int) -> int { return x + 2 }
fn apply(f: fn(int) -> int, x: int) -> int { return f(x) }
fn main() { print("{apply(add_one, 1)}") }
"#;
    let (ws, mut g) = graph_of(src);
    let bodies = skull::concrete_body::build(&ws, &mut g);
    let main_inst = g
        .expanded()
        .find(|i| ws.module(i.func.module).functions[i.func.func].name.as_str() == "main")
        .expect("`main` instance")
        .clone();

    let mut mf = prepared_body(&ws, "main");
    let mint = *mf
        .fn_value_symbols
        .keys()
        .next()
        .expect("`add_one` used as a value minted no recorded symbol");
    let recorded = mf.fn_value_symbols[&mint];
    assert_eq!(
        ws.module(recorded.module).functions[recorded.func].name.as_str(),
        "add_one",
        "the fixture recorded the wrong function before any mutation"
    );

    // CONTROL: unmodified, the mint is in the denominator and it JOINS.
    let scx = ScalarCtx::new(&ws).sharing(g.interner().clone());
    let before = skull::site_join::measure_prepared_body(&ws, &scx, &g, &bodies, &main_inst, &mf);
    assert_eq!(before.symbol_mints, 1, "the mint left the denominator: {before:?}");
    assert_eq!(
        before.joined_by_op.get("fn-value").copied().unwrap_or(0),
        1,
        "the correct record did not join, so the mismatch below proves nothing: {before:?}"
    );
    assert_eq!(
        before.unjoined.get(&Unjoined::CalleeMismatch).copied().unwrap_or(0),
        0,
        "the control already mismatches: {before:?}"
    );

    // The ALIEN declaration, taken from the workspace rather than invented.
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "add_two")
        .expect("`add_two`");
    let alien = skull_mir::mir::FuncRef { module: mid, func: fid };
    assert_ne!(alien, recorded, "the alien must differ, or the swap is a no-op");
    mf.fn_value_symbols.insert(mint, alien);

    let after = skull::site_join::measure_prepared_body(&ws, &scx, &g, &bodies, &main_inst, &mf);
    // The mint is still EXAMINED -- a corrupted record must not drop out of the
    // denominator, which would look like agreement.
    assert_eq!(
        after.symbol_mints, before.symbol_mints,
        "the corrupted mint left the denominator instead of failing: {after:?}"
    );
    assert_eq!(
        after.examined_by_op.get("fn-value").copied().unwrap_or(0),
        1,
        "the corrupted mint was examined under another operation: {after:?}"
    );
    assert_eq!(
        after.unjoined.get(&Unjoined::CalleeMismatch).copied().unwrap_or(0),
        1,
        "a mint recording another function was accepted by the census: {after:?}"
    );
    assert_eq!(
        after.joined_by_op.get("fn-value").copied().unwrap_or(0),
        0,
        "the corrupted mint still joined: {after:?}"
    );
    assert!(after.accounts(), "accounting identity broken: {after:?}");
}

// ---------------------------------------------------------------------------
// Place expressions
// ---------------------------------------------------------------------------

/// A synthesized call under an INDEX WRITE carries the `Expr::Index` node.
///
/// A place expression is never lowered as a value -- `lower_assign_effect`
/// dispatches on `expr_arena[target]` directly -- so nothing installed the site
/// cursor for it: in statement position the cursor was absent entirely and in
/// value position it named the enclosing assignment. The bounds check the write
/// synthesizes was therefore siteless (bitgun: 61 `__nb_checked_index`), while
/// the walk keys that obligation on the `Expr::Index` node itself.
///
/// The READ side is the control: it goes through `lower_expr_repr` and always
/// carried its site, so a fixture that only read would pass either way.
#[test]
fn a_bounds_check_under_an_index_write_carries_the_index_expression() {
    let c = census_of(
        r#"
value V2 { x: float, y: float }

fn fill(buf: NativeBuffer<V2>, n: int) {
    let i = 0
    while i < n {
        buf[i] = V2 { x: 1.0, y: 2.0 }
        i = i + 1
    }
}

fn main() {
    let b = NativeBuffer<V2>(4)
    fill(b, 4)
    print("{b[0].x}")
}
"#,
    );
    assert!(c.accounts(), "accounting identity broken: {c:?}");
    assert_eq!(
        c.unjoined.get(&Unjoined::NoSite).copied().unwrap_or(0),
        0,
        "a call under an index write carries no site: {c:?}"
    );
    assert_eq!(
        c.unjoined.get(&Unjoined::SiteNotInBody).copied().unwrap_or(0),
        0,
        "a call under an index write was stamped with an expression the body has no \
         obligation at -- the enclosing assignment rather than the index: {c:?}"
    );
    // The write must actually have happened, or neither zero means anything.
    assert!(c.joined > 0, "the fixture joined nothing: {c:?}");
}

/// A call inside a STRUCT-DEFAULT initializer JOINS, through its own arena.
///
/// `with_struct_default` installs the declaring struct's
/// `BodyOwner::StructDefaults` arena while `site_owner` still names the
/// enclosing function, so the call's `Idx<Expr>` is only meaningful against
/// THAT arena. `SiteDomain` is what states which one; before it existed the
/// stamp was withheld entirely, because an index read against the owner's arena
/// joins through whatever expression sits there -- a plausible wrong answer,
/// which is worse than a hole.
///
/// Both halves are asserted: the default's call joins, and the enclosing body's
/// own calls still do. A domain that swallowed the whole function would satisfy
/// neither.
#[test]
fn a_call_in_a_struct_default_joins_through_its_own_arena() {
    let c = census_of(
        r#"
fn seed() -> int { return 7 }

struct Cfg {
    n: int = seed(),
    m: int,
}

fn main() {
    let c = Cfg { m: 1 }
    print("{c.n + c.m}")
}
"#,
    );
    assert!(c.accounts(), "accounting identity broken: {c:?}");
    assert_eq!(
        c.unjoined.get(&Unjoined::SiteNotInBody).copied().unwrap_or(0),
        0,
        "the struct-default call reached MIR with no obligation recorded: {c:?}"
    );
    assert_eq!(
        c.unjoined.get(&Unjoined::NoSite).copied().unwrap_or(0),
        0,
        "the struct-default call carries no site at all: {c:?}"
    );
    // CONTROL: the enclosing body's own calls are unaffected.
    assert!(c.joined > 0, "the domain leaked to the enclosing body: {c:?}");
}
