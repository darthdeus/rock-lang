//! Identity storage that only substitution shows: `fn stash<T>(x: T) { let v = [x] }` stores nothing bone can see, but
//! `stash<Unit>` stores an identity struct in a Vec. The instance graph rejects
//! it; emission makes the rejection fatal for the package whose call site
//! instantiated it, and exempt packages keep compiling.

use crate::common::{
    try_emit_module, try_emit_module_enforcing_identity_storage, ws_with_stdlib,
    ws_with_stdlib_module,
};
use bone_semantic::inference::Workspace;
use skull::instance_graph;

const IDENTITY_STORAGE: &str = "identity-in-ordinary-storage";

fn identity_rejections(src: &str) -> Vec<String> {
    rejections_in(&ws_with_stdlib(src))
}

fn rejections_in(ws: &Workspace) -> Vec<String> {
    let roots = skull::mir_emit::emission_roots(ws, None);
    let g = instance_graph::build(ws, roots, false);
    g.rejections()
        .filter(|r| r.kind.name() == IDENTITY_STORAGE)
        .map(|r| {
            assert!(
                !instance_graph::is_spanless(&r.span),
                "spanless rejection: {r:?}"
            );
            format!("{}: {}", g.identity(ws, &r.at), r.message)
        })
        .collect()
}

const STASH: &str = r#"
struct Unit { hp: int }
fn stash<T>(x: T) -> int {
    let v = [x]
    return v.len()
}
fn main() {
    let u = Unit { hp: 1 }
    print("{stash(u)}")
}
"#;

#[test]
fn substituted_identity_in_a_vec_is_rejected() {
    let r = identity_rejections(STASH);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(r[0].contains("stash") && r[0].contains("`Unit`"), "{r:?}");
}

#[test]
fn identity_forwarded_through_a_generic_is_rejected_where_stored() {
    let r = identity_rejections(
        r#"
struct Unit { hp: int }
fn stash<T>(x: T) -> int {
    let v = [x]
    return v.len()
}
fn outer<T>(x: T) -> int { return stash(x) }
fn main() {
    let u = Unit { hp: 1 }
    print("{outer(u)}")
}
"#,
    );
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(r[0].contains("stash"), "{r:?}");
}

/// Controls: values, handles and scalars are ordinary storage, and an identity
/// bound to a generic local or parameter is a borrow.
#[test]
fn substituted_values_handles_and_borrows_are_not_rejected() {
    let r = identity_rejections(
        r#"
struct Unit { hp: int }
value Point { x: int }
fn stash<T>(x: T) -> int {
    let v = [x]
    return v.len()
}
fn keep<T>(x: T) -> T {
    let y = x
    return y
}
fn main() {
    let rows: Arena<Unit> = arena_new()
    let e = rows.insert(Unit { hp: 1 })
    let u = Unit { hp: 2 }
    print("{stash(1)} {stash(Point { x: 1 })} {stash(e)} {keep(u).hp}")
}
"#,
    );
    assert!(r.is_empty(), "{r:?}");
}

#[test]
fn substitution_only_identity_storage_is_fatal_for_an_enforced_package() {
    let err =
        try_emit_module_enforcing_identity_storage(STASH).expect_err("stash<Unit> must reject");
    assert!(
        err.message.contains("cannot store identity struct `Unit`"),
        "{}",
        err.message
    );
}

#[test]
fn substitution_only_identity_storage_is_counted_not_fatal_for_an_exempt_package() {
    try_emit_module(STASH).expect("unpackaged programs are exempt");
}

/// A generic `T -> any` coercion erases the identity `erase<Unit>` binds.
#[test]
fn substituted_identity_erased_to_any_is_rejected() {
    let r = identity_rejections(
        r#"
struct Unit { hp: int }
fn keep(x: any) -> int { return 0 }
fn erase<T>(x: T) -> int { return keep(x) }
fn main() {
    let u = Unit { hp: 1 }
    print("{erase(u)}")
}
"#,
    );
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(
        r[0].contains("erase") && r[0].contains("cannot erase identity struct `Unit`"),
        "{r:?}"
    );
}

/// Stdlib bodies are held to the rule under substitution too: only the exact
/// Arena and Scene mechanisms carry a capability.
#[test]
fn substituted_identity_erased_to_any_in_a_stdlib_body_is_rejected() {
    let ws = ws_with_stdlib_module(
        r#"
fn __probe_keep(x: any) -> int { return 0 }
fn __probe_erase<T>(x: T) -> int { return __probe_keep(x) }
"#,
        r#"
struct Unit { hp: int }
fn main() {
    let u = Unit { hp: 1 }
    print("{__probe_erase(u)}")
}
"#,
    );
    let r = rejections_in(&ws);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(
        r[0].contains("__probe_erase") && r[0].contains("cannot erase identity struct `Unit`"),
        "{r:?}"
    );
}

/// Arena rows and Scene roots are the legal homes of an identity: every Arena
/// and Scene operation instantiated at one stores it only through
/// `ArenaStore<T>.data` and the registries' `@identity_erasure` capability.
const ARENA_AND_SCENE_ROWS: &str = r#"
struct Unit { hp: int }
struct World { units: Arena<Unit> }
fn build_world(builder: SceneBuilder) -> World? {
    return World { units: builder.arena() }
}
fn main() {
    let units: Arena<Unit> = arena_new()
    let e = units.insert(Unit { hp: 1 })
    let again = units.reuse_retained_entity()
    let reused = units.reuse_retained()
    let got = units.get(e)
    let by_entity = e.get()
    let has = units.contains(e)
    let count = units.len() + arena_slot_count(units)
    let snapshot = arena_generation_snapshot(units)
    let picked = units.random()
    for unit in values(units) { count += unit.hp }
    for (entity, unit) in pairs(units) { count += unit.hp }
    for entity in keys(units) { count += 1 }
    for (entity, unit) in units { count += unit.hp }
    let removed = units.remove(e)
    let removed_by_entity = e.remove()
    units.clear()
    let destroyed = arena_destroy(units)
    let scene: Scene<World>? = scene_create((b) => build_world(b))
    if scene != null {
        let world = scene.get()
        let closed = scene_close(scene)
    }
    print("{count}")
}
"#;

#[test]
fn arena_rows_and_scene_roots_of_an_identity_are_not_rejected() {
    let r = identity_rejections(ARENA_AND_SCENE_ROWS);
    assert!(r.is_empty(), "{r:?}");
    try_emit_module_enforcing_identity_storage(ARENA_AND_SCENE_ROWS)
        .expect("Arena rows and Scene roots are identity storage");
}
