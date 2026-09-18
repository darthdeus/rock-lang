//! `rebuild_users` must be ATOMIC: a rebuild that fails (parse/expand/lower/infer)
//! must leave `self.ws` untouched (last-good), never half-built.
//!
//! `expand_and_desugar` / `lower_and_infer` intern modules into `ws.module_ids`
//! well before `insert_module` grows the `ws.modules` vec, so a mid-build `Err`
//! used to leave the workspace with `module_ids` running ahead of `modules`. Any
//! later `ws.module(mid)` then panicked `index out of bounds: the len is 1 but
//! the index is N`. This bit the LSP when typing a malformed `global x =` at the
//! top of a multi-module file: every keystroke failed the rebuild AND its
//! one-step revert target (the previous, still-broken keystroke) failed the
//! recovery rebuild too, so the corrupted workspace stuck around and the next
//! query crashed (`bone-query/tests/repro_camera_crash.rs` is the end-to-end
//! companion).

use bone_incremental::session::{Session, SessionFile};
use bone_semantic::prelude::*;

const WARM: &str = "
fn __op_add(a: int, b: int) -> int { 0 }
fn __op_sub(a: int, b: int) -> int { 0 }
";

fn warm() -> Vec<skull_parser::Ast> {
    vec![skull_parser::parse_module_clean(WARM, Ustr::from("<warm>")).expect("warm parses")]
}

#[test]
fn failed_rebuild_users_keeps_workspace_consistent() {
    // A valid multi-module program.
    let files = vec![
        SessionFile::legacy("a.rock", "struct S { v: int }\n"),
        SessionFile::legacy("b.rock", "fn use_s(s: S) -> int { s.v }\n"),
    ];
    let mut session = Session::new(warm(), files, &bone_core::modules::NsAliases::default()).expect("clean program builds");

    assert_eq!(
        session.ws.modules.len(),
        session.ws.module_ids.len(),
        "a healthy session has a module for every interned id"
    );
    let good_len = session.ws.modules.len();

    // Inject a DUPLICATE global. Type-resolution errors now recover (total
    // lowering), so they no longer fail the rebuild; a duplicate definition is a
    // genuine fail-fast that hits in `register_globals` -- AFTER
    // `expand_and_desugar` has interned the modules but BEFORE `insert_module`
    // grows `modules`. That is the exact interned-then-failed path that left the
    // workspace half-built (`module_ids` ahead of `modules`) before the atomic
    // rebuild_users fix.
    session.files.get_mut("b.rock").unwrap().text =
        "global dup: int = 1\nglobal dup: int = 2\n".to_string();

    let result = session.rebuild_users();
    assert!(result.is_err(), "a duplicate global must fail rebuild_users");

    assert_eq!(
        session.ws.modules.len(),
        session.ws.module_ids.len(),
        "a FAILED rebuild_users must not leave the workspace half-built \
         (modules.len={} module_ids.len={})",
        session.ws.modules.len(),
        session.ws.module_ids.len(),
    );
    assert_eq!(
        session.ws.modules.len(),
        good_len,
        "a failed rebuild_users keeps the last-good workspace, not a stub"
    );

    // The last-good workspace is still queryable -- every interned module
    // resolves without panicking.
    for (_, &mid) in session.ws.module_ids.iter() {
        let _ = session.ws.module(mid);
    }
}

/// The host resolves build variables once, onto the session's workspace
/// (`bone_session_for_emission`). A full rebuild replaces that workspace, so it
/// has to carry them over or emission after the rebuild sees none.
#[test]
fn rebuild_users_keeps_the_comptime_inputs() {
    let files = vec![SessionFile::legacy("a.rock", "fn f() -> int { 1 }\n")];
    let mut session = Session::new(warm(), files, &bone_core::modules::NsAliases::default()).expect("clean program builds");
    session.ws.comptime_inputs.channel = "dev".to_string();
    session.ws.comptime_inputs.modules_mode = "v1".to_string();

    session.rebuild_users().expect("an unchanged program rebuilds");

    assert_eq!(session.ws.comptime_inputs.channel, "dev");
    assert_eq!(session.ws.comptime_inputs.modules_mode, "v1");
}
