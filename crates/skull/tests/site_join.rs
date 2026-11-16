//! Controls for the MIR-call -> graph site records join
//! (`crate::support::site_join`). The join's value is that a call instruction
//! selects the RIGHT concrete body, so the properties are about two things
//! that must not collapse onto one key, plus mutation arms proving each half
//! of the identity is load-bearing.

use crate::common::{
    census_of, census_of_graph, fn_name, fn_ref, graph_of as graph_of_ws, prepared_body,
    scalar_ctx, unjoined, ws_with_stdlib,
};
use crate::support::site_join::{self, CalleeCheck, SiteJoinCensus, Unjoined, check_callee};
use bone_semantic::inference::Workspace;
use skull::concrete_body::ConcreteBodies;
use skull::instance_graph::{Instance, InstanceGraph};
use skull_mir::lower::ScalarCtx;
use skull_mir::mir::{FuncRef, Inst, MirFunc};
use skull_mir::monoty::{MonoCtor, MonoTy};
use skull_mir::site::{Site, SiteDomain, SiteOp};
use std::collections::BTreeMap;
use ustr::Ustr;

/// The backend the census lowers for; `check_callee` asks the emitter's own
/// declaration selection, so the comparison needs to know which target.
const LUA: bone_semantic::hir::FnBackend = bone_semantic::hir::FnBackend::Lua;

fn graph_of(src: &str) -> (Workspace, InstanceGraph) {
    let ws = ws_with_stdlib(src);
    let g = graph_of_ws(&ws);
    (ws, g)
}

fn concrete(ws: &Workspace) -> (InstanceGraph, ConcreteBodies, ScalarCtx) {
    let g = graph_of_ws(ws);
    let bodies = skull::concrete_body::build(ws, &g);
    let scx = scalar_ctx(ws, &g);
    (g, bodies, scx)
}

fn instance_of(ws: &Workspace, g: &InstanceGraph, name: &str) -> Instance {
    g.substitutions()
        .map(|(i, _)| i)
        .find(|i| fn_name(ws, i.func) == name)
        .unwrap_or_else(|| panic!("no `{name}` instance"))
        .clone()
}

fn by_op(m: &BTreeMap<&'static str, usize>, op: &str) -> usize {
    m.get(op).copied().unwrap_or(0)
}

/// `name`'s base instance, lowered but not yet prepared.
fn lowered(ws: &Workspace, bodies: &ConcreteBodies, scx: &ScalarCtx, name: &str) -> MirFunc {
    let plan = bodies
        .plans
        .get(&skull_mir::instance::Instance::base(fn_ref(ws, name)))
        .expect("concrete plan");
    skull_mir::lower::lower_instance_with_plans(ws, scx, plan, &bodies.plans).expect("lower")
}

/// The concrete (non-empty substitution) instance of the generic `name`,
/// lowered but not yet prepared.
fn lowered_concrete_generic(
    ws: &Workspace,
    bodies: &ConcreteBodies,
    scx: &ScalarCtx,
    name: &str,
) -> MirFunc {
    let fref = fn_ref(ws, name);
    let (_, plan) = bodies
        .plans
        .iter()
        .find(|(inst, _)| inst.func == fref && !inst.args.is_empty())
        .expect("concrete generic instance");
    skull_mir::lower::lower_instance_with_plans(ws, scx, plan, &bodies.plans).expect("lower")
}

/// Run emission's transform sequence over `name`'s lowered body.
fn prepare(ws: &Workspace, scx: &ScalarCtx, bodies: &ConcreteBodies, name: &str, mf: &mut MirFunc) {
    let fref = fn_ref(ws, name);
    let mangled = skull_mir::mangle::mangle_fn(ws, &ws.module(fref.module).functions[fref.func]);
    let mut cache = skull_transform::InlineCache::default();
    skull::mir_emit::prepare_instance_body_with_plans(
        ws,
        scx,
        fref,
        &mangled,
        mf,
        &mut cache,
        &bodies.plans,
    )
    .expect("prepare");
}

/// Reachable-arena direct calls to the declaration named `name`.
fn calls_to(ws: &Workspace, mf: &MirFunc, name: &str) -> usize {
    mf.blocks
        .iter()
        .flat_map(|(_, block)| block.insts.iter())
        .filter(|id| {
            matches!(&mf.insts[**id], Inst::Call { func, .. } | Inst::CallMulti { func, .. }
                if fn_name(ws, *func) == name)
        })
        .count()
}

/// Every recorded site in `mf`, including hoisted nested bodies.
fn sites(mf: &MirFunc) -> Vec<Site> {
    let mut out: Vec<Site> = mf.call_sites.values().map(|c| c.site.clone()).collect();
    for nf in &mf.nested_fns {
        out.extend(sites(nf));
    }
    out
}

fn count_insts(mf: &MirFunc, pred: fn(&Inst) -> bool) -> usize {
    mf.insts.iter().filter(|(_, i)| pred(i)).count()
        + mf.nested_fns
            .iter()
            .map(|nf| count_insts(nf, pred))
            .sum::<usize>()
}

fn count_bound_multi(mf: &MirFunc) -> usize {
    count_insts(mf, |i| matches!(i, Inst::CallBoundMulti { .. }))
}

fn count_bound_splits(mf: &MirFunc) -> usize {
    mf.bound_next_split.len() + mf.nested_fns.iter().map(count_bound_splits).sum::<usize>()
}

// ---------------------------------------------------------------------------
// Distinguishability
// ---------------------------------------------------------------------------

/// A bounded loop's `next` witness (`CallBoundMulti`) reaches MIR with a
/// `for-next` site on the caller-generic path as well as the concrete one,
/// keyed on the `for` head's expression. It carries a site but no target.
#[test]
fn a_bounded_loops_witness_reaches_mir_recorded() {
    let ws = ws_with_stdlib(
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
    let (_, bodies, scx) = concrete(&ws);
    let mf = lowered_concrete_generic(&ws, &bodies, &scx, "sum_all");
    let bound_calls = count_bound_multi(&mf);
    assert!(bound_calls > 0, "fixture emitted no `CallBoundMulti`");
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
    let heads: std::collections::BTreeSet<_> = witnesses.iter().map(|w| w.expr).collect();
    assert_eq!(
        heads.len(),
        1,
        "one bounded loop, one head expression: {witnesses:?}"
    );
}

/// One expression reached under different operations stays distinct, even
/// for one variant with different conformance paths, and `ForNext` stays
/// distinct from `BoundNext` at the same path (a `for` over a bounded call
/// carries both). Asserted on the type: no fixture is guaranteed to produce a
/// nested tree.
#[test]
fn one_expression_under_different_operations_stays_distinct() {
    let ws = ws_with_stdlib("fn main() { print(\"x\") }");
    let mf = prepared_body(&ws, "main");
    let expr = mf
        .call_sites
        .values()
        .next()
        .expect("main calls something")
        .site
        .expr;
    let ops = [
        SiteOp::DirectCall,
        SiteOp::FnValue,
        SiteOp::ForIter,
        SiteOp::ForNext(vec![].into()),
        SiteOp::ForNext(vec![0].into()),
        SiteOp::BoundNext(vec![].into()),
        SiteOp::BoundNext(vec![0].into()),
        SiteOp::ImplicitIter,
        SiteOp::Synthesized("__map_col_slot"),
        SiteOp::Synthesized("__map_col_free"),
    ];
    let mut set = std::collections::BTreeSet::new();
    for op in ops {
        let site = Site {
            domain: SiteDomain::Own,
            expr,
            op,
        };
        assert!(
            set.insert(site.clone()),
            "two operations at one expression collapsed: {site:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Survival through remapping and inlining
// ---------------------------------------------------------------------------

/// Inlining clones a callee's instructions under a new `Idx` space. Their
/// sites are remapped with them, and their OWNER re-interned into the
/// caller's table, where index 0 is the caller.
#[test]
fn sites_survive_inlining_and_carry_the_callee_owner() {
    let ws = ws_with_stdlib(
        r#"
@inline
fn leaf(x: int) -> int { return double(x) }
fn double(x: int) -> int { return x + x }

fn top(x: int) -> int { return leaf(x) }
fn main() { print("{top(3)}") }
"#,
    );
    let mf = prepared_body(&ws, "top");
    let owners: Vec<&str> = mf
        .call_sites
        .values()
        .map(|cs| fn_name(&ws, mf.site_owners[cs.owner.0 as usize].func))
        .collect();
    assert!(
        owners.contains(&"leaf"),
        "no spliced call attributed to `leaf`; owners = {owners:?}"
    );
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
    let ws = ws_with_stdlib(
        r#"
@inline
fn sum_inner(xs: [int]) -> int {
    let total = 0
    for x in xs.iter() { total = total + x }
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

#[test]
fn cloned_bounded_value_calls_consume_split_placeholders() {
    let ws = ws_with_stdlib(
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
    let (_, bodies, scx) = concrete(&ws);
    let raw = lowered_concrete_generic(&ws, &bodies, &scx, "sum_all");
    assert!(
        count_bound_multi(&raw) > 0,
        "fixture has no bounded placeholder"
    );
    assert!(
        !raw.bound_next_split.is_empty(),
        "fixture has no split placeholder"
    );
    let mf = prepared_body(&ws, "main");
    assert_eq!(
        count_bound_multi(&mf),
        0,
        "a concrete bounded call survived splicing: {mf:?}"
    );
    assert_eq!(
        count_bound_splits(&mf),
        0,
        "a split marker survived after its bounded placeholder was consumed"
    );
}

#[test]
fn concrete_lowering_carries_the_callee_bound_plan_to_the_call() {
    let ws = ws_with_stdlib(
        r#"
fn sum_all<I: Iterator<int>>(it: I) -> int {
    let total = 0
    for v in it { total = total + v }
    return total
}

fn main() { print("{sum_all([1, 2, 3].iter())}") }
"#,
    );
    let (_, bodies, scx) = concrete(&ws);
    let main_plan = &bodies.plans[&skull_mir::instance::Instance::base(fn_ref(&ws, "main"))];
    let callee_plan = main_plan
        .callees
        .values()
        .find_map(|inst| {
            bodies
                .plans
                .get(&inst.instance)
                .filter(|plan| !plan.bound_tree.is_empty())
        })
        .expect("main does not retain a bounded callee plan");
    let mf = lowered(&ws, &bodies, &scx, "main");
    assert!(
        !mf.resolved_bound_trees.is_empty(),
        "the concrete call lost its bound plan"
    );
    assert!(
        mf.resolved_bound_trees
            .values()
            .any(|tree| tree == &callee_plan.bound_tree),
        "the call carried a different resolved tree than its concrete callee"
    );
}

#[test]
fn standalone_bounded_resolution_records_a_nonempty_resolved_tree() {
    let (ws, g) = graph_of(
        r#"
struct Item { id: int, p: float }

fn main() {
    let xs = [Item { id: 1, p: 3.0 }, Item { id: 2, p: 2.0 }]
    let ids = xs.map(_.id).collect()
    print("{ids[0]}")
}
"#,
    );
    let bodies = skull::concrete_body::build(&ws, &g);
    let scx = scalar_ctx(&ws, &g);
    let lower = |inst: &Instance| {
        let plan = bodies.plans.get(inst)?;
        skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &bodies.plans).ok()
    };
    let (mut mf, tree) = g
        .bound_trees()
        .filter(|(candidate, tree)| {
            tree.next.values().any(|w| !w.sub.next.is_empty())
                && ws.module(candidate.func.module).functions[candidate.func.func]
                    .generic_bounds
                    .iter()
                    .any(Option::is_some)
        })
        .find_map(|(candidate, tree)| {
            let mf = lower(candidate)?;
            (count_bound_multi(&mf) > 0).then(|| (mf, tree.to_resolved()))
        })
        .expect("the fixture has no prepared bounded body with a tree");
    assert!(
        !tree.is_empty(),
        "the prepared bounded fixture has no incoming tree"
    );
    let placeholder_ids: Vec<_> = mf
        .insts
        .iter()
        .filter_map(|(id, inst)| matches!(inst, Inst::CallBoundMulti { .. }).then_some(id))
        .collect();
    assert!(
        !placeholder_ids.is_empty(),
        "the standalone fixture has no placeholder"
    );
    skull_transform::bound::resolve_standalone_bound_calls(
        &ws,
        &scx,
        scx.interner(),
        &mut mf,
        &tree,
        &bodies.plans,
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
    let src = r#"
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
"#;
    let mf = prepared_body(&ws_with_stdlib(src), "top");
    assert!(
        !mf.nested_fns.is_empty(),
        "the spliced named nested body was dropped"
    );
    let names: Vec<_> = mf.nested_fns.iter().map(|nf| nf.name).collect();
    assert_eq!(
        names.len(),
        names
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );
    assert!(
        !mf.nested_fns[0].nested_fns.is_empty(),
        "nested child body was dropped"
    );
    let child_name = mf.nested_fns[0].nested_fns[0].name;
    assert!(
        mf.nested_fns[0].hoisted_fn_globals.contains(&child_name),
        "the nested parent kept no reference to its preserved child"
    );
    assert!(
        mf.hoisted_fn_globals
            .iter()
            .any(|name| names.contains(name)),
        "the spliced body kept no reference to its preserved nested symbol"
    );
    let lua = crate::common::try_emit_module(src).expect("a spliced named nested body must emit");
    assert_eq!(crate::common::run_main(&lua), "6 8");
}

/// Accounting must catch a call filed under the WRONG operation although every
/// global total balances, and an operation that vanishes from every
/// histogram, which leaves no key for the pointwise check. Built by hand: the
/// census is correct on real input, so only a hand-made miscount tests the
/// guard.
#[test]
fn a_call_filed_under_the_wrong_operation_fails_accounting() {
    let mut c = SiteJoinCensus {
        obligations_examined: 2,
        joined: 1,
        joined_non_generic: 1,
        joined_rock: 1,
        ..SiteJoinCensus::default()
    };
    c.unjoined.insert(Unjoined::NoSite, 1);
    c.examined_by_op.insert("direct-call", 1);
    c.examined_by_op.insert("synthesized", 1);
    c.joined_by_op.insert("direct-call", 1);
    c.unjoined_by_op.insert("synthesized", 1);
    assert!(
        c.accounting_failures().is_empty(),
        "the consistent census must pass: {:?}",
        c.accounting_failures()
    );

    // Misfile the join on one side only; every global total is unchanged.
    let mut bad = c.clone();
    bad.joined_by_op.clear();
    bad.joined_by_op.insert("synthesized", 1);
    assert_eq!(
        bad.examined_by_op.values().sum::<usize>(),
        bad.obligations_examined
    );
    assert_eq!(bad.joined_by_op.values().sum::<usize>(), bad.joined);
    assert_eq!(
        bad.unjoined_by_op.values().sum::<usize>(),
        bad.unjoined_total()
    );
    let failures = bad.accounting_failures();
    for op in ["direct-call", "synthesized"] {
        assert!(
            failures
                .iter()
                .any(|f| f.contains(&format!("operation `{op}`"))),
            "misfiling not reported for `{op}`: {failures:?}"
        );
    }

    let mut vanished = c.clone();
    vanished.examined_by_op.remove("synthesized");
    vanished.joined_by_op.remove("synthesized");
    vanished.unjoined_by_op.remove("synthesized");
    vanished.admitted_indirect_by_op.remove("synthesized");
    let vf = vanished.accounting_failures();
    for sum in ["examined-by-op", "unjoined-by-op"] {
        assert!(
            vf.iter().any(|f| f.contains(sum)),
            "a vanished operation passed the `{sum}` check: {vf:?}"
        );
    }

    // Moving both sides is a consistent relabel, not a miscount.
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

// ---------------------------------------------------------------------------
// The callee comparison
// ---------------------------------------------------------------------------

/// Two instances of one generic share a `FuncRef`; feeding each the OTHER's
/// claim must be an `InstanceMismatch`, which a declaration-only comparison
/// would accept.
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
    let insts: Vec<&Instance> = g
        .substitutions()
        .map(|(i, _)| i)
        .filter(|i| fn_name(&ws, i.func) == "ident")
        .collect();
    assert_eq!(
        insts.len(),
        2,
        "fixture must produce TWO instances: {insts:?}"
    );
    let fref = insts[0].func;
    assert_eq!(
        insts[1].func, fref,
        "the two instances must share a FuncRef"
    );
    let (c0, c1) = (insts[0].clone(), insts[1].clone());
    assert_eq!((c0.args.len(), c1.args.len()), (1, 1));
    assert_ne!(c0.args, c1.args, "the two claims must differ");

    let check =
        |inst: &Instance, claim: &Instance| check_callee(&ws, &g, inst, fref, Some(claim), LUA);
    assert_eq!(check(insts[0], &c0), CalleeCheck::Instance);
    assert_eq!(check(insts[1], &c1), CalleeCheck::Instance);
    assert_eq!(
        check(insts[0], &c1),
        CalleeCheck::InstanceMismatch,
        "two instantiations of one declaration compared equal"
    );
    assert_eq!(check(insts[1], &c0), CalleeCheck::InstanceMismatch);
}

/// A callee instance whose declaration is not the call's target is a
/// `DeclMismatch`; against its own declaration a non-generic callee verifies.
#[test]
fn a_different_declaration_is_a_decl_mismatch() {
    let (ws, g) = graph_of(
        r#"
fn one(x: int) -> int { return x }
fn two(x: int) -> int { return x + 1 }
fn main() { print("{one(1)} {two(2)}") }
"#,
    );
    let one = instance_of(&ws, &g, "one");
    let two = instance_of(&ws, &g, "two");
    assert_ne!(one.func, two.func);
    assert_eq!(
        check_callee(&ws, &g, &one, one.func, None, LUA),
        CalleeCheck::NonGeneric
    );
    assert_eq!(
        check_callee(&ws, &g, &one, two.func, None, LUA),
        CalleeCheck::DeclMismatch,
        "a callee resolved to another declaration was accepted"
    );
}

/// A generic callee with no recorded instance is unverified; a record naming
/// another declaration is a recorder bug; a partial substitution names no
/// instance. The admission control is the graph's own substitution.
#[test]
fn a_generic_callee_without_a_recorded_instance_is_unverified() {
    let (ws, g) = graph_of(
        r#"
fn pair<A, B>(a: A, b: B) -> A { return a }
fn other<A, B>(a: A, b: B) -> B { return b }
fn main() { print("{pair(1, 2.0)} {other(1, 2.0)}") }
"#,
    );
    let inst = instance_of(&ws, &g, "pair");
    let alien = instance_of(&ws, &g, "other");
    assert_ne!(inst.func, alien.func);
    assert_eq!(inst.args.len(), 2, "fixture must declare two generics");
    let check = |c: Option<&Instance>| check_callee(&ws, &g, &inst, inst.func, c, LUA);

    assert_eq!(check(Some(&inst)), CalleeCheck::Instance);
    assert_eq!(check(None), CalleeCheck::NoInstance);
    let wrong_decl = Instance {
        func: alien.func,
        args: inst.args.clone(),
    };
    assert_eq!(check(Some(&wrong_decl)), CalleeCheck::ClaimDeclMismatch);
    let mut partial = inst.clone();
    partial.args.pop();
    assert_eq!(check(Some(&partial)), CalleeCheck::InstanceMismatch);
}

/// A generic boundary callee verifies by its declaration and class.
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
    let (inst, class) = g
        .call_sites()
        .flat_map(|(_, sites)| sites.values())
        .find_map(|record| {
            let skull::instance_graph::SiteOutcome::Resolved(i) = &record.outcome else {
                return None;
            };
            let function = &ws.module(i.func.module).functions[i.func.func];
            if function.generics.is_empty() {
                return None;
            }
            skull::instance_graph::boundary_class_for(function).map(|class| (i.clone(), class))
        })
        .expect("the fixture must reach a GENERIC boundary");
    assert_eq!(
        check_callee(&ws, &g, &inst, inst.func, None, LUA),
        CalleeCheck::Boundary(class)
    );
}

/// Dropping the resolved instance's substitution from the graph must stop the
/// correct claim from verifying: there is nothing left to compare it with.
#[test]
fn a_resolved_instance_without_a_substitution_is_unrecorded() {
    let (ws, mut g) = graph_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() { print("{ident(1)}") }
"#,
    );
    let inst = instance_of(&ws, &g, "ident");
    assert_eq!(
        check_callee(&ws, &g, &inst, inst.func, Some(&inst), LUA),
        CalleeCheck::Instance
    );
    assert!(g.index.subst.remove(&inst).is_some(), "nothing was removed");
    g.root_instances.remove(&inst);
    assert_eq!(
        check_callee(&ws, &g, &inst, inst.func, Some(&inst), LUA),
        CalleeCheck::ResolvedUnrecorded,
        "a resolved instance with no recorded substitution was treated as agreement"
    );
}

/// Generic calls at two concrete types, called directly or spliced through
/// an inlined generic wrapper, each verify their own concrete instance.
#[test]
fn generic_calls_join_distinct_concrete_instances() {
    for (generic, src) in [
        (
            "wrap",
            r#"
@inline
fn wrap<T>(x: T) -> T { return id2(x) }
fn id2<T>(x: T) -> T { return x }

fn top() -> int {
    let a = wrap(1)
    let b = wrap(Vec2 { x: 1, y: 2 })
    return a + int(b.x)
}
fn main() { print("{top()}") }
"#,
        ),
        (
            "ident",
            r#"
fn ident<T>(x: T) -> T { return x }
fn use_both() -> int {
    let a = ident(1)
    let b = ident(2.0)
    return a + int(b)
}
fn main() { print("{use_both()}") }
"#,
        ),
    ] {
        let (ws, g) = graph_of(src);
        let instances: Vec<_> = g
            .substitutions()
            .filter(|(i, _)| fn_name(&ws, i.func) == generic)
            .collect();
        assert_eq!(instances.len(), 2, "{generic}: {instances:?}");
        assert!(
            instances.iter().all(|(_, subst)| !subst.is_empty()),
            "{generic}: a generic declaration used an empty identity: {instances:?}"
        );
        let c = census_of_graph(&ws, &g);
        assert!(c.joined_instance_verified >= 2, "{generic}: {c:?}");
        assert!(c.accounts(), "{generic}: {c:?}");
        assert_eq!(
            unjoined(&c, Unjoined::OwnerNotAnInstance),
            0,
            "{generic}: {c:?}"
        );
        assert_eq!(
            unjoined(&c, Unjoined::OwnerNotInTable),
            0,
            "{generic}: {c:?}"
        );
    }
}

const WRAP_VEC2: &str = r#"
@inline
fn wrap<T>(x: T) -> T { return id2(x) }
fn id2<T>(x: T) -> T { return x }
fn top() -> float {
    let b = wrap(Vec2 { x: 1, y: 2 })
    return b.x
}
fn main() { print("{top()}") }
"#;

/// Candidate identity comes from `callee_instances`, not from the optional
/// delivery entry: with delivery removed the concrete call still splices,
/// and the spliced sites keep the callee as owner.
#[test]
fn concrete_inline_uses_owner_when_delivery_is_absent() {
    let ws = ws_with_stdlib(WRAP_VEC2);
    let (_, bodies, scx) = concrete(&ws);
    let mut mf = lowered(&ws, &bodies, &scx, "top");
    let call = mf
        .insts
        .iter()
        .find_map(|(id, inst)| match inst {
            Inst::Call { func, .. } | Inst::CallMulti { func, .. }
                if fn_name(&ws, *func) == "wrap" =>
            {
                Some(id)
            }
            _ => None,
        })
        .expect("concrete wrap call");
    assert!(
        !mf.callee_instances[&call].args.is_empty(),
        "the fixture must have a concrete wrap owner"
    );
    assert!(
        mf.spec_bindings.remove(&call).is_some(),
        "the fixture must have delivery to remove"
    );
    prepare(&ws, &scx, &bodies, "top", &mut mf);
    assert_eq!(
        calls_to(&ws, &mf, "wrap"),
        0,
        "the concrete owner did not admit the splice"
    );
    assert!(
        mf.call_sites
            .values()
            .any(|cs| fn_name(&ws, mf.site_owners[cs.owner.0 as usize].func) == "wrap"),
        "the spliced body lost its concrete callee owner"
    );
}

/// Same-width value instances are still different ABIs: repointing the call's
/// owner from `Vec2` to a two-int `Pair` must decline the splice.
#[test]
fn concrete_inline_declines_same_width_incompatible_abi() {
    let ws = ws_with_stdlib(
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
    let (_, bodies, scx) = concrete(&ws);
    let mut mf = lowered(&ws, &bodies, &scx, "top");
    let call = mf
        .insts
        .iter()
        .find_map(|(id, inst)| match inst {
            Inst::CallMulti { func, .. } if fn_name(&ws, *func) == "wrap" => Some(id),
            _ => None,
        })
        .expect("value call must use the multi-lane ABI");
    let pair = skull_mir::instance::Instance::intern(
        scx.interner(),
        mf.callee_instances[&call].func,
        vec![MonoTy::App {
            ctor: MonoCtor::Nominal(Ustr::from("Pair")),
            params: Vec::new(),
        }],
    );
    mf.callee_instances.insert(call, pair);
    assert!(
        mf.spec_bindings.remove(&call).is_some(),
        "the negative arm must remove delivery"
    );
    prepare(&ws, &scx, &bodies, "top", &mut mf);
    assert_eq!(
        calls_to(&ws, &mf, "wrap"),
        1,
        "an incompatible same-width owner was spliced"
    );
}

/// Every call that survives to this simple emitted body carries a site.
#[test]
fn every_call_in_a_simple_prepared_body_carries_a_site() {
    let ws = ws_with_stdlib(
        r#"
fn a(x: int) -> int { return x + 1 }
fn b(x: int) -> int { return a(x) + a(x + 1) }
fn main() { print("{b(2)}") }
"#,
    );
    let mf = prepared_body(&ws, "b");
    let calls = count_insts(&mf, |i| {
        matches!(
            i,
            Inst::Call { .. }
                | Inst::CallMulti { .. }
                | Inst::CallValue { .. }
                | Inst::CallValueMulti { .. }
        )
    });
    assert!(calls > 0, "fixture emitted no calls at all");
    assert_eq!(
        sites(&mf).len(),
        calls,
        "a call in this body carries no site"
    );
}

// ---------------------------------------------------------------------------
// The census population
// ---------------------------------------------------------------------------

/// Census fixtures, each with the property it must show. Every census must
/// also satisfy every accounting identity. Each property pairs a zero bucket
/// with a population, so the zero cannot pass over a fixture that never
/// exercised it.
#[test]
fn census_fixtures_join_their_calls() {
    let cases: [(&str, &str, fn(&SiteJoinCensus) -> bool); 11] = [
        (
            // `print` is a boundary while `helper` is Rock.
            "non-generic Rock and boundary calls both join",
            r#"
fn helper(x: int) -> int { return x * 2 }
fn main() {
    let v = helper(21)
    print("{v}")
}
"#,
            |c| c.bodies_measured > 0 && c.joined_rock > 0 && c.joined_boundary > 0,
        ),
        (
            "generics, inlining and iteration: every call joined or given a cause",
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
            |c| {
                c.obligations_examined > 0
                    && c.joined + c.unjoined_total() == c.obligations_examined
            },
        ),
        (
            // Column-Map runtime entries sit at the source operation's
            // expression; without the operation half of the site they would
            // resolve to that expression's callee.
            "synthesized column calls join under their own operation",
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
            |c| {
                by_op(&c.joined_by_op, "synthesized") > 0
                    && unjoined(c, Unjoined::CalleeMismatch) == 0
            },
        ),
        (
            // `lower_expr_repr`, the scalar ABI's entry into expression
            // lowering, must narrow the site cursor to the nested call.
            "a call under a scalarized argument carries its own expression",
            r#"
value Pt { x: float, y: float }

fn pt(x: float, y: float) -> Pt { return Pt { x: x, y: y } }
fn len2(p: Pt) -> float { return p.x * p.x + p.y * p.y }

fn main() {
    print("{len2(pt(1.0, 2.0))}")
}
"#,
            |c| c.joined >= 2 && unjoined(c, Unjoined::CalleeMismatch) == 0,
        ),
        (
            "a call under a boxed argument carries its own expression",
            r#"
fn inner(x: int) -> int { return x + 1 }
fn outer(x: int) -> int { return x * 2 }

fn main() {
    print("{outer(inner(3))}")
}
"#,
            |c| c.joined >= 2 && unjoined(c, Unjoined::CalleeMismatch) == 0,
        ),
        (
            // An indirect call renders its operand, so it spells no symbol;
            // the name-based check keeps that from being a licence.
            "an indirect call through a value is admitted and spells no symbol",
            r#"
fn add_one(x: int) -> int { return x + 1 }

fn apply(f: fn(int) -> int, x: int) -> int { return f(x) }

fn main() { print("{apply(add_one, 3)}") }
"#,
            |c| {
                let runtime = c
                    .indirect_forms_by_producer
                    .get(&site_join::IndirectProducer::RuntimeValue);
                runtime.is_some_and(|n| *n > 0) && c.indirect_unrecorded_symbol == 0
            },
        ),
        (
            // A mint passed or stored rather than called in place is verified
            // only as its own `fn-value` obligation.
            "fn-value mints passed, stored and called through a local all join",
            r#"
fn add_one(x: int) -> int { return x + 1 }
fn add_two(x: int) -> int { return x + 2 }
fn add_three(x: int) -> int { return x + 3 }

fn apply(f: fn(int) -> int, x: int) -> int { return f(x) }

fn main() {
    let a = apply(add_one, 1)
    let g = add_two
    let b = g(2)
    let h = add_three
    let d = apply(h, 3)
    print("{a} {b} {d}")
}
"#,
            |c| {
                let examined = by_op(&c.examined_by_op, "fn-value");
                c.symbol_mints >= 3
                    && examined == c.symbol_mints
                    && by_op(&c.joined_by_op, "fn-value") == examined
            },
        ),
        (
            // A place expression is never lowered as a value, so the bounds
            // check an index write synthesizes needs the `Expr::Index` site.
            "a bounds check under an index write carries the index expression",
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
            |c| {
                c.joined > 0
                    && unjoined(c, Unjoined::NoSite) == 0
                    && unjoined(c, Unjoined::SiteNotInBody) == 0
            },
        ),
        (
            // The default's call indexes the struct-defaults arena, which
            // `SiteDomain` names; the enclosing body's calls still join.
            "a call in a struct default joins through its own arena",
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
            |c| {
                c.joined > 0
                    && unjoined(c, Unjoined::NoSite) == 0
                    && unjoined(c, Unjoined::SiteNotInBody) == 0
            },
        ),
        (
            // A non-null value has no absent arm, so lowering must not
            // synthesize the `__map_col_free` call the graph never models.
            "a non-null column map store synthesizes no free",
            r#"
value P { x: float, y: float }
fn make(a: float) -> P { return P { x: a, y: a } }
fn retain(m: Map<string, P>, v: P?) {
    if v == null { return }
    m["v"] = v
}
fn main() {
    let m: Map<string, P> = map_new()
    m["k"] = make(1.0)
    retain(m, make(2.0))
    print("{len(m)}")
}
"#,
            |c| c.joined > 0 && unjoined(c, Unjoined::SiteNotInBody) == 0,
        ),
        (
            "descriptor field writes join their copy-on-write calls",
            r#"
struct Owner { nb: NativeBuffer<float> }
global OWNED: NativeBuffer<float> = native_buffer_float(4)
fn shrink(o: Owner) {
    o.nb.len = 1
    OWNED.len = 3
}
fn main() {
    shrink(Owner { nb: native_buffer_float(2) })
    print("{OWNED.len}")
}
"#,
            |c| by_op(&c.joined_by_op, "synthesized") >= 2 && c.unjoined.is_empty(),
        ),
    ];
    for (property, src, holds) in cases {
        let c = census_of(src);
        assert!(
            c.accounts(),
            "{property}: accounting broken: {:?}",
            c.accounting_failures()
        );
        assert!(holds(&c), "{property}: {c:?}");
    }
}

/// The join fails closed on an owner the graph does not have. Removing the
/// spliced callee's substitution (one side only: the measured body still
/// stamps the same owner) must refuse new calls as `owner-not-an-instance`,
/// record the victim's lost body by cause, and never reach past the owner
/// table.
#[test]
fn an_owner_the_graph_does_not_have_is_refused() {
    let (ws, mut g) = graph_of(WRAP_VEC2);
    let bodies = skull::concrete_body::build(&ws, &g);
    let scx = scalar_ctx(&ws, &g);
    // The stdlib splices its own generics, so the claim is about the delta.
    let before = site_join::census(&ws, &scx, &g, &bodies);
    assert!(before.joined > 0, "the control joined nothing: {before:?}");
    assert!(before.joined_spliced > 0, "nothing spliced: {before:?}");
    let victim = g
        .substitutions()
        .map(|(i, _)| i)
        .find(|i| {
            fn_name(&ws, i.func) == "wrap" && g.substitution(i).is_some_and(|s| !s.is_empty())
        })
        .expect("`wrap` instance with a concrete substitution")
        .clone();
    assert!(
        g.index.subst.remove(&victim).is_some(),
        "nothing was removed"
    );
    g.root_instances.remove(&victim);
    let after = site_join::census(&ws, &scx, &g, &bodies);
    assert!(
        unjoined(&after, Unjoined::OwnerNotAnInstance)
            > unjoined(&before, Unjoined::OwnerNotAnInstance),
        "removing the spliced callee's substitution refused nothing new: {after:?}"
    );
    assert!(
        after.joined < before.joined,
        "nothing stopped joining: {after:?}"
    );
    assert!(
        after
            .bodies_skipped
            .contains_key(&site_join::BodySkip::NoSubstitution),
        "the victim's body vanished without a recorded cause: {after:?}"
    );
    assert!(
        after.accounts(),
        "accounting broken under the mutation: {after:?}"
    );
    assert_eq!(
        unjoined(&after, Unjoined::OwnerNotInTable),
        0,
        "an owner index escaped its table: {after:?}"
    );
}

/// The census compares a mint's RECORD, not merely its presence: recording
/// another function must move the mint from joined to `callee-decl-mismatch`
/// while it stays in the denominator. Only the record is perturbed, so the
/// graph still resolves the mint to the function the source named.
#[test]
fn a_mint_recording_the_wrong_function_is_a_census_mismatch() {
    let (ws, g) = graph_of(
        r#"
fn add_one(x: int) -> int { return x + 1 }
fn add_two(x: int) -> int { return x + 2 }
fn apply(f: fn(int) -> int, x: int) -> int { return f(x) }
fn main() { print("{apply(add_one, 1)}") }
"#,
    );
    let bodies = skull::concrete_body::build(&ws, &g);
    let scx = scalar_ctx(&ws, &g);
    let main_inst = g
        .expanded()
        .find(|i| fn_name(&ws, i.func) == "main")
        .expect("`main` instance")
        .clone();
    let mut mf = prepared_body(&ws, "main");
    let mint = *mf
        .fn_value_symbols
        .keys()
        .next()
        .expect("`add_one` used as a value minted no recorded symbol");
    assert_eq!(fn_name(&ws, mf.fn_value_symbols[&mint]), "add_one");
    let measure =
        |mf: &MirFunc| site_join::measure_prepared_body(&ws, &scx, &g, &bodies, &main_inst, mf);

    let before = measure(&mf);
    assert_eq!(before.symbol_mints, 1, "{before:?}");
    assert_eq!(by_op(&before.joined_by_op, "fn-value"), 1, "{before:?}");
    assert_eq!(unjoined(&before, Unjoined::CalleeMismatch), 0, "{before:?}");

    let alien: FuncRef = fn_ref(&ws, "add_two");
    mf.fn_value_symbols.insert(mint, alien);
    let after = measure(&mf);
    assert_eq!(
        after.symbol_mints, 1,
        "the corrupted mint left the denominator: {after:?}"
    );
    assert_eq!(by_op(&after.examined_by_op, "fn-value"), 1, "{after:?}");
    assert_eq!(
        unjoined(&after, Unjoined::CalleeMismatch),
        1,
        "a mint recording another function was accepted by the census: {after:?}"
    );
    assert_eq!(by_op(&after.joined_by_op, "fn-value"), 0, "{after:?}");
    assert!(after.accounts(), "accounting broken: {after:?}");
}
