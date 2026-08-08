//! **`InstanceGraph::update_bodies` against a fresh build**, one causal
//! producer per test (`docs/parked/mono.md`, "The persistent graph updater").
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

use bone_incremental::session::{Session, UpdateTier};
use bone_semantic::inference::Workspace;
use skull::graph_snapshot::{canonical, instance_key};
use skull::instance_graph::{self, Instance, InstanceGraph, RejectionKind, UpdateStats};
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
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
    instance_graph::build(ws, instance_graph::emission_roots(ws, Some(PROJ)), false)
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
            vec![(MAIN.to_string(), main.to_string()), (LIB.to_string(), lib.to_string())],
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
    fn edit(&mut self, path: &str, text: &str) -> UpdateStats {
        let res = self.session.update_file(path, text).expect("incremental update");
        assert_eq!(
            res.tier,
            UpdateTier::BodyOnly,
            "the fixture's edit is not body-only ({:?}); an interface change moves the root \
             set and the manglings, which `update_bodies` does not claim to handle",
            res.tier
        );
        assert!(!res.reinferred.is_empty(), "a body-only edit reinferred nothing");
        let ws = self.session.workspace();
        let roots = instance_graph::emission_roots(ws, Some(PROJ));
        self.graph.update_bodies(ws, roots, &res.reinferred)
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
        self.graph.instances.iter().any(|i| self.identity(i).contains(name))
    }

    fn identity(&self, i: &Instance) -> String {
        instance_key(self.ws(), &self.graph, i)
    }

    fn instance_names(&self) -> BTreeSet<String> {
        self.graph.instances.iter().map(|i| self.identity(i)).collect()
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
        assert!(case.has_instance_named(name).eq(&false), "`{name}` survived the deletion");
    }
    assert!(case.has_instance_named("other"), "an untouched sibling was pruned too");
    let removed: Vec<String> =
        before.difference(&case.instance_names()).map(short).collect();
    assert_eq!(
        removed,
        vec!["chain_a_ret_int", "chain_b_ret_int", "chain_c_ret_int"],
        "expected exactly the three stranded instances, out of {} before",
        before.len()
    );
    assert_eq!(stats.pruned, removed.len());
    case.assert_matches_fresh("subtree deletion");
    assert_scoped(&stats, "subtree deletion");
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
    let mut case = Case::new(
        "fn main() { print(\"{cyc_a(4)} {other()}\") }\n",
        CYCLE_LIB,
    );
    assert!(case.has_instance_named("cyc_a") && case.has_instance_named("cyc_b"));
    // The cycle is real: each member has an incoming edge from the other, so a
    // "nothing points at it" rule would keep both alive forever.
    let incoming = |name: &str| {
        case.graph
            .edges
            .iter()
            .filter(|e| case.identity(&e.to).contains(name))
            .count()
    };
    assert!(incoming("cyc_a") > 0 && incoming("cyc_b") > 0, "the fixture is not a cycle");
    let before = case.instance_names();

    let stats = case.edit(MAIN, "fn main() { print(\"{other()}\") }\n");

    assert!(!case.has_instance_named("cyc_a"), "`cyc_a` survived: the cycle kept itself alive");
    assert!(!case.has_instance_named("cyc_b"), "`cyc_b` survived: the cycle kept itself alive");
    // BY NAME, not by count: the third is `__op_sub_int_int_ret_int`, whose
    // only caller was the cycle, and a count would hide a swap.
    let removed: Vec<String> =
        before.difference(&case.instance_names()).map(short).collect();
    assert_eq!(removed, vec!["__op_sub_int_int_ret_int", "cyc_a_int_ret_int", "cyc_b_int_ret_int"]);
    assert_eq!(stats.pruned, removed.len());
    case.assert_matches_fresh("cycle pruning");
    assert_scoped(&stats, "cycle pruning");
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

    let stats =
        case.edit(MAIN, "fn main() { print(\"{ident(1)} {ident(\"s\")}\") }\n");

    assert!(has_string_ident(&case), "the added call reached no new specialization");
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
            .rejections
            .iter()
            .filter(|r| r.kind == RejectionKind::ChangingInstanceRecursion)
            .count()
    };
    assert_eq!(recursion_rejections(&case), 1, "the fixture produced no recursion rejection");
    assert!(!case.graph.depth_capped.is_empty(), "the fixture capped no expansion");

    let stats = case.edit(LIB, FLAT_LIB);

    assert_eq!(
        recursion_rejections(&case),
        0,
        "the recursion rejection survived a program that no longer has the recursion"
    );
    assert!(
        case.graph.depth_capped.is_empty(),
        "capped instances survived: {:?}",
        case.graph.depth_capped.len()
    );
    assert!(stats.pruned > 0, "the deep instantiation chain was not pruned");
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
    assert!(s1.pruned == 3 && s1.newly_reached == 0, "step 1 is a deletion: {s1:?}");
    case.assert_matches_fresh("after delete");

    let s2 = case.edit(MAIN, "fn main() { print(\"{chain_a()} {other()}\") }\n");
    assert!(s2.newly_reached == 3 && s2.pruned == 0, "step 2 is an addition: {s2:?}");
    case.assert_matches_fresh("after re-add");

    let s3 = case.edit(MAIN, "fn main() { print(\"{chain_b()}\") }\n");
    assert!(s3.pruned > 0, "step 3 strands `chain_a`: {s3:?}");
    assert!(!case.has_instance_named("chain_a"), "`chain_a` survived the third edit");
    assert!(case.has_instance_named("chain_c"), "`chain_c` is still reached through `chain_b`");
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
    changed: Vec<(bone_semantic::prelude::ModuleId, bone_semantic::hir::BodyOwner)>,
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
) -> (bone_semantic::prelude::ModuleId, bone_semantic::hir::BodyOwner) {
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
        let body = v1.module(mid).bodies.get(&owner).expect("the fn has a body").clone();
        let inf = v1.inferences[mid.raw() as usize]
            .as_mut()
            .expect("the module has an inference result")
            .per_fn
            .get_mut(&owner)
            .expect("the body has an inference record");
        let removed = perturb(&body, inf);
        assert!(removed > 0, "the perturbation removed nothing from `{fn_name}`");
        Perturbed { v1, v2, changed: vec![(mid, owner)] }
    }

    /// Build against v1, update to v2, and compare with a fresh v2 build.
    fn update(&self) -> (InstanceGraph, UpdateStats) {
        let mut g = graph_of(&self.v1);
        let roots = instance_graph::emission_roots(&self.v2, Some(PROJ));
        let stats = g.update_bodies(&self.v2, roots, &self.changed);
        (g, stats)
    }

    fn assert_matches_fresh(&self, g: &InstanceGraph, label: &str) {
        let fresh = graph_of(&self.v2);
        let d = canonical(&self.v2, g).diff(&canonical(&self.v2, &fresh));
        assert!(d.is_empty(), "{label}: updated and fresh disagree:\n{}", d.join("\n"));
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
    let stale: Vec<&str> =
        before.unresolved.iter().filter(|u| u.reason == "no-bindings").map(|u| u.reason).collect();
    assert_eq!(stale.len(), 1, "the fixture recorded no unresolved call: {:?}", before.unresolved);

    let (g, stats) = p.update();
    assert!(
        g.unresolved.is_empty(),
        "an unresolved record survived the call resolving: {:?}",
        g.unresolved.iter().map(|u| u.reason).collect::<Vec<_>>()
    );
    assert!(stats.newly_reached > 0, "the resolved call reached no instance");
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
                let E::Call { callee: Callee::Unresolved { name, .. }, args } = expr else {
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
        !before.storage_failures.is_empty(),
        "the fixture recorded no storage failure"
    );

    let (g, _stats) = p.update();
    assert!(
        g.storage_failures.is_empty(),
        "a storage failure survived the conversion resolving: {:?}",
        g.storage_failures
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
        case.graph.depth_capped.len(),
        1,
        "the fixture capped no expansion; `MAX_INSTANTIATION_DEPTH` may have moved"
    );
    assert!(
        case.graph
            .rejections
            .iter()
            .all(|r| r.kind != RejectionKind::ChangingInstanceRecursion),
        "this fixture must cap WITHOUT polymorphic recursion, or it tests two producers"
    );

    let stats = case.edit(MAIN, shallow_main);

    assert!(
        case.graph.depth_capped.is_empty(),
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
    assert!(case.has_instance_named("pick_a"), "v1 never reached `pick_a`");
    assert!(!case.has_instance_named("pick_b"));
    assert!(
        !case.graph.always_emitted_targets.is_empty(),
        "the const's callee must be recorded as a root, not as an edge target"
    );

    let stats = case.edit(MAIN, "const C: int = pick_b()\nfn main() { print(\"{C}\") }\n");

    assert!(!case.has_instance_named("pick_a"), "the old always-emitted root survived");
    assert!(case.has_instance_named("pick_b"), "the new always-emitted root was not reached");
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
        ("struct defaults", base.replace("struct S { v: int = 1 }", "struct S { v: int = 2 }")),
        ("enum", base.replace("enum E { A, B }", "enum E { A, B, C }")),
        ("newtype", base.replace("newtype N(int)", "newtype N(float)")),
        ("decorator body", base.replace("print(target.name)", "print(\"x\")")),
    ];
    for (what, edited) in edits {
        let mut s = Session::new(
            STDLIB.clone(),
            vec![(MAIN.to_string(), main.to_string()), (LIB.to_string(), base.to_string())],
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
        vec![(MAIN.to_string(), main.to_string()), (LIB.to_string(), PICK_LIB.to_string())],
    )
    .expect("session");
    let r = s
        .update_file(MAIN, "global g: int = pick_b()\nfn main() { print(\"{g}\") }\n")
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
        c.graph.uninstantiated_roots.values().map(|h| h.always_emitted_sites.len()).sum()
    };
    assert_eq!(holes(&case), 1, "the fixture recorded no always-emitted failure");

    let stats = case.edit(MAIN, "const C: int = takes(mono_id)\nfn main() { print(\"{C}\") }\n");

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
    assert!(stats.newly_reached > 0, "`mono_id` was not reached as a new root");
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
        .find(|(o, _)| o.instance().is_some_and(|i| case.identity(i).starts_with("leaf")))
        .map(|(_, c)| c.span_files.len())
        .expect("`leaf` owns a contribution");
    assert_eq!(leaf_owner, 0, "`leaf` must record no span, or the arm is not separated");
    assert!(!case.has_instance_named("added"));

    let stats = case.edit(LIB, "fn leaf() -> int { return added() }\nfn added() -> int { return 2 }\n");

    assert!(case.has_instance_named("added"), "the call added to an empty body was not seen");
    assert_eq!(stats.newly_reached, 1);
    case.assert_matches_fresh("empty body gains a call");
}

/// The same, for an ALWAYS-EMITTED body: a `const` whose initializer had no
/// callee at all and gains one.
#[test]
fn a_const_initializer_with_no_facts_that_gains_a_call_is_still_re_walked() {
    let mut case = Case::new("const C: int = 1\nfn main() { print(\"{C}\") }\n", PICK_LIB);
    assert!(!case.has_instance_named("pick_a"));

    let stats = case.edit(MAIN, "const C: int = pick_a()\nfn main() { print(\"{C}\") }\n");

    assert!(case.has_instance_named("pick_a"), "the call added to a const initializer was lost");
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
    let mut roots = instance_graph::emission_roots(ws, Some(PROJ));
    roots.pop().expect("the fixture has roots");
    let mut graph = graph_of(ws);
    graph.update_bodies(ws, roots, &[]);
}
