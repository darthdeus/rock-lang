//! Logging emission (T8d): the log_once / log_throttled dedup+throttle surface
//! (stdlib/logging.rock `@nomangle inline(lua)` impls) maps to JS runtime helpers
//! of the same name; `__stone_emit` prints `[category] msg` into the shared
//! output buffer. Node execution is the correctness oracle; `tests/js_log.rock`
//! cross-checks against luajit.

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

// Emit `root`, run it under node with the log + string runtimes and an `__G`,
// capturing the `[cat] msg` lines `print` collects; assert the joined buffer.
fn eval_log(src: &str, expect: &str) {
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("log emits");
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let program = format!(
        "{}{}{}\nvar __G = {{}};\nvar __buf = [];\nfunction print() {{ var p = []; for (var i=0;i<arguments.length;i++) p.push(lua_tostring(arguments[i])); __buf.push(p.join(\"\\t\") + \"\\n\"); }}\n{js}\nroot();\nprocess.stdout.write(JSON.stringify(__buf.join(\"\").replace(/^\\s+/, \"\").replace(/\\s+$/, \"\")));",
        skull_codegen_js::JS_STRING_RUNTIME,
        skull_codegen_js::JS_CALL_HELPER,
        skull_codegen_js::JS_LOG_RUNTIME,
    );
    let out = Command::new("node").arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Compare the JSON-encoded buffer (avoids a serde dep): the expected string
    // with `\n` -> `\\n` inside JSON double-quotes.
    let want = format!("\"{}\"", expect.replace('\n', "\\n"));
    let got = String::from_utf8_lossy(&out.stdout);
    assert_eq!(got.trim(), want, "log output\n{program}");
}

// log_once dedupes a single call site inside a loop.
#[test]
fn log_once_dedupes_in_loop() {
    eval_log(
        "fn root() { log_reset_dedup(); log_set_level(0); for _i in 0..50 { log_once(\"ONCE\", \"hit\") } }",
        "[ONCE] hit",
    );
}

// reset clears dedup so the same explicit site fires again.
#[test]
fn log_reset_refires() {
    eval_log(
        "fn root() { log_reset_dedup(); log_set_level(0); log_once(\"X\", \"hi\", \"k\"); log_reset_dedup(); log_once(\"X\", \"hi\", \"k\") }",
        "[X] hi\n[X] hi",
    );
}

// throttle suppresses within the interval, fires at the boundary.
#[test]
fn log_throttle_interval() {
    eval_log(
        "fn root() { log_reset_dedup(); log_set_level(0); \
         log_throttled_with_now(0.0, \"T\", \"tick\", 1.0, \"k\"); \
         log_throttled_with_now(0.5, \"T\", \"tick\", 1.0, \"k\"); \
         log_throttled_with_now(1.0, \"T\", \"tick\", 1.0, \"k\") }",
        "[T] tick\n[T] tick",
    );
}
