//! An interface change that keeps a type's name must still re-lower the
//! dependents whose signatures reference it. Generic arity is checked at lower
//! time ("requires N type argument(s)"), so an arity change has to invalidate
//! dependents like a removed type does, or the dependent is re-inferred
//! against a stale lowering. A fresh workspace is the ground truth.

use crate::fixture::Project;
use bone_query::BoneWorkspace;

#[test]
fn arity_changes_relower_dependents_like_a_fresh_workspace() {
    // `b.v` is read so the unused-param lint stays out of it.
    let user = "fn f(b: Box) -> int { b.v }\n";
    // (case, decl before, decl after, a message the result reports; "" is clean)
    let cases = [
        (
            "arity grows",
            "struct Box { v: int }\n",
            "struct Box<T> { v: T }\n",
            "requires 1 type argument",
        ),
        (
            "arity shrinks",
            "struct Box<T> { v: T }\n",
            "struct Box { v: int }\n",
            "",
        ),
    ];
    for (case, before, after, fragment) in cases {
        let project = Project::new(&[("decl.rock", before), ("user.rock", user)]);
        let (dp, up) = (project.path("decl.rock"), project.path("user.rock"));
        let mut inc = BoneWorkspace::new();
        inc.update(&dp, before.to_string());
        inc.update(&up, user.to_string());
        std::fs::write(&dp, after).unwrap();
        inc.update(&dp, after.to_string());

        let mut fresh = BoneWorkspace::new();
        fresh.update(&dp, after.to_string());
        fresh.update(&up, user.to_string());

        let diags = |b: &BoneWorkspace| {
            let mut v: Vec<_> = b
                .diagnostics_for(&up)
                .into_iter()
                .map(|d| d.message)
                .collect();
            v.sort();
            v
        };
        let incremental = diags(&inc);
        assert_eq!(incremental, diags(&fresh), "{case}");
        if fragment.is_empty() {
            assert!(incremental.is_empty(), "{case}: {incremental:?}");
        } else {
            assert!(
                incremental.iter().any(|m| m.contains(fragment)),
                "{case}: {incremental:?}"
            );
        }
    }
}
