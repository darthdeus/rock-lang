//! Misc scalar-intrinsic emission (T8d): tuple_first/tuple_second (Lua
//! `tuple[1]`/`[2]` -> the 0-based JS array `[0]`/`[1]`), __range (materialized
//! `a..b` -> a Vec-shaped runtime object), is_release_mode (-> false; the JS
//! harness has no release mode). Node-executed checks are the correctness
//! oracle; `tests/js_misc_intrinsics.rock` cross-checks against luajit.

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
        "{}{}{}\n{defs}\nconsole.log({root}());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_STRING_RUNTIME,
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

// tuple_first / tuple_second project the 0-based array (Lua 1-based [1]/[2]).
#[test]
fn tuple_projections() {
    let src = "fn root() -> int { let t = (10, 3); tuple_first(t) * 10 + tuple_second(t) }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("tuple emits");
    assert!(js.contains("[0]") && js.contains("[1]"), "0-based projection:\n{js}");
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

// is_release_mode is always false in the JS harness.
#[test]
fn is_release_mode_false() {
    let src = "fn root() -> int { if is_release_mode() { 1 } else { 0 } }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("release emits");
    eval_int(&js, "root", 0);
}
