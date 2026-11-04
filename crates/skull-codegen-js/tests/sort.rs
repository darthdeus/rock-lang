//! Vec.sort emission (T8d). The stdlib inline(lua) `table.sort(_data, |a,b|
//! key(a) < key(b))` maps to the `__vec_sort` runtime: sort the logical
//! `0..len` slice in place (0-based hole-free `_data`), key_fn through
//! `__rock_call`. The node-executed checks are the correctness oracle;
//! `tests/js_sort.rock` cross-checks against luajit (tie order is unspecified,
//! so only order-invariant properties are asserted).

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

fn eval_str(defs: &str, root: &str, expect: &str) {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let program = format!(
        "{}{}{}{}\n{defs}\nconsole.log({root}());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_CALL_HELPER,
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
    assert_eq!(s.trim_end_matches('\n'), expect, "wrong result\n{program}");
}

// Ascending sort by the identity key over a 0-based hole-free `_data` (a stray
// index-0 `[null,` hole would corrupt the readback).
#[test]
fn sort_ascending_by_identity() {
    let src = "fn root() -> string {\n\
                   let xs = [5, 3, 8, 1, 2]\n\
                   xs.sort((x: int) => float(x))\n\
                   let parts: [string] = []\n\
                   for x in xs { parts.push(tostring(x)) }\n\
                   join(parts, \" \")\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("sort emits");
    assert!(
        js.contains("__vec_sort"),
        "renders the slice-sort runtime:\n{js}"
    );
    assert!(
        !js.contains("[null,"),
        "0-based hole-free Vec literal, no index-0 hole:\n{js}"
    );
    eval_str(&js, "root", "1 2 3 5 8");
}

// A derived key (negation) sorts descending -- the key_fn actually runs.
#[test]
fn sort_descending_by_derived_key() {
    let src = "fn root() -> string {\n\
                   let xs = [5, 3, 8, 1, 2]\n\
                   xs.sort((x: int) => float(-x))\n\
                   let parts: [string] = []\n\
                   for x in xs { parts.push(tostring(x)) }\n\
                   join(parts, \" \")\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("sort emits");
    eval_str(&js, "root", "8 5 3 2 1");
}
