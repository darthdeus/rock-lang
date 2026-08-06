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

fn instance_key(
    g: &InstanceGraph,
    inst: &skull::instance_graph::Instance,
) -> skull_mir::lower::SpecKey {
    inst.args
        .iter()
        .map(|id| Some(g.arena.get(*id).clone()))
        .collect()
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
    let mf = prepared_body(&ws, "sum_all");
    // The fixture must actually CONTAIN the unstamped form. Without this the
    // emptiness below is satisfied by any body with no bounded loop at all --
    // the `print` call alone would do it -- and the sentinel pins nothing.
    let bound_calls = count_bound_multi(&mf);
    assert!(
        bound_calls > 0,
        "fixture emitted no `CallBoundMulti`, so it does not exercise the gap it pins"
    );
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
    c.call_insts = 2;
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
    assert_eq!(bad.examined_by_op.values().sum::<usize>(), bad.call_insts);
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
        .substitutions
        .keys()
        .filter(|i| ws.module(i.func.module).functions[i.func.func].name.as_str() == "ident")
        .collect();
    assert_eq!(insts.len(), 2, "fixture must produce TWO instances of one declaration: {insts:?}");
    let fref = insts[0].func;
    assert_eq!(insts[1].func, fref, "the two instances must share a FuncRef to be a control");

    // Build each key from its instance's declaration-ordered arguments, not a
    // hand-written type. `spec_subst_from_key` then recovers the declaration's
    // VarId, independently of the candidate-selection oracle under test.
    let k0 = instance_key(&g, insts[0]);
    let k1 = instance_key(&g, insts[1]);
    assert_eq!(k0.len(), 1, "fixture has one generic: {k0:?}");
    assert_eq!(k1.len(), 1, "fixture has one generic: {k1:?}");
    assert!(k0.iter().all(Option::is_some) && k1.iter().all(Option::is_some), "keys must be complete");
    assert_ne!(k0, k1, "the two keys must differ, or the swap below is a no-op");

    // Matched: the instance and its OWN key.
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, insts[0], fref, Some(&k0)),
        skull::site_join::CalleeCheck::Instance,
        "an instance did not verify against its own key"
    );
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, insts[1], fref, Some(&k1)),
        skull::site_join::CalleeCheck::Instance
    );
    // SWAPPED: same FuncRef, other instantiation. A declaration-only comparison
    // returns agreement here; the instance comparison must not.
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, insts[0], fref, Some(&k1)),
        skull::site_join::CalleeCheck::InstanceMismatch,
        "two instantiations of one declaration compared equal -- the comparison is \
         declaration-level, not instance-level"
    );
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, insts[1], fref, Some(&k0)),
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
        g.substitutions
            .keys()
            .find(|i| ws.module(i.func.module).functions[i.func.func].name.as_str() == name)
            .expect("instance")
            .clone()
    };
    let one = find("one");
    let two = find("two");
    assert_ne!(one.func, two.func, "the fixture must give two declarations");
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, &one, one.func, None),
        skull::site_join::CalleeCheck::NonGeneric,
        "a non-generic callee against its own declaration must verify"
    );
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, &one, two.func, None),
        skull::site_join::CalleeCheck::DeclMismatch,
        "a callee resolved to another declaration was accepted"
    );
}

/// A generic callee whose key cannot name an instance is UNVERIFIED, and each
/// way of failing is its own answer.
///
/// Key arity is POSITIONAL, and a wrong-length key has already been a real bug
/// on this branch -- it names another function's instantiation rather than
/// nothing -- so too-short, too-long and EMPTY join the partial case. The
/// too-short fixture is deliberately non-empty, because an empty key is a
/// different input from an absent one and both are covered.
#[test]
fn a_generic_callee_key_that_names_no_instance_is_unverified() {
    let (ws, g) = graph_of(
        r#"
fn pair<A, B>(a: A, b: B) -> A { return a }
fn main() { print("{pair(1, 2.0)}") }
"#,
    );
    let inst = g
        .substitutions
        .keys()
        .find(|i| ws.module(i.func.module).functions[i.func.func].name.as_str() == "pair")
        .expect("instance")
        .clone();
    let generics = ws.module(inst.func.module).functions[inst.func.func].generics.len();
    assert_eq!(generics, 2, "fixture must declare two generics");

    let check = |k: Option<&skull_mir::lower::SpecKey>| {
        skull::site_join::check_callee(&ws, &g, &inst, inst.func, k)
    };

    // ADMISSION CONTROL, built INDEPENDENTLY: the key is laid out in
    // declaration-generic order from the graph's own `Instance.args`, never by
    // asking the oracle which candidate it likes. A key the oracle helped choose
    // would make the rejections below circular.
    let good = instance_key(&g, &inst);
    assert_eq!(good.len(), generics, "independent key has one slot per generic: {good:?}");
    assert!(good.iter().all(Option::is_some), "independent key is complete: {good:?}");
    assert_eq!(
        check(Some(&good)),
        skull::site_join::CalleeCheck::Instance,
        "the independently-built correct key did not verify, so the rejections below \
         prove nothing about key shape"
    );

    let bindings: Vec<skull_mir::monoty::MonoTy> =
        good.iter().map(|b| b.clone().expect("complete")).collect();

    // No key at all.
    assert_eq!(check(None), skull::site_join::CalleeCheck::NoKey);
    // A PRESENT but EMPTY key is a different input from an absent one: it has
    // the wrong arity for a two-generic declaration.
    let empty: skull_mir::lower::SpecKey = Vec::new();
    assert_eq!(check(Some(&empty)), skull::site_join::CalleeCheck::KeyArity);
    // TOO SHORT, non-empty: one slot for a two-generic declaration still
    // positionally names a different instantiation.
    let short: skull_mir::lower::SpecKey = vec![Some(bindings[0].clone())];
    assert_eq!(check(Some(&short)), skull::site_join::CalleeCheck::KeyArity);
    // TOO LONG.
    let long: skull_mir::lower::SpecKey = vec![
        Some(bindings[0].clone()),
        Some(bindings[1].clone()),
        Some(bindings[0].clone()),
    ];
    assert_eq!(check(Some(&long)), skull::site_join::CalleeCheck::KeyArity);
    // Right length, holds a `None`.
    let partial: skull_mir::lower::SpecKey = vec![Some(bindings[0].clone()), None];
    assert_eq!(check(Some(&partial)), skull::site_join::CalleeCheck::PartialKey);
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
        .substitutions
        .keys()
        .find(|i| ws.module(i.func.module).functions[i.func.func].name.as_str() == "ident")
        .expect("instance")
        .clone();
    let key = instance_key(&g, &inst);
    // Control: with the substitution present, the key verifies.
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, &inst, inst.func, Some(&key)),
        skull::site_join::CalleeCheck::Instance
    );
    // Remove ONLY the resolved instance's record -- the call still carries the
    // same complete key, so nothing about the CALL changed.
    let removed = g.substitutions.remove(&inst);
    assert!(removed.is_some(), "nothing was removed, so the arm below is untested");
    assert_eq!(
        skull::site_join::check_callee(&ws, &g, &inst, inst.func, Some(&key)),
        skull::site_join::CalleeCheck::ResolvedUnrecorded,
        "a resolved instance with no recorded substitution was treated as agreement"
    );
}

/// SENTINEL -- flip when specialization becomes total (mono Stage 3, cut C).
///
/// The SUBSTITUTION half of the owner, which the non-generic inlining control
/// above cannot reach: one GENERIC `@inline` declaration instantiated at two
/// concrete types in ONE caller. CORRECT behavior is two owner entries with
/// distinct, NON-EMPTY substitutions, because both snapshots splice into the
/// same body and index the same expression arena -- the substitution is the
/// only thing that tells their sites apart.
///
/// TODAY specialization is CONDITIONAL, and this fixture straddles the
/// condition. `wrap<Vec2>` specializes (its element has value-struct storage,
/// which is a correctness trigger rather than an optimization) and its owner is
/// already concrete. `wrap<int>` does NOT: the lowerer keeps the shared generic
/// snapshot and splices it under the EMPTY substitution, which is not any
/// instance. That EMPTY entry is the minimal reproduction of the corpus's
/// largest unjoined bucket (`owner-not-an-instance`), and it is why
/// instance-owned definition emission must not land first.
///
/// When specialization is total this goes red: the EMPTY entry becomes a second
/// concrete substitution.
#[test]
fn an_inlined_generic_splices_one_shared_snapshot_sentinel() {
    let ws = check(
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
    );
    let mf = prepared_body(&ws, "top");
    let wrap_substs: Vec<&skull_mir::monoty::MonoSubst> = mf
        .site_owners
        .iter()
        .filter(|o| ws.module(o.func.module).functions[o.func.func].name.as_str() == "wrap")
        .map(|o| &o.subst)
        .collect();
    // The fixture must actually splice BOTH instantiations, or it pins nothing:
    // a single entry would mean one of the two calls vanished.
    assert_eq!(
        wrap_substs.len(),
        2,
        "the fixture must splice both instantiations of `wrap`, got {wrap_substs:?}"
    );
    let empty = wrap_substs.iter().filter(|s| s.is_empty()).count();
    let concrete = wrap_substs.iter().filter(|s| !s.is_empty()).count();
    // The CONTROL: the instantiation that already specializes. Without it the
    // assertion below could pass because nothing specializes at all, which is a
    // different defect wearing the same number.
    assert_eq!(
        concrete, 1,
        "the value-struct instantiation must already be instance-concrete: {wrap_substs:?}"
    );
    assert_eq!(
        empty, 1,
        "PINNED: `wrap<int>` splices the SHARED generic snapshot under the EMPTY \
         substitution. Zero here means specialization went total -- flip this test to \
         assert two distinct non-empty substitutions."
    );
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
    assert!(c.accounts(), "accounting identity broken: {c:?}");
    // The REFUSAL population must be non-zero, or this test passes without ever
    // exercising a shared generic splice -- the exact thing it claims to cover.
    let refused = c.unjoined.get(&Unjoined::OwnerNotAnInstance).copied().unwrap_or(0);
    assert!(
        refused > 0,
        "no shared-generic splice was refused, so the fail-closed path is untested: {c:?}"
    );
    // And nothing may reach past the owner table: that would be a guess, not a
    // refusal.
    let guessed = c.unjoined.get(&Unjoined::OwnerNotInTable).copied().unwrap_or(0);
    assert_eq!(guessed, 0, "an owner index escaped its table: {c:?}");
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

/// The other side of that admission: a function used as a VALUE mints a
/// workspace symbol, and the mint is RECORDED (`MirFunc::fn_value_symbols`) so
/// it is not swept into the permanent class. Without the record the operand
/// would be an unremarkable `Inst::Global` and the census would call it a
/// runtime value -- which is exactly what `indirect_unrecorded_symbol` refuses.
#[test]
fn a_function_used_as_a_value_records_its_minted_symbol() {
    let ws = check(
        r#"
fn add_one(x: int) -> int { return x + 1 }
fn take(f: fn(int) -> int) -> int { return f(1) }
fn main() { print("{take(add_one)}") }
"#,
    );
    let mf = prepared_body(&ws, "main");
    let minted: usize = mf.fn_value_symbols.len()
        + mf.nested_fns.iter().map(|n| n.fn_value_symbols.len()).sum::<usize>();
    assert!(
        minted > 0,
        "`add_one` used as a value minted no recorded symbol, so the fail-open \
         check has nothing to compare against"
    );
}
