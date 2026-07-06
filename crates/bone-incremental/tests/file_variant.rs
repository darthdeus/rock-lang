// `Session::with_file_variant` -- the speculative-analysis primitive for IDE
// paths (completion munges): apply a text variant of one tracked file through
// the normal update tiers, run a query closure against the workspace, restore.
// Contract: (1) the closure observes the variant, (2) the session afterwards is
// equivalent to never having applied it (text restored, variant items gone,
// re-applying the original text is a NoChange).

use bone_incremental::session::{Session, UpdateTier};
use bone_semantic::prelude::*;

fn files(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect()
}

fn fixture() -> Vec<(String, String)> {
    files(&[
        (
            "a.rock",
            "struct Foo { bar: int }\nfn make_foo() -> Foo { return Foo { bar: 1 } }\n",
        ),
        (
            "b.rock",
            "fn use_foo() -> int {\n    let f = make_foo()\n    return f.bar\n}\n",
        ),
    ])
}

fn has_fn(ws: &Workspace, name: &str) -> bool {
    let n = Ustr::from(name);
    ws.modules.iter().any(|m| m.functions_by_name.contains_key(&n))
}

#[test]
fn variant_visible_inside_and_gone_after() {
    let mut session = Session::new(vec![], fixture()).expect("session init");
    let original = session.files["b.rock"].text.clone();

    let variant = format!("{original}fn __variant_probe() -> int {{ return 2 }}\n");
    let seen = session
        .with_file_variant("b.rock", &variant, |ws| has_fn(ws, "__variant_probe"))
        .expect("variant update");
    assert!(seen, "closure must observe the variant text's items");

    assert_eq!(session.files["b.rock"].text, original, "text must be restored");
    assert!(
        !has_fn(session.workspace(), "__variant_probe"),
        "restore must remove the variant's items"
    );
    let res = session.update_file("b.rock", &original).expect("noop re-update");
    assert_eq!(res.tier, UpdateTier::NoChange, "restored state must be hash-identical");
}

#[test]
fn broken_member_munge_still_infers_cross_file() {
    // The munge shape completion feeds: a mid-edit `f.<sentinel>` member access.
    // The variant module must still lower + infer (recovery), and the receiver's
    // cross-file type (Foo from a.rock) must be resolvable inside the closure.
    let mut session = Session::new(vec![], fixture()).expect("session init");
    let original = session.files["b.rock"].text.clone();

    let variant = "fn use_foo() -> int {\n    let f = make_foo()\n    let x = f.__rock_completion__\n    return f.bar\n}\n";
    let inferred = session
        .with_file_variant("b.rock", variant, |ws| {
            let mid = ws.module_ids[&Ustr::from("b.rock")];
            ws.inference(mid).is_some()
        })
        .expect("variant update");
    assert!(inferred, "variant module must be inferred under recovery");
    assert_eq!(session.files["b.rock"].text, original, "text must be restored");
}

#[test]
fn untracked_path_is_added_then_removed() {
    // A file the session REFUSED (its live text never lowered clean) is still
    // completable: the variant is speculatively tracked, analyzed, and removed
    // again -- the session ends as untracked as it started.
    let mut session = Session::new(vec![], fixture()).expect("session init");
    let seen = session
        .with_file_variant("c.rock", "fn __variant_probe() -> int { return 3 }\n", |ws| {
            has_fn(ws, "__variant_probe")
        })
        .expect("untracked variant update");
    assert!(seen, "closure must observe the speculatively-tracked variant");
    assert!(!session.is_tracked("c.rock"), "variant file must be removed after");
    assert!(
        !has_fn(session.workspace(), "__variant_probe"),
        "variant items must be gone after removal"
    );
}
