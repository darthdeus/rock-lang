// `Session::with_file_variant`: analyze a text variant of one tracked file, then restore.

use bone_incremental::session::{Session, UpdateTier};
use bone_semantic::prelude::*;

use crate::common;

fn session() -> Session {
    common::session(&[
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
    ws.modules.iter().any(|m| {
        m.functions_by_name
            .keys()
            .any(|key| key == &n || key.as_str().rsplit("::").next() == Some(name))
    })
}

#[test]
fn variant_visible_inside_and_gone_after() {
    let mut session = session();
    let original = session.files["b.rock"].text.clone();

    let variant = format!("{original}fn __variant_probe() -> int {{ return 2 }}\n");
    let seen = session
        .with_file_variant("b.rock", &variant, |ws| has_fn(ws, "__variant_probe"))
        .expect("variant update");
    assert!(seen, "closure must observe the variant text's items");

    assert_eq!(
        session.files["b.rock"].text, original,
        "text must be restored"
    );
    assert!(
        !has_fn(session.workspace(), "__variant_probe"),
        "restore must remove the variant's items"
    );
    let res = session
        .update_file("b.rock", &original)
        .expect("noop re-update");
    assert_eq!(
        res.tier,
        UpdateTier::NoChange,
        "restored state must be hash-identical"
    );
}

#[test]
fn untracked_path_is_added_then_removed() {
    // A refused file is still completable, and the session ends as untracked as it started.
    let mut session = session();
    let seen = session
        .with_file_variant(
            "c.rock",
            "fn __variant_probe() -> int { return 3 }\n",
            |ws| has_fn(ws, "__variant_probe"),
        )
        .expect("untracked variant update");
    assert!(
        seen,
        "closure must observe the speculatively-tracked variant"
    );
    assert!(
        !session.is_tracked("c.rock"),
        "variant file must be removed after"
    );
    assert!(
        !has_fn(session.workspace(), "__variant_probe"),
        "variant items must be gone after removal"
    );
}
