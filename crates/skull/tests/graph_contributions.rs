//! The primary-record model behind the persistent updater
//! (`docs/parked/mono.md`): every flat field of an [`InstanceGraph`] is derived
//! from one [`Contribution`] per body, so a re-walk can REPLACE a body's facts
//! instead of the graph accumulating them.
//!
//! What is worth asserting here is what the derivation does NOT define. `edges`
//! and `reached` are written by two different recorders on two different lines
//! of the walk, so "the instances a root reaches over the edges" and "the
//! instances the walk said it reached" are genuinely two views, and their
//! agreement is a property rather than a restatement.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use bone_semantic::inference::Workspace;
use skull::instance_graph::{self, Instance, InstanceGraph, Owner};
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

const SRC: &str = r#"
struct Holder<T> { v: T }

fn passthru<T>(x: T) -> T { return x }
fn ident<T>(x: T) -> T { return passthru(x) }

co fn ticks(n: int) -> Coroutine<int> { yield n }

fn plain(a: int) -> int { return a + 1 }

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
    // Taken as a VALUE, so the walk records it through a different recorder
    // than a call -- the arm the cross-view check would otherwise not cover.
    let f: fn(int) -> int = plain
    print("{ident(1)} {ident("s")} {f(2)} {c.resume() ?? -1} {sum_all(v)} {g_total} {h.v}")
}
"#;

const POPULATION_SRC: &str = r#"
value Point { x: float, y: float }
struct Defaults { value: int = plain(7) }

fn passthru<T>(x: T) -> T { return x }
fn ident<T>(x: T) -> T { return passthru(x) }
fn plain(a: int) -> int { return a + 1 }
fn apply(f: fn(int) -> int) -> int { return f(1) }
co fn ticks(n: int) -> Coroutine<int> { yield n }

fn deep<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    let wrapped: [T] = [x]
    return 1 + deep(wrapped, n - 1)
}

fn sum_all(v: Vec<int>) -> int {
    let total = 0
    for x in v { total = total + x }
    for y in v.iter().map(_ * 2) { total = total + y }
    return total
}

global defaults: Defaults = Defaults {}

fn main() {
    let v: Vec<int> = vec![1, 2, 3]
    let c = ticks(3)
    let generic_value: fn(int) -> int = ident
    let m: Map<string, Point> = map_new()
    m["p"] = Point { x: 1.0, y: 2.0 }
    let point = m["p"]
    print("{ident(1)} {ident(\"s\")} {apply(plain)} {c.resume() ?? -1} {sum_all(v)} {deep(1, 3)} {defaults.value} {generic_value(1)} {point.x}")
}
"#;

fn build() -> (Workspace, InstanceGraph) {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(SRC, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots, false);
    (ws, g)
}

fn build_population() -> InstanceGraph {
    let mut modules = STDLIB.clone();
    modules.push(
        skull_parser::parse_module_clean(POPULATION_SRC, Ustr::from("<population-test>"))
            .expect("parse"),
    );
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    instance_graph::build(&ws, skull::mir_emit::emission_roots(&ws, None), false)
}

/// **Two independent views of one fact.** `instances` is the closure over
/// `Contribution::edges`; `reached` is what the walk's own instantiation
/// recorder wrote. A callee recorded as reached but not as an edge target
/// would be an instance nothing points at -- emitted with no caller -- and one
/// recorded as an edge target but never reached would carry no substitution.
#[test]
fn the_edge_closure_and_the_reach_record_name_the_same_instances() {
    let (_ws, g) = build();
    let mut reached: BTreeSet<Instance> = g.root_instances.clone();
    for c in g.contributions.values() {
        reached.extend(c.reached.iter().map(|(i, _)| i.clone()));
    }
    assert!(!reached.is_empty(), "the fixture reached nothing");
    // Each recorder in the walk is its own arm, so the fixture has to exercise
    // more than one of them or the check covers one line.
    //
    // What it can DISCRIMINATE is narrower than what it covers, and the
    // difference is worth stating: this harness passes `emission_roots(ws,
    // None)`, under which every declaration in the test module is a root, so a
    // recorder that dropped a reach to a LOCAL callee leaves it live through
    // `root_instances`. The arm bites on instances no root names -- every
    // specialization, and every stdlib callee reached transitively -- which is
    // where an updater gets reachability wrong in the first place. Verified by
    // mutation: dropping the `GenericCall` reach fails both arms below.
    let kinds: BTreeSet<&str> = g.edges().map(|e| e.kind.name()).collect();
    for required in ["direct-call", "generic-call", "fn-value", "bound-next-witness"] {
        assert!(kinds.contains(required), "the fixture has no `{required}` edge: {kinds:?}");
    }
    assert_eq!(
        reached.len(),
        g.instances.len(),
        "the reach record and the edge closure disagree on the instance population"
    );
    assert_eq!(reached, g.instances, "the two views name different instances");
}

/// Every live instance owns a contribution, so the derivation has a record for
/// every fact it must be able to remove. A boundary's contribution is empty and
/// present, which is the distinction `expanded` carries.
#[test]
fn every_live_instance_owns_a_contribution() {
    let (ws, g) = build();
    let missing: Vec<String> = g
        .instances
        .iter()
        .filter(|i| !g.contributions.contains_key(&Owner::Instance((*i).clone())))
        .map(|i| g.identity(&ws, i))
        .collect();
    assert!(missing.is_empty(), "live instances with no primary record: {missing:?}");
    assert!(
        g.contributions.len() > g.instances.len(),
        "the always-emitted bodies own contributions too: {} owners vs {} instances",
        g.contributions.len(),
        g.instances.len()
    );
}

/// The censuses are DERIVED from the primary records rather than accumulated
/// beside them, so a count that drifted from the population it summarizes is a
/// failure here rather than a silently wrong number.
#[test]
fn the_operational_censuses_are_sums_of_the_primary_records() {
    let (_ws, g) = build();
    let sum = |f: fn(&skull::instance_graph::Contribution) -> usize| -> usize {
        g.contributions.values().map(f).sum()
    };
    let census = &g.run_stats;
    assert!(g.sites_attempted > 0, "the fixture attempted no sites");
    assert_eq!(g.sites_attempted, sum(|c| c.sites_attempted));
    assert_eq!(g.always_emitted_seeds, sum(|c| c.always_emitted_seeds));
    assert_eq!(g.synthesized_examined, sum(|c| c.synthesized_examined));
    assert_eq!(g.synthesized_approximated, sum(|c| c.synthesized_approximated));
    assert_eq!(census.body_exprs_scanned, sum(|c| c.body_exprs_scanned));
    assert_eq!(census.resolved_overload_rows, sum(|c| c.resolved_overload_rows));
    assert_eq!(census.fn_value_rows, sum(|c| c.fn_value_rows));
    assert_eq!(census.generated_rows, sum(|c| c.generated_rows));
    assert_eq!(census.dispatched_rows, sum(|c| c.dispatched_rows));
    assert_eq!(census.implicit_iter_rows, sum(|c| c.implicit_iter_rows));
    assert_eq!(census.bound_tree_rows, sum(|c| c.bound_tree_rows));
    assert_eq!(census.bound_call_rows, sum(|c| c.bound_call_rows));
    assert_eq!(census.iterator_rows, sum(|c| c.iterator_rows));
    assert_eq!(g.edges().count(), sum(|c| c.edges.len()));
    assert_eq!(
        g.call_sites().map(|(_, m)| m).map(|s| s.len()).sum::<usize>(),
        sum(|c| c.call_sites.len())
    );
    assert!(census.owner_visits > 0 && census.first_owner_visits > 0);
    assert!(census.census_enabled, "the graph fixture did not enable its census");
    assert!(
        census.body_exprs_scanned > 0
            && census.resolved_overload_rows > 0
            && census.fn_value_rows > 0
            && census.bound_tree_rows > 0
            && census.bound_call_rows > 0
            && census.iterator_rows > 0,
        "the repeated body-fact census has no production population: {census:?}"
    );
    assert!(
        census.first_owner_visits > 1 && census.body_exprs_scanned > census.first_owner_visits,
        "the fixture did not expose repeated source-body materialization: {census:?}"
    );
    assert!(census.frontier_count > 1, "the fixture exposed no wavefront expansion: {census:?}");
    assert_eq!(
        census.frontier_ready_owners + census.frontier_same_frontier_dependency_owners,
        census.expanded,
        "frontier readiness did not partition expanded owners: {census:?}"
    );
    assert!(
        census.ordinary_owner_visits > 0
            && census.always_emitted_owner_visits > 0
            && census.frontier_owner_p95 >= census.frontier_owner_p50
            && census.frontier_owner_max >= census.frontier_owner_p95,
        "owner-class or frontier-size census is empty: {census:?}"
    );
    assert!(
        census.expansion_ns_total > 0
            && census.expansion_ns_p50 > 0
            && census.expansion_ns_p95 >= census.expansion_ns_p50
            && census.expansion_ns_max >= census.expansion_ns_p95,
        "per-owner expansion census is empty: {census:?}"
    );
    assert!(
        census.mono_intern_attempts > 0
            && census.mono_new_types > 0
            && census.mono_write_acqs > 0
            && census.mono_read_acqs > 0
            && census.mono_interner_self_ns > 0
            && census.mono_timed_mutation_calls > 0
            && census.mono_timed_mutation_calls as u64 == census.mono_write_acqs,
        "graph interner census is empty: {census:?}"
    );
    assert!(
        census.contribution_reached > 0
            && census.contribution_bound_trees > 0
            && census.contribution_edges > 0
            && census.contribution_sites > 0
            && census.contribution_always_emitted_targets > 0,
        "contribution population census is empty: {census:?}"
    );
    assert!(
        census.index_subst_unique > 0
            && census.index_trees_unique > 0
            && census.index_always_emitted_unique > 0
            && census.index_preds_unique > 0
            && census.index_decls_unique > 0,
        "unique graph-index census is empty: {census:?}"
    );
    assert!(
        census.index_erased_updates >= census.index_erased_unique,
        "erased-entry unique count exceeds its update denominator: {census:?}"
    );
    assert!(
        census.duplicate_pending_suppressed > 0,
        "the fixture must expose duplicate pending owners before deduplication"
    );
    assert_eq!(
        census.duplicate_pending_visits, 0,
        "the production pending queue should not revisit an owner already queued"
    );
    assert_eq!(
        census.owner_visits,
        census.first_owner_visits
            + census.duplicate_pending_visits
            + census.witness_settlement_rewalks
            + census.incremental_forced_rewalks
            + census.skipped_stranded_owners,
        "the graph census denominator lost an owner population"
    );
    assert_eq!(
        census.expanded,
        census.first_owner_visits
            + census.duplicate_pending_visits
            + census.witness_settlement_rewalks
            + census.incremental_forced_rewalks,
        "the graph census expanded denominator lost an owner population"
    );
    assert!(
        census.index_subst_updates > 0
            && census.index_trees_updates > 0
            && census.index_always_emitted_updates > 0
            && census.index_preds_updates > 0
            && census.index_decls_updates > 0,
        "the graph fixture did not expose the maintained-index populations: {census:?}"
    );
    assert_eq!(
        census.index_maintenance_ns,
        census.index_subst_ns
            + census.index_trees_ns
            + census.index_erased_ns
            + census.index_always_emitted_ns
            + census.index_preds_ns
            + census.index_decls_ns
            + census.index_commit_ns,
        "graph index timing attribution lost or double-counted a lane: {census:?}"
    );
}

#[test]
fn the_graph_population_gate_reaches_every_contribution_family() {
    let g = build_population();
    let c = &g.run_stats;
    assert!(c.ordinary_owner_visits > 0, "no ordinary owners: {c:?}");
    assert!(c.always_emitted_owner_visits > 0, "no always-emitted owners: {c:?}");
    assert!(c.resolved_overload_rows > 0, "no ordinary call rows: {c:?}");
    assert!(c.bound_tree_rows > 0, "no bound-tree rows: {c:?}");
    assert!(c.iterator_rows > 0, "no iterator rows: {c:?}");
    assert!(c.fn_value_rows > 0, "no function-value rows: {c:?}");
    assert!(c.generated_rows > 0, "no synthesized/generated rows: {c:?}");
    assert!(c.contribution_synthesized_examined > 0, "no synthesized-call census: {c:?}");
    assert!(c.contribution_rejections > 0, "no rejection population: {c:?}");
    assert!(c.contribution_struct_default_domains > 0, "no struct-default domains: {c:?}");
}
