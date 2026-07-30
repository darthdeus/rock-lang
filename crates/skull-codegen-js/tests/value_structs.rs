//! GC-free value-struct emission (T8b). An all-scalar `value` struct used as a
//! WHOLE value (a `MakeValueStruct` that survives cleanup -- `==`, a Vec element,
//! a boxed return) emits as a plain `{__ty, ...}` object; its structural deep-eq
//! (`JS_STRUCTURAL_EQ`) reproduces luajit's byte-value equality for all-scalar
//! leaves. A GC-carrying value struct rejects (its pointer fields compare by
//! REFERENCE on luajit, which a generic contents-eq cannot reproduce). The
//! node-executed checks are the correctness oracle.

use std::process::Command;

use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;

mod common;
use common::ws_with;

fn node_available() -> bool {
    Command::new("node").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn emit_closure(ws: &bone_semantic::inference::Workspace, root: &str) -> Result<String, String> {
    let scx = ScalarCtx::new(ws);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let mut r = None;
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == root {
            r = Some(FuncRef { module: mid, func: fid });
        }
    }
    skull_codegen_js::emit::emit_with_closure(ws, &scx, &[(r.unwrap(), root.to_string())])
        .map_err(|e| e.message)
}

fn eval_int(defs: &str, root: &str, expect: i64) {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let program = format!(
        "{}{}{}{}\n{defs}\nconsole.log({root}());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_TABLE_PRIM,
        skull_codegen_js::JS_STRUCTURAL_EQ,
    );
    let out = Command::new("node").arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    let got: i64 = s.trim().parse().unwrap_or_else(|_| panic!("not an int: {s:?}\n{program}"));
    assert_eq!(got, expect, "wrong result\n{program}");
}

// Two distinct constructions with equal contents compare EQUAL, unequal contents
// compare UNEQUAL -- structural deep-eq matching luajit byte-value eq.
#[test]
fn all_scalar_value_struct_eq_by_contents() {
    let src = "value V { x: int, y: int }\n\
               fn root() -> int {\n\
                   let a = V { x: 1, y: 2 }\n\
                   let b = V { x: 1, y: 2 }\n\
                   let c = V { x: 1, y: 3 }\n\
                   let eq = if a == b { 1 } else { 0 }\n\
                   let ne = if a == c { 1 } else { 0 }\n\
                   eq * 10 + ne\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("all-scalar value struct emits");
    assert!(js.contains("__ty: \"V\""), "value struct emits a `{{__ty, ...}}` object:\n{js}");
    eval_int(&js, "root", 10); // a==b -> 1, a==c -> 0
}

// A nested all-scalar value struct emits nested `{__ty, ...}` objects and its
// deep-eq is by contents at every level.
#[test]
fn nested_value_struct_eq() {
    let src = "value V { x: int, y: int }\n\
               value Line { a: V, b: V }\n\
               fn root() -> int {\n\
                   let p = Line { a: V { x: 1, y: 2 }, b: V { x: 3, y: 4 } }\n\
                   let q = Line { a: V { x: 1, y: 2 }, b: V { x: 3, y: 4 } }\n\
                   if p == q { 1 } else { 0 }\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("nested value struct emits");
    eval_int(&js, "root", 1);
}

// A GC-carrying value struct (a boxed string field) now emits (T8g): the
// per-type `__rock_vt_eq` registry supplies the field split -- a `string` field
// compares BY REFERENCE (luajit interns; JS `===` is by-value), a scalar field
// structurally. Two boxes with equal string + scalar compare EQUAL.
#[test]
fn gc_carrying_value_struct_eq() {
    let src = "value W { s: string, n: int }\n\
               fn root() -> int {\n\
                   let a = W { s: \"hi\", n: 1 }\n\
                   let b = W { s: \"hi\", n: 1 }\n\
                   if a == b { 1 } else { 0 }\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("GC-carrying value struct emits (T8g)");
    assert!(
        js.contains("__rock_vt_eq[\"W\"] = function(a, b) { return a.s === b.s && __op_eq(a.n, b.n); };"),
        "emits the W registry entry (string ref, scalar structural):\n{js}"
    );
    eval_int(&js, "root", 1);
}

/// A `nobox` return crosses the JS call boundary as LEAVES, not as anything the
/// guarantee forbids. This replaced a blanket rejection, so it asserts the
/// property that rejection was protecting rather than the rejection itself: the
/// value must not materialize as an object OR as a transport array (which is an
/// allocation just the same, and does not vanish under V8's escape analysis --
/// measured in docs/js-nobox-return-abi-plan.md).
#[test]
fn nobox_value_return_crosses_as_leaves_not_a_box() {
    let src = "nobox value V { x: int, y: int }\n\
               fn mk() -> V { V { x: 1, y: 2 } }\n\
               fn root() -> int {\n\
                   let v = mk()\n\
                   return v.x * 10 + v.y\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("a nobox return emits under the scratch ABI");
    assert!(!js.contains("\"V\""), "nobox value materialized an object:\n{js}");
    assert!(!js.contains("return ["), "nobox return allocated a transport array:\n{js}");
    assert!(js.contains("__rock_ret1 = "), "leaf 1 must ride the scratch bank:\n{js}");
    eval_int(&js, "root", 12);
}
