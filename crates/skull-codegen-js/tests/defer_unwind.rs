//! Ordinary-fn `defer` unwinding on JS: a body that registers a defer catches
//! whatever escapes it and unwinds the cleanups still registered above the
//! depth it was entered at. A body without `defer` carries none of that.
use crate::common::{node, ws_with};

const FIXTURE: &str = r#"
@nomangle inline(js) fn raised(f: fn()) -> bool = """
    try { __rock_call(f); return false; } catch (e) { return true; }
"""

fn plain(x: int) -> int {
    if x > 0 {
        return x * 2
    }
    return 0
}

fn guarded(x: int) -> int {
    defer { print("g") }
    if x > 100 {
        assert(false, "too big")
    }
    return plain(x) + 1
}

fn main() {
    let v = guarded(1)
    let r = raised(() => {
        let _x = guarded(1000)
    })
    print("{v} {r}")
}
"#;

fn program() -> skull_codegen_js::emit::JsProgram {
    let ws = ws_with(FIXTURE);
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    skull_codegen_js::emit::emit_program(&ws, mid)
        .unwrap_or_else(|e| panic!("emit_program rejected: {}", e.message))
}

/// The emitted `function <name>...(` definition, up to the next top-level one.
fn function_body<'a>(js: &'a str, name: &str) -> &'a str {
    let start = js
        .match_indices("function ")
        .map(|(i, _)| i)
        .find(|&i| js[i + "function ".len()..].starts_with(name))
        .unwrap_or_else(|| panic!("no function `{name}` in:\n{js}"));
    let rest = &js[start..];
    let end = rest.find("\n}\n").map_or(rest.len(), |e| e + 3);
    &rest[..end]
}

#[test]
fn defer_fn_unwinds_what_escapes_it() {
    let prog = program();
    let body = function_body(&prog.program, "guarded");
    assert!(
        body.contains("var __rock_defer_base = __rock_defer_top;")
            && body.contains("__rock_defer_unwind(__rock_defer_base, __rock_e)"),
        "a fn registering a defer must unwind its cleanups on error:\n{body}"
    );
    assert!(
        body.contains("__rock_defer_push") && body.contains("__rock_defer_pop"),
        "the defer must register its cleanup and pop it at scope exit:\n{body}"
    );
}

#[test]
fn fn_without_defer_has_no_unwind_machinery() {
    let prog = program();
    let body = function_body(&prog.program, "plain");
    assert!(
        !body.contains("__rock_defer") && !body.contains("try {"),
        "a fn without defer must not pay for the unwind:\n{body}"
    );
}

#[test]
fn defer_runs_on_normal_exit_and_on_error() {
    let prog = program();
    let mut js = skull_codegen_js::assemble_prelude(
        &prog.program,
        prog.has_code_refs(),
        skull_codegen_js::JS_NODE_HOST,
        &[],
    );
    js.push_str(&prog.program);
    let main = prog.main_js_name.as_deref().expect("program has a main");
    js.push_str(&format!("\n{main}();\n"));
    let output = node().arg("-e").arg(js).output().expect("run node");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "g\ng\n3 true\n");
}
