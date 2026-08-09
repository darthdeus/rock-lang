//! A specialized generic callee must be REACHED on JS.
//!
//! `compute_call_spec` decides specialization per target and says yes for a
//! column-eligible element on JS. That decision is a CORRECTNESS one, not an
//! optimization: for a column-stored element the shared generic body reads a
//! `_data` the layout does not have. So the emitter must resolve such a call to
//! the instantiation's own standalone copy (`spec_fn_name`) exactly as the
//! luajit emitter's `callee_name` does, and a call whose specialization was not
//! emitted must reject rather than fall back to the boxed shared body.
//!
//! `sort` reaches the leaves through `__rock_col_sort_leaves`, which now has a
//! JS body too; it rejected here for as long as that body was missing, which was
//! the correct answer -- falling back to the boxed body died on
//! `undefined._data`.

use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;

use crate::common::ws_with;

const SRC: &str = r#"
nobox value NV2 { x: float, y: float }

fn sorted() -> float {
    let a: [NV2] = [NV2 { x: 3.0, y: 1.0 }, NV2 { x: 1.0, y: 2.0 }]
    a.sort(_.x)
    // `y` is the column the key never reads, so returning it is what catches a
    // reorder that moved the key column alone. Sorted: y = 2 then 1 -> 21.
    return a[0].y * 10.0 + a[1].y
}

fn pick<T>(xs: [T], i: int, f: fn(T) -> float) -> float {
    return f(xs[i])
}

fn specialized() -> int {
    let a: [NV2] = [NV2 { x: 3.0, y: 1.0 }, NV2 { x: 1.0, y: 2.0 }]
    let x = pick(a, 1, _.x)
    let y = pick(a, 0, _.y)
    return int(x) * 10 + int(y)
}

fn main() { let _ = specialized() }
"#;

fn node_available() -> bool {
    crate::common::node().arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

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

/// Control: the literal DOES take the column layout on JS, so the assertions
/// below are pinning the CALL and not column storage at large (stage A works).
#[test]
fn a_nobox_literal_still_columns_on_js() {
    let js = emit_js("specialized").expect("emit");
    assert!(
        js.contains("\"@vec\"") && js.contains("columns"),
        "the Vec literal should emit column storage on JS:\n{js}"
    );
}

/// The call resolves to the instantiation's own body, not the shared generic
/// one. Without spec-aware resolution the emitted call names the boxed body,
/// which reads a `_data` a column vec does not have.
#[test]
fn a_column_element_call_reaches_its_specialized_body() {
    let js = emit_js("specialized").expect("emit");
    assert!(
        js.contains("_spec_"),
        "the column-element call must target a specialized body:\n{js}"
    );
    assert!(
        js.contains("function pick_arr_T_int_fn_T_ret_float_ret_float_spec_"),
        "expected a standalone specialized `pick` definition:\n{js}"
    );
}

/// Emitted shape cannot tell a specialized body from a mis-lowered one, so RUN
/// it: two calls at two sites, each reading a different leaf at a different
/// index, so a column mixup or an off-by-one is a wrong number rather than a
/// crash.
#[test]
fn a_specialized_column_call_round_trips_under_node() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let defs = emit_js("specialized").expect("emit");
    let program = format!(
        "{}{}{}{}{}\n{defs}\nconsole.log(specialized());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_TABLE_PRIM,
        skull_codegen_js::JS_CALL_HELPER,
        skull_codegen_js::JS_STRUCTURAL_EQ,
    );
    let out = crate::common::node().arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert_eq!(s.trim(), "11", "wrong result\n{program}");
}

/// `sort` over a column element, mechanism G: the key is applied to the LEAVES
/// through `__rock_col_sort_leaves`. JS used to reject here for want of that
/// body -- correctly, since resolving to the shared boxed body instead is the
/// soundness hole `nobox` exists to prevent, and it died on `undefined._data`.
/// The body now exists, so this is a round-trip.
///
/// It RUNS rather than checking emitted shape, and it returns the `y` column,
/// which the key never reads: a sort that reorders the key column alone still
/// emits column-shaped code and reads back as a plausible wrong number.
#[test]
fn a_column_sort_round_trips_on_js() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let defs = emit_js("sorted").expect("column-stored nobox sort must emit on JS");
    let program = format!(
        "{}{}{}{}{}\n{defs}\nconsole.log(sorted());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_TABLE_PRIM,
        skull_codegen_js::JS_CALL_HELPER,
        skull_codegen_js::JS_STRUCTURAL_EQ,
    );
    let out = crate::common::node().arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "21",
        "the non-key column did not follow the sort\n{program}"
    );
}
