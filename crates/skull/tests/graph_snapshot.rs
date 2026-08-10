//! The canonical-snapshot oracle the persistent graph updater will be gated on
//! (`docs/parked/mono.md`).
//!
//! Two properties, and the second is what makes the first worth anything: the
//! snapshot must be STABLE across builds that mean the same thing, and it must
//! CHANGE for any one-sided semantic change. A snapshot that omitted a field
//! would satisfy stability perfectly and detect nothing.

use std::sync::LazyLock;

use bone_semantic::inference::Workspace;
use skull::graph_snapshot::{canonical, CanonicalGraph};
use skull::instance_graph::{self, InstanceGraph};
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

co fn ticks(n: int) -> Coroutine<int> {
    yield n
}

fn plain(a: int) -> int { return a + 1 }

fn main() {
    let c = ticks(3)
    print("{ident(1)} {ident("s")} {plain(2)} {c.resume() ?? -1}")
}
"#;

fn build(perturb: bool) -> (Workspace, InstanceGraph) {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(SRC, Ustr::from("<test>")).expect("parse"));
    let ws =
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let roots = instance_graph::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots, perturb);
    (ws, g)
}

fn snap(ws: &Workspace, g: &InstanceGraph) -> CanonicalGraph {
    canonical(ws, g)
}

/// Two builds of one program that differ only in DISCOVERY ORDER mean the same
/// thing, so the canonical form must not be able to tell them apart. This is
/// the same property `S 20g.4` asserts of the rendered manifest, at the level
/// the updater will actually be compared on.
#[test]
fn discovery_order_does_not_change_the_canonical_form() {
    let (ws_a, a) = build(false);
    let (ws_b, b) = build(true);
    let (sa, sb) = (snap(&ws_a, &a), snap(&ws_b, &b));
    let d = sa.diff(&sb);
    assert!(d.is_empty(), "perturbing discovery order moved the canonical form:\n{}", d.join("\n"));
}

/// The fixture must actually populate the sections this file mutates below, or
/// each of those arms passes over an empty set.
#[test]
fn the_fixture_populates_the_sections_under_test() {
    let (ws, g) = build(false);
    let s = snap(&ws, &g);
    for section in ["instances", "expanded", "edges", "substitutions", "call_sites"] {
        assert!(
            s.0.get(section).is_some_and(|v| !v.is_empty()),
            "section `{section}` is empty, so a mutation of it would prove nothing"
        );
    }
    assert!(
        !g.coroutine_helpers.is_empty(),
        "the fixture's `co fn` must produce coroutine helpers"
    );
}

/// **A one-sided semantic change must fail equality, in EVERY section.**
///
/// Each arm removes or adds exactly one fact and requires the canonical form to
/// notice. This is a completeness check on the oracle itself: a field left out
/// of `canonical` cannot fail an equality check, and would show up here as a
/// mutation nothing detects.
#[test]
fn every_semantic_section_is_observed() {
    let (ws, base) = build(false);
    let before = snap(&ws, &base);

    // (name, mutation). Each takes a fresh clone of the built graph.
    let mutations: Vec<(&str, Box<dyn Fn(&mut InstanceGraph)>)> = vec![
        ("instances", Box::new(|g: &mut InstanceGraph| {
            let victim = g.instances.iter().next().expect("an instance").clone();
            g.instances.remove(&victim);
        })),
        ("expanded", Box::new(|g: &mut InstanceGraph| {
            let victim = g.expanded.iter().next().expect("an expanded instance").clone();
            g.expanded.remove(&victim);
        })),
        ("edges", Box::new(|g: &mut InstanceGraph| {
            g.edges.pop().expect("an edge");
        })),
        ("substitutions", Box::new(|g: &mut InstanceGraph| {
            let victim = g.substitutions.keys().next().expect("a substitution").clone();
            g.substitutions.remove(&victim);
        })),
        ("call_sites", Box::new(|g: &mut InstanceGraph| {
            let victim = g.call_sites.keys().next().expect("a call site owner").clone();
            g.call_sites.remove(&victim);
        })),
        ("coroutine_helpers", Box::new(|g: &mut InstanceGraph| {
            let victim = g.coroutine_helpers.iter().next().expect("a helper").clone();
            g.coroutine_helpers.remove(&victim);
        })),
        // The four stale-state producers, each cleared independently: a fixed
        // body must drop ITS entry, and an updater that clears one of these
        // while leaving another behind keeps reporting an error the program no
        // longer has.
        ("rejections", Box::new(|g: &mut InstanceGraph| {
            let at = g.instances.iter().next().expect("an instance").clone();
            g.rejections.push(skull::instance_graph::Rejection {
                kind: skull::instance_graph::RejectionKind::ChangingInstanceRecursion,
                at,
                span: bone_semantic::prelude::Span::unknown(),
                message: "synthetic".to_string(),
            });
        })),
        ("unresolved", Box::new(|g: &mut InstanceGraph| {
            let from = g.instances.iter().next().expect("an instance").clone();
            let callee = from.func;
            g.unresolved.push(skull::instance_graph::Unresolved {
                from,
                callee,
                reason: "synthetic",
                span: bone_semantic::prelude::Span::unknown(),
            });
        })),
        ("depth_capped", Box::new(|g: &mut InstanceGraph| {
            let victim = g.instances.iter().next().expect("an instance").clone();
            g.depth_capped.push(victim);
        })),
        ("storage_failures", Box::new(|g: &mut InstanceGraph| {
            g.storage_failures.push(skull_mir::monoty::MonoTyReason::NoNode);
        })),
    ];

    for (name, mutate) in mutations {
        let mut g = clone_graph(&base);
        mutate(&mut g);
        let after = snap(&ws, &g);
        assert!(
            !before.diff(&after).is_empty(),
            "mutating `{name}` did not change the canonical form -- either the section is \
             missing from `graph_snapshot::canonical`, or the mutation was a no-op"
        );
        // And the diff must NAME the section that moved, so a failure is
        // actionable rather than "something differs".
        let d = before.diff(&after);
        assert!(
            d.iter().any(|l| l.starts_with(name)),
            "mutating `{name}` moved the canonical form, but the diff blames {d:?}"
        );
    }
}

/// **Operational counters and arena contents stay OUT of equality.** An updater
/// that reaches the right graph by a different route differs on work counters,
/// and the arena keeps types whose instances were pruned -- neither is a
/// semantic difference and neither may fail the comparison.
#[test]
fn counters_and_arena_growth_are_not_semantic() {
    let (ws, base) = build(false);
    let before = snap(&ws, &base);

    let mut g = clone_graph(&base);
    g.sites_attempted += 1234;
    g.subst_comparisons += 7;
    g.bound_tree_comparisons += 7;
    g.recursion_starts += 99;
    g.synthesized_examined += 5;
    g.synthesized_approximated += 5;
    g.always_emitted_seeds += 3;
    // An orphan type interned but referenced by no instance -- exactly what a
    // persisted arena accumulates across edits.
    g.arena.intern(skull_mir::monoty::MonoTy::Never);

    let after = snap(&ws, &g);
    let d = before.diff(&after);
    assert!(
        d.is_empty(),
        "work counters or arena growth leaked into semantic equality:\n{}",
        d.join("\n")
    );
}

/// `InstanceGraph` has no `Clone`, and giving it one for a test would be a
/// production API added for a test's convenience. Rebuilding is deterministic
/// (the arm above proves discovery order does not matter), so each mutation
/// starts from its own fresh build.
fn clone_graph(_base: &InstanceGraph) -> InstanceGraph {
    let (_ws, g) = build(false);
    g
}
