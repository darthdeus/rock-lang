//! The host seam: every generated-JS touch of the outside world (stdout,
//! stderr, environment, RNG seed) goes through the embedder's `__rock_host`.
//! A browser Worker has no `process`, so each check here runs the runtime in a
//! node `vm` context that has none, and the node default is pinned separately.

use crate::common::{node, ws_with};

/// Run `body` in a fresh `vm` context holding only the JS builtins plus a
/// capturing `__rock_host`; prints `JSON.stringify({out, err})`.
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
process.stdout.write(vm.runInContext("JSON.stringify({{out: __out.join(''), err: __err.join('')}})", ctx));
"#,
        body = js_string(body)
    );
    let output = node().arg("-e").arg(script).output().expect("run node");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    (field(&stdout, "out"), field(&stdout, "err"))
}

fn js_string(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// The string value of `"name":"..."` in the harness's flat JSON line.
fn field(json: &str, name: &str) -> String {
    let key = format!("\"{name}\":\"");
    let start = json
        .find(&key)
        .unwrap_or_else(|| panic!("no `{name}` in {json}"))
        + key.len();
    let mut out = String::new();
    let mut chars = json[start..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return out,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => break,
            },
            c => out.push(c),
        }
    }
    panic!("unterminated `{name}` in {json}");
}

/// A failing coroutine defer is reported on the host's error stream, never
/// stdout, and needs no `process`.
#[test]
fn coroutine_defer_failure_goes_to_the_host_error_stream() {
    let body = format!(
        "{}{}\n__rock_coroutine_run_defers({{ __defers: [function() {{ throw new Error('boom'); }}] }});",
        skull_codegen_js::JS_FOREIGN_CALL_HELPER,
        skull_codegen_js::JS_CORO_RUNTIME,
    );
    let (out, err) = run_without_process(&body);
    assert_eq!(out, "");
    assert_eq!(err, "coroutine defer failed during cleanup: boom\n");
}

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
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let prog = skull_codegen_js::emit::emit_program(&ws, mid)
        .unwrap_or_else(|e| panic!("emit_program rejected: {}", e.message));
    let mut js = skull_codegen_js::assemble_prelude(&prog.program, prog.has_code_refs(), host, &[]);
    js.push_str(&prog.program);
    let main = prog.main_js_name.as_deref().expect("program has a main");
    js.push_str(&format!("\n{main}();\n"));
    js
}

/// The assembled prelude runs a whole program in a context with no `process`:
/// print/put reach the host's stdout, `env_var` and the RNG seed read the
/// host's environment.
#[test]
fn a_whole_program_runs_against_an_embedder_host() {
    let (out, err) = run_without_process(&whole_program(""));
    assert_eq!(err, "");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(&lines[..3], &["a\t1\t2.5", "raw", "5"], "{out}");
    // Seeded from the host's ROCK_TEST_SEED, so the draw matches the node
    // default given the same seed.
    let node_out = run_node_default("5");
    assert_eq!(out, node_out);
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

/// The node default writes to stdout and seeds from `ROCK_TEST_SEED`: equal
/// seeds draw equally, different seeds differ.
#[test]
fn the_node_host_prints_and_seeds_from_the_environment() {
    let a = run_node_default("11");
    assert!(a.starts_with("a\t1\t2.5\nraw\n11\n"), "{a}");
    assert_eq!(a, run_node_default("11"));
    assert_ne!(a, run_node_default("12"));
}
