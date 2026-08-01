//! Host-bodied string-op emission (T8d). string.find / substring / unit_at /
//! upper / lower / string_char get hand-written JS bindings (indexOf / slice /
//! charCodeAt / toUpperCase / toLowerCase / fromCharCode); the Layer-2 string
//! algorithms (split / replace / trim ...) are pure Rock over them and co-emit.
//! `find`'s defaulted `from` param is padded to `undefined` at the call site
//! (indexOf reads that as 0). Node-executed checks are the correctness oracle;
//! `tests/js_string_primitives.rock` cross-checks against luajit.

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
    let program = format!(
        "{}{}\n{defs}\nconsole.log({root}());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
    );
    let out = Command::new("node").arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert_eq!(s.trim_end_matches('\n'), expect, "wrong result\n{program}");
}

// find with the default `from` omitted: padded to `undefined`, read as 0.
#[test]
fn find_default_from_padded() {
    let src = "fn root() -> int { \"hello\".find(\"l\") }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("find emits");
    assert!(js.contains(".indexOf(") && js.contains("undefined"), "pads default:\n{js}");
    eval_str(&js, "root", "2");
}

// find with an explicit `from`.
#[test]
fn find_explicit_from() {
    let src = "fn root() -> int { \"abcabc\".find(\"a\", 1) }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("find emits");
    eval_str(&js, "root", "3");
}

// substring is a 0-based end-exclusive slice.
#[test]
fn substring_slice() {
    let src = "fn root() -> string { \"hello\".substring(1, 4) }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("substring emits");
    eval_str(&js, "root", "ell");
}

// The Layer-2 `split` co-emits over find/substring; joined here for a scalar
// oracle.
#[test]
fn split_co_emits() {
    let src = "fn root() -> string { join(\"a,b,c\".split(\",\"), \"|\") }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("split emits");
    eval_str(&js, "root", "a|b|c");
}

// unit_at returns the ASCII code, 0 out of bounds.
#[test]
fn unit_at_ascii_and_oob() {
    let src = "fn root() -> int { \"e\".unit_at(0) * 1000 + \"e\".unit_at(9) }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("unit_at emits");
    eval_str(&js, "root", "101000"); // 'e' = 101, oob = 0
}
