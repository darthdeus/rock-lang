//! Lowering error recovery. Lowering is a TOTAL function ("plan A",
//! `docs/bone-total-lowering-plan.md`): an unresolved type name in ANY position
//! -- body, signature, field, global -- resolves to `BuiltinTy::Error` and
//! records a diagnostic; `semantic_pass` returns `Ok` with a coherent workspace,
//! never aborting. This supersedes the earlier "plan C" boundary (recover
//! bodies, fail-fast interfaces) -- see the plan doc for why (the camera crash
//! `88fc6e0bd` exposed plan C's cross-module gap). Session-side cascade +
//! convergence: `incremental.rs`.

use bone_semantic::prelude::*;
use bone_incremental::session::Session;

fn parse(src: &str) -> skull_parser::Ast {
    skull_parser::parse_module(src, Ustr::from("<error_recovery>")).expect("parse")
}

/// All lowering + inference diagnostics in the workspace (the two sinks
/// `diagnostics()` reads). An interface-position type error must show up here.
fn diags_of(ws: &Workspace) -> Vec<String> {
    ws.inferences_iter()
        .flat_map(|(_, inf)| inf.errors.values().flatten())
        .chain(ws.modules.iter().flat_map(|m| m.lowering_errors.values().flatten()))
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter()))
        .map(|e| e.message.clone())
        .collect()
}

fn ws_of(src: &str) -> (Workspace, ModuleId) {
    let ws = semantic_pass(&mut [parse(src)]).expect("a body lowering error must not abort the pass");
    let mid = ws
        .modules
        .iter()
        .map(|m| m.id)
        .find(|m| *m != ModuleId::builtin())
        .expect("user module survives");
    (ws, mid)
}

// A function whose BODY fails to lower keeps its signature (so goto-def /
// references / signature-help still see it), and sibling functions are
// untouched. Body errors are module-local, so recovering them is safe.
#[test]
fn body_lowering_error_recovers_function_with_signature() {
    let src = "fn broken() -> int {\n    let v: Nonexistent = 0\n    1\n}\nfn healthy() -> int { 3 }";
    let (ws, mid) = ws_of(src);
    let m = &ws.modules[mid.index()];

    assert!(
        m.functions_by_name.contains_key(&Ustr::from("broken")),
        "the broken-body fn keeps its signature in the module"
    );
    assert!(
        m.functions_by_name.contains_key(&Ustr::from("healthy")),
        "a sibling fn is untouched by a body lowering error"
    );
}

// A SIGNATURE type error recovers: the param type lowers to `Error`, the pass
// returns Ok, the function keeps its signature, the sibling is untouched, and
// the error surfaces as a diagnostic. (Total lowering -- the cascade in
// incremental.rs then re-lowers dependents instead of the pass fail-fasting.)
#[test]
fn signature_lowering_error_recovers() {
    let src = "fn broken(p: Nonexistent) -> int { 1 }\nfn healthy() -> int { 3 }";
    let ws = semantic_pass(&mut [parse(src)])
        .expect("an interface type error must recover, not abort the pass");
    let mid = ws.modules.iter().map(|m| m.id).find(|m| *m != ModuleId::builtin()).expect("user module");
    let m = &ws.modules[mid.index()];
    assert!(m.functions_by_name.contains_key(&Ustr::from("broken")), "broken keeps its signature");
    assert!(m.functions_by_name.contains_key(&Ustr::from("healthy")), "sibling untouched");
    assert!(
        diags_of(&ws).iter().any(|d| d.contains("Nonexistent")),
        "the unresolved param type surfaces as a diagnostic: {:?}",
        diags_of(&ws)
    );
}

// A struct FIELD type error recovers the same way: the field lowers to `Error`,
// the pass returns Ok, the diagnostic surfaces, the sibling fn is fine.
#[test]
fn field_lowering_error_recovers() {
    let src = "struct Bad { v: Nonexistent }\nfn f() -> int { 1 }";
    let ws = semantic_pass(&mut [parse(src)])
        .expect("a struct-field type error must recover, not abort the pass");
    let mid = ws.modules.iter().map(|m| m.id).find(|m| *m != ModuleId::builtin()).expect("user module");
    assert!(
        ws.modules[mid.index()].functions_by_name.contains_key(&Ustr::from("f")),
        "the sibling fn survives a field type error"
    );
    assert!(
        diags_of(&ws).iter().any(|d| d.contains("Nonexistent")),
        "the unresolved field type surfaces as a diagnostic: {:?}",
        diags_of(&ws)
    );
}

// A GLOBAL's type annotation referencing an unknown type recovers: lowers to
// `Error`, surfaces a diagnostic, siblings fine. Globals are interface (their
// type, not their value), so this is the exact cross-module case the cascade
// must handle.
#[test]
fn global_unknown_type_recovers() {
    let src = "global g: Nonexistent = 0\nfn f() -> int { 1 }";
    let ws = semantic_pass(&mut [parse(src)])
        .expect("a global's unknown type must recover, not abort the pass");
    let mid = ws.modules.iter().map(|m| m.id).find(|m| *m != ModuleId::builtin()).expect("user module");
    assert!(
        ws.modules[mid.index()].functions_by_name.contains_key(&Ustr::from("f")),
        "the sibling fn survives a global type error"
    );
    assert!(
        diags_of(&ws).iter().any(|d| d.contains("Nonexistent")),
        "the unresolved global type surfaces as a diagnostic: {:?}",
        diags_of(&ws)
    );
}

// Convergence (the cardinal invariant): break a SIGNATURE type, then fix it; no
// stale diagnostics may remain. Trivial under fail-fast (revert wiped it);
// non-trivial under total lowering -- the edited module is rebuilt fresh, so the
// `Error` node and its diagnostic must clear.
#[test]
fn interface_error_then_fix_converges() {
    let broken = "fn f(p: Nonexistent) -> int { 1 }\nfn g() -> int { 2 }";
    let fixed = "fn f(p: int) -> int { p }\nfn g() -> int { 2 }";

    let mut s = Session::new(vec![], vec![]).expect("session");
    let _ = s.update_file("c.rock", broken);
    assert!(!s.diagnostics().is_empty(), "broken signature reports the error");

    let _ = s.update_file("c.rock", fixed);
    assert!(
        s.diagnostics().is_empty(),
        "no stale diagnostics after the signature is fixed: {:?}",
        s.diagnostics()
    );
}

// A recovered body error must surface as a diagnostic (never swallowed) AND the
// session must COMMIT the partial state (function present), not revert -- body
// errors are local, so there is nothing unsafe to keep.
#[test]
fn recovered_body_error_surfaces_and_commits() {
    let src = "fn f() -> int {\n    let v: Nonexistent = 0\n    1\n}";
    let mut s = Session::new(vec![], vec![]).expect("session");
    let _ = s.update_file("c.rock", src);

    assert!(
        !s.diagnostics().is_empty(),
        "the recovered body error must surface as a diagnostic"
    );

    let ws = s.workspace();
    let mid = ws
        .modules
        .iter()
        .map(|m| m.id)
        .find(|m| *m != ModuleId::builtin())
        .expect("the module is committed (body errors do not revert)");
    assert!(
        ws.modules[mid.index()].functions_by_name.contains_key(&Ustr::from("f")),
        "f is present with its signature after body recovery"
    );
}

// Convergence -- the trust test. Recover a body error, then fix it; no stale
// diagnostics may remain (the edited module is rebuilt fresh every edit).
#[test]
fn body_error_then_fix_converges() {
    let broken = "fn f() -> int {\n    let v: Nonexistent = 0\n    1\n}\nfn g() -> int { 2 }";
    let fixed = "fn f() -> int { 1 }\nfn g() -> int { 2 }";

    let mut s = Session::new(vec![], vec![]).expect("session");
    let _ = s.update_file("c.rock", broken);
    assert!(!s.diagnostics().is_empty(), "broken edit reports the error");

    let _ = s.update_file("c.rock", fixed);
    assert!(
        s.diagnostics().is_empty(),
        "no stale diagnostics after the body is fixed: {:?}",
        s.diagnostics()
    );
}
