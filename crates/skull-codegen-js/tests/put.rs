//! `put` emission (T8d): the test-local `@intrinsic @inline inline(lua) fn put(s: string) =
//! "io.write(s)"` idiom maps to a `put($1)` call the harness routes into the same
//! per-test output buffer as `print` -- raw, no trailing newline. Node execution
//! is the correctness oracle; `tests/library/js_put.rock` cross-checks against luajit.

use crate::common::ws_with;

// `put` appends its argument to the shared buffer raw (no newline); the joined
// buffer is the exact output.
#[test]
fn put_writes_raw_no_newline() {
    let src = "@intrinsic @inline inline(lua) fn put(s: string) = \"io.write(s)\"\n\
               fn root() { put(\"a\"); put(\"b\"); put(\"\\n\"); put(\"c\") }";
    let ws = ws_with(src);
    let js = crate::common::emit_closure(&ws, "root").expect("put emits");
    assert!(js.contains("put("), "put renders as a call:\n{js}");
    if !crate::common::require_node() {
        return;
    }
    let program = format!(
        "{}{}\nvar __buf = [];\nfunction put(s) {{ __buf.push(s); }}\n{js}\nroot();\nprocess.stdout.write(JSON.stringify(__buf.join(\"\")));",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_STRING_RUNTIME,
    );
    let out = crate::common::run_node(&program);
    let got = out;
    assert_eq!(got.trim(), "\"ab\\nc\"", "raw buffer content\n{program}");
}
