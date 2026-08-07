//! Oracle for skull incremental codegen (`docs/skull-incremental-emit-plan.md`):
//! after re-emitting only the bodies a (simulated) edit re-inferred, the assembled
//! Lua must be BYTE-IDENTICAL to a fresh full emit of the edited workspace. The
//! interesting case is editing an INLINED callee — its callers' chunks contain its
//! body, so they must re-emit too (the inline codegen-dep), even though a body edit
//! never re-infers the callers.

use bone_semantic::hir::BodyOwner;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use skull::mir_emit::{emit_full, emit_incremental, emit_overlay};
use crate::common::ws_of;

/// The `(ModuleId, BodyOwner)` reinferred-id for the named function in `ws`.
fn body_id(ws: &Workspace, name: &str) -> (ModuleId, BodyOwner) {
    // Every module, not `user_module_id`: that returns the FIRST non-builtin
    // module, which over a stdlib workspace is a stdlib module rather than the
    // fixture's own.
    for module in ws.modules.iter() {
        for (fid, f) in module.functions.iter() {
            if f.name.as_str() == name {
                return (module.id, BodyOwner::Function(fid));
            }
        }
    }
    panic!("no fn named {name}");
}

/// Drive one body edit through the cache and assert incremental == fresh.
fn check_edit(v1: &str, v2: &str, edited: &[&str]) {
    check_edit_with(ws_of, v1, v2, edited)
}

/// [`check_edit`] over a chosen workspace builder. A fixture needing the stdlib
/// (any `+`, any collection method) passes `common::ws_with_stdlib`; the bare
/// `ws_of` keeps the cheap fixtures cheap.
fn check_edit_with(build: fn(&str) -> Workspace, v1: &str, v2: &str, edited: &[&str]) {
    let ws1 = build(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (_full1, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");

    let ws2 = build(v2);
    let mid2 = skull::user_module_id(&ws2).unwrap();
    let reinferred: Vec<_> = edited.iter().map(|n| body_id(&ws2, n)).collect();

    let incremental = emit_incremental(&ws2, &mut cache, &reinferred).expect("emit_incremental");
    let fresh = emit_full(&ws2, mid2).expect("emit_full v2").0;
    assert_eq!(
        incremental.lua, fresh.lua,
        "incremental assembly != fresh emit after editing {edited:?}"
    );
}

#[test]
fn body_edit_to_inlined_callee_reemits_callers() {
    // `helper` is a tiny straight callee -> inlined into `use_it`. Editing its
    // body must re-emit `use_it` (its chunk holds the inlined value), not just
    // `helper`. `other` is untouched and reused from cache.
    let v1 = "@inline\nfn helper() -> int { 3 }\n\
              fn use_it() -> float { helper() }\n\
              fn other() -> int { 7 }";
    let v2 = "@inline\nfn helper() -> int { 5 }\n\
              fn use_it() -> float { helper() }\n\
              fn other() -> int { 7 }";
    check_edit(v1, v2, &["helper"]);
}

#[test]
fn body_edit_to_leaf_only_reemits_itself() {
    let v1 = "@inline\nfn helper() -> int { 3 }\n\
              fn use_it() -> float { helper() }\n\
              fn other() -> int { 7 }";
    let v2 = "@inline\nfn helper() -> int { 3 }\n\
              fn use_it() -> float { helper() }\n\
              fn other() -> int { 9 }"; // only `other` changes
    check_edit(v1, v2, &["other"]);
}

#[test]
fn value_struct_ctor_edit_reemits_inliner() {
    // `mk` returns a `value` struct -> inlined into `use_v` as a scalarized
    // CallMulti (deps must capture CallMulti callees, not just plain Call).
    // Editing the ctor body must re-emit `use_v` with the new inlined leaves.
    let v1 = "value V2 { x: int, y: int }\n\
              @inline\nfn mk() -> V2 { V2 { x: 1, y: 2 } }\n\
              fn use_v() -> int { let v = mk()\n v.x }";
    let v2 = "value V2 { x: int, y: int }\n\
              @inline\nfn mk() -> V2 { V2 { x: 9, y: 2 } }\n\
              fn use_v() -> int { let v = mk()\n v.x }";
    check_edit(v1, v2, &["mk"]);
}

#[test]
fn sequential_edits_stay_consistent() {
    // Apply a sequence of edits to ONE cache (like a watch session): after each
    // incremental update the assembly must still equal a fresh emit of that state.
    // Exercises cache reuse across multiple updates, not just the first.
    let ws0 = ws_of(
        "@inline\nfn c() -> int { 1 }\n@inline\nfn b() -> int { c() }\nfn a() -> int { b() }\nfn z() -> int { 9 }",
    );
    let mid = skull::user_module_id(&ws0).unwrap();
    let (_f0, mut cache) = emit_full(&ws0, mid).unwrap();

    let edits = [
        ("@inline\nfn c() -> int { 2 }\n@inline\nfn b() -> int { c() }\nfn a() -> int { b() }\nfn z() -> int { 9 }", "c"),
        ("@inline\nfn c() -> int { 2 }\n@inline\nfn b() -> int { c() }\nfn a() -> int { b() }\nfn z() -> int { 5 }", "z"),
        ("@inline\nfn c() -> int { 2 }\n@inline\nfn b() -> int { c() }\nfn a() -> int { b() }\nfn z() -> int { 7 }", "z"),
    ];
    for (src, edited) in edits {
        let ws = ws_of(src);
        let m = skull::user_module_id(&ws).unwrap();
        let reinferred = vec![body_id(&ws, edited)];
        let inc = emit_incremental(&ws, &mut cache, &reinferred).unwrap();
        let fresh = emit_full(&ws, m).unwrap().0;
        assert_eq!(inc.lua, fresh.lua, "incremental drifted from fresh after editing {edited}");
    }
}

#[test]
fn transitive_inline_edit_reemits_root() {
    // c() inlined into b() inlined into a(): a's chunk holds c's body. Editing c
    // must re-emit a (transitive inline dep).
    let v1 = "@inline\nfn c() -> int { 1 }\n\
              @inline\nfn b() -> int { c() }\n\
              fn a() -> int { b() }";
    let v2 = "@inline\nfn c() -> int { 2 }\n\
              @inline\nfn b() -> int { c() }\n\
              fn a() -> int { b() }";
    check_edit(v1, v2, &["c"]);
}

/// A SPECIALIZED body is a cached emitted body like any other, so editing the
/// generic it specializes must re-emit it.
///
/// Nothing else names that connection: a specialization is not reached through
/// its declaration's own chunk, so without the declaration edge in its deps a
/// stale copy is reused and the incremental output keeps the OLD body. The
/// global initializer is what forces a standalone specialized copy -- it never
/// runs an inliner, so the call cannot be served by a splice
/// (`vec_value_columns.rs` documents the same route).
#[test]
fn body_edit_to_a_generic_reemits_its_specializations() {
    let v1 = "value V2 { x: float, y: float }\n\
              fn first<T>(xs: [T]) -> T { return xs[0] }\n\
              global G: float = first([V2 { x: 1.0, y: 2.0 }]).x\n\
              fn use_g() -> float { return G }";
    let v2 = "value V2 { x: float, y: float }\n\
              fn first<T>(xs: [T]) -> T { return xs[1] }\n\
              global G: float = first([V2 { x: 1.0, y: 2.0 }]).x\n\
              fn use_g() -> float { return G }";
    check_edit(v1, v2, &["first"]);
}

/// The control: the specialized body is REALLY there, and it is what the edit
/// above moves. Without this the test could pass over a program that emits no
/// specialization at all, which is the shape every arm of it would satisfy.
#[test]
fn the_generic_fixture_emits_a_standalone_specialization() {
    let v1 = "value V2 { x: float, y: float }\n\
              fn first<T>(xs: [T]) -> T { return xs[0] }\n\
              global G: float = first([V2 { x: 1.0, y: 2.0 }]).x\n\
              fn use_g() -> float { return G }";
    let ws = crate::common::ws_of(v1);
    let mid = skull::user_module_id(&ws).unwrap();
    let lua = emit_full(&ws, mid).expect("emit_full").0.lua;
    assert!(
        lua.contains("function first_arr_T_ret_T_spec_a_n2_V2_0_z("),
        "fixture must emit a standalone specialized body"
    );
}

/// The OVERLAY carries a changed specialization, not just its declaration.
///
/// A reload dofiles the overlay after `main.lua`, so a definition the overlay
/// omits keeps running the version main.lua carries. Redefining only the
/// declaration's shared body would leave the specialized copy -- which is what
/// the unspliceable call site actually calls -- pinned at the pre-edit body,
/// and nothing about the reload would look wrong.
#[test]
fn an_overlay_redefines_a_changed_specialization() {
    let v1 = "value V2 { x: float, y: float }\n\
              fn first<T>(xs: [T]) -> T { return xs[0] }\n\
              global G: float = first([V2 { x: 1.0, y: 2.0 }]).x\n\
              fn use_g() -> float { return G }";
    let v2 = "value V2 { x: float, y: float }\n\
              fn first<T>(xs: [T]) -> T { return xs[1] }\n\
              global G: float = first([V2 { x: 1.0, y: 2.0 }]).x\n\
              fn use_g() -> float { return G }";
    let ws1 = crate::common::ws_of(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");
    let spec = "function first_arr_T_ret_T_spec_a_n2_V2_0_z(";
    assert!(full.lua.contains(spec), "control: the full build defines the specialization");

    let ws2 = crate::common::ws_of(v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "first")]);
    assert!(
        overlay.lua.contains(spec),
        "the overlay must redefine the specialization the edit moved:\n{}",
        overlay.lua
    );
}

/// The OVERLAY carries a specialization the edit made NEWLY reachable
/// (mono cut C+D, `B*-backfill`).
///
/// The sibling above edits a body whose specialization already exists, so the
/// overlay only has to REDEFINE it. Here the edit adds the first call to a new
/// instantiation, so the definition has to be ADDED -- and a reload dofiles the
/// overlay after `main.lua`, which carries no such definition. The edited
/// caller's call spells `..._spec_...`, so omitting it is a nil global at
/// reload rather than a stale body.
///
/// The two populations are asserted in the same test: the P specialization must
/// already exist (so this is about the NEW one) and the Q one must not.
#[test]
fn an_overlay_carries_a_newly_reachable_specialization() {
    let types = "value P { x: float, y: float }\n\
                 value Q { a: float, b: float, c: float }\n\
                 fn tally_of<T>(xs: [T]) -> T { return xs[0] }\n";
    let v1 = types.to_string()
        + "global G1: float = tally_of([P { x: 1.0, y: 0.0 }]).x\n\
           fn use_it() -> float { return G1 }";
    let v2 = types.to_string()
        + "global G1: float = tally_of([P { x: 1.0, y: 0.0 }]).x\n\
           fn use_it() -> float { return tally_of([Q { a: 1.0, b: 2.0, c: 3.0 }]).a }";

    let ws1 = crate::common::ws_of(&v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");
    let spec_of = |lua: &str, ty: &str| {
        lua.lines().any(|l| l.starts_with("function tally_of_") && l.contains(ty))
    };
    assert!(spec_of(&full.lua, "P"), "control: v1 already defines the P specialization");
    assert!(!spec_of(&full.lua, "Q"), "control: v1 must NOT define the Q one yet");

    let ws2 = crate::common::ws_of(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "use_it")]);
    assert!(
        spec_of(&overlay.lua, "Q"),
        "the overlay must ADD the specialization the edit made reachable, or the \
         reload calls a nil global:\n{}",
        overlay.lua
    );
}

/// The overlay backfills a newly-reachable `Iterator`-BOUNDED body, which is
/// the case its demand tree is load-bearing for (mono cut C+D, D-1 +
/// `B*-backfill`).
///
/// A bounded body cannot be lowered from its declaration alone: its placeholder
/// `next` calls are resolved by the demand, not by the body. So this is the arm
/// that says the backfill carries the TREE and not just the key -- a backfill
/// that rebuilt from the declaration would reject rather than emit.
///
/// Runs with inlining DISABLED, where a bounded call has no splice to fall back
/// on.
#[test]
fn an_overlay_carries_a_newly_reachable_bounded_body() {
    let _g = crate::common::AblationGuard::on();
    let common = "fn total<I: Iterator<int>>(it: I) -> int {\n\
                  \x20   let s = 0\n\
                  \x20   for v in it { s = s + v }\n\
                  \x20   return s\n\
                  }\n";
    let v1 = common.to_string() + "fn use_it() -> int { return 0 }";
    let v2 = common.to_string()
        + "fn use_it() -> int {\n\
           \x20   let xs: [int] = [1, 2, 3]\n\
           \x20   return total(xs.iter())\n\
           }";

    let ws1 = crate::common::ws_with_stdlib(&v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");
    let standalone = |lua: &str| {
        lua.lines().any(|l| l.starts_with("function total_") && l.contains("_spec_"))
    };
    assert!(
        !standalone(&full.lua),
        "control: v1 must NOT already define the bounded body, or the backfill has \
         nothing to add"
    );

    let ws2 = crate::common::ws_with_stdlib(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "use_it")]);
    assert!(
        standalone(&overlay.lua),
        "the overlay must ADD the bounded body the edit made reachable, with its \
         placeholders resolved from the demand's tree:\n{}",
        overlay.lua
    );
}

/// ONE liveness answer decides both kinds of emitted body.
///
/// The renderer no longer discovers or builds: it orders text the walk already
/// reached. So a specialized body reaching a FURTHER specialization has to come
/// out of the walk, and if the walk stopped carrying that edge the build fails
/// loudly (`no liveness walk reached it`) rather than quietly rebuilding it in a
/// second place. Measured on bitgun: cutting the edge fires that rejection.
///
/// Here the property is observable as output: the specialized body is emitted at
/// all, and it is emitted ONCE -- two deciders would each be entitled to render
/// it.
#[test]
fn one_liveness_walk_decides_the_specialized_bodies() {
    let src = "value V2 { x: float, y: float }\n\
               fn first<T>(xs: [T]) -> T { return xs[0] }\n\
               global G: float = first([V2 { x: 1.0, y: 2.0 }]).x\n\
               fn use_g() -> float { return G }";
    let ws = crate::common::ws_of(src);
    let mid = skull::user_module_id(&ws).unwrap();
    let lua = emit_full(&ws, mid).expect("emit_full").0.lua;
    let def = "function first_arr_T_ret_T_spec_a_n2_V2_0_z(";
    assert_eq!(
        lua.matches(def).count(),
        1,
        "the specialized body is decided once and emitted once"
    );
}

/// Editing a generic `co fn` must re-emit the SPECIALIZED coroutines of it.
///
/// A specialization is not reached through its declaration's chunk, so the only
/// thing naming the connection is the declaration edge the specialized chunk
/// records in its deps. Byte identity cannot see this at all -- it compares
/// fresh full builds and never performs an edit -- so the arm lives here.
///
/// The element is column-stored on purpose: that is what makes the call
/// specialize on the default path, so the fixture has a specialized coroutine to
/// invalidate rather than one shared body.
#[test]
fn a_body_edit_to_a_generic_coroutine_reemits_its_specializations() {
    let v1 = "nobox value P { x: float, y: float }\n\
              co fn walk<T>(v: [T]) -> Coroutine<int, int, ()> {\n\
              \x20   let n = v.len\n\
              \x20   yield n\n\
              \x20   return 1\n\
              }\n\
              fn use_it() -> Coroutine<int, int, ()> {\n\
              \x20   let xs: [P] = [P { x: 1.0, y: 0.0 }]\n\
              \x20   return walk(xs)\n\
              }";
    let v2 = v1.replace("return 1", "return 2");
    // The population, asserted in the same test: without a specialized
    // coroutine in v1 there is nothing for the edit to invalidate and the
    // comparison would pass over an empty set.
    let ws = crate::common::ws_of(v1);
    let mid = skull::user_module_id(&ws).unwrap();
    let lua = emit_full(&ws, mid).expect("emit_full v1").0.lua;
    let spec_steps = lua
        .match_indices("__step(")
        .map(|(i, _)| lua[..i].rsplit("function ").next().unwrap_or("").to_string())
        .filter(|n| n.contains("_spec_"))
        .count();
    assert_eq!(spec_steps, 1, "the fixture must produce a SPECIALIZED coroutine step:\n{lua}");
    check_edit(v1, &v2, &["walk"]);
}

/// An edit that introduces a NEW specialization demand must BACKFILL it
/// (mono cut C+D, the item B\* left open and D-1 absorbs).
///
/// Invalidation and backfill are different failures. The sibling above edits a
/// callee whose specializations already exist, so the cache knows the key set and
/// only has to re-emit; here the edit is in the CALLER and produces a key the
/// cache has never seen. A cache that only re-emits what it already holds is
/// green on that one and silently short a definition on this one -- the caller
/// would then spell a specialized symbol nothing defines.
///
/// It works because backfill is NOT a second mechanism: `emit_incremental`
/// reassembles through the same `live_bodies` walk `emit_full` runs, so a demand
/// that appears for the first time is discovered and BUILT there. Pinning it
/// here is what keeps a future "only reassemble what changed" optimization from
/// removing that without noticing.
///
/// Asserted on the incremental output's own definition COUNT as well as against
/// a fresh build. Equality alone would be satisfied by two builds that both lack
/// the new body, and the count alone would not catch a stale one.
#[test]
fn an_edit_that_introduces_a_new_specialization_demand_backfills_it() {
    let types = "value P { x: float, y: float }\n\
                 value Q { a: float, b: float, c: float }\n\
                 fn tally_of<T>(xs: [T]) -> T { return xs[0] }\n";
    let v1 = types.to_string()
        + "fn use_it() -> float {\n\
           \x20   let ps: [P] = [P { x: 1.0, y: 0.0 }]\n\
           \x20   return tally_of(ps).x\n\
           }";
    let v2 = types.to_string()
        + "fn use_it() -> float {\n\
           \x20   let ps: [P] = [P { x: 1.0, y: 0.0 }]\n\
           \x20   let qs: [Q] = [Q { a: 1.0, b: 2.0, c: 3.0 }]\n\
           \x20   return tally_of(ps).x + tally_of(qs).a\n\
           }";
    let specs_in = |src: &str| {
        let ws = crate::common::ws_with_stdlib(src);
        let mid = skull::user_module_id(&ws).unwrap();
        emit_full(&ws, mid)
            .expect("emit_full")
            .0
            .lua
            .lines()
            .filter(|l| l.starts_with("function tally_of_") && l.contains("_spec_"))
            .count()
    };
    assert_eq!(specs_in(&v1), 1, "v1 must hold exactly one SPECIALIZED `tally_of`");
    assert_eq!(
        specs_in(&v2),
        2,
        "v2 must hold TWO, or the edit introduces no new demand and this proves nothing"
    );
    check_edit_with(crate::common::ws_with_stdlib, &v1, &v2, &["use_it"]);

    // The direct arm: the INCREMENTAL assembly holds both specialized bodies.
    let ws1 = crate::common::ws_with_stdlib(&v1);
    let mid1 = skull::user_module_id(&ws1).unwrap();
    let (_, mut cache) = emit_full(&ws1, mid1).expect("emit_full v1");
    let ws2 = crate::common::ws_with_stdlib(&v2);
    let reinferred = vec![body_id(&ws2, "use_it")];
    let incremental = emit_incremental(&ws2, &mut cache, &reinferred).expect("emit_incremental");
    let defs = incremental
        .lua
        .lines()
        .filter(|l| l.starts_with("function tally_of_") && l.contains("_spec_"))
        .count();
    assert_eq!(
        defs, 2,
        "the incremental assembly must BACKFILL the specialization the edit introduced"
    );
}

/// A standalone `Iterator`-bounded body survives an incremental REBUILD
/// (mono cut C+D step D-1).
///
/// Its placeholders are resolved from the demand's tree, not from the body, so a
/// rebuild that has the body and not the tree cannot re-emit it -- and that is
/// precisely what an incremental rebuild does: it re-lowers from the declaration
/// under a chunk key. The tree therefore lives in the cache beside the chunk
/// key, and this is the arm that reads it back.
///
/// Byte identity cannot see any of this. It compares fresh full builds and never
/// performs an edit, so the tree could be dropped on the cache path and every
/// artifact would still match.
///
/// Runs with inlining DISABLED, which is where a bounded body has no splice to
/// fall back on -- and is what E1 does by construction.
#[test]
fn a_standalone_bounded_body_survives_an_incremental_rebuild() {
    let _g = crate::common::AblationGuard::on();
    let v1 = "fn total<I: Iterator<int>>(it: I) -> int {\n\
              \x20   let s = 0\n\
              \x20   for v in it { s = s + v }\n\
              \x20   return s\n\
              }\n\
              fn use_it() -> int {\n\
              \x20   let xs: [int] = [1, 2, 3]\n\
              \x20   return total(xs.iter())\n\
              }";
    let v2 = v1.replace("let s = 0", "let s = 100");

    let ws1 = crate::common::ws_with_stdlib(v1);
    let mid1 = skull::user_module_id(&ws1).unwrap();
    let (full1, mut cache) = emit_full(&ws1, mid1).expect("emit_full v1");
    // The population, in the same run: without a standalone bounded definition
    // in v1 the rebuild below has nothing to lose and the arm proves nothing.
    let standalone = |lua: &str| {
        lua.lines().filter(|l| l.starts_with("function total_") && l.contains("_spec_")).count()
    };
    assert_eq!(standalone(&full1.lua), 1, "v1 must emit ONE standalone bounded body");

    let ws2 = crate::common::ws_with_stdlib(&v2);
    let reinferred = vec![body_id(&ws2, "total")];
    let incremental = emit_incremental(&ws2, &mut cache, &reinferred).expect("emit_incremental");
    assert_eq!(
        standalone(&incremental.lua),
        1,
        "the rebuilt bounded body is gone -- its demand tree did not survive the cache"
    );
    let fresh = emit_full(&ws2, skull::user_module_id(&ws2).unwrap()).expect("emit_full v2").0;
    assert_eq!(
        incremental.lua, fresh.lua,
        "incremental assembly != fresh emit after editing a bounded body"
    );
}
