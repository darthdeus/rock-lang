//! JS-side emission of per-backend decls.
//!
//! - a same-name `inline(lua)`+`inline(js)` pair emits the JS body VERBATIM and
//!   never the lua body (the collapse + resolve_funcref backend swap picks the JS
//!   variant);
//! - `extern(js)` emits a bare driver-/runtime-provided call, no definition;
//! - a lua-only non-host-mapped fn reached from JS still rejects.
use crate::common::ws_with;

#[test]
fn extern_js_emits_bare_call_no_def() {
    let ws = ws_with(
        r#"
extern(js) fn js_now() -> float

fn main() {
    let t = js_now()
    print("{t}")
}
"#,
    );
    let prog = crate::common::emit_program_js(&ws).expect("emit js program");
    assert!(
        prog.contains("js_now("),
        "extern(js) must emit a bare call:\n{prog}"
    );
    assert!(
        !prog.contains("function js_now"),
        "extern(js) must NOT emit a definition (driver/runtime provides it):\n{prog}"
    );
}

#[test]
fn unmapped_backend_declarations_reject_from_js() {
    for (label, source, name) in [
        (
            "lua-only function",
            r#"
inline(lua) fn helper(x: int) -> int = """return x * 2"""
fn main() {
    let a = helper(21)
    print("{a}")
}
"#,
            "helper",
        ),
        (
            "unmapped FFI function",
            r#"
extern(ffi, "host") fn host_save_screenshot(path: string, width: int, height: int, max_size: int) -> bool
fn main() {
    let ok = host_save_screenshot("shot.png", 1, 1, 1)
    print("{ok}")
}
"#,
            "host_save_screenshot",
        ),
    ] {
        let ws = ws_with(source);
        let err =
            crate::common::emit_program_js(&ws).expect_err(&format!("{label} must reject from JS"));
        assert!(
            err.contains(name) && err.contains("not a mapped JS intrinsic"),
            "{label}: unexpected reject message: {err}"
        );
    }
}

#[test]
fn unwrap_failure_uses_the_declared_js_sibling() {
    let ws = ws_with(
        r#"
fn main() {
    let missing: int? = null
    print(missing.unwrap())
}
"#,
    );
    let prog = crate::common::emit_program_js(&ws).expect("emit js program");
    assert!(
        prog.contains("(new Error().stack || \"\")"),
        "the declared JS failure body must own the JS implementation:\n{prog}"
    );
    let source = format!(
        "var __G = {{}};\nfunction print(value) {{ console.log(value); }}\n{}\n{}\n\
         try {{ main(); }} catch (e) {{ console.log(e.message); }}",
        skull_codegen_js::JS_GLOBALS_RUNTIME,
        prog,
    );
    let stdout = crate::common::run_node(&source);
    assert!(
        stdout.starts_with("unwrap() called on null value\nError\n"),
        "the JS failure lost its message or traceback: {stdout:?}"
    );
}

#[test]
fn js_function_values_require_a_target_body() {
    let mut rejected = Vec::new();
    for body in [
        r#"fn main() { let g = rep; print("{g("ab", 2)}") }"#,
        r#"global R: (string, int) -> string = rep
fn main() { let g = R; print("{g("ab", 2)}") }"#,
        r#"global RS: [(string, int) -> string] = [rep]
fn main() { let g = RS[0]; print("{g("ab", 2)}") }"#,
        r#"fn apply(f: (string, int) -> string) -> string { return f("ab", 2) }
fn main() { print("{apply(rep)}") }"#,
    ] {
        rejected.push((
            "FFI function value",
            format!("extern(ffi, \"string\") fn rep(s: string, n: int) -> string\n{body}\n"),
            "rep",
            true,
        ));
    }
    rejected.extend([
        (
            "bare extern function value",
            "extern(lua) fn rawequal(a: int, b: int) -> bool\n\
             fn main() { let g = rawequal; print(\"{g(7, 7)}\") }"
                .to_string(),
            "rawequal",
            false,
        ),
        (
            "inline(lua) function value",
            "inline(lua) fn twice(a: int) -> int = \"\"\"return a * 2\"\"\"\n\
             fn main() { let g = twice; print(\"{g(3)}\") }"
                .to_string(),
            "twice",
            false,
        ),
    ]);
    for (label, source, symbol, needs_reason) in rejected {
        let ws = ws_with(&source);
        let err = crate::common::emit_program_js(&ws).expect_err(&format!("{label} must reject"));
        assert!(
            err.contains(symbol),
            "{label}: rejection should name `{symbol}`, got: {err}"
        );
        if needs_reason {
            assert!(
                err.contains("cannot be used as a function value")
                    || err.contains("ArtifactView")
                    || err.contains("no MIR fn-value record"),
                "{label}: rejection did not explain the missing JS implementation: {err}"
            );
        }
    }

    for (label, source, marker) in [
        (
            "portable Rock function",
            "fn dbl(x: int) -> int { return x + x }\n\
             fn apply(f: (int) -> int) -> int { return f(21) }\n\
             fn main() { print(\"{apply(dbl)}\") }",
            "dbl",
        ),
        (
            "inline(js) sibling",
            "inline(lua) fn twice(a: int) -> int = \"\"\"return a * 2\"\"\"\n\
             inline(js) fn twice(a: int) -> int = \"\"\"return a + a\"\"\"\n\
             fn apply(f: (int) -> int) -> int { return f(3) }\n\
             fn main() { print(\"{apply(twice)}\") }",
            "return a + a",
        ),
    ] {
        let prog = crate::common::emit_program_js(&ws_with(source))
            .unwrap_or_else(|error| panic!("{label} must remain a function value: {error}"));
        assert!(
            prog.contains(marker),
            "{label}: missing `{marker}`:\n{prog}"
        );
    }
}
