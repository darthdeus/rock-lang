//! Interface-edit re-lower staleness: a change to a type's INTERFACE that keeps
//! the type's name must still re-lower the dependents whose signatures reference
//! it. Generic ARITY is a lower-time property (the bare-generic "requires N type
//! argument(s)" check), so an arity change has to invalidate dependents the same
//! way a removed/renamed type does -- otherwise the dependent is only re-inferred
//! against a stale lowering and reports a garbled error until an LSP restart.
//!
//! Each test compares the INCREMENTAL diagnostics against a fresh ("restart")
//! workspace, which is ground truth.

use bone_query::BoneWorkspace;

/// Run `decl_a` + `user`, edit decl to `decl_b` incrementally, and return
/// `(incremental_user_diags, restart_user_diags)` for comparison.
fn incr_vs_restart(
    subdir: &str,
    decl_a: &str,
    decl_b: &str,
    user_src: &str,
) -> (Vec<String>, Vec<String>) {
    let dir = tempfile::Builder::new().prefix(subdir).tempdir().unwrap();
    let decl = dir.path().join("decl.rock");
    let user = dir.path().join("user.rock");
    let dp = decl.to_str().unwrap();
    let up = user.to_str().unwrap();

    std::fs::write(&decl, decl_a).unwrap();
    std::fs::write(&user, user_src).unwrap();
    let mut inc = BoneWorkspace::new();
    inc.update(dp, decl_a.to_string());
    inc.update(up, user_src.to_string());
    std::fs::write(&decl, decl_b).unwrap();
    inc.update(dp, decl_b.to_string());

    let mut fresh = BoneWorkspace::new();
    fresh.update(dp, decl_b.to_string());
    fresh.update(up, user_src.to_string());

    let diags = |b: &BoneWorkspace| {
        let mut v = b
            .diagnostics_for(up)
            .into_iter()
            .map(|d| d.message)
            .collect::<Vec<_>>();
        v.sort();
        v
    };
    (diags(&inc), diags(&fresh))
}

#[test]
fn arity_grow_relowers_dependent_signature() {
    // `struct Box` (arity 0) -> `struct Box<T>` (arity 1). A dependent's bare
    // `fn f(b: Box)` must re-lower and report the arity error, not a garbled
    // body-inference error against the stale arity-0 lowering.
    let (incremental, restart) = incr_vs_restart(
        "rock_relower_arity_grow",
        "struct Box { v: int }\n",
        "struct Box<T> { v: T }\n",
        "fn f(b: Box) -> int { b.v }\n",
    );
    assert_eq!(
        incremental, restart,
        "incremental diagnostics must match a fresh workspace after arity grew"
    );
    assert!(
        incremental.iter().any(|m| m.contains("requires 1 type argument")),
        "the proper arity diagnostic should appear: {incremental:?}"
    );
}

#[test]
fn arity_shrink_clears_dependent_error() {
    // The reverse direction (already handled via the interface-error re-lower
    // path): `struct Box<T>` -> `struct Box` clears the bare-generic error.
    let (incremental, restart) = incr_vs_restart(
        "rock_relower_arity_shrink",
        "struct Box<T> { v: T }\n",
        "struct Box { v: int }\n",
        "fn f(b: Box) -> int { b.v }\n", // read `b` so the unused-param lint stays out of it
    );
    assert_eq!(
        incremental, restart,
        "incremental diagnostics must match a fresh workspace after arity shrank"
    );
    assert!(incremental.is_empty(), "arity error should clear: {incremental:?}");
}
