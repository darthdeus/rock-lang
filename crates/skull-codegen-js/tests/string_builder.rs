//! StringBuilder emission (T8d). The three `@intrinsic inline(lua)` accumulator
//! ops map to a JS array: `sb_new` -> `[]`, `sb_push` -> `.push`, `sb_build` ->
//! `.join("")`. `join` (stdlib/strings.rock) is pure Rock over them and
//! co-emits. The node-executed check is the correctness oracle; the corpus
//! `tests/js_string_builder.rock` cross-checks against luajit.

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

// Direct StringBuilder: push several fragments, build the concatenation.
#[test]
fn sb_direct_concat() {
    let src = "fn root() -> string {\n\
                   let sb = sb_new()\n\
                   sb_push(sb, \"foo\")\n\
                   sb_push(sb, \"bar\")\n\
                   sb_build(sb)\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("sb emits");
    assert!(
        js.contains(".push(") && js.contains(".join("),
        "renders array ops:\n{js}"
    );
    eval_str(&js, "root", "foobar");
}

// An empty builder builds the empty string.
#[test]
fn sb_empty_builds_empty() {
    let src = "fn root() -> string {\n\
                   let sb = sb_new()\n\
                   sb_build(sb)\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("sb emits");
    eval_str(&js, "root", "");
}

// `join` co-emits the sb_* ops (it is pure Rock over them).
#[test]
fn join_co_emits_over_sb() {
    let src = "fn root() -> string {\n\
                   let parts = [\"a\", \"b\", \"c\"]\n\
                   join(parts, \", \")\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("join emits");
    eval_str(&js, "root", "a, b, c");
}
