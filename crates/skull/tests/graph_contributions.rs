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

fn build() -> (Workspace, InstanceGraph) {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(SRC, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let roots = instance_graph::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots, false);
    (ws, g)
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
    let kinds: BTreeSet<&str> = g.edges.iter().map(|e| e.kind.name()).collect();
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
    assert!(g.sites_attempted > 0, "the fixture attempted no sites");
    assert_eq!(g.sites_attempted, sum(|c| c.sites_attempted));
    assert_eq!(g.always_emitted_seeds, sum(|c| c.always_emitted_seeds));
    assert_eq!(g.synthesized_examined, sum(|c| c.synthesized_examined));
    assert_eq!(g.synthesized_approximated, sum(|c| c.synthesized_approximated));
    assert_eq!(g.edges.len(), sum(|c| c.edges.len()));
    assert_eq!(
        g.call_sites.values().map(|s| s.len()).sum::<usize>(),
        sum(|c| c.call_sites.len())
    );
}
