//! **`InstanceGraph::update_bodies` against a fresh build.**
//!
//! The oracle is `graph_snapshot::canonical`: raw field equality is invalid
//! across arenas, because the updated graph's arena is the one v1 interned into
//! and a fresh build's is not.
//!
//! **Equality alone is not the whole assertion.** An updater that quietly
//! rebuilt the world would satisfy it perfectly, so edits also assert that the
//! update walked a small part of the graph. And an arm that asserted only "the
//! fact is gone" would pass over a fixture that never produced it, so every arm
//! asserts its v1 population first.
//!
//! Edits go through a real `bone_incremental::Session`, so the changed-body set
//! is `UpdateResult::reinferred` rather than a hand-written guess at what an
//! edit touches.

use crate::common::{body_id, stdlib_asts, ws_of_files};
use std::collections::BTreeSet;

use crate::support::graph_snapshot::{canonical, instance_key};
use bone_incremental::session::{BodyId, NsAliases, Session, SessionFile, UpdateTier};
use bone_semantic::inference::Workspace;
use skull::instance_graph::{
    self, Instance, InstanceGraph, RejectionKind, TextEditDelta, UpdateStats,
};

/// The compilation target's own directory. Everything outside it is a LIBRARY,
/// which is what makes anything prunable at all: `mir_emit::is_emission_root`
/// roots every declaration of a target without `main`, so a fixture whose
/// functions all live in `main.rock` can never lose one. The fixtures name
/// their entry `zz_start` to stay such a target.
const PROJ: &str = "proj/";
const MAIN: &str = "proj/main.rock";
const LIB: &str = "lib/helpers.rock";

/// An instance key without its declaration indices, so an expected set can be
/// written down and still name the declaration exactly.
fn short(key: &str) -> String {
    key.split('#').next().unwrap_or(key).to_string()
}

fn graph_of(ws: &Workspace) -> InstanceGraph {
    instance_graph::build(ws, skull::mir_emit::emission_roots(ws, Some(PROJ)), false)
}

fn session(main: &str, lib: &str) -> Session {
    Session::new(
        stdlib_asts(),
        vec![
            SessionFile::legacy(MAIN, main),
            SessionFile::legacy(LIB, lib),
        ],
        &NsAliases::default(),
    )
    .expect("session")
}

#[track_caller]
fn assert_same_graph(ws: &Workspace, updated: &InstanceGraph, fresh: &InstanceGraph, label: &str) {
    let d = canonical(ws, updated).diff(&canonical(ws, fresh));
    assert!(
        d.is_empty(),
        "{label}: the updated graph and a fresh build disagree:\n{}",
        d.join("\n")
    );
}

/// One persisted graph across a sequence of edits.
struct Case {
    session: Session,
    graph: InstanceGraph,
}

impl Case {
    fn new(main: &str, lib: &str) -> Self {
        let session = session(main, lib);
        let graph = graph_of(session.workspace());
        Case { session, graph }
    }

    fn ws(&self) -> &Workspace {
        self.session.workspace()
    }

    /// Apply one BODY-ONLY edit to the session and return the exact owners
    /// incremental inference reinferred, with the text edit.
    fn body_edit(&mut self, path: &str, text: &str) -> (Vec<BodyId>, Vec<TextEditDelta>) {
        let res = self
            .session
            .update_file(path, text)
            .expect("incremental update");
        assert_eq!(
            res.tier,
            UpdateTier::BodyOnly,
            "the fixture's edit is not body-only ({:?}); an interface change moves the root \
             set and the manglings, which `update_bodies` does not claim to handle",
            res.tier
        );
        assert!(
            !res.reinferred.is_empty(),
            "a body-only edit reinferred nothing"
        );
        (res.reinferred, res.body_update.edit.into_iter().collect())
    }

    /// Apply one body-only edit and update the persisted graph against the
    /// recomputed root set.
    fn edit(&mut self, path: &str, text: &str) -> UpdateStats {
        let (reinferred, edits) = self.body_edit(path, text);
        let ws = self.session.workspace();
        let roots = skull::mir_emit::emission_roots(ws, Some(PROJ));
        self.graph
            .update_bodies_with_edits(ws, roots, &reinferred, &edits)
    }

    /// The same edit through the retained-root path.
    fn edit_keeping_roots(&mut self, path: &str, text: &str) -> (UpdateStats, Vec<TextEditDelta>) {
        let (reinferred, edits) = self.body_edit(path, text);
        let stats = self.graph.update_bodies_with_root_delta(
            self.session.workspace(),
            &[],
            &[],
            &reinferred,
            &edits,
        );
        (stats, edits)
    }

    /// The updated graph must MEAN the same thing as a build from scratch.
    #[track_caller]
    fn assert_matches_fresh(&self, label: &str) {
        assert_same_graph(self.ws(), &self.graph, &graph_of(self.ws()), label);
    }

    fn has_instance_named(&self, name: &str) -> bool {
        self.graph
            .instances
            .iter()
            .any(|i| self.identity(i).contains(name))
    }

    fn has_instance_with(&self, prefix: &str, arg: &str) -> bool {
        self.graph.instances.iter().any(|i| {
            let k = self.identity(i);
            k.starts_with(prefix) && k.contains(arg)
        })
    }

    fn identity(&self, i: &Instance) -> String {
        instance_key(self.ws(), &self.graph, i)
    }

    fn short_names(&self, instances: &BTreeSet<Instance>) -> BTreeSet<String> {
        instances.iter().map(|i| short(&self.identity(i))).collect()
    }

    fn recursion_findings(&self) -> Vec<String> {
        self.graph
            .rejections()
            .filter(|r| r.kind == RejectionKind::ChangingInstanceRecursion)
            .map(|r| self.identity(&r.at))
            .collect()
    }

    fn always_emitted_failures(&self) -> usize {
        self.graph
            .uninstantiated_roots
            .values()
            .map(|h| h.always_emitted_sites.len())
            .sum()
    }
}

/// An update must touch the AFFECTED REGION, not the program. Stated with both
/// populations in one assertion: a bound with no denominator beside it cannot
/// tell a scoped update from a rebuild that happens to be small.
#[track_caller]
fn assert_scoped(stats: &UpdateStats, label: &str) {
    assert!(
        stats.expanded * 4 < stats.owners,
        "{label}: the update walked {} of {} owners -- that is a rebuild, not an update \
         ({} affected, {} pruned, {} newly reached)",
        stats.expanded,
        stats.owners,
        stats.affected,
        stats.pruned,
        stats.newly_reached
    );
    assert!(
        stats.census.incremental_forced_rewalks > 0,
        "{label}: the update fixture did not expose an incremental forced re-walk"
    );
    assert_eq!(
        stats.prune_search_population,
        stats.suspect_region
            + stats.boundary_predecessor_edges_examined
            + stats.support_component_nodes_examined
            + stats.support_component_edges_examined
            + stats.reverse_instances_examined
            + stats.reverse_predecessor_edges_examined,
        "{label}: prune denominator omitted pruning work: {stats:?}"
    );
    assert_eq!(
        stats.broad_scans, 0,
        "{label}: a broad prune scan ran: {stats:?}"
    );
    assert!(
        stats.prune_search_population < stats.instances,
        "{label}: prune search examined the whole instance population: {stats:?}"
    );
}

/// [`assert_scoped`], plus: the prune search stopped at the lost region's
/// boundary instead of asking rootward reachability questions.
#[track_caller]
fn assert_local_boundary(stats: &UpdateStats, label: &str) {
    assert!(
        stats.suspect_region > 0,
        "{label}: no forward population: {stats:?}"
    );
    assert!(
        stats.boundary_predecessor_edges_examined > 0,
        "{label}: no boundary predecessor edges examined: {stats:?}"
    );
    assert_eq!(
        stats.reverse_instances_examined, 0,
        "{label}: performed a rootward reverse query: {stats:?}"
    );
    assert_eq!(
        stats.reverse_predecessor_edges_examined, 0,
        "{label}: examined rootward predecessor edges: {stats:?}"
    );
    assert!(
        stats.prune_search_population <= stats.suspect_region * 4,
        "{label}: boundary work escaped the locality bound: {stats:?}"
    );
    assert_scoped(stats, label);
}

// ---------------------------------------------------------------------------
// Edit scenarios: build v1, apply one body edit, update, compare with fresh
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
enum Locality {
    #[default]
    Scoped,
    Local,
}

/// One body edit on one persisted graph. `gone` and `kept` must be reached in
/// v1; `added` must not be.
#[derive(Default)]
struct Scenario {
    label: String,
    main: String,
    lib: String,
    path: &'static str,
    v2: String,
    gone: Vec<String>,
    kept: Vec<String>,
    added: Vec<String>,
    /// Instances the edit must make lost seeds, or a supported subtree's
    /// survival says nothing about the prune search.
    seeds: Vec<String>,
    pruned: Option<usize>,
    newly_reached: Option<usize>,
    suspect_below: Option<usize>,
    locality: Locality,
}

fn strings<S: ToString>(names: impl IntoIterator<Item = S>) -> Vec<String> {
    names.into_iter().map(|n| n.to_string()).collect()
}

fn edit(label: impl Into<String>, main: &str, lib: &str, path: &'static str, v2: &str) -> Scenario {
    Scenario {
        label: label.into(),
        main: main.to_string(),
        lib: lib.to_string(),
        path,
        v2: v2.to_string(),
        ..Default::default()
    }
}

impl Scenario {
    fn gone<S: ToString>(mut self, names: impl IntoIterator<Item = S>) -> Self {
        self.gone.extend(strings(names));
        self
    }
    fn kept<S: ToString>(mut self, names: impl IntoIterator<Item = S>) -> Self {
        self.kept.extend(strings(names));
        self
    }
    fn added<S: ToString>(mut self, names: impl IntoIterator<Item = S>) -> Self {
        self.added.extend(strings(names));
        self
    }
    fn seeds<S: ToString>(mut self, names: impl IntoIterator<Item = S>) -> Self {
        self.seeds.extend(strings(names));
        self
    }
    fn pruned(mut self, n: usize) -> Self {
        self.pruned = Some(n);
        self
    }
    fn newly_reached(mut self, n: usize) -> Self {
        self.newly_reached = Some(n);
        self
    }
    fn suspect_below(mut self, n: usize) -> Self {
        self.suspect_below = Some(n);
        self
    }
    fn locality(mut self, l: Locality) -> Self {
        self.locality = l;
        self
    }

    fn run(&self) {
        let l = &self.label;
        let mut case = Case::new(&self.main, &self.lib);
        for n in self.gone.iter().chain(&self.kept) {
            assert!(case.has_instance_named(n), "{l}: v1 never reached `{n}`");
        }
        for n in &self.added {
            assert!(!case.has_instance_named(n), "{l}: v1 already reached `{n}`");
        }

        let stats = case.edit(self.path, &self.v2);

        for n in &self.gone {
            assert!(
                !case.has_instance_named(n),
                "{l}: `{n}` survived the edit: {stats:?}"
            );
        }
        for n in self.kept.iter().chain(&self.added) {
            assert!(
                case.has_instance_named(n),
                "{l}: `{n}` is absent after the edit: {stats:?}"
            );
        }
        for s in &self.seeds {
            assert!(
                stats
                    .prune_seeds
                    .iter()
                    .any(|i| case.identity(i).starts_with(s.as_str())),
                "{l}: `{s}` was not a lost seed: {stats:?}"
            );
        }
        if let Some(n) = self.pruned {
            assert_eq!(stats.pruned, n, "{l}: pruned population: {stats:?}");
        }
        if let Some(n) = self.newly_reached {
            assert_eq!(
                stats.newly_reached, n,
                "{l}: newly reached population: {stats:?}"
            );
        }
        if let Some(n) = self.suspect_below {
            assert!(
                stats.suspect_region < n,
                "{l}: the prune search expanded the supported subtree: {stats:?}"
            );
        }
        case.assert_matches_fresh(l);
        match self.locality {
            Locality::Scoped => assert_scoped(&stats, l),
            Locality::Local => assert_local_boundary(&stats, l),
        }
    }
}

const CHAIN_LIB: &str = r#"
fn chain_a() -> int { return chain_b() + 1 }
fn chain_b() -> int { return chain_c() + 1 }
fn chain_c() -> int { return 3 }
fn other() -> int { return 7 }
"#;
const CHAIN_MAIN: &str = "fn zz_start() { print(\"{chain_a()} {other()}\") }\n";
const OTHER_MAIN: &str = "fn zz_start() { print(\"{other()}\") }\n";

/// Every member of the cycle still has an incoming edge after the external one
/// is cut, so only reachability FROM A ROOT can see that it is dead.
const CYCLE_LIB: &str = r#"
fn cyc_a(n: int) -> int { if n <= 0 { return 0 } return cyc_b(n - 1) }
fn cyc_b(n: int) -> int { if n <= 0 { return 1 } return cyc_a(n - 1) }
fn other() -> int { return 7 }
"#;

/// A cycle of `length` library functions entered at `cycle_0`, optionally kept
/// alive by a second root after the edit.
fn generated_cycle(length: usize, rooted: bool) -> Scenario {
    let lib: String = (0..length)
        .map(|i| {
            format!(
                "fn cycle_{i}() -> int {{ return cycle_{}() }}\n",
                (i + 1) % length
            )
        })
        .collect();
    let anchor = if rooted {
        "fn root_anchor() -> int { return cycle_0() }\n"
    } else {
        ""
    };
    let v1 = format!("fn zz_start() {{ print(\"{{cycle_0()}}\") }}\n{anchor}");
    let v2 = format!("fn zz_start() {{ print(\"0\") }}\n{anchor}");
    let label = format!(
        "{} cycle of length {length}",
        if rooted { "rooted" } else { "unanchored" }
    );
    let members = (0..length).map(|i| format!("cycle_{i}"));
    let s = edit(label, &v1, &lib, MAIN, &v2).seeds(["cycle_0"]);
    if rooted {
        s.kept(members)
    } else {
        s.gone(members)
    }
}

/// The fourteen-node tree under a `shared` root, so a prune search that
/// expands it is visible in `suspect_region`.
const SHARED_BRANCHES: &str = r#"
fn shared_left() -> int { return shared_left_a() + shared_left_b() }
fn shared_right() -> int { return shared_right_a() + shared_right_b() }
fn shared_left_a() -> int { return shared_left_a_0() + shared_left_a_1() }
fn shared_left_b() -> int { return shared_left_b_0() + shared_left_b_1() }
fn shared_right_a() -> int { return shared_right_a_0() + shared_right_a_1() }
fn shared_right_b() -> int { return shared_right_b_0() + shared_right_b_1() }
fn shared_left_a_0() -> int { return 1 }
fn shared_left_a_1() -> int { return 2 }
fn shared_left_b_0() -> int { return 3 }
fn shared_left_b_1() -> int { return 4 }
fn shared_right_a_0() -> int { return 5 }
fn shared_right_a_1() -> int { return 6 }
fn shared_right_b_0() -> int { return 7 }
fn shared_right_b_1() -> int { return 8 }
"#;
const SHARED_BRANCH_NAMES: &[&str] = &[
    "shared_left",
    "shared_right",
    "shared_left_a",
    "shared_left_b",
    "shared_right_a",
    "shared_right_b",
    "shared_left_a_0",
    "shared_left_a_1",
    "shared_left_b_0",
    "shared_left_b_1",
    "shared_right_a_0",
    "shared_right_a_1",
    "shared_right_b_0",
    "shared_right_b_1",
];
const SHARED_SUBTREE_SIZE: usize = 15;

/// `shared<int>` loses its edge from the entry but stays reached through a
/// live non-root chain of `depth` functions from an independent root.
fn support_chain(depth: usize) -> Scenario {
    let mut lib = format!(
        "fn shared<T>(x: T) -> int {{ return shared_left() + shared_right() }}\n{SHARED_BRANCHES}"
    );
    for i in 0..depth {
        let callee = if i + 1 == depth {
            "shared(1)".to_string()
        } else {
            format!("support_{}()", i + 1)
        };
        lib.push_str(&format!("fn support_{i}() -> int {{ return {callee} }}\n"));
    }
    let root = "fn independent_root() -> int { return support_0() }\n";
    edit(
        format!("support chain of depth {depth}"),
        &format!("fn zz_start() {{ print(\"{{shared(1)}}\") }}\n{root}"),
        &lib,
        MAIN,
        &format!("fn zz_start() {{ print(\"0\") }}\n{root}"),
    )
    .seeds(["shared"])
    .kept(["shared"])
    .kept(SHARED_BRANCH_NAMES)
    .kept((0..depth).map(|i| format!("support_{i}")))
    .suspect_below(SHARED_SUBTREE_SIZE)
}

/// Two lost seeds whose tentative support components intersect at
/// `support_join`; the closed component must release together.
fn intersecting_support() -> Scenario {
    let lib = format!(
        r#"
fn lost_a() -> int {{ return shared() + support_a() }}
fn lost_b() -> int {{ return shared() + support_b() }}
fn support_a() -> int {{ return lost_a() + support_join() }}
fn support_b() -> int {{ return lost_b() + support_join() }}
fn support_join() -> int {{ return support_a() + support_b() }}
fn shared() -> int {{ return shared_left() + shared_right() }}
fn independent_root() -> int {{ return shared() }}
{SHARED_BRANCHES}"#
    );
    edit(
        "intersecting support components",
        "fn zz_start() { print(\"{lost_a()} {lost_b()} {independent_root()}\") }\n",
        &lib,
        MAIN,
        "fn zz_start() { print(\"{independent_root()}\") }\n",
    )
    .seeds(["lost_a", "lost_b"])
    .gone(["lost_a", "lost_b", "support_a", "support_b", "support_join"])
    .kept(["shared"])
    .kept(SHARED_BRANCH_NAMES)
    .suspect_below(SHARED_SUBTREE_SIZE)
}

/// `target` has a rooted and a dead predecessor branch; a support cache that
/// remembered the dead branch as live would keep it. Declaration order decides
/// which branch the search meets first.
fn branching_support(dead_first: bool) -> Scenario {
    let (first, second) = (
        "fn root_branch() -> int { return target() }\n",
        "fn dead_branch() -> int { return dead_c() + target() }\n",
    );
    let (first, second) = if dead_first {
        (first, second)
    } else {
        (second, first)
    };
    let lib = format!(
        "fn target() -> int {{ return lost_b() }}\n\
         fn dead_c() -> int {{ return dead_a() }}\n\
         fn lost_b() -> int {{ return 7 }}\n\
         {first}{second}fn dead_a() -> int {{ return dead_branch() }}\n"
    );
    let root = "fn independent_root() -> int { return root_branch() }\n";
    edit(
        format!("branching support, dead_first={dead_first}"),
        &format!("fn zz_start() {{ print(\"{{lost_b()}} {{dead_c()}}\") }}\n{root}"),
        &lib,
        MAIN,
        &format!("fn zz_start() {{ print(\"0\") }}\n{root}"),
    )
    .seeds(["dead_c", "lost_b"])
    .gone(["dead_c", "dead_a", "dead_branch"])
    .kept(["target", "lost_b", "root_branch"])
    .pruned(3)
}

const BRANCHED_ANCHOR_LIB: &str = r#"
fn entry() -> int { return shared_root() + dead_root() }
fn anchor() -> int { return shared_root() }
fn shared_root() -> int { return shared_left() + shared_right() }
fn shared_left() -> int { return shared_left_a() + shared_left_b() }
fn shared_right() -> int { return shared_right_a() + shared_right_b() }
fn shared_left_a() -> int { return 1 }
fn shared_left_b() -> int { return 2 }
fn shared_right_a() -> int { return 3 }
fn shared_right_b() -> int { return 4 }
fn dead_root() -> int { return dead_leaf() }
fn dead_leaf() -> int { return 5 }
"#;

/// `shared_root` keeps an external anchor reached only through 32 hops.
fn long_anchor_lib() -> String {
    let mut lib = String::from(
        "fn entry() -> int { return shared_root() + dead_root() }\n\
         fn anchor() -> int { return anchor_0() }\n",
    );
    for i in 0..31 {
        lib.push_str(&format!(
            "fn anchor_{i}() -> int {{ return anchor_{}() }}\n",
            i + 1
        ));
    }
    lib.push_str(
        "fn anchor_31() -> int { return shared_root() }\n\
         fn shared_root() -> int { return shared_left() + shared_right() }\n\
         fn shared_left() -> int { return 1 }\n\
         fn shared_right() -> int { return 2 }\n\
         fn dead_root() -> int { return dead_leaf() }\n\
         fn dead_leaf() -> int { return 3 }\n",
    );
    lib
}

const DEFERRED_BOUNDARY_LIB: &str = r#"
fn entry() -> int { return branch() }
fn branch() -> int { return shared() + apparent() }
fn apparent() -> int { return shared() }
fn shared() -> int { return dead() }
fn dead() -> int { return 1 }
fn other() -> int { return 7 }
"#;

const ROOTED_CYCLE_LIB: &str = r#"
fn entry() -> int { return rooted_a() + dead() }
fn rooted_a() -> int { return rooted_b() }
fn rooted_b() -> int { return rooted_a() }
fn dead() -> int { return dead_leaf() }
fn dead_leaf() -> int { return 1 }
"#;

/// Deleting a call prunes exactly what it stranded: a subtree, a cycle that
/// keeps itself referenced, closed support components, and lost branches
/// beside anchored ones.
#[test]
fn deletions_prune_exactly_what_they_strand() {
    let entry_anchor = "fn zz_start() { print(\"{entry()} {anchor()}\") }\n";
    let anchor_only = "fn zz_start() { print(\"{anchor()}\") }\n";
    let rooted_anchor = "fn root_anchor() -> int { return rooted_a() }\n";
    let scenarios = [
        edit("subtree", CHAIN_MAIN, CHAIN_LIB, MAIN, OTHER_MAIN)
            .gone(["chain_a", "chain_b", "chain_c"])
            .kept(["other"])
            .pruned(3),
        // `__op_sub_int_int`'s only caller was the cycle.
        edit(
            "mutually reachable cycle",
            "fn zz_start() { print(\"{cyc_a(4)} {other()}\") }\n",
            CYCLE_LIB,
            MAIN,
            OTHER_MAIN,
        )
        .gone(["cyc_a", "cyc_b", "__op_sub_int_int"])
        .kept(["other"])
        .pruned(3),
        generated_cycle(3, false),
        generated_cycle(8, false),
        intersecting_support(),
        branching_support(true),
        branching_support(false),
        // `apparent` looks like a boundary for `shared` until it is itself
        // found lost.
        edit(
            "deferred boundary release",
            "fn zz_start() { print(\"{entry()}\") }\n",
            DEFERRED_BOUNDARY_LIB,
            MAIN,
            OTHER_MAIN,
        )
        .gone(["entry", "branch", "apparent", "shared", "dead"])
        .pruned(5),
        edit(
            "rooted cycle beside a lost branch",
            &format!("fn zz_start() {{ print(\"{{entry()}} {{rooted_a()}}\") }}\n{rooted_anchor}"),
            ROOTED_CYCLE_LIB,
            MAIN,
            &format!("fn zz_start() {{ print(\"0\") }}\n{rooted_anchor}"),
        )
        .seeds(["rooted_a"])
        .gone(["entry", "dead", "dead_leaf"])
        .kept(["rooted_a", "rooted_b"])
        .pruned(3),
        edit(
            "external anchor beside a stranded branch",
            entry_anchor,
            BRANCHED_ANCHOR_LIB,
            MAIN,
            anchor_only,
        )
        .gone(["entry", "dead_root", "dead_leaf"])
        .kept(["shared_root", "shared_left", "shared_right"])
        .pruned(3),
        edit(
            "long external anchor path",
            entry_anchor,
            &long_anchor_lib(),
            MAIN,
            anchor_only,
        )
        .gone(["entry", "dead_root", "dead_leaf"])
        .kept(["shared_root", "anchor_31"])
        .pruned(3)
        .locality(Locality::Local),
    ];
    for s in scenarios {
        s.run();
    }
}

/// A lost seed that is still supported -- by a second root, a live non-root
/// chain, a root-reachable generic caller, or an always-emitted initializer --
/// keeps its whole subtree, and the search stops before expanding it.
#[test]
fn supported_lost_seeds_keep_their_subtrees() {
    let direct_root = "fn independent_root() -> int { return shared_root() }\n";
    let nested_root = "fn independent_root() { print(\"{outer(1)}\") }\n";
    let scenarios = [
        generated_cycle(3, true),
        generated_cycle(8, true),
        support_chain(1),
        support_chain(3),
        support_chain(8),
        edit(
            "directly rooted seed",
            &format!("fn zz_start() {{ print(\"{{shared_root()}}\") }}\n{direct_root}"),
            &format!(
                "fn shared_root() -> int {{ return shared_left() + shared_right() }}\n{SHARED_BRANCHES}"
            ),
            MAIN,
            &format!("fn zz_start() {{ print(\"0\") }}\n{direct_root}"),
        )
        .seeds(["shared_root"])
        .kept(["shared_root"])
        .kept(SHARED_BRANCH_NAMES)
        .pruned(0)
        .suspect_below(SHARED_SUBTREE_SIZE)
        .locality(Locality::Local),
        // `outer<int>` is a library generic, so it is neither a root nor
        // always-emitted: only its own reachability supports `shared<int>`.
        edit(
            "root-reachable non-root predecessor",
            &format!("fn zz_start() {{ print(\"{{shared(1)}}\") }}\n{nested_root}"),
            &format!(
                "fn shared<T>(x: T) -> int {{ return shared_left() + shared_right() }}\n\
                 fn outer<T>(x: T) -> int {{ return shared(x) }}\n{SHARED_BRANCHES}"
            ),
            MAIN,
            &format!("fn zz_start() {{ print(\"0\") }}\n{nested_root}"),
        )
        .seeds(["shared"])
        .kept(["shared", "outer"])
        .kept(SHARED_BRANCH_NAMES)
        .pruned(0)
        .suspect_below(SHARED_SUBTREE_SIZE)
        .locality(Locality::Local),
        // The const's call is not an edge -- an always-emitted body's callees
        // are roots -- so only the always-emitted arm keeps `shared` alive.
        // The const lives in the library because an initializer in the edited
        // file is re-walked itself and would never let `shared` become lost.
        edit(
            "always-emitted anchor",
            "fn zz_start() { print(\"{shared()} {other()} {C}\") }\n",
            "const C: int = shared()\nfn shared() -> int { return 1 }\nfn other() -> int { return 7 }\n",
            MAIN,
            "fn zz_start() { print(\"{other()} {C}\") }\n",
        )
        .seeds(["shared"])
        .kept(["shared", "other"])
        .pruned(0),
    ];
    for s in scenarios {
        s.run();
    }
}

const PICK_LIB: &str = r#"
fn pick_a() -> int { return 1 }
fn pick_b() -> int { return 2 }
"#;

/// A `const` initializer is a body whose callees are ROOTS with no incoming
/// edge: replacing one must drop the old root and seed the new one, and an
/// initializer with no facts that gains a call must still be re-walked.
#[test]
fn const_initializer_edits_replace_their_always_emitted_roots() {
    let with =
        |call: &str| format!("const C: int = {call}\nfn zz_start() {{ print(\"{{C}}\") }}\n");
    let scenarios = [
        edit(
            "const replaced",
            &with("pick_a()"),
            PICK_LIB,
            MAIN,
            &with("pick_b()"),
        )
        .gone(["pick_a"])
        .added(["pick_b"])
        .pruned(1)
        .newly_reached(1),
        edit(
            "const gains a call",
            &with("1"),
            PICK_LIB,
            MAIN,
            &with("pick_a()"),
        )
        .added(["pick_a"])
        .newly_reached(1),
    ];
    for s in scenarios {
        s.run();
    }
}

// ---------------------------------------------------------------------------
// Root deltas
// ---------------------------------------------------------------------------

#[test]
fn explicit_root_delta_adds_and_removes_a_library_subtree() {
    let mut case = Case::new(OTHER_MAIN, CHAIN_LIB);
    let ws = case.session.workspace();
    let chain_a = crate::common::fn_ref(ws, "chain_a");
    let base_roots = skull::mir_emit::emission_roots(ws, Some(PROJ));
    assert!(!base_roots.contains(&chain_a));
    assert!(!case.has_instance_named("chain_a"), "v1 already reached it");
    let subtree = BTreeSet::from([
        "chain_a_0".to_string(),
        "chain_b_0".to_string(),
        "chain_c_0".to_string(),
    ]);

    let added = case
        .graph
        .update_bodies_with_root_delta(ws, &[chain_a], &[], &[], &[]);
    assert_eq!(added.newly_reached, 3);
    assert_eq!(case.short_names(&added.added), subtree);
    let mut roots = base_roots.clone();
    roots.push(chain_a);
    let fresh = instance_graph::build(ws, roots, false);
    assert_same_graph(ws, &case.graph, &fresh, "root added");

    let removed = case
        .graph
        .update_bodies_with_root_delta(ws, &[], &[chain_a], &[], &[]);
    assert_eq!(removed.pruned, 3);
    assert_eq!(case.short_names(&removed.removed), subtree);
    case.assert_matches_fresh("root removed");
}

#[test]
fn a_replaced_root_can_be_restored_by_an_earlier_install_in_its_wave() {
    let ws = ws_of_files(&[
        (MAIN, "fn zz_start() {}"),
        (
            LIB,
            "fn old_root() -> int { return leaf() }\n\
             fn leaf() -> int { return 1 }\n\
             fn new_root() -> int { return old_root() }\n",
        ),
    ]);
    let old_root = crate::common::fn_ref(&ws, "old_root");
    let new_root = crate::common::fn_ref(&ws, "new_root");
    let mut graph = instance_graph::build(&ws, vec![old_root], false);
    let old_instance = Instance {
        func: old_root,
        args: Vec::new(),
    };
    assert!(graph.substitution(&old_instance).is_some());
    assert!(!graph.instances.iter().any(|i| i.func == new_root));

    // Root removal makes old_root unwalkable at the wave snapshot. new_root
    // installs first in reverse declaration order and restores its demand.
    let stats = graph.update_bodies_with_root_delta(
        &ws,
        &[new_root],
        &[old_root],
        &[body_id(&ws, "old_root")],
        &[],
    );
    assert!(stats.expanded >= 2);
    assert!(graph.substitution(&old_instance).is_some());
    let fresh = instance_graph::build(&ws, vec![new_root], false);
    assert_same_graph(&ws, &graph, &fresh, "root replaced");
}

/// The root set is a function of the DECLARATIONS, so a body-only edit cannot
/// move it -- and a caller that hands `update_bodies` an interface change is
/// refused rather than absorbed.
#[test]
#[should_panic(expected = "the root set moved during a body-only update")]
fn a_moved_root_set_is_refused_rather_than_absorbed() {
    let case = Case::new(OTHER_MAIN, CHAIN_LIB);
    let ws = case.ws();
    let mut roots = skull::mir_emit::emission_roots(ws, Some(PROJ));
    roots.pop().expect("the fixture has roots");
    let mut graph = graph_of(ws);
    graph.update_bodies_with_edits(ws, roots, &[], &[]);
}

// ---------------------------------------------------------------------------
// Additions and sequences
// ---------------------------------------------------------------------------

#[test]
fn adding_a_call_records_a_new_specialization() {
    let mut case = Case::new(
        "fn zz_start() { print(\"{ident(1)}\") }\n",
        "fn passthru<T>(x: T) -> T { return x }\nfn ident<T>(x: T) -> T { return passthru(x) }\n",
    );
    assert!(!case.has_instance_with("ident", "string"));

    let stats = case.edit(
        MAIN,
        "fn zz_start() { print(\"{ident(1)} {ident(\"s\")}\") }\n",
    );

    assert!(
        case.has_instance_with("ident", "string"),
        "the added call reached no new specialization"
    );
    assert!(
        stats.newly_reached >= 2,
        "`ident<string>` and the `passthru<string>` it reaches are two new instances, got {}",
        stats.newly_reached
    );
    case.assert_matches_fresh("addition");
    assert_scoped(&stats, "addition");
}

/// Delete, then add, then delete, comparing after every edit. Each step starts
/// from the graph the previous one produced, so a residue that survives one
/// edit and is masked by the next is a failure here rather than invisible.
#[test]
fn a_sequential_edit_chain_agrees_with_a_fresh_build_at_every_step() {
    let mut case = Case::new(CHAIN_MAIN, CHAIN_LIB);
    case.assert_matches_fresh("v1");

    let s1 = case.edit(MAIN, OTHER_MAIN);
    assert!(
        s1.pruned == 3 && s1.newly_reached == 0,
        "step 1 is a deletion: {s1:?}"
    );
    case.assert_matches_fresh("after delete");

    let s2 = case.edit(MAIN, CHAIN_MAIN);
    assert!(
        s2.newly_reached == 3 && s2.pruned == 0,
        "step 2 is an addition: {s2:?}"
    );
    case.assert_matches_fresh("after re-add");

    let s3 = case.edit(MAIN, "fn zz_start() { print(\"{chain_b()}\") }\n");
    assert!(s3.pruned > 0, "step 3 strands `chain_a`: {s3:?}");
    assert!(!case.has_instance_named("chain_a"));
    assert!(
        case.has_instance_named("chain_c"),
        "`chain_c` is still reached through `chain_b`"
    );
    case.assert_matches_fresh("after second delete");
}

/// A body with NO recorded facts that gains a call. An empty contribution
/// quotes no file, so the "this owner cites the edited file" rule cannot see
/// it; only "this owner IS the edited body" reaches it.
#[test]
fn a_body_with_no_facts_that_gains_a_call_is_still_re_walked() {
    let mut case = Case::new(
        "fn zz_start() { print(\"{leaf()}\") }\n",
        "fn leaf() -> int { return 1 }\nfn added() -> int { return 2 }\n",
    );
    let leaf_span_files = case
        .graph
        .contributions
        .iter()
        .find(|(o, _)| {
            o.instance()
                .is_some_and(|i| case.identity(i).starts_with("leaf"))
        })
        .map(|(_, c)| c.span_files.len())
        .expect("`leaf` owns a contribution");
    assert_eq!(
        leaf_span_files, 0,
        "`leaf` must record no span, or the arm is not separated"
    );
    assert!(!case.has_instance_named("added"));

    let stats = case.edit(
        LIB,
        "fn leaf() -> int { return added() }\nfn added() -> int { return 2 }\n",
    );

    assert!(
        case.has_instance_named("added"),
        "the call added to an empty body was not seen"
    );
    assert_eq!(stats.newly_reached, 1);
    case.assert_matches_fresh("empty body gains a call");
}

#[test]
fn a_semantic_edit_shifts_unedited_owner_spans_without_rewalking_that_owner() {
    let mut case = Case::new(
        "fn zz_start() { print(other()) }\n",
        "fn leaf() -> int { return 1 }\nfn other() -> int { return leaf() }\n",
    );
    let other_call_offset = |case: &Case| {
        case.graph
            .contributions
            .iter()
            .find(|(owner, _)| {
                owner
                    .instance()
                    .is_some_and(|i| case.identity(i).starts_with("other"))
            })
            .and_then(|(_, contribution)| contribution.call_sites.values().next())
            .map(|site| site.span.offset_range().0)
            .expect("the unedited owner must have a call span")
    };
    let before = other_call_offset(&case);

    let stats = case.edit(
        LIB,
        "fn leaf() -> int {\n  return 2\n}\nfn other() -> int { return leaf() }\n",
    );
    assert_eq!(
        stats.affected, 1,
        "the edit should rewalk only `leaf`: {stats:?}"
    );
    assert_eq!(
        stats.revisited_owners.len(),
        1,
        "span relocation widened the semantic delta"
    );
    assert!(
        other_call_offset(&case) > before,
        "the unedited call span did not relocate"
    );
    case.assert_matches_fresh("semantic edit with span relocation");
}

/// **A pruned instance takes its BOUND TREE out of the index with it.**
/// `bound_trees` is a maintained cross-owner index, so a tree that outlives
/// its last owner is a stale fact a fresh build does not have.
#[test]
fn pruning_an_instance_retracts_its_bound_tree() {
    let mut case = Case::new(
        "fn zz_start() { print(\"{total([1, 2])} {other()}\") }\n",
        r#"
fn total(v: Vec<int>) -> int {
    let t = 0
    for x in v.iter().filter(_ > 0) { t = t + x }
    return t
}
fn other() -> int { return 7 }
"#,
    );
    let before = case.graph.bound_trees().count();
    assert!(before > 0, "the fixture recorded no bound tree");

    let stats = case.edit(MAIN, OTHER_MAIN);

    assert!(stats.pruned > 0, "the deletion stranded nothing");
    assert!(
        case.graph.bound_trees().count() < before,
        "every one of the {before} bound trees outlived the instances that owned them"
    );
    case.assert_matches_fresh("bound tree retracted");
    assert_scoped(&stats, "bound tree retracted");
}

// ---------------------------------------------------------------------------
// Stale-state producers, each clearing on its own
// ---------------------------------------------------------------------------

/// A call that could not be turned into an instance, and then can be.
///
/// `unresolved` needs a generic call with no binding record, which no valid
/// program produces, so v1 is the intact workspace with that record removed
/// and v2 is the intact workspace: the declarations are identical and one
/// body's facts changed, which is exactly the shape a body edit has.
#[test]
fn resolving_a_call_removes_its_stale_unresolved_record() {
    let v2 = ws_of_files(&[
        (MAIN, "fn zz_start() { print(\"{caller()}\") }\n"),
        (
            LIB,
            "fn passthru<T>(x: T) -> T { return x }\nfn caller() -> int { return passthru(1) }\n",
        ),
    ]);
    let mut v1 = v2.clone();
    let (mid, owner) = body_id(&v1, "caller");
    v1.inferences[mid.raw() as usize]
        .as_mut()
        .expect("the module has an inference result")
        .per_fn
        .get_mut(&owner)
        .expect("the body has an inference record")
        .call_generic_bindings
        .clear();
    let mut g = graph_of(&v1);
    assert_eq!(
        g.unresolved().filter(|u| u.reason == "no-bindings").count(),
        1,
        "the fixture recorded no unresolved call"
    );

    let roots = skull::mir_emit::emission_roots(&v2, Some(PROJ));
    let stats = g.update_bodies_with_edits(&v2, roots, &[(mid, owner)], &[]);

    assert!(
        g.unresolved().next().is_none(),
        "an unresolved record survived the call resolving: {:?}",
        g.unresolved().map(|u| u.reason).collect::<Vec<_>>()
    );
    assert!(
        stats.newly_reached > 0,
        "the resolved call reached no instance"
    );
    assert_same_graph(&v2, &g, &graph_of(&v2), "unresolved cleared");
}

/// The producer is a WHOLE-GRAPH post-pass over the edge set, not the
/// per-instance walk, so dropping the instances' own records cannot clear it.
#[test]
fn removing_polymorphic_recursion_clears_the_whole_graph_rejection() {
    let poly = r#"
fn deep<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    let wrapped: [T] = [x]
    return 1 + deep(wrapped, n - 1)
}
"#;
    let flat = r#"
fn deep<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    return 1 + deep(x, n - 1)
}
"#;
    let mut case = Case::new("fn zz_start() { print(\"{deep(1, 3)}\") }\n", poly);
    assert_eq!(case.recursion_findings().len(), 1);
    assert!(
        case.graph.depth_capped().next().is_some(),
        "the fixture capped no expansion"
    );

    let stats = case.edit(LIB, flat);

    assert!(
        case.recursion_findings().is_empty(),
        "the recursion rejection survived a program that no longer has the recursion"
    );
    assert!(
        case.graph.depth_capped().next().is_none(),
        "capped instances survived"
    );
    assert!(
        stats.pruned > 0,
        "the deep instantiation chain was not pruned"
    );
    case.assert_matches_fresh("recursion removed");
}

/// `depth_capped` WITHOUT polymorphic recursion, so the two producers are
/// separated: `too_deep_to_instantiate` reads the arguments' own depth.
#[test]
fn reducing_a_too_deep_instantiation_removes_the_depth_cap() {
    let deep_main = "fn zz_start() {\n    let v: [[[[[[[[[[[[[[int]]]]]]]]]]]]]] = \
                     [[[[[[[[[[[[[[1]]]]]]]]]]]]]]\n    print(\"{take(v)}\")\n}\n";
    let shallow_main = "fn zz_start() {\n    let v: [[int]] = [[1]]\n    print(\"{take(v)}\")\n}\n";
    let mut case = Case::new(deep_main, "fn take<T>(x: T) -> int { return 1 }\n");
    assert_eq!(
        case.graph.depth_capped().count(),
        1,
        "the fixture capped no expansion; `MAX_INSTANTIATION_DEPTH` may have moved"
    );
    assert!(
        case.recursion_findings().is_empty(),
        "this fixture must cap WITHOUT polymorphic recursion, or it tests two producers"
    );

    let stats = case.edit(MAIN, shallow_main);

    assert!(
        case.graph.depth_capped().next().is_none(),
        "a capped instance survived the instantiation getting shallower"
    );
    assert!(stats.pruned > 0, "the deep instance was not pruned");
    case.assert_matches_fresh("depth cap cleared");
}

#[test]
fn intermediate_body_edits_add_and_remove_recursion_findings() {
    let flat = r#"
fn descend<T>(x: T) -> int { return relay([x]) }
fn relay<T>(x: [T]) -> int { return 0 }
"#;
    let recursive = flat.replace("return 0", "return descend(x)");
    let mut case = Case::new("fn zz_start() { print(\"{descend(1)}\") }\n", flat);
    assert!(case.recursion_findings().is_empty());
    case.edit(LIB, &recursive);
    assert!(
        !case.recursion_findings().is_empty(),
        "helper edit produced no changing recursion"
    );
    case.assert_matches_fresh("recursion added through a helper");

    case.edit(LIB, flat);
    assert!(case.recursion_findings().is_empty());
    case.assert_matches_fresh("recursion removed through a helper");
}

/// Issue #7. `walk` has two DISCONNECTED changing-recursion regions, one per
/// iterator witness. A full build reports only the first start per declaration,
/// so the second region's finding is suppressed while the first exists.
/// Retracting the first must expose the second on the incremental path too,
/// even though nothing the edit touched reaches it.
#[test]
fn retracting_one_recursion_witness_exposes_a_disconnected_alternative() {
    let recursive = r#"
fn walk<I: Iterator<int>>(it: I) -> int {
    let n = 0
    for v in it { n = n + v }
    return n
}
struct ItA { i: int }
fn ItA.next(it) -> (bool, int) { return (false, walk(ItC { i: 0 })) }
struct ItB { i: int }
fn ItB.next(it) -> (bool, int) { return (false, walk(ItD { i: 0 })) }
struct ItC { i: int }
fn ItC.next(it) -> (bool, int) { return (false, 0) }
struct ItD { i: int }
fn ItD.next(it) -> (bool, int) { return (false, 0) }
"#;
    let retracted = recursive.replace("walk(ItC { i: 0 })", "0");
    // `walk<ItC>` stays reached from `main`, so the edit removes only the
    // edge that witnessed the first finding: no instance of `walk` enters or
    // leaves, and nothing seeds the `ItB` region through its declaration.
    let main = "fn zz_start() { print(\"{walk(ItA { i: 0 })} {walk(ItB { i: 0 })} {walk(ItC { i: 0 })}\") }\n";
    let mut case = Case::new(main, recursive);
    let before = case.recursion_findings();
    assert_eq!(before.len(), 1, "fixture findings: {before:?}");
    assert!(
        before[0].contains("ItA"),
        "the fixture must report the region the edit retracts first: {before:?}"
    );

    case.edit(LIB, &retracted);
    let after = case.recursion_findings();
    assert_eq!(after.len(), 1, "the ItB region was not exposed: {after:?}");
    assert!(after[0].contains("ItB"), "{after:?}");
    case.assert_matches_fresh("recursion witness retracted");

    // Restoring the first witness must move the finding back.
    case.edit(LIB, recursive);
    assert_eq!(
        case.recursion_findings(),
        before,
        "the restored witness did not win again"
    );
    case.assert_matches_fresh("recursion witness restored");
}

// ---------------------------------------------------------------------------
// Always-emitted failures (root holes)
// ---------------------------------------------------------------------------

/// A generic function taken as a VALUE, which the concrete-program model
/// refuses: no call site decides its instantiation.
const FNVALUE_LIB: &str = r#"
fn gen_id<T>(x: T) -> T { return x }
fn mono_id(x: int) -> int { return x }
fn takes(f: fn(int) -> int) -> int { return f(1) }
"#;

/// A failed call in an ALWAYS-EMITTED body has no `from` instance, so it lands
/// on the root hole rather than in `unresolved`, and dropping an instance's
/// records cannot clear it.
#[test]
fn fixing_an_always_emitted_call_clears_its_root_hole() {
    let mut case = Case::new(
        "const C: int = takes(gen_id)\nfn zz_start() { print(\"{C}\") }\n",
        FNVALUE_LIB,
    );
    assert_eq!(case.always_emitted_failures(), 1);

    let stats = case.edit(
        MAIN,
        "const C: int = takes(mono_id)\nfn zz_start() { print(\"{C}\") }\n",
    );

    assert_eq!(
        case.always_emitted_failures(),
        0,
        "an always-emitted failure survived the call that produced it"
    );
    assert!(
        stats.newly_reached > 0,
        "`mono_id` was not reached as a new root"
    );
    case.assert_matches_fresh("always-emitted hole cleared");
}

/// Fixing one of two owners' failures on one hole keeps the other owner's, and
/// a backend sibling of the generic root does not double the hole.
#[test]
fn root_hole_refresh_retains_other_owners_failures() {
    let main = r#"
const A: int = takes(gen_id)
const B: int = takes(gen_id)
const C: int = takes(other_id)
fn zz_start() { let n = gen_id(7) }
"#;
    let lib = r#"
@nomangle fn gen_id<T>(x: T) -> T { return x }
@nomangle inline(js) fn gen_id<T>(x: T) -> T = "return x;"
fn mon_id(x: int) -> int { return x }
fn takes(f: fn(int) -> int) -> int { return f(1) }
fn other_id<T>(x: T) -> T { return x }
"#;
    let mut case = Case::new(main, lib);
    assert_eq!(
        case.ws().functions_by_name[&ustr::Ustr::from("gen_id")].len(),
        2,
        "the fixture must contain two backend siblings"
    );
    let check = |case: &Case, failures: usize, label: &str| {
        let gen_hole = &case.graph.uninstantiated_roots["gen_id"];
        assert_eq!(gen_hole.always_emitted_sites.len(), failures, "{label}");
        assert!(gen_hole.generic_root, "{label}");
        assert_eq!(gen_hole.instances, 1, "{label}");
        assert_eq!(case.graph.uninstantiated_roots.len(), 4, "{label}");
        assert_eq!(
            case.graph.uninstantiated_roots["other_id_T"]
                .always_emitted_sites
                .len(),
            1,
            "{label}"
        );
    };
    check(&case, 2, "v1");

    // Equal-length edits isolate refresh from span relocation.
    let one_fixed = main.replace(
        "const A: int = takes(gen_id)",
        "const A: int = takes(mon_id)",
    );
    let both_fixed = one_fixed.replace(
        "const B: int = takes(gen_id)",
        "const B: int = takes(mon_id)",
    );
    for (source, remaining) in [(one_fixed, 1), (both_fixed, 0)] {
        case.edit_keeping_roots(MAIN, &source);
        let label = format!("{remaining} remaining");
        check(&case, remaining, &label);
        case.assert_matches_fresh(&label);
    }
}

#[test]
fn root_hole_spans_follow_insertions_and_deletions() {
    let original = "const A: int = takes(gen_id)\n\
                    const B: int = takes(gen_id)\n\
                    const C: int = takes(other_id) + takes(gen_id)\n\
                    fn zz_start() {}\n";
    let longer = original.replace(
        "const A: int = takes(gen_id)",
        "const A: int = takes(mono_id)",
    );
    let multiline = original.replacen("takes(gen_id)", "takes(\n  mono_id\n)", 1);
    let lib = format!(
        "{FNVALUE_LIB}\nfn other_id<T>(x: T) -> T {{ return x }}\n\
         fn unmoved_id<T>(x: T) -> T {{ return x }}\n\
         const D: int = takes(unmoved_id)\n"
    );
    let mut differences = Vec::new();
    for (label, before, after) in [
        ("insert byte", original, longer.as_str()),
        ("delete byte", longer.as_str(), original),
        ("insert lines", original, multiline.as_str()),
        ("delete lines", multiline.as_str(), original),
    ] {
        let mut case = Case::new(before, &lib);
        assert_eq!(case.graph.uninstantiated_roots.len(), 5, "{label}");
        let other_span = |case: &Case| {
            case.graph.uninstantiated_roots["other_id_T"].always_emitted_sites[0].span
        };
        let old_other_span = other_span(&case);
        let later_owners: Vec<_> = case
            .graph
            .contributions
            .iter()
            .filter(|(_, contribution)| {
                contribution.always_emitted_holes.iter().any(|hole| {
                    hole.failure.span.file.as_str() == MAIN
                        && hole.failure.span.offset_range().0 >= before.find("const B").unwrap()
                })
            })
            .map(|(owner, _)| owner.clone())
            .collect();
        assert_eq!(
            later_owners.len(),
            2,
            "{label}: both later owners must exist"
        );

        let (stats, edits) = case.edit_keeping_roots(MAIN, after);
        assert_eq!(edits.len(), 1, "{label}");
        assert_ne!(edits[0].byte_delta, 0, "{label}");
        for owner in later_owners {
            assert!(
                !stats.revisited_owners.contains(&owner)
                    && !stats.retracted_owners.contains(&owner),
                "{label}: a shifted owner was re-walked instead of relocated: {owner:?}"
            );
        }
        let new_other_span = other_span(&case);
        let mut expected_span = old_other_span;
        assert!(edits[0].shift_span(&mut expected_span));
        if new_other_span.offset_range() != expected_span.offset_range()
            || new_other_span.line_range() != expected_span.line_range()
        {
            differences.push(format!("{label}: retained failure span was not relocated"));
        }
        let fresh = graph_of(case.ws());
        let diff = canonical(case.ws(), &case.graph).diff(&canonical(case.ws(), &fresh));
        differences.extend(diff.into_iter().map(|line| format!("{label}: {line}")));
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}

#[test]
fn mangled_root_holes_count_concrete_instances() {
    let main = "const C: int = takes(gen_id)\nfn zz_start() { let n = gen_id(7) }\n";
    let lib = format!("{FNVALUE_LIB}\ninline(js) fn gen_id<T>(x: T) -> T = \"return x;\"\n");
    let mut case = Case::new(main, &lib);
    assert_eq!(case.graph.uninstantiated_roots["gen_id_T"].instances, 1);
    let two_instances = main.replace(
        "let n = gen_id(7)",
        "let n = gen_id(7)\nlet s = gen_id(\"s\")",
    );
    for (source, expected) in [(two_instances.as_str(), 2), (main, 1)] {
        case.edit_keeping_roots(MAIN, source);
        assert_eq!(
            case.graph.uninstantiated_roots["gen_id_T"].instances,
            expected
        );
        case.assert_matches_fresh("mangled root-hole instance count changed");
    }
}

// ---------------------------------------------------------------------------
// What never reaches `update_bodies`, and reachability it must preserve
// ---------------------------------------------------------------------------

/// **Only function and `const` bodies can appear in a body-only result**, so
/// `update_bodies` rejects every other owner rather than pretending to
/// maintain it. This goes red if the classifier ever gains a body tier for one
/// of these item kinds -- at which point the arm needs an implementation.
#[test]
fn non_function_item_edits_are_interface_tier() {
    let use_all = "fn zz_start() { print(\"{use_all()}\") }\n";
    let base = concat!(
        "struct S { v: int = 1 }\n",
        "enum E { A, B }\n",
        "newtype N(int)\n",
        "deco init mark {\n    print(target.name)\n}\n",
        "fn use_all() -> int { let s = S {}\n return s.v }\n",
    );
    let global =
        |call: &str| format!("global g: int = {call}\nfn zz_start() {{ print(\"{{g}}\") }}\n");
    let cases = [
        (
            "struct defaults",
            use_all.to_string(),
            base,
            LIB,
            base.replace("v: int = 1", "v: int = 2"),
        ),
        (
            "enum",
            use_all.to_string(),
            base,
            LIB,
            base.replace("A, B }", "A, B, C }"),
        ),
        (
            "newtype",
            use_all.to_string(),
            base,
            LIB,
            base.replace("N(int)", "N(float)"),
        ),
        (
            "decorator body",
            use_all.to_string(),
            base,
            LIB,
            base.replace("print(target.name)", "print(\"x\")"),
        ),
        (
            "mutable global",
            global("pick_a()"),
            PICK_LIB,
            MAIN,
            global("pick_b()"),
        ),
    ];
    for (what, main, lib, path, edited) in cases {
        let mut s = session(&main, lib);
        let r = s.update_file(path, &edited).expect("update");
        assert_ne!(
            r.tier,
            UpdateTier::BodyOnly,
            "a {what} edit classified BodyOnly; `update_bodies` rejects that owner and now \
             needs a real implementation for it"
        );
    }
}

/// `make<int>` is reached only through `Holder`'s field default, entered only
/// by the global initializer's `Holder {}` -- an `Owner::AlwaysEmitted`, not an
/// instance. Without the construction it must be absent, so the assertion is
/// about the struct-default edge rather than `make` being always present.
#[test]
fn a_global_initializers_struct_default_reaches_its_omitted_fields_callee() {
    let lib = "fn make<T>(x: T) -> T { return x }\nstruct Holder { v: int = make(5) }\n";
    for (main, expected) in [
        (
            "global G: Holder = Holder {}\nfn zz_start() { print(\"{1}\") }\n",
            true,
        ),
        ("fn zz_start() { print(\"{1}\") }\n", false),
    ] {
        let case = Case::new(main, lib);
        assert_eq!(
            case.has_instance_with("make", "int"),
            expected,
            "`make<int>` reachability through Holder's default from `{main}`"
        );
    }
}
