//! The scratch-slot bank must cover every writer, including the ones MIR cannot
//! see.
//!
//! `max_multi_wire` sizes `var __rock_ret1..N` from MIR. An `inline(js)` body is
//! an opaque STRING copied out verbatim, and a multi-lane one publishes lanes of
//! its own, so the bank is sized over those too -- from the body's concrete
//! result ABI (`max_published_lanes`), never from the body text.
//!
//! The miss it prevents is silent by construction: the emitted program is
//! sloppy-mode, so an undeclared write becomes an implicit global and everything
//! keeps working -- until the same program is evaluated as strict/module code,
//! where that write is a ReferenceError.
//!
//! `typeof` cannot express this assertion (a declared-but-unassigned `var` also
//! reads "undefined"), which is why the shape check below looks for the
//! declaration itself.

use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;

use crate::common::ws_with;

fn emit(src: &str, root: &str) -> String {
    let ws = ws_with(src);
    let scx = ScalarCtx::new(&ws).with_target(bone_semantic::hir::FnBackend::Js);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == root)
        .map(|(fid, _)| fid)
        .expect("root fn");
    skull_codegen_js::emit::emit_with_closure(
        &ws,
        &scx,
        &[(FuncRef { module: mid, func: fid }, root.to_string())],
    )
    .unwrap_or_else(|e| panic!("emit: {}", e.message))
}

/// The program's widest MIR wire is 1 (nothing multi-value), so MIR sizing alone
/// declares nothing -- yet a 2-lane `inline(js)` body publishes a slot.
#[test]
fn a_published_lane_is_declared() {
    let js = emit(
        r#"
@nomangle inline(js) fn publish(v: int) -> (bool, int) = "return __rock_publish(false, v);"
fn main() { let (ok, n) = publish(1) }
"#,
        "main",
    );
    assert!(
        js.contains("var __rock_ret1"),
        "an `inline(js)` body published lane 1 and the bank never declared it:\n{js}"
    );
}

/// Wider than any MIR wire in the program: a 3-lane result declares two slots.
#[test]
fn a_wide_published_result_declares_every_lane() {
    let js = emit(
        r#"
nobox value P2 { x: float, y: float }
@nomangle @scalar_abi inline(js) fn publish_wide(p: P2) -> (bool, P2) =
    "return __rock_publish(false, p__x, p__y);"
fn main() { let (ok, p) = publish_wide(P2 { x: 1.0, y: 2.0 }) }
"#,
        "main",
    );
    assert!(
        js.contains("var __rock_ret1, __rock_ret2"),
        "the bank must span every ABI lane the publisher carries:\n{js}"
    );
}

/// **The arm's own population**: a publisher emitted with NO Rock call site.
/// Every CALLED publisher is already covered by the MIR wire its caller
/// captures, so this is the only shape in which the ABI-derived half of the
/// sizing decides anything -- without it the arm would be untestable and dead.
#[test]
fn a_publisher_with_no_mir_caller_is_still_declared() {
    // Reached the way the web boot seam reaches its targets: named in another
    // opaque body's TEXT, so it is emitted with no MIR call site to size from.
    let js = emit(
        r#"
@nomangle inline(js) fn lone_publisher(v: int) -> (bool, int) = "return __rock_publish(false, v);"
@nomangle inline(js) fn seam() -> int = "lone_publisher(1); return 0;"
fn main() { let _ = seam() }
"#,
        "main",
    );
    assert!(
        js.contains("var __rock_ret1"),
        "a publisher with no MIR call site left its lane undeclared:\n{js}"
    );
}

/// **The bank is not sized by the body TEXT.** A slot named in a comment widens
/// nothing -- and writing one directly rejects, which is the legacy mechanism
/// this sizing replaced.
#[test]
fn body_text_neither_widens_the_bank_nor_writes_it() {
    let js = emit(
        r#"
@nomangle inline(js) fn commented() -> int = "// not a lane: __rock_ret7
return 1;"
fn main() { let _ = commented() }
"#,
        "main",
    );
    assert!(!js.contains("var __rock_ret"), "a commented slot name sized a bank:\n{js}");

    let ws = ws_with(
        r#"
@nomangle inline(js) fn hand_published() -> int = "__rock_ret1 = 7; return 1;"
fn main() { let _ = hand_published() }
"#,
    );
    let scx = ScalarCtx::new(&ws).with_target(bone_semantic::hir::FnBackend::Js);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "main")
        .map(|(fid, _)| fid)
        .expect("root fn");
    let e = skull_codegen_js::emit::emit_with_closure(
        &ws,
        &scx,
        &[(FuncRef { module: mid, func: fid }, "main".to_string())],
    )
    .expect_err("a hand-published slot must reject");
    assert!(e.message.contains("scratch bank"), "unexpected rejection: {}", e.message);
}

/// The control: no published lane and no multi-value wire means no bank at
/// all. Without it the assertions above would pass just as well if the emitter
/// had started declaring slots unconditionally.
#[test]
fn a_program_with_no_wire_declares_no_bank() {
    let js = emit(
        r#"
@nomangle inline(js) fn plain() -> int = "return 1;"
fn main() { let _ = plain() }
"#,
        "main",
    );
    assert!(!js.contains("var __rock_ret"), "no wire, so no bank was expected:\n{js}");
}
