//! Column storage (`docs/vec-value-columns-plan.md`) is a luajit-only emitter
//! arm, so `[V2]` lowering has to ask the TARGET before choosing it.
//!
//! The two halves are decided differently on purpose. For an UNMARKED value
//! struct columns are a representation OPTIMIZATION, so a backend without the
//! arm keeps the box -- same semantics, different bytes. For a `nobox` one they
//! are a language-level GUARANTEE, so the column form is still chosen and JS
//! rejects it: silently handing back the box a program was promised would not
//! exist is the failure this pins against.
//!
//! Without the target check every game broke on `rock web`: `[Vec2]`-shaped
//! storage is everywhere, so the whole-program emit hit either the missing
//! `MakeVecColumns` arm or a scalar-ABI mismatch at a `push` whose specialized
//! callee only Lua emits.

use skull_mir::lower::{lower_function, ScalarCtx};
use skull_mir::mir::FuncRef;

mod common;
use common::ws_with;

const SRC: &str = r#"
value V2 { x: float, y: float }
nobox value NV2 { x: float, y: float }

fn unmarked() -> float {
    let xs: [V2] = []
    xs.push(V2 { x: 1.0, y: 2.0 })
    return xs[0].x
}

fn marked() -> float {
    let xs: [NV2] = []
    xs.push(NV2 { x: 1.0, y: 2.0 })
    return xs[0].x
}

fn main() { let _ = unmarked() }
"#;

fn emit_for(target: bone_semantic::hir::FnBackend, name: &str) -> Result<String, String> {
    let ws = ws_with(SRC);
    let scx = ScalarCtx::new(&ws).with_target(target);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .map(|(fid, _)| fid)
        .expect("fn in <test>");
    if target == bone_semantic::hir::FnBackend::Js {
        // Whole-closure emission, not a single body: `push` is a stdlib callee,
        // and emitting the root alone rejects it as an unmapped intrinsic long
        // before the storage question is reached.
        return skull_codegen_js::emit::emit_with_closure(
            &ws,
            &scx,
            &[(FuncRef { module: mid, func: fid }, name.to_string())],
        )
        .map_err(|e| e.message);
    }
    // Lua's own emission is covered by `skull/tests/vec_value_columns.rs`; here
    // the MIR text is enough to show the same source DID take columns.
    let mf = lower_function(&ws, &scx, mid, fid)
        .unwrap_or_else(|e| panic!("lower `{name}` for {target:?}: {}", e.message));
    Ok(mf.to_string())
}

/// The control: the same source still columns for luajit. Without this the JS
/// assertions below would pass just as well if columns had stopped happening
/// everywhere, which is the vacuity class this whole area keeps producing.
#[test]
fn an_unmarked_element_still_columns_for_lua() {
    let mir = emit_for(bone_semantic::hir::FnBackend::Lua, "unmarked").expect("lua lowering");
    assert!(
        mir.contains("VecColumn"),
        "[V2] stopped taking column storage on the luajit target:\n{mir}"
    );
}

/// The regression: JS keeps the boxed representation and EMITS. A rejection
/// here is `rock web` broken for every game.
#[test]
fn an_unmarked_element_stays_boxed_for_js() {
    let js = emit_for(bone_semantic::hir::FnBackend::Js, "unmarked")
        .expect("[V2] must emit on JS by keeping the boxed representation");
    assert!(
        js.contains("__vec_checked_get"),
        "expected the boxed Vec accessor in the emitted JS:\n{js}"
    );
}

/// The other half: a `nobox` element is a GUARANTEE, so it columns on every
/// target and JS rejects rather than silently boxing. The message must name the
/// element and say why, so the reader is not left diffing MIR dumps.
#[test]
fn a_nobox_element_rejects_on_js_instead_of_boxing() {
    let msg = emit_for(bone_semantic::hir::FnBackend::Js, "marked")
        .expect_err("a `nobox` element must not silently take the boxed representation on JS");
    assert!(
        msg.contains("NV2") && msg.contains("nobox"),
        "the rejection must name the element and the marker that forced columns: {msg}"
    );
}
