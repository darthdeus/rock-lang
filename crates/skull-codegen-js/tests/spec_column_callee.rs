//! A specialized generic callee must be REACHED on JS.
//!
//! `compute_call_spec` decides specialization per target and says yes for a
//! column-eligible element on JS (`spec_key_has_column_elem` -> true, verified
//! by probe). The Lua emitter honours that decision in `callee_name`, which
//! consults `MirFunc::spec_bindings` and falls back to the shared mangled name
//! only when there is no key. The JS emitter has no such lookup, so it resolves
//! EVERY callee to the shared name and no `_spec_` body is ever emitted.
//!
//! For a column-stored element the shared body is not merely slower, it is
//! wrong: it reads a `_data` the column layout does not have. `nobox` makes
//! that a soundness hole -- the marker's whole promise is that a backend which
//! cannot serve columns REJECTS rather than silently handing back the box.
//! Lua rejects (`cannot sort a column-stored nobox value 'NV2'`); JS emits the
//! boxed call and dies at runtime on `undefined._data`.
//!
//! SENTINEL (docs/test-sentinels.md): the assertions below match the CURRENT
//! broken behaviour. When JS learns spec-aware callee resolution they go red --
//! invert them to require the `_spec_` callee (or a loud reject, matching Lua)
//! and delete this paragraph.

use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;

mod common;
use common::ws_with;

const SRC: &str = r#"
nobox value NV2 { x: float, y: float }

fn sorted() -> float {
    let a: [NV2] = [NV2 { x: 3.0, y: 1.0 }, NV2 { x: 1.0, y: 2.0 }]
    a.sort(_.x)
    return a[0].x
}

fn main() { let _ = sorted() }
"#;

fn emit_js(name: &str) -> Result<String, String> {
    let ws = ws_with(SRC);
    let scx = ScalarCtx::new(&ws).with_target(bone_semantic::hir::FnBackend::Js);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .map(|(fid, _)| fid)
        .expect("fn in <test>");
    skull_codegen_js::emit::emit_with_closure(
        &ws,
        &scx,
        &[(FuncRef { module: mid, func: fid }, name.to_string())],
    )
    .map_err(|e| e.message)
}

/// Control: the literal DOES take the column layout on JS, so the sentinel
/// below is pinning the CALL and not column storage at large (stage A works).
#[test]
fn a_nobox_literal_still_columns_on_js() {
    let js = emit_js("sorted").expect("emit");
    assert!(
        js.contains("\"@vec\"") && js.contains("columns"),
        "the Vec literal should emit column storage on JS:\n{js}"
    );
}

/// SENTINEL. The `sort` call resolves to the SHARED generic body -- no `_spec_`
/// suffix anywhere -- even though `compute_call_spec` returned a key for this
/// instantiation, and the shared body is the boxed `__vec_sort` over `_data`.
#[test]
fn a_column_element_call_reaches_the_shared_body_sentinel() {
    let js = emit_js("sorted").expect("emit");
    assert!(
        !js.contains("_spec_"),
        "JS emitted a specialized body -- the sentinel's bug is fixed, invert it:\n{js}"
    );
    assert!(
        js.contains("__vec_sort("),
        "expected the boxed `__vec_sort` shared-body call:\n{js}"
    );
}
