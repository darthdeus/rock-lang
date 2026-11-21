//! `Workspace::new` keeps the synthetic `$builtin` module in the ordinary module table.

use bone_incremental::session::{Session, UpdateTier};
use bone_semantic::prelude::*;

use crate::common;

const WARM: &str = "
fn __op_add(a: int, b: int) -> int { 0 }
";

fn session() -> Session {
    let warm =
        vec![skull_parser::parse_module_clean(WARM, Ustr::from("<warm>")).expect("warm parses")];
    let files = vec![
        common::v1("a.rock", "struct S { v: int }\nfn f() -> int { 1 }\n"),
        common::v1("b.rock", "fn use_s(s: S) -> int { s.v }\n"),
    ];
    common::with_warm(warm, files)
}

fn user_files(session: &Session) -> Vec<String> {
    let mut files: Vec<String> = session
        .ws
        .user_modules(&session.warm_mids)
        .map(|m| m.file.to_string())
        .collect();
    files.sort();
    files
}

fn assert_builtins_valid(session: &Session, stage: &str) {
    let ws = &session.ws;
    let builtin = ws.builtin_module();
    assert!(builtin.id.is_builtin(), "{stage}");
    assert_eq!(builtin.file.as_str(), "$builtin", "{stage}");
    assert_eq!(ws.module_ids[&Ustr::from("$builtin")], ModuleId::builtin());
    let b = &ws.builtins;
    for (ty, name) in [
        (b.int(), "int"),
        (b.uint(), "uint"),
        (b.float(), "float"),
        (b.bool(), "bool"),
        (b.string(), "string"),
        (b.unit(), "()"),
        (b.any(), "any"),
        (b.never(), "never"),
    ] {
        assert_eq!(ws.render(None, ty), name, "{stage}");
    }
    assert!(Workspace::is_permanent_module(ModuleId::builtin()));
}

fn assert_counts(session: &Session, stage: &str) {
    let ws = &session.ws;
    assert_eq!(ws.source_modules().count(), ws.modules.len() - 1, "{stage}");
    assert!(ws.source_modules().all(|m| !m.id.is_builtin()), "{stage}");
    assert_eq!(user_files(session), ["a.rock", "b.rock"], "{stage}");
    for mid in &session.warm_mids {
        assert!(!Workspace::is_user_module(*mid, &session.warm_mids));
    }
    assert!(!Workspace::is_user_module(
        ModuleId::builtin(),
        &session.warm_mids
    ));
}

#[test]
fn builtin_module_survives_the_session_lifecycle() {
    let mut s = session();
    assert_builtins_valid(&s, "new");
    assert_counts(&s, "new");
    // Warm stdlib plus two user files, never the builtin module.
    assert_eq!(s.ws.source_modules().count(), 3);

    for (stage, text) in [
        ("body edit", "struct S { v: int }\nfn f() -> int { 2 }\n"),
        (
            "interface edit",
            "struct S { v: int, w: float }\nfn f() -> int { 2 }\n",
        ),
    ] {
        let result = s.update_file("a.rock", text).expect(stage);
        assert_ne!(result.tier, UpdateTier::NoChange);
        assert_builtins_valid(&s, stage);
        assert_counts(&s, stage);
    }
    s.rebuild_users().expect("full rebuild");
    assert_builtins_valid(&s, "rebuild_users");
    assert_counts(&s, "rebuild_users");
}

#[test]
fn failed_rebuild_users_keeps_workspace_consistent() {
    let mut s = session();
    assert_eq!(s.ws.modules.len(), s.ws.module_ids.len());
    let good_len = s.ws.modules.len();

    s.files.get_mut("b.rock").unwrap().text =
        "global dup: int = 1\nglobal dup: int = 2\n".to_string();
    assert!(
        s.rebuild_users().is_err(),
        "a duplicate global fails the rebuild"
    );

    assert_eq!(s.ws.modules.len(), s.ws.module_ids.len());
    assert_eq!(s.ws.modules.len(), good_len);
    for &mid in s.ws.module_ids.values() {
        let _ = s.ws.module(mid);
    }
}

#[test]
fn failed_file_changes_restore_text_and_tracking_order() {
    let mut session = common::session(&[
        ("a.rock", "pub mod a;\npub fn f() -> int { 1 }\n"),
        ("b.rock", "pub use a::f;\n"),
        ("c.rock", "fn c() -> int { f() }\n"),
    ]);
    let paths = session.paths.clone();
    let texts: FxHashMap<_, _> = session
        .files
        .iter()
        .map(|(path, file)| (path.clone(), file.text.clone()))
        .collect();
    let diagnostics = session.diagnostics();
    assert!(session.remove_file("a.rock").is_err());
    assert!(
        session
            .update_file("b.rock", "pub use missing::f;\n")
            .is_err()
    );
    assert!(
        session
            .update_file("new.rock", "use missing::f;\n")
            .is_err()
    );
    assert_eq!(session.paths, paths);
    assert_eq!(session.files.len(), texts.len());
    for (path, text) in texts {
        assert_eq!(session.files[&path].text, text);
        assert_eq!(
            session.update_file(&path, &text).unwrap().tier,
            UpdateTier::NoChange
        );
    }
    assert_eq!(
        format!("{:?}", session.diagnostics()),
        format!("{diagnostics:?}")
    );
}
