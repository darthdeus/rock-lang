//! The host seam: every generated-JS touch of the outside world (stdout,
//! stderr, environment, RNG seed) goes through the embedder's `__rock_host`.
//! A browser Worker has no `process`, so each check here runs the runtime in a
//! node `vm` context that has none, and the node default is pinned separately.

use crate::common::{node, ws_with};

/// Run `body` in a fresh `vm` context holding only the JS builtins plus a
/// capturing `__rock_host`; prints `JSON.stringify([out, err])`.
fn run_without_process(body: &str) -> (String, String) {
    let script = format!(
        r#"
var vm = require("vm");
var ctx = vm.createContext({{}});
vm.runInContext("var __out = [], __err = [];\n"
  + "var __rock_host = {{ write: function(s) {{ __out.push(s); }},"
  + " write_err: function(s) {{ __err.push(s); }},"
  + " env_var: function(n) {{ return n === 'ROCK_TEST_SEED' ? '5' : null; }} }};\n", ctx);
vm.runInContext({body}, ctx);
process.stdout.write(vm.runInContext("JSON.stringify([__out.join(''), __err.join('')])", ctx));
"#,
        body = skull_codegen_js::emit::js_string_lit(body)
    );
    serde_json::from_str(&crate::common::run_node(&script)).expect("decode captured streams")
}

/// The coroutine and log runtimes plus a program global table, then `tail`.
fn with_log_runtime(tail: &str) -> String {
    format!(
        "{}{}{}\nvar __G = {{}};\n{tail}",
        skull_codegen_js::JS_FOREIGN_CALL_HELPER,
        skull_codegen_js::JS_CORO_RUNTIME,
        skull_codegen_js::JS_LOG_RUNTIME,
    )
}

const FAILING_DEFER: &str =
    "__rock_coroutine_run_defers({ __defers: [function() { throw new Error('boom'); }] });\n";

const PROGRAM: &str = r#"
@intrinsic @inline inline(lua) fn put(s: string) = "io.write(s)"

fn main() {
    print("a", 1, 2.5)
    put("raw")
    put("\n")
    print(env_var("ROCK_TEST_SEED") ?? "unset")
    print(random_int(0, 1000000))
}
"#;

fn whole_program(host: &str) -> String {
    let ws = ws_with(PROGRAM);
    let mid = crate::common::test_mid(&ws);
    let prog = skull_codegen_js::emit::emit_program(&ws, mid)
        .unwrap_or_else(|e| panic!("emit_program rejected: {}", e.message));
    let mut js = skull_codegen_js::assemble_prelude(&prog.program, prog.has_code_refs(), host, &[]);
    js.push_str(&prog.program);
    let main = prog.main_js_name.as_deref().expect("program has a main");
    js.push_str(&format!("\n{main}();\n"));
    js
}

fn run_node_default(seed: &str) -> String {
    let output = node()
        .env("ROCK_TEST_SEED", seed)
        .arg("-e")
        .arg(whole_program(skull_codegen_js::JS_NODE_HOST))
        .output()
        .expect("run node");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}
