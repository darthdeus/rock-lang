//! Misc scalar-intrinsic emission (T8d): tuple_first/tuple_second (Lua
//! `tuple[1]`/`[2]` -> the 0-based JS array `[0]`/`[1]`), __range (materialized
//! `a..b` -> a Vec-shaped runtime object). Node-executed checks are the
//! correctness
//! oracle; `tests/js_misc_intrinsics.rock` cross-checks against luajit.

use crate::common::ws_with;
use skull_mir::mir::FuncRef;

fn node_available() -> bool {
    crate::common::node()
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn emit_closure(ws: &bone_semantic::inference::Workspace, root: &str) -> Result<String, String> {
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let mut r = None;
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == root {
            r = Some(FuncRef {
                module: mid,
                func: fid,
            });
        }
    }
    skull_codegen_js::emit::emit_with_closure(ws, &[(r.unwrap(), root.to_string())])
        .map_err(|e| e.message)
}

fn eval_int(defs: &str, root: &str, expect: i64) {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let program = format!(
        "{}{}{}\n{defs}\nconsole.log({root}());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_STRING_RUNTIME,
    );
    let out = crate::common::node()
        .arg("-e")
        .arg(&program)
        .output()
        .expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    let got: i64 = s
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("not an int: {s:?}\n{program}"));
    assert_eq!(got, expect, "wrong result\n{program}");
}

// tuple_first / tuple_second project the 0-based array (Lua 1-based [1]/[2]).
#[test]
fn tuple_projections() {
    let src = "fn root() -> int { let t = (10, 3); tuple_first(t) * 10 + tuple_second(t) }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("tuple emits");
    assert!(
        js.contains("[0]") && js.contains("[1]"),
        "0-based projection:\n{js}"
    );
    eval_int(&js, "root", 103);
}

// A materialized range carries the right length and elements.
#[test]
fn range_materializes_vec() {
    let src = "fn root() -> int {\n\
                   let r = (2..5).collect()\n\
                   let s = 0\n\
                   for x in r { s = s + x }\n\
                   s * 10 + r.len\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("range emits");
    assert!(js.contains("__range"), "renders the range runtime:\n{js}");
    eval_int(&js, "root", 93); // 2+3+4 = 9, len 3
}

// `tuple_first`/`tuple_second` templates project ONE element of the array, but
// the generic `A`/`B` may itself be a tuple: the call then returns as many lanes
// as that tuple has, and the caller rebuilds the value from them. A lane read
// that goes to the return bank instead of the projected element leaves every
// lane past the first undefined, so a nested tuple element reads as
// `(1, 2, nil)`.
#[test]
fn a_tuple_intrinsic_result_decomposes_into_all_of_its_lanes() {
    let src = "fn root() -> int {\n\
                   let nested = [((1, 2), (3, 4))]\n\
                   let item = nested[0]\n\
                   let left = tuple_first(item)\n\
                   let right = tuple_second(item)\n\
                   return tuple_first(left) * 1000 + tuple_second(left) * 100\n\
                       + tuple_first(right) * 10 + tuple_second(right)\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("nested tuple element emits");
    eval_int(&js, "root", 1234);
}

#[test]
fn a_tuple_intrinsic_rejects_an_unprojectable_nullable_tuple_result() {
    let src = "nobox value NP { x: int, y: int }\n\
               fn root(input: (((NP, int)?, int), int)) -> int {\n\
                   let result = tuple_first(input)\n\
                   return result.1\n\
               }";
    let ws = ws_with(src);
    let err =
        emit_closure(&ws, "root").expect_err("nullable tuple lanes cannot use the return bank");
    assert!(
        err.contains("tuple_first"),
        "the unprojectable intrinsic diagnostic must name its callee: {err}"
    );
}

#[test]
fn a_tuple_intrinsic_rejects_dotted_nobox_leaf_paths() {
    let src = "nobox value Inner { x: int, y: int }\n\
               nobox value Outer { inner: Inner, tag: int }\n\
               @intrinsic inline(lua) fn tuple_first(tuple: ((int, Outer), int)) -> (int, Outer) = \"return tuple[1]\"\n\
               fn root() -> int {\n\
                   let nested = [((1, Outer { inner: Inner { x: 2, y: 3 }, tag: 4 }), 0)]\n\
                   let result = tuple_first(nested[0])\n\
                   return tuple_first(result) + tuple_second(result).inner.x\n\
               }";
    let ws = ws_with(src);
    let err = emit_closure(&ws, "root").expect_err("dotted leaf lanes cannot use the return bank");
    assert!(
        err.contains("tuple_first"),
        "the unprojectable intrinsic diagnostic must name its callee: {err}"
    );
}
