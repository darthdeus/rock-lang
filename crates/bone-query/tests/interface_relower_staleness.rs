//! Generic arity is checked at lower time, so an arity change must re-lower dependents.

use crate::fixture::Project;
use bone_query::BoneWorkspace;

#[test]
fn arity_changes_relower_dependents_like_a_fresh_workspace() {
    // `b.v` is read so the unused-param lint stays out of it.
    let user = "pub mod user;\nuse super::decl::Box;\nfn f(b: Box) -> int { b.v }\n";
    let manifest = (
        "stick.toml",
        "[package]\nname = \"relower\"\ntype = \"lib\"\n\n[language]\nmodules = \"v1\"\n",
    );
    // (case, decl before, decl after, a message the result reports; "" is clean)
    let cases = [
        (
            "arity grows",
            "pub mod decl;\npub struct Box { pub v: int }\n",
            "pub mod decl;\npub struct Box<T> { pub v: T }\n",
            "requires 1 type argument",
        ),
        (
            "arity shrinks",
            "pub mod decl;\npub struct Box<T> { pub v: T }\n",
            "pub mod decl;\npub struct Box { pub v: int }\n",
            "",
        ),
    ];
    for (case, before, after, fragment) in cases {
        let project = Project::new(&[
            manifest,
            ("main.rock", "fn main() -> int { 0 }\n"),
            ("decl.rock", before),
            ("user.rock", user),
        ]);
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
