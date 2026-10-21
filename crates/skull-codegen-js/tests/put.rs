//! `put` emission (T8d): the test-local `@intrinsic @inline inline(lua) fn put(s: string) =
//! "io.write(s)"` idiom maps to a `put($1)` call the harness routes into the same
//! per-test output buffer as `print` -- raw, no trailing newline. Node execution
//! is the correctness oracle; `tests/js_put.rock` cross-checks against luajit.

use skull_mir::mir::FuncRef;
use crate::common::ws_with;

fn node_available() -> bool {
    crate::common::node().arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn emit_closure(ws: &bone_semantic::inference::Workspace, root: &str) -> Result<String, String> {
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let mut r = None;
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == root {
            r = Some(FuncRef { module: mid, func: fid });
        }
    }
    skull_codegen_js::emit::emit_with_closure(ws, &[(r.unwrap(), root.to_string())])
        .map_err(|e| e.message)
}

// `put` appends its argument to the shared buffer raw (no newline); the joined
// buffer is the exact output.
#[test]
fn put_writes_raw_no_newline() {
    let src = "@intrinsic @inline inline(lua) fn put(s: string) = \"io.write(s)\"\n\
               fn root() { put(\"a\"); put(\"b\"); put(\"\\n\"); put(\"c\") }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("put emits");
    assert!(js.contains("put("), "put renders as a call:\n{js}");
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let program = format!(
        "{}{}\nvar __buf = [];\nfunction put(s) {{ __buf.push(s); }}\n{js}\nroot();\nprocess.stdout.write(JSON.stringify(__buf.join(\"\")));",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_STRING_RUNTIME,
    );
    let out = crate::common::node().arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got = String::from_utf8_lossy(&out.stdout);
    assert_eq!(got.trim(), "\"ab\\nc\"", "raw buffer content\n{program}");
}
