//! Identity storage that only substitution shows (docs/identity-storage-plan.md
//! S0c): `fn stash<T>(x: T) { let v = [x] }` stores nothing bone can see, but
//! `stash<Unit>` stores an identity struct in a Vec. The instance graph rejects
//! it; emission makes the rejection fatal for the package whose call site
//! instantiated it, and exempt packages keep compiling.

use crate::common::{try_emit_module, try_emit_module_enforcing_identity_storage, ws_with_stdlib};
use skull::instance_graph;

const IDENTITY_STORAGE: &str = "identity-in-ordinary-storage";

fn identity_rejections(src: &str) -> Vec<String> {
    let ws = ws_with_stdlib(src);
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots, false);
    g.rejections()
        .filter(|r| r.kind.name() == IDENTITY_STORAGE)
        .map(|r| {
            assert!(!instance_graph::is_spanless(&r.span), "spanless rejection: {r:?}");
            format!("{}: {}", g.identity(&ws, &r.at), r.message)
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
    let err = try_emit_module_enforcing_identity_storage(STASH).expect_err("stash<Unit> must reject");
    assert!(err.message.contains("cannot store identity struct `Unit`"), "{}", err.message);
}

#[test]
fn substitution_only_identity_storage_is_counted_not_fatal_for_an_exempt_package() {
    try_emit_module(STASH).expect("unpackaged programs are exempt");
}
