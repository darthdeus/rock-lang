//! The scratch-slot bank must cover every writer, including the ones MIR cannot
//! see.
//!
//! `max_multi_wire` sizes `var __rock_ret1..N` from MIR. An `inline(js)` body is
//! an opaque STRING copied out verbatim, and the imgui stubs legitimately publish
//! `__rock_ret1`/`__rock_ret2` by hand, so MIR alone under-counts the moment such
//! a body writes past the wire its own declared return implies.
//!
//! The miss is silent by construction: the emitted program is sloppy-mode, so an
//! undeclared write becomes an implicit global and everything keeps working --
//! until the program is ever evaluated as strict/module code, where the same
//! write is a ReferenceError.
//!
//! `typeof` cannot express this assertion (a declared-but-unassigned `var` also
//! reads "undefined"), which is why the shape check below looks for the
//! declaration itself.

use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;

mod common;
use common::ws_with;

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
/// declares nothing -- yet the body writes a slot.
#[test]
fn a_hand_published_slot_is_declared() {
    let js = emit(
        r#"
@nomangle inline(js) fn publish() -> int = "__rock_ret1 = 777; return 1;"
fn main() { let _ = publish() }
"#,
        "main",
    );
    assert!(
        js.contains("var __rock_ret1"),
        "an `inline(js)` body published __rock_ret1 and the bank never declared it:\n{js}"
    );
}

/// Past the declared arity too -- the shape no real body has today, and the one
/// the MIR-only sizing could never have covered.
#[test]
fn a_slot_past_the_declared_wire_is_still_declared() {
    let js = emit(
        r#"
@nomangle inline(js) fn publish_wide() -> int = "__rock_ret4 = 5; return 1;"
fn main() { let _ = publish_wide() }
"#,
        "main",
    );
    assert!(
        js.contains("var __rock_ret1") && js.contains("__rock_ret4"),
        "the bank must span up to the widest hand-published slot:\n{js}"
    );
}

/// The control: no hand-published slot and no multi-value wire means no bank at
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
