//! Host-bodied string-op emission (T8d). string.find / substring / unit_at /
//! upper / lower / string_char get hand-written JS bindings (indexOf / slice /
//! charCodeAt / toUpperCase / toLowerCase / fromCharCode); the Layer-2 string
//! algorithms (split / replace / trim ...) are pure Rock over them and co-emit.
//! `find`'s omitted `from` param uses the declared default 0 at the call site.
//! Node-executed checks are the correctness oracle;
//! `tests/strings/js_string_primitives.rock` cross-checks against luajit.

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
        "{}{}\n{defs}\nconsole.log({root}());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
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

// find with the default `from` omitted: the declared default is 0.
#[test]
fn find_default_from_padded() {
    let src = "fn root() -> int { \"hello\".find(\"l\") }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("find emits");
    assert!(js.contains(".indexOf(\"l\", 0)"), "pads default:\n{js}");
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

// pad_left/pad_right fill to `width` with spaces and never truncate.
#[test]
fn pad_left_and_right() {
    let src = "fn root() -> string { join([\"ab\".pad_left(4), \"ab\".pad_right(4), \"abcde\".pad_left(3)], \"|\") }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("pad emits");
    eval_str(&js, "root", "  ab|ab  |abcde");
}

// unit_at returns the ASCII code, 0 out of bounds.
#[test]
fn unit_at_ascii_and_oob() {
    let src = "fn root() -> int { \"e\".unit_at(0) * 1000 + \"e\".unit_at(9) }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("unit_at emits");
    eval_str(&js, "root", "101000"); // 'e' = 101, oob = 0
}
