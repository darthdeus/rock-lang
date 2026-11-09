//! Logging emission (T8d): the log_once / log_throttled dedup+throttle surface
//! (stdlib/logging.rock `@nomangle inline(lua)` impls) maps to JS runtime helpers
//! of the same name; `__rock_log_emit` prints `[category] msg` into the shared
//! output buffer. Node execution is the correctness oracle; `tests/library/js_log.rock`
//! cross-checks against luajit.

use crate::common::ws_with;

// Emit `root`, run it under node with the log + string runtimes and an `__G`,
// capturing the `[cat] msg` lines `print` collects; assert the joined buffer.
fn eval_log(src: &str, expect: &str) {
    let ws = ws_with(src);
    let js = crate::common::emit_closure(&ws, "root").expect("log emits");
    if !crate::common::require_node() {
        return;
    }
    let program = format!(
        "{}{}{}\nvar __G = {{}};\nvar __buf = [];\nfunction print() {{ var p = []; for (var i=0;i<arguments.length;i++) p.push(lua_tostring(arguments[i])); __buf.push(p.join(\"\\t\") + \"\\n\"); }}\n{js}\nroot();\nprocess.stdout.write(JSON.stringify(__buf.join(\"\").replace(/^\\s+/, \"\").replace(/\\s+$/, \"\")));",
        skull_codegen_js::JS_STRING_RUNTIME,
        skull_codegen_js::JS_CALL_HELPER,
        skull_codegen_js::JS_LOG_RUNTIME,
    );
    let out = crate::common::run_node(&program);
    // Compare the JSON-encoded buffer (avoids a serde dep): the expected string
    // with `\n` -> `\\n` inside JSON double-quotes.
    let want = format!("\"{}\"", expect.replace('\n', "\\n"));
    let got = out;
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

// A warning site obeys the level gate an error site ignores, summary included.
#[test]
fn log_warn_sites_obey_the_level_gate() {
    eval_log(
        "fn root() { log_reset_dedup(); log_set_level(0); \
         log_warn_once(\"W\", \"dropped\", \"wq\"); \
         log_warn_throttled_with_now(0.0, \"WT\", \"dropped\", 1.0, \"wt\"); \
         log_once(\"E\", \"kept\", \"ek\"); \
         log_reset_dedup(); log_set_level(1); \
         log_warn_once(\"W\", \"shown\", \"wk\"); log_warn_once(\"W\", \"shown\", \"wk\"); \
         log_suppression_summary() }",
        "[E] kept\n[W] shown\n[W] suppressed x2 at wk: shown",
    );
}

// Equal counts order by site name, as the luajit summary does, not by insertion.
#[test]
fn log_summary_breaks_count_ties_by_site() {
    eval_log(
        "fn root() { log_reset_dedup(); log_set_level(0); \
         log_once(\"B\", \"b\", \"zz\"); log_once(\"B\", \"b\", \"zz\"); \
         log_once(\"A\", \"a\", \"aa\"); log_once(\"A\", \"a\", \"aa\"); \
         log_suppression_summary() }",
        "[B] b\n[A] a\n[A] suppressed x2 at aa: a\n[B] suppressed x2 at zz: b",
    );
}
