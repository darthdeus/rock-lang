//! Workspace invariants through a production session lifecycle.
//! `Workspace::new` puts the synthetic `$builtin` module, owner of every
//! builtin type node, in the ordinary module table. Editing, re-lowering,
//! rebuilding and evicting must keep it, and source-module counts and user
//! iteration must leave it out. A failed full rebuild keeps the last-good
//! workspace.

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
    Session::new(warm, files, &bone_core::modules::NsAliases::default()).expect("session builds")
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

/// The builtin module is still the type owner every builtin `TyId` points at.
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
    assert_eq!(ws.source_module_count(), ws.modules.len() - 1, "{stage}");
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
    assert_eq!(s.ws.source_module_count(), 3);

    let body = s
        .update_file("a.rock", "struct S { v: int }\nfn f() -> int { 2 }\n")
        .expect("body edit");
    assert_ne!(body.tier, UpdateTier::NoChange);
    assert_builtins_valid(&s, "body edit");
    assert_counts(&s, "body edit");

    s.update_file(
        "a.rock",
        "struct S { v: int, w: float }\nfn f() -> int { 2 }\n",
    )
    .expect("interface edit");
    assert_builtins_valid(&s, "interface edit");
    assert_counts(&s, "interface edit");

    s.rebuild_users().expect("full rebuild");
    assert_builtins_valid(&s, "rebuild_users");
    assert_counts(&s, "rebuild_users");
}

#[test]
fn eviction_keeps_the_builtin_module() {
    let mut s = session();
    let before = s.ws.inference(ModuleId::builtin()).is_some();
    s.retain_modules(|_| false);
    assert_builtins_valid(&s, "evict all");
    assert_eq!(
        s.ws.inference(ModuleId::builtin()).is_some(),
        before,
        "evicting every module must leave the builtin module's slot alone"
    );
}

/// A failed `rebuild_users` leaves the last-good workspace. Modules are
/// interned into `module_ids` before `insert_module` grows `modules`, so a
/// mid-build error once left `module_ids` ahead of `modules` and the next
/// `ws.module(mid)` panicked out of bounds. A duplicate global fails in
/// `register_globals`, after interning and before insertion.
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

/// The host resolves build variables once onto the session's workspace
/// (`bone_session_for_emission`); a full rebuild replaces that workspace and
/// has to carry them over.
#[test]
fn rebuild_users_keeps_the_comptime_inputs() {
    let mut s = session();
    s.ws.comptime_inputs.channel = "dev".to_string();
    s.ws.comptime_inputs.modules_mode = "v1".to_string();
    s.rebuild_users().expect("an unchanged program rebuilds");
    assert_eq!(s.ws.comptime_inputs.channel, "dev");
    assert_eq!(s.ws.comptime_inputs.modules_mode, "v1");
}
