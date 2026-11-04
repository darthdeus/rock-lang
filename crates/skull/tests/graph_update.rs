//! **`InstanceGraph::update_bodies` against a fresh build**, one causal
//! producer per test.
//!
//! The oracle is `graph_snapshot::canonical`: raw field equality is invalid
//! across arenas, because the updated graph's arena is the one v1 interned into
//! and a fresh build's is not.
//!
//! **Equality alone is not sufficient and is not the whole assertion.** An
//! updater that quietly rebuilt the world would satisfy it perfectly, so every
//! arm also asserts the structural census: how many owners were walked against
//! how many the graph holds. And an arm that asserted only "the fact is gone"
//! would pass over a fixture that never produced it, so every arm asserts its
//! v1 population first.
//!
//! Edits go through a real `bone_incremental::Session`, so the changed-body set
//! is `UpdateResult::reinferred` rather than a hand-written guess at what an
//! edit touches.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use bone_incremental::session::{NsAliases, Session, SessionFile, UpdateTier};
use bone_semantic::inference::Workspace;
use skull::graph_snapshot::{canonical, instance_key};
use skull::instance_graph::{self, Instance, InstanceGraph, RejectionKind, UpdateStats};
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

/// The compilation target's own directory. Everything outside it is a LIBRARY,
/// which is what makes anything prunable at all: `mir_emit::is_emission_root`
/// roots every declaration of the target itself, so a fixture whose functions
/// all live in `main.rock` can never lose one.
const PROJ: &str = "proj/";
const MAIN: &str = "proj/main.rock";
const LIB: &str = "lib/helpers.rock";

/// An instance key without its declaration indices, so an expected set can be
/// written down and still name the declaration exactly.
fn short(key: &String) -> String {
    key.split('#').next().unwrap_or(key).to_string()
}

fn graph_of(ws: &Workspace) -> InstanceGraph {
    instance_graph::build(ws, skull::mir_emit::emission_roots(ws, Some(PROJ)), false)
}

/// One persisted graph across a sequence of edits.
struct Case {
    session: Session,
    graph: InstanceGraph,
}

impl Case {
    fn new(main: &str, lib: &str) -> Self {
        let session = Session::new(
            STDLIB.clone(),
            vec![
                SessionFile::legacy(MAIN, main),
                SessionFile::legacy(LIB, lib),
            ],
            &NsAliases::default(),
        )
        .expect("session");
        let graph = graph_of(session.workspace());
        Case { session, graph }
    }

    fn ws(&self) -> &Workspace {
        self.session.workspace()
    }

    /// Apply one BODY-ONLY edit and update the persisted graph with the exact
    /// owners incremental inference reinferred.
    ///
    /// Each arm compares the incremental result with a fresh whole-graph build.
    fn edit(&mut self, path: &str, text: &str) -> UpdateStats {
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
        let ws = self.session.workspace();
        let roots = skull::mir_emit::emission_roots(ws, Some(PROJ));
        let edits = res
            .body_update
            .edit
            .into_iter()
            .map(|edit| instance_graph::SpanEdit {
                file: edit.file,
                at: edit.at,
                byte_delta: edit.byte_delta,
                line_delta: edit.line_delta,
            })
            .collect::<Vec<_>>();
        self.graph
            .update_bodies_with_edits(ws, roots, &res.reinferred, &edits)
    }

    /// The updated graph must MEAN the same thing as a build from scratch.
    fn assert_matches_fresh(&self, label: &str) {
        let fresh = graph_of(self.ws());
        let a = canonical(self.ws(), &self.graph);
        let b = canonical(self.ws(), &fresh);
        let d = a.diff(&b);
        assert!(
            d.is_empty(),
            "{label}: the updated graph and a fresh build disagree:\n{}",
            d.join("\n")
        );
    }

    fn has_instance_named(&self, name: &str) -> bool {
        self.graph
            .instances
            .iter()
            .any(|i| self.identity(i).contains(name))
    }

    fn identity(&self, i: &Instance) -> String {
        instance_key(self.ws(), &self.graph, i)
    }

    fn instance_names(&self) -> BTreeSet<String> {
        self.graph
            .instances
            .iter()
            .map(|i| self.identity(i))
            .collect()
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

// ---------------------------------------------------------------------------
// 1. Deletion strands an ordinary subtree
// ---------------------------------------------------------------------------

const CHAIN_LIB: &str = r#"
fn chain_a() -> int { return chain_b() + 1 }
fn chain_b() -> int { return chain_c() + 1 }
fn chain_c() -> int { return 3 }
fn other() -> int { return 7 }
"#;

#[test]
fn incremental_update_preserves_graph_verification_configuration() {
    let before = std::env::var_os("ROCK_GRAPH_VERIFY");
    let mut case = Case::new(
        "fn main() { print(\"{chain_a()} {other()}\") }\n",
        CHAIN_LIB,
    );

    case.edit(MAIN, "fn main() { print(\"{other()}\") }\n");
    case.assert_matches_fresh("subtree deletion");

    assert_eq!(
        std::env::var_os("ROCK_GRAPH_VERIFY"),
        before,
        "an incremental test changed the process verification configuration"
    );
}

#[test]
fn deleting_the_only_edge_into_a_subtree_removes_the_whole_subtree() {
    let mut case = Case::new(
        "fn main() { print(\"{chain_a()} {other()}\") }\n",
        CHAIN_LIB,
    );
    for name in ["chain_a", "chain_b", "chain_c"] {
        assert!(case.has_instance_named(name), "v1 never reached `{name}`");
    }
    let before = case.instance_names();

    let stats = case.edit(MAIN, "fn main() { print(\"{other()}\") }\n");

    for name in ["chain_a", "chain_b", "chain_c"] {
        assert!(
            case.has_instance_named(name).eq(&false),
            "`{name}` survived the deletion"
        );
    }
    assert!(
        case.has_instance_named("other"),
        "an untouched sibling was pruned too"
    );
    let removed: Vec<String> = before
        .difference(&case.instance_names())
        .map(short)
        .collect();
    assert_eq!(
        removed,
        vec!["chain_a_0", "chain_b_0", "chain_c_0"],
        "expected exactly the three stranded instances, out of {} before",
        before.len()
    );
    assert_eq!(stats.pruned, removed.len());
    case.assert_matches_fresh("subtree deletion");
    assert_scoped(&stats, "subtree deletion");
}

#[test]
fn explicit_root_delta_adds_and_removes_a_library_subtree() {
    let mut case = Case::new("fn main() { print(\"{other()}\") }\n", CHAIN_LIB);
    let ws = case.session.workspace();
    let chain_a = crate::common::fn_ref(ws, "chain_a");
    let base_roots = skull::mir_emit::emission_roots(ws, Some(PROJ));
    assert!(!base_roots.contains(&chain_a));
    for name in ["chain_a", "chain_b", "chain_c"] {
        assert!(
            !case.has_instance_named(name),
            "v1 already reached `{name}`"
        );
    }

    let added = case
        .graph
        .update_bodies_with_root_delta(ws, &[chain_a], &[], &[], &[]);
    let subtree = BTreeSet::from([
        "chain_a_0".to_string(),
        "chain_b_0".to_string(),
        "chain_c_0".to_string(),
    ]);
    let added_names: BTreeSet<String> = added
        .added
        .iter()
        .map(|i| short(&case.identity(i)))
        .collect();
    assert_eq!(added.newly_reached, 3);
    assert_eq!(added_names, subtree);
    let mut roots = base_roots.clone();
    roots.push(chain_a);
    let fresh = instance_graph::build(ws, roots, false);
    let diff = canonical(ws, &case.graph).diff(&canonical(ws, &fresh));
    assert!(diff.is_empty(), "root added: {}", diff.join("\n"));

    let removed = case
        .graph
        .update_bodies_with_root_delta(ws, &[], &[chain_a], &[], &[]);
    let removed_names: BTreeSet<String> = removed
        .removed
        .iter()
        .map(|i| short(&case.identity(i)))
        .collect();
    assert_eq!(removed.pruned, 3);
    assert_eq!(removed_names, subtree);
    case.assert_matches_fresh("root removed");
}

#[test]
fn a_replaced_root_can_be_restored_by_an_earlier_install_in_its_wave() {
    let ws = check_two_files(
        "fn main() {}",
        "fn old_root() -> int { return leaf() }\n\
         fn leaf() -> int { return 1 }\n\
         fn new_root() -> int { return old_root() }\n",
    );
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
        &[body_of(&ws, "old_root")],
        &[],
    );
    assert!(stats.expanded >= 2);
    assert!(graph.substitution(&old_instance).is_some());
    let fresh = instance_graph::build(&ws, vec![new_root], false);
    let diff = canonical(&ws, &graph).diff(&canonical(&ws, &fresh));
    assert!(diff.is_empty(), "root replaced: {}", diff.join("\n"));
}

// ---------------------------------------------------------------------------
// 2. Deletion prunes an unreachable CYCLE
// ---------------------------------------------------------------------------

/// The arm a reachability-free updater gets wrong. Every member of the cycle
/// still has an incoming edge after the external one is cut, so nothing about
/// it looks unreferenced -- only reachability FROM A ROOT can see it.
const CYCLE_LIB: &str = r#"
fn cyc_a(n: int) -> int { if n <= 0 { return 0 } return cyc_b(n - 1) }
fn cyc_b(n: int) -> int { if n <= 0 { return 1 } return cyc_a(n - 1) }
fn other() -> int { return 7 }
"#;

#[test]
fn deleting_the_only_external_edge_prunes_a_mutually_reachable_cycle() {
    let mut case = Case::new("fn main() { print(\"{cyc_a(4)} {other()}\") }\n", CYCLE_LIB);
    assert!(case.has_instance_named("cyc_a") && case.has_instance_named("cyc_b"));
    // The cycle is real: each member has an incoming edge from the other, so a
    // "nothing points at it" rule would keep both alive forever.
    let incoming = |name: &str| {
        case.graph
            .edges()
            .filter(|e| case.identity(&e.to).contains(name))
            .count()
    };
    assert!(
        incoming("cyc_a") > 0 && incoming("cyc_b") > 0,
        "the fixture is not a cycle"
    );
    let before = case.instance_names();

    let stats = case.edit(MAIN, "fn main() { print(\"{other()}\") }\n");

    assert!(
        !case.has_instance_named("cyc_a"),
        "`cyc_a` survived: the cycle kept itself alive"
    );
    assert!(
        !case.has_instance_named("cyc_b"),
        "`cyc_b` survived: the cycle kept itself alive"
    );
    // BY NAME, not by count: the third is `__op_sub_int_int`, whose
    // only caller was the cycle, and a count would hide a swap.
    let removed: Vec<String> = before
        .difference(&case.instance_names())
        .map(short)
        .collect();
    assert_eq!(removed, vec!["__op_sub_int_int", "cyc_a_int", "cyc_b_int"]);
    assert_eq!(stats.pruned, removed.len());
    case.assert_matches_fresh("cycle pruning");
    assert_scoped(&stats, "cycle pruning");
}

fn generated_cycle_sources(length: usize, rooted: bool) -> (String, String, String) {
    let mut lib = String::new();
    for i in 0..length {
        let next = (i + 1) % length;
        lib.push_str(&format!(
            "fn cycle_{i}() -> int {{ return cycle_{next}() }}\n"
        ));
    }
    let v1 = if rooted {
        "fn main() { print(\"{cycle_0()}\") }\nfn root_anchor() -> int { return cycle_0() }\n"
            .to_string()
    } else {
        "fn main() { print(\"{cycle_0()}\") }\n".to_string()
    };
    let v2 = if rooted {
        "fn main() { print(\"0\") }\nfn root_anchor() -> int { return cycle_0() }\n".to_string()
    } else {
        "fn main() { print(\"0\") }\n".to_string()
    };
    (v1, v2, lib)
}

fn assert_generated_cycle(length: usize, rooted: bool) {
    let (v1, v2, lib) = generated_cycle_sources(length, rooted);
    let mut case = Case::new(&v1, &lib);
    for i in 0..length {
        assert!(
            case.has_instance_named(&format!("cycle_{i}")),
            "v1 never reached cycle member {i} at length {length}"
        );
    }
    let seed = case
        .graph
        .instances
        .iter()
        .find(|instance| case.identity(instance).starts_with("cycle_0"))
        .cloned()
        .expect("cycle_0 instance");
    let blocker_name = format!("cycle_{}", length - 1);
    let incoming = case.graph.index.preds.get(&seed).expect("cycle seed row");
    assert_eq!(
        incoming.len(),
        if rooted { 3 } else { 2 },
        "unexpected seed predecessor population at cycle length {length}: {incoming:?}"
    );
    assert_eq!(
        incoming
            .iter()
            .filter(|pred| case.identity(pred).starts_with(&blocker_name))
            .count(),
        1,
        "the cycle seed has no exact predecessor blocker at length {length}: {incoming:?}"
    );
    if rooted {
        assert!(
            incoming
                .iter()
                .any(|pred| case.identity(pred).starts_with("root_anchor")),
            "the rooted cycle has no root boundary at length {length}: {incoming:?}"
        );
    }

    let stats = case.edit(MAIN, &v2);
    assert_eq!(
        stats.prune_seeds.len(),
        1,
        "the cycle edit changed more than one initial seed at length {length}: {stats:?}"
    );
    assert!(
        stats
            .prune_seeds
            .iter()
            .any(|instance| case.identity(instance).starts_with("cycle_0")),
        "cycle_0 was not the exact lost seed at length {length}: {stats:?}"
    );
    if rooted {
        for i in 0..length {
            assert!(
                case.has_instance_named(&format!("cycle_{i}")),
                "rooted cycle member {i} was pruned at length {length}: {stats:?}"
            );
        }
    } else {
        assert_eq!(
            stats.support_component_nodes_examined,
            length - 1,
            "the unanchored cycle support component did not cover every non-seed member: {stats:?}"
        );
        assert_eq!(
            stats.support_component_edges_examined,
            length - 1,
            "the unanchored cycle support component did not inspect every predecessor edge: {stats:?}"
        );
        for i in 0..length {
            assert!(
                !case.has_instance_named(&format!("cycle_{i}")),
                "unanchored cycle member {i} survived at length {length}: {stats:?}"
            );
        }
    }
    case.assert_matches_fresh(&format!(
        "{} cycle length {length}",
        if rooted { "rooted" } else { "unanchored" }
    ));
    assert_scoped(&stats, &format!("cycle length {length}"));
}

#[test]
fn an_unanchored_three_cycle_is_depth_independently_pruned() {
    assert_generated_cycle(3, false);
}

#[test]
fn an_unanchored_eight_cycle_is_depth_independently_pruned() {
    assert_generated_cycle(8, false);
}

#[test]
fn a_rooted_three_cycle_is_depth_independently_preserved() {
    assert_generated_cycle(3, true);
}

#[test]
fn a_rooted_eight_cycle_is_depth_independently_preserved() {
    assert_generated_cycle(8, true);
}

fn generated_support_sources(depth: usize) -> (String, String, String, Vec<String>) {
    let mut lib = String::from(
        "fn shared<T>(x: T) -> int { return shared_left() + shared_right() }\n\
         fn shared_left() -> int { return shared_left_a() + shared_left_b() }\n\
         fn shared_right() -> int { return shared_right_a() + shared_right_b() }\n\
         fn shared_left_a() -> int { return shared_left_a_0() + shared_left_a_1() }\n\
         fn shared_left_b() -> int { return shared_left_b_0() + shared_left_b_1() }\n\
         fn shared_right_a() -> int { return shared_right_a_0() + shared_right_a_1() }\n\
         fn shared_right_b() -> int { return shared_right_b_0() + shared_right_b_1() }\n\
         fn shared_left_a_0() -> int { return 1 }\n\
         fn shared_left_a_1() -> int { return 2 }\n\
         fn shared_left_b_0() -> int { return 3 }\n\
         fn shared_left_b_1() -> int { return 4 }\n\
         fn shared_right_a_0() -> int { return 5 }\n\
         fn shared_right_a_1() -> int { return 6 }\n\
         fn shared_right_b_0() -> int { return 7 }\n\
         fn shared_right_b_1() -> int { return 8 }\n",
    );
    for i in 0..depth {
        if i + 1 == depth {
            lib.push_str(&format!("fn support_{i}() -> int {{ return shared(1) }}\n"));
        } else {
            lib.push_str(&format!(
                "fn support_{i}() -> int {{ return support_{}() }}\n",
                i + 1
            ));
        }
    }
    let v1 = format!(
        "fn main() {{ print(\"{{shared(1)}}\") }}\n\
         fn independent_root() -> int {{ return support_0() }}\n"
    );
    let v2 = "fn main() { print(\"0\") }\nfn independent_root() -> int { return support_0() }\n"
        .to_string();
    let subtree = [
        "shared",
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
    ]
    .into_iter()
    .map(str::to_string)
    .chain((0..depth).map(|i| format!("support_{i}")))
    .collect();
    (v1, v2, lib, subtree)
}

fn assert_support_depth(depth: usize) {
    let (v1, v2, lib, population) = generated_support_sources(depth);
    let mut case = Case::new(&v1, &lib);
    for name in &population {
        assert!(
            case.has_instance_named(name),
            "v1 never reached {name} at support depth {depth}"
        );
    }
    let shared = case
        .graph
        .instances
        .iter()
        .find(|instance| {
            let identity = case.identity(instance);
            identity.starts_with("shared")
                && !identity.starts_with("shared_left")
                && !identity.starts_with("shared_right")
        })
        .cloned()
        .expect("shared generic seed");
    let incoming = case
        .graph
        .index
        .preds
        .get(&shared)
        .expect("shared seed row");
    assert_eq!(
        incoming.len(),
        2,
        "the support-depth fixture has an unexpected seed population at depth {depth}: {incoming:?}"
    );
    assert!(
        incoming.iter().any(|pred| case
            .identity(pred)
            .starts_with(&format!("support_{}", depth - 1))),
        "the support chain does not directly block shared at depth {depth}: {incoming:?}"
    );

    let stats = case.edit(MAIN, &v2);
    assert_eq!(
        stats.prune_seeds.len(),
        1,
        "the support edit changed more than one initial seed at depth {depth}: {stats:?}"
    );
    assert!(
        stats
            .prune_seeds
            .iter()
            .any(|instance| case.identity(instance).starts_with("shared")),
        "shared was not the exact lost seed at support depth {depth}: {stats:?}"
    );
    assert_eq!(
        stats.support_component_nodes_examined, depth,
        "the support component did not cover the full live chain at depth {depth}: {stats:?}"
    );
    assert_eq!(
        stats.support_component_edges_examined, depth,
        "the support component did not inspect the full live chain at depth {depth}: {stats:?}"
    );
    for name in &population {
        assert!(
            case.has_instance_named(name),
            "live support or shared subtree {name} was pruned at depth {depth}: {stats:?}"
        );
    }
    case.assert_matches_fresh(&format!("support chain depth {depth}"));
    assert!(
        stats.suspect_region < 15,
        "the shared subtree was expanded at support depth {depth}: {stats:?}"
    );
    assert_scoped(&stats, &format!("support chain depth {depth}"));
}

#[test]
fn a_live_non_root_support_chain_of_depth_one_stays_local() {
    assert_support_depth(1);
}

#[test]
fn a_live_non_root_support_chain_of_depth_three_stays_local() {
    assert_support_depth(3);
}

#[test]
fn a_live_non_root_support_chain_of_depth_eight_stays_local() {
    assert_support_depth(8);
}

const INTERSECTING_SUPPORT_MAIN_V1: &str =
    "fn main() { print(\"{lost_a()} {lost_b()} {independent_root()}\") }\n";
const INTERSECTING_SUPPORT_MAIN_V2: &str = "fn main() { print(\"{independent_root()}\") }\n";
const INTERSECTING_SUPPORT_LIB: &str = r#"
fn lost_a() -> int { return shared() + support_a() }
fn lost_b() -> int { return shared() + support_b() }
fn support_a() -> int { return lost_a() + support_join() }
fn support_b() -> int { return lost_b() + support_join() }
fn support_join() -> int { return support_a() + support_b() }
fn shared() -> int { return shared_left() + shared_right() }
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
fn independent_root() -> int { return shared() }
"#;

#[test]
fn intersecting_tentative_support_components_release_together() {
    let mut case = Case::new(INTERSECTING_SUPPORT_MAIN_V1, INTERSECTING_SUPPORT_LIB);
    for name in [
        "lost_a",
        "lost_b",
        "support_a",
        "support_b",
        "support_join",
        "shared",
    ] {
        assert!(case.has_instance_named(name), "v1 never reached {name}");
    }
    let find = |name: &str, case: &Case| {
        case.graph
            .instances
            .iter()
            .find(|instance| case.identity(instance).starts_with(name))
            .cloned()
            .expect("intersecting-support instance")
    };
    let lost_a = find("lost_a", &case);
    let lost_b = find("lost_b", &case);
    let support_a = find("support_a", &case);
    let support_b = find("support_b", &case);
    let support_join = find("support_join", &case);
    let preds_a = case.graph.index.preds.get(&lost_a).expect("lost_a row");
    let preds_b = case.graph.index.preds.get(&lost_b).expect("lost_b row");
    assert_eq!(
        preds_a.len(),
        2,
        "lost_a has an unexpected blocker population"
    );
    assert_eq!(
        preds_b.len(),
        2,
        "lost_b has an unexpected blocker population"
    );
    assert!(
        preds_a.contains(&support_a),
        "support_a does not block lost_a"
    );
    assert!(
        preds_b.contains(&support_b),
        "support_b does not block lost_b"
    );
    assert!(
        case.graph
            .index
            .preds
            .get(&support_a)
            .is_some_and(|preds| preds.contains(&lost_a)),
        "the intersecting component has no lost_a boundary"
    );
    assert!(
        case.graph
            .index
            .preds
            .get(&support_join)
            .is_some_and(|preds| preds.contains(&support_a) && preds.contains(&support_b)),
        "the two tentative support components do not intersect at support_join"
    );

    let stats = case.edit(MAIN, INTERSECTING_SUPPORT_MAIN_V2);
    assert_eq!(
        stats.prune_seeds.len(),
        2,
        "the intersecting fixture changed an unexpected seed population: {stats:?}"
    );
    for name in ["lost_a", "lost_b"] {
        assert!(
            stats
                .prune_seeds
                .iter()
                .any(|instance| case.identity(instance).starts_with(name)),
            "{name} was not an exact lost seed: {stats:?}"
        );
    }
    for name in ["lost_a", "lost_b", "support_a", "support_b", "support_join"] {
        assert!(
            !case.has_instance_named(name),
            "{name} survived the closed support component: {stats:?}"
        );
    }
    assert!(
        stats.support_component_nodes_examined > 0 && stats.support_component_edges_examined > 0,
        "the intersecting support components had no measured population: {stats:?}"
    );
    for name in [
        "shared",
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
    ] {
        assert!(
            case.has_instance_named(name),
            "{name} lost its external root"
        );
    }
    case.assert_matches_fresh("intersecting support components");
    assert!(
        stats.suspect_region < 15,
        "the intersecting support fixture expanded shared's subtree: {stats:?}"
    );
    assert_scoped(&stats, "intersecting support components");
}

fn branching_support_sources(dead_first: bool) -> (String, String, String) {
    let v1 = "fn main() { print(\"{lost_b()} {dead_c()}\") }\n\
fn independent_root() -> int { return root_branch() }\n"
        .to_string();
    let v2 = "fn main() { print(\"0\") }\n\
fn independent_root() -> int { return root_branch() }\n"
        .to_string();
    let mut lib = String::from(
        "fn target() -> int { return lost_b() }\n\
         fn dead_c() -> int { return dead_a() }\n\
         fn lost_b() -> int { return 7 }\n",
    );
    if dead_first {
        lib.push_str(
            "fn root_branch() -> int { return target() }\n\
             fn dead_branch() -> int { return dead_c() + target() }\n",
        );
    } else {
        lib.push_str(
            "fn dead_branch() -> int { return dead_c() + target() }\n\
             fn root_branch() -> int { return target() }\n",
        );
    }
    lib.push_str("fn dead_a() -> int { return dead_branch() }\n");
    (v1, v2, lib)
}

fn assert_branching_support_fixture(dead_first: bool) {
    let (v1, v2, lib) = branching_support_sources(dead_first);
    let mut case = Case::new(&v1, &lib);
    for name in [
        "target",
        "lost_b",
        "dead_c",
        "dead_a",
        "dead_branch",
        "root_branch",
    ] {
        assert!(case.has_instance_named(name), "v1 never reached {name}");
    }
    let find = |name: &str, case: &Case| {
        case.graph
            .instances
            .iter()
            .find(|instance| case.identity(instance).starts_with(name))
            .cloned()
            .expect("branching-support instance")
    };
    let target = find("target", &case);
    let root_branch = find("root_branch", &case);
    let dead_branch = find("dead_branch", &case);
    let target_preds = case.graph.index.preds.get(&target).expect("target row");
    assert_eq!(
        target_preds.len(),
        2,
        "target has an unexpected predecessor population: {target_preds:?}"
    );
    assert!(
        target_preds.contains(&root_branch),
        "rooted target branch is absent"
    );
    assert!(
        target_preds.contains(&dead_branch),
        "dead target branch is absent"
    );
    let dead_c = find("dead_c", &case);
    let lost_b = find("lost_b", &case);
    assert_eq!(
        case.graph
            .index
            .preds
            .get(&dead_c)
            .expect("dead_c row")
            .len(),
        2,
        "dead cycle seed has an unexpected predecessor population"
    );
    assert_eq!(
        case.graph
            .index
            .preds
            .get(&lost_b)
            .expect("lost_b row")
            .len(),
        2,
        "second lost seed has an unexpected predecessor population"
    );

    let stats = case.edit(MAIN, &v2);
    assert_eq!(
        stats.prune_seeds.len(),
        2,
        "branching support changed an unexpected seed population: {stats:?}"
    );
    assert!(
        stats
            .prune_seeds
            .iter()
            .any(|instance| case.identity(instance).starts_with("dead_c")),
        "dead_c was not an initial lost seed: {stats:?}"
    );
    assert!(
        stats
            .prune_seeds
            .iter()
            .any(|instance| case.identity(instance).starts_with("lost_b")),
        "lost_b was not an initial lost seed: {stats:?}"
    );
    assert_eq!(
        stats.suspect_region, 4,
        "unexpected branching forward population: {stats:?}"
    );
    assert_eq!(
        stats.boundary_predecessor_edges_examined, 8,
        "unexpected branching boundary population: {stats:?}"
    );
    assert_eq!(
        stats.support_component_nodes_examined, 4,
        "unexpected branching support-node population: {stats:?}"
    );
    assert_eq!(
        stats.support_component_edges_examined, 5,
        "unexpected branching support-edge population: {stats:?}"
    );
    assert_eq!(
        stats.prune_search_population, 21,
        "unexpected branching prune denominator: {stats:?}"
    );
    assert_eq!(
        stats.pruned, 3,
        "unexpected branching pruned population: {stats:?}"
    );
    assert!(
        case.has_instance_named("target"),
        "the rooted target was pruned: {stats:?}"
    );
    for name in ["dead_c", "dead_a", "dead_branch"] {
        assert!(
            !case.has_instance_named(name),
            "dead branch member {name} survived: {stats:?}"
        );
    }
    assert!(
        stats.support_component_nodes_examined > 0 && stats.support_component_edges_examined > 0,
        "the branching support search had no measured population: {stats:?}"
    );
    case.assert_matches_fresh("branching support component");
    assert_scoped(&stats, "branching support component");
}

#[test]
fn branching_support_cache_does_not_launder_dead_branch_liveness() {
    assert_branching_support_fixture(true);
}

#[test]
fn branching_support_cache_is_independent_of_branch_order() {
    assert_branching_support_fixture(false);
}

// ---------------------------------------------------------------------------
// 7. Addition reaches a new specialization
// ---------------------------------------------------------------------------

const GENERIC_LIB: &str = r#"
fn passthru<T>(x: T) -> T { return x }
fn ident<T>(x: T) -> T { return passthru(x) }
"#;

#[test]
fn adding_a_call_records_a_new_specialization() {
    let mut case = Case::new("fn main() { print(\"{ident(1)}\") }\n", GENERIC_LIB);
    let has_string_ident = |c: &Case| {
        c.graph.instances.iter().any(|i| {
            let k = c.identity(i);
            k.starts_with("ident") && k.contains("string")
        })
    };
    assert!(!has_string_ident(&case), "v1 already had `ident<string>`");

    let stats = case.edit(MAIN, "fn main() { print(\"{ident(1)} {ident(\"s\")}\") }\n");

    assert!(
        has_string_ident(&case),
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

// ---------------------------------------------------------------------------
// 6. Removing changing-instance recursion clears the whole-graph rejection
// ---------------------------------------------------------------------------

/// The producer is a WHOLE-GRAPH post-pass over the edge set, not the
/// per-instance walk, so dropping the instances' own records cannot clear it.
const POLY_LIB: &str = r#"
fn deep<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    let wrapped: [T] = [x]
    return 1 + deep(wrapped, n - 1)
}
"#;

const FLAT_LIB: &str = r#"
fn deep<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    return 1 + deep(x, n - 1)
}
"#;

#[test]
fn removing_polymorphic_recursion_clears_the_whole_graph_rejection() {
    let mut case = Case::new("fn main() { print(\"{deep(1, 3)}\") }\n", POLY_LIB);
    let recursion_rejections = |c: &Case| {
        c.graph
            .rejections()
            .filter(|r| r.kind == RejectionKind::ChangingInstanceRecursion)
            .count()
    };
    assert_eq!(
        recursion_rejections(&case),
        1,
        "the fixture produced no recursion rejection"
    );
    assert!(
        case.graph.depth_capped().next().is_some(),
        "the fixture capped no expansion"
    );

    let stats = case.edit(LIB, FLAT_LIB);

    assert_eq!(
        recursion_rejections(&case),
        0,
        "the recursion rejection survived a program that no longer has the recursion"
    );
    assert!(
        case.graph.depth_capped().next().is_none(),
        "capped instances survived: {:?}",
        case.graph.depth_capped().count()
    );
    assert!(
        stats.pruned > 0,
        "the deep instantiation chain was not pruned"
    );
    case.assert_matches_fresh("recursion removed");
}

// ---------------------------------------------------------------------------
// 8. A sequential chain on ONE persisted graph
// ---------------------------------------------------------------------------

/// Delete, then add, then delete, comparing after every edit. Each step starts
/// from the graph the previous one produced, so a residue that survives one
/// edit and is masked by the next is a failure here rather than invisible.
#[test]
fn a_sequential_edit_chain_agrees_with_a_fresh_build_at_every_step() {
    let mut case = Case::new(
        "fn main() { print(\"{chain_a()} {other()}\") }\n",
        CHAIN_LIB,
    );
    case.assert_matches_fresh("v1");

    let s1 = case.edit(MAIN, "fn main() { print(\"{other()}\") }\n");
    assert!(
        s1.pruned == 3 && s1.newly_reached == 0,
        "step 1 is a deletion: {s1:?}"
    );
    case.assert_matches_fresh("after delete");

    let s2 = case.edit(MAIN, "fn main() { print(\"{chain_a()} {other()}\") }\n");
    assert!(
        s2.newly_reached == 3 && s2.pruned == 0,
        "step 2 is an addition: {s2:?}"
    );
    case.assert_matches_fresh("after re-add");

    let s3 = case.edit(MAIN, "fn main() { print(\"{chain_b()}\") }\n");
    assert!(s3.pruned > 0, "step 3 strands `chain_a`: {s3:?}");
    assert!(
        !case.has_instance_named("chain_a"),
        "`chain_a` survived the third edit"
    );
    assert!(
        case.has_instance_named("chain_c"),
        "`chain_c` is still reached through `chain_b`"
    );
    case.assert_matches_fresh("after second delete");
}

// ---------------------------------------------------------------------------
// 3, 4, 5. The stale-state producers, each clearing on its own
// ---------------------------------------------------------------------------

/// A v1 whose INFERENCE is missing a record, and the intact v2 beside it.
///
/// Two of the four stale-state producers cannot be reached from a valid program
/// at all: `unresolved` needs a generic call with no binding record, and
/// `storage_failures` needs a receiver with no concrete type inside an
/// INSTANTIATED body -- both are compiler state disagreeing with itself.
/// `instance_graph.rs`'s own arms reach them the same way, by removing the
/// record from a `Workspace` rather than by corrupting the source, and the
/// "fix" is then the intact workspace. The graph is built against v1 and
/// updated against v2, which is exactly the shape a body edit has: the
/// declarations are identical, one body's facts changed.
struct Perturbed {
    v1: Workspace,
    v2: Workspace,
    changed: Vec<(
        bone_semantic::prelude::ModuleId,
        bone_semantic::hir::BodyOwner,
    )>,
}

fn check_two_files(main: &str, lib: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(main, Ustr::from(MAIN)).expect("parse main"));
    modules.push(skull_parser::parse_module_clean(lib, Ustr::from(LIB)).expect("parse lib"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

fn body_of(
    ws: &Workspace,
    fn_name: &str,
) -> (
    bone_semantic::prelude::ModuleId,
    bone_semantic::hir::BodyOwner,
) {
    for module in ws.modules.iter() {
        for (fid, f) in module.functions.iter() {
            if f.name.as_str() == fn_name {
                return (module.id, bone_semantic::hir::BodyOwner::Function(fid));
            }
        }
    }
    panic!("no fn named `{fn_name}`");
}

impl Perturbed {
    /// `perturb` returns how many records it removed -- the denominator, so a
    /// helper that silently removed nothing cannot make its test vacuous.
    fn new(
        main: &str,
        lib: &str,
        fn_name: &str,
        perturb: impl FnOnce(
            &bone_semantic::hir::Body,
            &mut bone_semantic::inference::BodyInference,
        ) -> usize,
    ) -> Self {
        let v2 = check_two_files(main, lib);
        let mut v1 = v2.clone();
        let (mid, owner) = body_of(&v1, fn_name);
        let body = v1
            .module(mid)
            .bodies
            .get(&owner)
            .expect("the fn has a body")
            .clone();
        let inf = v1.inferences[mid.raw() as usize]
            .as_mut()
            .expect("the module has an inference result")
            .per_fn
            .get_mut(&owner)
            .expect("the body has an inference record");
        let removed = perturb(&body, inf);
        assert!(
            removed > 0,
            "the perturbation removed nothing from `{fn_name}`"
        );
        Perturbed {
            v1,
            v2,
            changed: vec![(mid, owner)],
        }
    }

    /// Build against v1, update to v2, and compare with a fresh v2 build.
    fn update(&self) -> (InstanceGraph, UpdateStats) {
        let mut g = graph_of(&self.v1);
        let roots = skull::mir_emit::emission_roots(&self.v2, Some(PROJ));
        let stats = g.update_bodies_with_edits(&self.v2, roots, &self.changed, &[]);
        (g, stats)
    }

    fn assert_matches_fresh(&self, g: &InstanceGraph, label: &str) {
        let fresh = graph_of(&self.v2);
        let d = canonical(&self.v2, g).diff(&canonical(&self.v2, &fresh));
        assert!(
            d.is_empty(),
            "{label}: updated and fresh disagree:\n{}",
            d.join("\n")
        );
    }
}

const IDENT_LIB: &str = r#"
fn passthru<T>(x: T) -> T { return x }
fn caller() -> int { return passthru(1) }
"#;

/// 3. A call that could not be turned into an instance, and then can be.
#[test]
fn resolving_a_call_removes_its_stale_unresolved_record() {
    let p = Perturbed::new(
        "fn main() { print(\"{caller()}\") }\n",
        IDENT_LIB,
        "caller",
        |_body, inf| {
            let n = inf.call_generic_bindings.len();
            inf.call_generic_bindings.clear();
            n
        },
    );
    let before = graph_of(&p.v1);
    let stale: Vec<&str> = before
        .unresolved()
        .filter(|u| u.reason == "no-bindings")
        .map(|u| u.reason)
        .collect();
    assert_eq!(
        stale.len(),
        1,
        "the fixture recorded no unresolved call: {:?}",
        before.unresolved().map(|u| u.reason).collect::<Vec<_>>()
    );

    let (g, stats) = p.update();
    assert!(
        g.unresolved().next().is_none(),
        "an unresolved record survived the call resolving: {:?}",
        g.unresolved().map(|u| u.reason).collect::<Vec<_>>()
    );
    assert!(
        stats.newly_reached > 0,
        "the resolved call reached no instance"
    );
    p.assert_matches_fresh(&g, "unresolved cleared");
}

const MAP_LIB: &str = r#"
fn probe(m: Map<string, int>) -> int { return m.len() }
"#;

/// 5. A type conversion that could not decide, and then can.
#[test]
fn a_resolvable_conversion_removes_its_stale_storage_failure() {
    let p = Perturbed::new(
        "fn main() { let m: Map<string, int> = map_new()\n print(\"{probe(m)}\") }\n",
        MAP_LIB,
        "probe",
        |body, inf| {
            // The RECEIVER of a synthesized-call trigger is what the storage
            // condition is asked about (`len` on a `Map` can lower to
            // `__map_col_len`). With no recorded type the query cannot decide,
            // which is `MonoTyReason::NoNode` -- exactly one entry, so the
            // denominator is 1.
            use bone_semantic::hir::{Callee, Expr as E};
            let mut n = 0;
            for (_, expr) in body.expr_arena.iter() {
                let E::Call {
                    callee: Callee::Unresolved { name, .. },
                    args,
                    ..
                } = expr
                else {
                    continue;
                };
                if name.as_str() != "len" {
                    continue;
                }
                let recv = args.clone().next().expect("a UFCS receiver");
                n += usize::from(inf.expr_types.remove(recv).is_some());
            }
            n
        },
    );
    let before = graph_of(&p.v1);
    assert!(
        before.storage_failures().next().is_some(),
        "the fixture recorded no storage failure"
    );

    let (g, _stats) = p.update();
    assert!(
        g.storage_failures().next().is_none(),
        "a storage failure survived the conversion resolving: {:?}",
        g.storage_failures().collect::<Vec<_>>()
    );
    p.assert_matches_fresh(&g, "storage failure cleared");
}

// ---------------------------------------------------------------------------
// 4. A too-deep instantiation, reduced
// ---------------------------------------------------------------------------

/// `depth_capped` WITHOUT polymorphic recursion, so the two producers are
/// separated: `too_deep_to_instantiate` reads the arguments' own depth, which a
/// deeply nested type reaches on its own.
const TAKE_LIB: &str = "fn take<T>(x: T) -> int { return 1 }\n";

#[test]
fn reducing_a_too_deep_instantiation_removes_the_depth_cap() {
    let deep_main = "fn main() {\n    let v: [[[[[[[[[[[[[[int]]]]]]]]]]]]]] = \
                     [[[[[[[[[[[[[[1]]]]]]]]]]]]]]\n    print(\"{take(v)}\")\n}\n";
    let shallow_main = "fn main() {\n    let v: [[int]] = [[1]]\n    print(\"{take(v)}\")\n}\n";
    let mut case = Case::new(deep_main, TAKE_LIB);
    assert_eq!(
        case.graph.depth_capped().count(),
        1,
        "the fixture capped no expansion; `MAX_INSTANTIATION_DEPTH` may have moved"
    );
    assert!(
        case.graph
            .rejections()
            .all(|r| r.kind != RejectionKind::ChangingInstanceRecursion),
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
// ---------------------------------------------------------------------------
// BodyOwner coverage: which owners a body-only edit can actually move
// ---------------------------------------------------------------------------

const PICK_LIB: &str = r#"
fn pick_a() -> int { return 1 }
fn pick_b() -> int { return 2 }
"#;

/// An ALWAYS-EMITTED owner replaced in place. A `const`'s initializer is a
/// body (`bone_incremental::item_prints`), its callees are ROOTS with no
/// incoming edge, and the reachability walk seeds from them -- so this is the
/// arm where dropping the old contribution and re-seeding both have to happen.
#[test]
fn editing_a_const_initializer_replaces_its_always_emitted_owner() {
    let mut case = Case::new(
        "const C: int = pick_a()\nfn main() { print(\"{C}\") }\n",
        PICK_LIB,
    );
    assert!(
        case.has_instance_named("pick_a"),
        "v1 never reached `pick_a`"
    );
    assert!(!case.has_instance_named("pick_b"));
    assert!(
        case.graph.always_emitted_targets().next().is_some(),
        "the const's callee must be recorded as a root, not as an edge target"
    );

    let stats = case.edit(
        MAIN,
        "const C: int = pick_b()\nfn main() { print(\"{C}\") }\n",
    );

    assert!(
        !case.has_instance_named("pick_a"),
        "the old always-emitted root survived"
    );
    assert!(
        case.has_instance_named("pick_b"),
        "the new always-emitted root was not reached"
    );
    assert_eq!(stats.pruned, 1);
    assert_eq!(stats.newly_reached, 1);
    case.assert_matches_fresh("const initializer");
    assert_scoped(&stats, "const initializer");
}

/// **The other four `BodyOwner` variants cannot appear in a body-only result**,
/// so `update_bodies` rejects them rather than pretending to maintain them.
///
/// This is the measurement behind that rejection, pinned so it goes red if the
/// classifier ever gains a body tier for one of these item kinds -- at which
/// point the arm needs an implementation, not a panic.
/// `bone_incremental::item_prints` hashes a struct, an enum, a newtype and a
/// decorator WHOLE-item into the interface print.
#[test]
fn a_struct_enum_newtype_or_decorator_edit_is_interface_tier() {
    let main = "fn main() { print(\"{use_all()}\") }\n";
    let base = concat!(
        "struct S { v: int = 1 }\n",
        "enum E { A, B }\n",
        "newtype N(int)\n",
        "deco init mark {\n    print(target.name)\n}\n",
        "fn use_all() -> int { let s = S {}\n return s.v }\n",
    );
    let edits = [
        (
            "struct defaults",
            base.replace("struct S { v: int = 1 }", "struct S { v: int = 2 }"),
        ),
        (
            "enum",
            base.replace("enum E { A, B }", "enum E { A, B, C }"),
        ),
        (
            "newtype",
            base.replace("newtype N(int)", "newtype N(float)"),
        ),
        (
            "decorator body",
            base.replace("print(target.name)", "print(\"x\")"),
        ),
    ];
    for (what, edited) in edits {
        let mut s = Session::new(
            STDLIB.clone(),
            vec![
                SessionFile::legacy(MAIN, main),
                SessionFile::legacy(LIB, base),
            ],
            &NsAliases::default(),
        )
        .expect("session");
        let r = s.update_file(LIB, &edited).expect("update");
        assert_ne!(
            r.tier,
            UpdateTier::BodyOnly,
            "a {what} edit classified BodyOnly; `update_bodies` rejects that owner and now \
             needs a real implementation for it"
        );
    }
}

/// A mutable `global`, for contrast with the `const` above: whole-item
/// interface, so its initializer never reaches `update_bodies` either.
#[test]
fn editing_a_mutable_global_initializer_is_interface_tier() {
    let main = "global g: int = pick_a()\nfn main() { print(\"{g}\") }\n";
    let mut s = Session::new(
        STDLIB.clone(),
        vec![
            SessionFile::legacy(MAIN, main),
            SessionFile::legacy(LIB, PICK_LIB),
        ],
        &NsAliases::default(),
    )
    .expect("session");
    let r = s
        .update_file(
            MAIN,
            "global g: int = pick_b()\nfn main() { print(\"{g}\") }\n",
        )
        .expect("update");
    assert_ne!(r.tier, UpdateTier::BodyOnly);
}

/// A failed call in an ALWAYS-EMITTED body is stale state of its own: it has no
/// `from` instance to be filed against, so it lands on the root hole rather
/// than in `unresolved`, and dropping an instance's records cannot clear it.
///
/// The producer here is a generic function taken as a VALUE, which the
/// concrete-program model refuses -- no call site decides its instantiation.
const FNVALUE_LIB: &str = r#"
fn gen_id<T>(x: T) -> T { return x }
fn mono_id(x: int) -> int { return x }
fn takes(f: fn(int) -> int) -> int { return f(1) }
"#;

#[test]
fn fixing_an_always_emitted_call_clears_its_root_hole() {
    let mut case = Case::new(
        "const C: int = takes(gen_id)\nfn main() { print(\"{C}\") }\n",
        FNVALUE_LIB,
    );
    let holes = |c: &Case| -> usize {
        c.graph
            .uninstantiated_roots
            .values()
            .map(|h| h.always_emitted_sites.len())
            .sum()
    };
    assert_eq!(
        holes(&case),
        1,
        "the fixture recorded no always-emitted failure"
    );

    let stats = case.edit(
        MAIN,
        "const C: int = takes(mono_id)\nfn main() { print(\"{C}\") }\n",
    );

    assert_eq!(
        holes(&case),
        0,
        "an always-emitted failure survived the call that produced it: {:?}",
        case.graph
            .uninstantiated_roots
            .iter()
            .map(|(n, h)| (n.clone(), h.always_emitted_sites.len()))
            .collect::<Vec<_>>()
    );
    assert!(
        stats.census.root_hole_names_examined > 0,
        "root-hole refresh had no name denominator: {stats:?}"
    );
    assert!(
        stats.census.root_hole_rows_examined > 0,
        "root-hole refresh had no failure-row denominator: {stats:?}"
    );
    assert!(
        stats.newly_reached > 0,
        "`mono_id` was not reached as a new root"
    );
    case.assert_matches_fresh("always-emitted hole cleared");
}

/// A body with NO recorded facts that gains one.
///
/// The two invalidation causes come apart here: an empty contribution quotes no
/// file, so the "this owner cites the edited file" rule cannot see it, and only
/// "this owner IS the edited body" reaches it. Without that arm the added call
/// is silently absent.
#[test]
fn a_body_with_no_facts_that_gains_a_call_is_still_re_walked() {
    let mut case = Case::new(
        "fn main() { print(\"{leaf()}\") }\n",
        "fn leaf() -> int { return 1 }\nfn added() -> int { return 2 }\n",
    );
    let leaf_owner = case
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
        leaf_owner, 0,
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
        "fn main() { print(other()) }\n",
        "fn leaf() -> int { return 1 }\nfn other() -> int { return leaf() }\n",
    );
    let span_before = case
        .graph
        .contributions
        .iter()
        .find(|(owner, _)| {
            owner
                .instance()
                .is_some_and(|i| case.identity(i).starts_with("other"))
        })
        .and_then(|(_, contribution)| contribution.call_sites.values().next())
        .map(|site| site.span.offset_range)
        .expect("the unedited owner must have a call span");

    let stats = case.edit(
        LIB,
        "fn leaf() -> int {\n  return 2\n}\nfn other() -> int { return leaf() }\n",
    );
    assert_eq!(
        stats.affected, 1,
        "the edit should rewalk only `leaf`: {stats:?}"
    );
    let span_after = case
        .graph
        .contributions
        .iter()
        .find(|(owner, _)| {
            owner
                .instance()
                .is_some_and(|i| case.identity(i).starts_with("other"))
        })
        .and_then(|(_, contribution)| contribution.call_sites.values().next())
        .map(|site| site.span.offset_range)
        .expect("the unedited owner must retain its call span");
    assert!(
        span_after.0 > span_before.0,
        "the unedited call span did not relocate"
    );
    assert_eq!(
        stats.revisited_owners.len(),
        1,
        "span relocation widened the semantic delta"
    );
    case.assert_matches_fresh("semantic edit with span relocation");
}

/// The same, for an ALWAYS-EMITTED body: a `const` whose initializer had no
/// callee at all and gains one.
#[test]
fn a_const_initializer_with_no_facts_that_gains_a_call_is_still_re_walked() {
    let mut case = Case::new("const C: int = 1\nfn main() { print(\"{C}\") }\n", PICK_LIB);
    assert!(!case.has_instance_named("pick_a"));

    let stats = case.edit(
        MAIN,
        "const C: int = pick_a()\nfn main() { print(\"{C}\") }\n",
    );

    assert!(
        case.has_instance_named("pick_a"),
        "the call added to a const initializer was lost"
    );
    assert_eq!(stats.newly_reached, 1);
    case.assert_matches_fresh("const initializer gains a call");
}

/// The root set is a function of the DECLARATIONS, so a body-only edit cannot
/// move it -- and a caller that hands `update_bodies` an interface change is
/// refused rather than absorbed. The guard has no population on any fixture
/// above, so this is what gives it one.
#[test]
#[should_panic(expected = "the root set moved during a body-only update")]
fn a_moved_root_set_is_refused_rather_than_absorbed() {
    let case = Case::new("fn main() { print(\"{other()}\") }\n", CHAIN_LIB);
    let ws = case.ws();
    let mut roots = skull::mir_emit::emission_roots(ws, Some(PROJ));
    roots.pop().expect("the fixture has roots");
    let mut graph = graph_of(ws);
    graph.update_bodies_with_edits(ws, roots, &[], &[]);
}

// ---------------------------------------------------------------------------
// The two prune arms that had no population until here
// ---------------------------------------------------------------------------

/// A library holding an always-emitted `const` whose callee is ALSO called from
/// the target. The const lives in the library on purpose: an initializer in the
/// edited file is re-walked itself, and a re-installed owner takes its own
/// targets back out of `lost`, which would keep `shared` out of the suspect
/// region entirely and leave the arm below unexercised.
const ANCHOR_LIB: &str = r#"
const C: int = shared()
fn shared() -> int { return 1 }
fn other() -> int { return 7 }
"#;

/// **An always-emitted target is an ANCHOR, so what only it reaches survives.**
///
/// `shared` loses its edge from `main` and enters the suspect region, and the
/// const's call is NOT an edge -- an always-emitted body's callees are roots
/// with no incoming edge -- so `root_instances` and `preds` both answer no. The
/// always-emitted arm is the only thing keeping it alive.
///
/// Written because mutation found the arm uncovered: deleting it left every
/// compact fixture AND all three real projects green, so nothing measured
/// reached this shape.
#[test]
fn an_always_emitted_target_anchors_what_the_edit_stranded() {
    let mut case = Case::new(
        "fn main() { print(\"{shared()} {other()} {C}\") }\n",
        ANCHOR_LIB,
    );
    assert!(
        case.has_instance_named("shared"),
        "v1 never reached `shared`"
    );
    let anchored: Vec<String> = case
        .graph
        .always_emitted_targets()
        .map(|i| case.identity(i))
        .collect();
    assert!(
        anchored.iter().any(|k| k.contains("shared")),
        "the fixture's anchor is not an always-emitted target: {anchored:?}"
    );

    let stats = case.edit(MAIN, "fn main() { print(\"{other()} {C}\") }\n");

    assert!(
        case.has_instance_named("shared"),
        "`shared` was pruned although the const initializer still calls it"
    );
    assert_eq!(stats.pruned, 0, "nothing should have been stranded");
    assert!(
        stats.suspect_region > 0,
        "the arm is vacuous unless `shared` actually entered the suspect region"
    );
    case.assert_matches_fresh("always-emitted anchor");
    assert_scoped(&stats, "always-emitted anchor");
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

#[test]
fn an_external_anchor_stops_a_branched_strand_search() {
    let mut case = Case::new(
        "fn main() { print(\"{entry()} {anchor()}\") }\n",
        BRANCHED_ANCHOR_LIB,
    );
    for name in [
        "shared_root",
        "shared_left",
        "shared_right",
        "dead_root",
        "dead_leaf",
    ] {
        assert!(case.has_instance_named(name), "v1 never reached `{name}`");
    }
    let shared = case
        .graph
        .instances
        .iter()
        .find(|instance| case.identity(instance).starts_with("shared_root"))
        .cloned()
        .expect("the shared branch instance");
    let incoming = case
        .graph
        .index
        .preds
        .get(&shared)
        .expect("shared predecessor index row");
    assert!(
        incoming.len() >= 2,
        "the fixture has no external anchor: {incoming:?}"
    );

    let stats = case.edit(MAIN, "fn main() { print(\"{anchor()}\") }\n");

    assert!(
        case.has_instance_named("shared_root"),
        "the external anchor was pruned"
    );
    assert!(
        !case.has_instance_named("dead_root"),
        "the stranded branch survived"
    );
    assert!(
        !case.has_instance_named("dead_leaf"),
        "the stranded leaf survived"
    );
    assert_eq!(
        stats.pruned, 3,
        "the fixture must remove the caller, root, and leaf: {stats:?}"
    );
    assert_eq!(
        stats.affected, 1,
        "one caller body should be affected: {stats:?}"
    );
    assert!(
        stats.expanded > 0,
        "the update expanded no owner: {stats:?}"
    );
    assert!(
        stats.suspect_region > 0,
        "the prune search had no population: {stats:?}"
    );
    assert!(
        stats.census.index_preds_updates > 0,
        "the predecessor index had no population"
    );
    assert!(
        stats.suspect_region <= stats.expanded * 4,
        "the forward anchor boundary was not local: {stats:?}"
    );
    case.assert_matches_fresh("external anchor with stranded branch");
    assert_scoped(&stats, "external anchor with stranded branch");
}

const LONG_ANCHOR_LIB: &str = r#"
fn entry() -> int { return shared_root() + dead_root() }
fn anchor() -> int { return anchor_0() }
fn anchor_0() -> int { return anchor_1() }
fn anchor_1() -> int { return anchor_2() }
fn anchor_2() -> int { return anchor_3() }
fn anchor_3() -> int { return anchor_4() }
fn anchor_4() -> int { return anchor_5() }
fn anchor_5() -> int { return anchor_6() }
fn anchor_6() -> int { return anchor_7() }
fn anchor_7() -> int { return anchor_8() }
fn anchor_8() -> int { return anchor_9() }
fn anchor_9() -> int { return anchor_10() }
fn anchor_10() -> int { return anchor_11() }
fn anchor_11() -> int { return anchor_12() }
fn anchor_12() -> int { return anchor_13() }
fn anchor_13() -> int { return anchor_14() }
fn anchor_14() -> int { return anchor_15() }
fn anchor_15() -> int { return anchor_16() }
fn anchor_16() -> int { return anchor_17() }
fn anchor_17() -> int { return anchor_18() }
fn anchor_18() -> int { return anchor_19() }
fn anchor_19() -> int { return anchor_20() }
fn anchor_20() -> int { return anchor_21() }
fn anchor_21() -> int { return anchor_22() }
fn anchor_22() -> int { return anchor_23() }
fn anchor_23() -> int { return anchor_24() }
fn anchor_24() -> int { return anchor_25() }
fn anchor_25() -> int { return anchor_26() }
fn anchor_26() -> int { return anchor_27() }
fn anchor_27() -> int { return anchor_28() }
fn anchor_28() -> int { return anchor_29() }
fn anchor_29() -> int { return anchor_30() }
fn anchor_30() -> int { return anchor_31() }
fn anchor_31() -> int { return shared_root() }
fn shared_root() -> int { return shared_left() + shared_right() }
fn shared_left() -> int { return 1 }
fn shared_right() -> int { return 2 }
fn dead_root() -> int { return dead_leaf() }
fn dead_leaf() -> int { return 3 }
"#;

#[test]
fn a_long_anchor_path_is_included_in_prune_locality() {
    let mut case = Case::new(
        "fn main() { print(\"{entry()} {anchor()}\") }\n",
        LONG_ANCHOR_LIB,
    );
    for name in ["shared_root", "dead_root", "dead_leaf", "anchor_31"] {
        assert!(case.has_instance_named(name), "v1 never reached `{name}`");
    }
    let shared = case
        .graph
        .instances
        .iter()
        .find(|instance| case.identity(instance).starts_with("shared_root"))
        .cloned()
        .expect("the shared branch instance");
    let incoming = case
        .graph
        .index
        .preds
        .get(&shared)
        .expect("shared predecessor index row");
    assert!(
        incoming.len() >= 2,
        "the fixture has no shared external anchor: {incoming:?}"
    );

    let stats = case.edit(MAIN, "fn main() { print(\"{anchor()}\") }\n");

    assert!(
        case.has_instance_named("shared_root"),
        "the external anchor was pruned"
    );
    assert!(
        !case.has_instance_named("dead_root"),
        "the stranded branch survived"
    );
    assert!(
        !case.has_instance_named("dead_leaf"),
        "the stranded leaf survived"
    );
    assert_eq!(
        stats.pruned, 3,
        "only entry and the dead branch should be pruned: {stats:?}"
    );
    assert_eq!(
        stats.affected, 1,
        "one caller body should be affected: {stats:?}"
    );
    assert!(
        stats.expanded > 0,
        "the update expanded no owner: {stats:?}"
    );
    assert!(
        stats.suspect_region > 0,
        "the forward prune population is empty: {stats:?}"
    );
    assert_eq!(
        stats.reverse_instances_examined, 0,
        "the affected-region search still performed a reverse reachability walk: {stats:?}"
    );
    assert!(
        stats.reverse_predecessor_edges_examined == 0,
        "the affected-region search still examined rootward predecessor edges: {stats:?}"
    );
    assert!(
        stats.boundary_predecessor_edges_examined > 0,
        "the local boundary classification examined no predecessor edges: {stats:?}"
    );
    assert!(
        stats.prune_search_population >= stats.suspect_region,
        "the prune denominator dropped forward work: {stats:?}"
    );
    assert_eq!(
        stats.prune_search_population,
        stats.suspect_region
            + stats.boundary_predecessor_edges_examined
            + stats.support_component_nodes_examined
            + stats.support_component_edges_examined
            + stats.reverse_instances_examined
            + stats.reverse_predecessor_edges_examined,
        "the prune denominator does not include every reported pruning population: {stats:?}"
    );
    assert!(
        stats.suspect_region <= stats.expanded * 4,
        "the forward prune region escaped the locality bound: {stats:?}"
    );
    assert!(
        stats.prune_search_population <= stats.suspect_region * 4,
        "forward plus boundary prune work escaped the locality bound: {stats:?}"
    );
    case.assert_matches_fresh("long external anchor path");
    assert_scoped(&stats, "long external anchor path");
}

const DIRECT_SEED_MAIN_V1: &str = "fn main() { print(\"{shared_root()}\") }\nfn independent_root() -> int { return shared_root() }\n";
const DIRECT_SEED_MAIN_V2: &str =
    "fn main() { print(\"0\") }\nfn independent_root() -> int { return shared_root() }\n";
const DIRECT_SEED_LIB: &str = r#"
fn shared_root() -> int { return shared_left() + shared_right() }
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
const DIRECT_SEED_SUBTREE: &[&str] = &[
    "shared_root",
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

#[test]
fn an_externally_supported_lost_seed_stops_before_its_subtree() {
    let mut case = Case::new(DIRECT_SEED_MAIN_V1, DIRECT_SEED_LIB);
    for name in DIRECT_SEED_SUBTREE {
        assert!(case.has_instance_named(name), "v1 never reached `{name}`");
    }
    assert!(
        case.graph
            .root_instances
            .iter()
            .any(|instance| case.identity(instance).starts_with("independent_root")),
        "the independent external root is not rooted"
    );
    let shared = case
        .graph
        .instances
        .iter()
        .find(|instance| case.identity(instance).starts_with("shared_root"))
        .cloned()
        .expect("the direct shared seed");
    let incoming = case
        .graph
        .index
        .preds
        .get(&shared)
        .expect("shared predecessor row");
    assert!(
        incoming
            .iter()
            .any(|pred| case.identity(pred).starts_with("independent_root")),
        "the direct seed has no independent external predecessor: {incoming:?}"
    );

    let stats = case.edit(MAIN, DIRECT_SEED_MAIN_V2);

    assert!(
        stats
            .prune_seeds
            .iter()
            .any(|seed| case.identity(seed).starts_with("shared_root")),
        "shared_root was not an initial lost seed: {stats:?}"
    );
    for name in DIRECT_SEED_SUBTREE {
        assert!(
            case.has_instance_named(name),
            "shared subtree `{name}` was pruned"
        );
    }
    assert_eq!(
        stats.pruned, 0,
        "the externally supported subtree was pruned: {stats:?}"
    );
    case.assert_matches_fresh("direct externally supported seed");
    assert!(
        stats.suspect_region > 0,
        "the direct seed had no forward population: {stats:?}"
    );
    assert!(
        stats.boundary_predecessor_edges_examined > 0,
        "the direct seed classified no external predecessor edge: {stats:?}"
    );
    assert_eq!(
        stats.reverse_instances_examined, 0,
        "the direct seed used a rootward reverse query: {stats:?}"
    );
    assert_eq!(
        stats.reverse_predecessor_edges_examined, 0,
        "the direct seed examined rootward predecessor edges: {stats:?}"
    );
    assert_eq!(
        stats.prune_search_population,
        stats.suspect_region
            + stats.boundary_predecessor_edges_examined
            + stats.support_component_nodes_examined
            + stats.support_component_edges_examined
            + stats.reverse_instances_examined
            + stats.reverse_predecessor_edges_examined,
        "the direct-seed denominator omits pruning work: {stats:?}"
    );
    assert!(
        stats.suspect_region < DIRECT_SEED_SUBTREE.len(),
        "the prune search expanded the shared subtree: {stats:?}"
    );
    assert!(
        stats.suspect_region <= stats.expanded * 4,
        "the direct external seed's forward region escaped the locality bound: {stats:?}"
    );
    assert!(
        stats.prune_search_population <= stats.suspect_region * 4,
        "the direct external seed's boundary work escaped the locality bound: {stats:?}"
    );
    assert_scoped(&stats, "direct externally supported seed");
}

const NESTED_SEED_MAIN_V1: &str =
    "fn main() { print(\"{shared(1)}\") }\nfn independent_root() { print(\"{outer(1)}\") }\n";
const NESTED_SEED_MAIN_V2: &str =
    "fn main() { print(\"0\") }\nfn independent_root() { print(\"{outer(1)}\") }\n";
const NESTED_SEED_LIB: &str = r#"
fn shared<T>(x: T) -> int { return shared_left() + shared_right() }
fn outer<T>(x: T) -> int { return shared(x) }
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
const NESTED_SEED_SUBTREE: &[&str] = &[
    "shared",
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

#[test]
fn a_root_reachable_non_root_seed_predecessor_stops_before_its_subtree() {
    let mut case = Case::new(NESTED_SEED_MAIN_V1, NESTED_SEED_LIB);
    for name in NESTED_SEED_SUBTREE {
        assert!(case.has_instance_named(name), "v1 never reached `{name}`");
    }
    let outer = case
        .graph
        .instances
        .iter()
        .find(|instance| case.identity(instance).starts_with("outer"))
        .cloned()
        .expect("the nested outer instance");
    assert!(
        !case.graph.root_instances.contains(&outer),
        "the nested predecessor unexpectedly became a root: {}",
        case.identity(&outer)
    );
    assert!(
        !case.graph.index.always_emitted.contains_key(&outer),
        "the nested predecessor unexpectedly became always-emitted: {}",
        case.identity(&outer)
    );
    assert!(
        case.graph.reachable_instances().contains(&outer),
        "the nested predecessor is not independently root-reachable: {}",
        case.identity(&outer)
    );

    let stats = case.edit(MAIN, NESTED_SEED_MAIN_V2);

    assert!(
        stats
            .prune_seeds
            .iter()
            .any(|seed| case.identity(seed).starts_with("shared")),
        "shared<T> was not an initial lost seed: {stats:?}"
    );
    let shared = case
        .graph
        .instances
        .iter()
        .find(|instance| case.identity(instance).starts_with("shared"))
        .cloned()
        .expect("the shared generic seed after the edit");
    let incoming = case
        .graph
        .index
        .preds
        .get(&shared)
        .expect("shared predecessor row");
    assert!(
        incoming
            .iter()
            .any(|pred| case.identity(pred).starts_with("outer")),
        "the shared seed lost its nested predecessor: {incoming:?}"
    );
    assert!(
        incoming
            .iter()
            .any(|pred| !case.graph.root_instances.contains(pred)),
        "the shared seed's predecessor row has no non-root instance: {incoming:?}"
    );
    for name in NESTED_SEED_SUBTREE {
        assert!(
            case.has_instance_named(name),
            "shared subtree `{name}` was pruned"
        );
    }
    assert_eq!(
        stats.pruned, 0,
        "the rooted nested subtree was pruned: {stats:?}"
    );
    case.assert_matches_fresh("root-reachable nested seed");
    assert!(
        stats.suspect_region > 0,
        "the nested seed had no forward population: {stats:?}"
    );
    assert!(
        stats.boundary_predecessor_edges_examined > 0,
        "the nested seed classified no boundary predecessor edge: {stats:?}"
    );
    assert_eq!(
        stats.reverse_instances_examined, 0,
        "the nested seed used a rootward reverse query: {stats:?}"
    );
    assert_eq!(
        stats.reverse_predecessor_edges_examined, 0,
        "the nested seed examined rootward predecessor edges: {stats:?}"
    );
    assert_eq!(
        stats.prune_search_population,
        stats.suspect_region
            + stats.boundary_predecessor_edges_examined
            + stats.support_component_nodes_examined
            + stats.support_component_edges_examined
            + stats.reverse_instances_examined
            + stats.reverse_predecessor_edges_examined,
        "the nested-seed denominator omits pruning work: {stats:?}"
    );
    assert!(
        stats.suspect_region < NESTED_SEED_SUBTREE.len(),
        "the prune search expanded the nested shared subtree: {stats:?}"
    );
    assert!(
        stats.prune_search_population <= stats.suspect_region * 4,
        "the nested-seed boundary work escaped the locality bound: {stats:?}"
    );
    assert_scoped(&stats, "root-reachable nested seed");
}

const DEFERRED_BOUNDARY_LIB: &str = r#"
fn entry() -> int { return branch() }
fn branch() -> int { return shared() + apparent() }
fn apparent() -> int { return shared() }
fn shared() -> int { return dead() }
fn dead() -> int { return 1 }
fn other() -> int { return 7 }
"#;

#[test]
fn a_later_lost_predecessor_releases_a_tentative_boundary() {
    let mut case = Case::new(
        "fn main() { print(\"{entry()}\") }\n",
        DEFERRED_BOUNDARY_LIB,
    );
    for name in ["entry", "branch", "apparent", "shared", "dead"] {
        assert!(case.has_instance_named(name), "v1 never reached `{name}`");
    }

    let stats = case.edit(MAIN, "fn main() { print(\"{other()}\") }\n");

    for name in ["entry", "branch", "apparent", "shared", "dead"] {
        assert!(
            !case.has_instance_named(name),
            "`{name}` survived the lost-seed closure"
        );
    }
    assert!(
        stats.pruned >= 5,
        "the deferred boundary had no population: {stats:?}"
    );
    assert!(stats.suspect_region >= stats.pruned);
    assert!(stats.prune_search_population > 0);
    case.assert_matches_fresh("deferred boundary release");
    assert_scoped(&stats, "deferred boundary release");
}

const ROOTED_CYCLE_LIB: &str = r#"
fn entry() -> int { return rooted_a() + dead() }
fn rooted_a() -> int { return rooted_b() }
fn rooted_b() -> int { return rooted_a() }
fn dead() -> int { return dead_leaf() }
fn dead_leaf() -> int { return 1 }
"#;

#[test]
fn a_rooted_cycle_survives_while_the_lost_branch_is_pruned() {
    let mut case = Case::new(
        "fn main() { print(\"{entry()} {rooted_a()}\") }\nfn root_anchor() -> int { return rooted_a() }\n",
        ROOTED_CYCLE_LIB,
    );
    for name in ["entry", "rooted_a", "rooted_b", "dead", "dead_leaf"] {
        assert!(case.has_instance_named(name), "v1 never reached `{name}`");
    }

    let stats = case.edit(
        MAIN,
        "fn main() { print(\"0\") }\nfn root_anchor() -> int { return rooted_a() }\n",
    );

    assert!(
        stats
            .prune_seeds
            .iter()
            .any(|seed| case.identity(seed).starts_with("rooted_a")),
        "the rooted cycle member was not an initial lost seed: {stats:?}"
    );
    for name in ["rooted_a", "rooted_b"] {
        assert!(
            case.has_instance_named(name),
            "rooted cycle member `{name}` was pruned"
        );
    }
    for name in ["entry", "dead", "dead_leaf"] {
        assert!(
            !case.has_instance_named(name),
            "lost branch `{name}` survived"
        );
    }
    assert!(
        stats.pruned >= 3,
        "the rooted-cycle fixture had no stranded branch: {stats:?}"
    );
    case.assert_matches_fresh("rooted cycle");
    assert_scoped(&stats, "rooted cycle");
}

const BOUNDED_LIB: &str = r#"
fn total(v: Vec<int>) -> int {
    let t = 0
    for x in v.iter().filter(_ > 0) { t = t + x }
    return t
}
fn other() -> int { return 7 }
"#;

/// **A pruned instance takes its BOUND TREE out of the index with it.**
///
/// `bound_trees` is a maintained cross-owner index and a semantic field of the
/// canonical form, so a tree that outlives its last owner is a stale fact a
/// fresh build does not have.
///
/// Written because mutation found `uninstall`'s tree retraction uncovered:
/// deleting it left every compact fixture AND all three real projects green.
/// The compact fixtures had no bounded call at all, and the projects' chosen
/// edits prune no bounded instance.
#[test]
fn pruning_an_instance_retracts_its_bound_tree() {
    let mut case = Case::new(
        "fn main() { print(\"{total([1, 2])} {other()}\") }\n",
        BOUNDED_LIB,
    );
    let before = case.graph.bound_trees().count();
    assert!(
        before > 0,
        "the fixture recorded no bound tree, so the arm has no population"
    );

    let stats = case.edit(MAIN, "fn main() { print(\"{other()}\") }\n");

    assert!(
        stats.pruned > 0,
        "the deletion stranded nothing, so nothing was retracted"
    );
    assert!(
        case.graph.bound_trees().count() < before,
        "every one of the {before} bound trees outlived the instances that owned them"
    );
    case.assert_matches_fresh("bound tree retracted");
    assert_scoped(&stats, "bound tree retracted");
}

// ---------------------------------------------------------------------------
// A struct default reached ONLY from an always-emitted owner (a global
// initializer's struct literal, never a function body)
// ---------------------------------------------------------------------------

const STRUCT_DEFAULT_LIB: &str = r#"
fn make<T>(x: T) -> T { return x }
struct Holder { v: int = make(5) }
"#;

/// **`walk_struct_default_edges` covered instance owners only; an
/// always-emitted owner's own struct literal was invisible.**
///
/// `global G: Holder = Holder {}` never calls `make` explicitly -- the
/// omitted field's default does, and the default lives in `Holder`'s
/// declaring struct, entered only when something constructs `Holder {}`. No
/// function body in this fixture ever does; only the global initializer does,
/// which is `Owner::AlwaysEmitted`, not `Owner::Instance`. Found by the E1
/// Phase-2 population audit (`skull::mir_emit`), identically across every
/// real corpus project's `Poller`/`CoState` global (`arena_new<CoState>` and
/// its own `__arena_new_owned<int>` call) before this fix.
#[test]
fn a_global_initializers_struct_default_reaches_its_omitted_fields_callee() {
    let case = Case::new(
        "global G: Holder = Holder {}\nfn main() { print(\"{1}\") }\n",
        STRUCT_DEFAULT_LIB,
    );
    let has_make_int = case.graph.instances.iter().any(|i| {
        let k = case.identity(i);
        k.starts_with("make") && k.contains("int")
    });
    assert!(
        has_make_int,
        "`make<int>` is reached only through Holder's field default at the global \
         initializer `G`, and the graph does not have it: {:?}",
        case.instance_names()
    );
}

/// The control for the arm above: with no `Holder {}` construction anywhere
/// (global deleted), `make<int>` must NOT be reached -- proving the assertion
/// is about the struct-default EDGE, not about `make` being unconditionally
/// present.
#[test]
fn with_no_holder_construction_the_defaults_callee_is_absent() {
    let case = Case::new("fn main() { print(\"{1}\") }\n", STRUCT_DEFAULT_LIB);
    let has_make_int = case
        .graph
        .instances
        .iter()
        .any(|i| case.identity(i).starts_with("make") && case.identity(i).contains("int"));
    assert!(
        !has_make_int,
        "`make<int>` is reached with no `Holder {{}}` construction anywhere in the fixture"
    );
}

#[test]
fn intermediate_body_edits_add_and_remove_recursion_findings() {
    let flat = r#"
fn descend<T>(x: T) -> int { return relay([x]) }
fn relay<T>(x: [T]) -> int { return 0 }
"#;
    let recursive = flat.replace("return 0", "return descend(x)");
    let mut case = Case::new("fn main() { print(\"{descend(1)}\") }\n", flat);
    let count = |case: &Case| {
        case.graph
            .rejections()
            .filter(|r| r.kind == RejectionKind::ChangingInstanceRecursion)
            .count()
    };
    assert_eq!(count(&case), 0);
    case.edit(LIB, &recursive);
    assert!(
        count(&case) > 0,
        "helper edit produced no changing recursion"
    );
    case.assert_matches_fresh("recursion added through a helper");

    let stats = case.edit(LIB, flat);
    assert!(stats.census.recursion_findings_retracted > 0);
    assert_eq!(count(&case), 0);
    case.assert_matches_fresh("recursion removed through a helper");
}
