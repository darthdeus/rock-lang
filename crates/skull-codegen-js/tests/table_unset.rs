//! `table_unset` emission (T8d). The stdlib `inline(lua)` primitive `t[k] = nil`
//! backs Map.remove; the Vec drain ops now route through `__vec_data_unset`
//! (`_data[i] = undefined` on the 0-based hole-free array). `__table_unset`
//! dispatches on `instanceof Map` for the Map path. The node-executed checks are
//! the correctness oracle; the corpus `tests/js_table_unset.rock` cross-checks
//! against luajit.

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

// A removed Map key reads back as nil (undefined -> null via the loose rule);
// siblings survive.
#[test]
fn map_remove_deletes_key() {
    let src = "fn root() -> int {\n\
                   let m = map_new()\n\
                   m[1] = 10\n\
                   m[2] = 20\n\
                   m.remove(1)\n\
                   let gone = if m.get(1) == null { 1 } else { 0 }\n\
                   gone * 100 + (m[2])\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("map remove emits");
    assert!(js.contains("__table_unset"), "renders the delete primitive:\n{js}");
    eval_int(&js, "root", 120); // key 1 gone (1), key 2 survives (20)
}

// Vec.pop drains the tail slot; the tracked len shrinks. The `[1,2,3]` literal
// is the 0-based hole-free `_data` (no leading `[null,`).
#[test]
fn vec_pop_drains_tail() {
    let src = "fn root() -> int {\n\
                   let xs = [1, 2, 3]\n\
                   let last = xs.pop() ?? 0\n\
                   last * 10 + xs.len\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("vec pop emits");
    assert!(!js.contains("[null,"), "0-based hole-free Vec literal, no index-0 hole:\n{js}");
    eval_int(&js, "root", 32); // popped 3, len now 2
}

// Map.remove returns the old value.
#[test]
fn map_remove_returns_old() {
    let src = "fn root() -> int {\n\
                   let m = map_new()\n\
                   m[7] = 42\n\
                   (m.remove(7) ?? 0)\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("map remove emits");
    eval_int(&js, "root", 42);
}
