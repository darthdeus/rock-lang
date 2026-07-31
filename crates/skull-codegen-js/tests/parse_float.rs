//! parse_float emission (T8d). `string.parse_float` -> `__parse_float`, a strict
//! whole-string numeric parse reproducing Lua `tonumber` (empty/whitespace ->
//! null, non-numeric tail -> null, else the parsed double). The node-executed
//! checks are the correctness oracle; `tests/js_parse_float.rock` cross-checks
//! against luajit.

use std::process::Command;

use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;
use crate::common::ws_with;

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

fn eval_str(defs: &str, root: &str, expect: &str) {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let program = format!("{}\n{defs}\nconsole.log({root}());", skull_codegen_js::JS_STRING_RUNTIME);
    let out = Command::new("node").arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert_eq!(s.trim_end_matches('\n'), expect, "wrong result\n{program}");
}

// A valid numeric string parses to its double.
#[test]
fn parses_valid_number() {
    let src = "fn root() -> string { let r = \"1.5\".parse_float(); if r == null { \"nil\" } else { tostring(r) } }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("parse_float emits");
    assert!(js.contains("__parse_float"), "renders the strict parse:\n{js}");
    eval_str(&js, "root", "1.5");
}

// A non-numeric tail is nil (NaN -> null), NOT a lenient prefix parse.
#[test]
fn non_numeric_is_null() {
    let src = "fn root() -> string { let r = \"12abc\".parse_float(); if r == null { \"nil\" } else { tostring(r) } }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("parse_float emits");
    eval_str(&js, "root", "nil");
}

// The empty string is nil (Lua tonumber), NOT 0 (JS Number).
#[test]
fn empty_is_null_not_zero() {
    let src = "fn root() -> string { let r = \"\".parse_float(); if r == null { \"nil\" } else { tostring(r) } }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("parse_float emits");
    eval_str(&js, "root", "nil");
}
