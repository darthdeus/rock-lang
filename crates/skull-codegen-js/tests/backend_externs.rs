//! JS-side emission of per-backend decls.
//!
//! - a same-name `inline(lua)`+`inline(js)` pair emits the JS body VERBATIM and
//!   never the lua body (the collapse + resolve_funcref backend swap picks the JS
//!   variant);
//! - `extern(js)` emits a bare driver-/runtime-provided call, no definition;
//! - a lua-only non-host-mapped fn reached from JS still rejects.
use crate::common::{node, ws_with};

#[test]
fn inline_js_body_emits_verbatim_not_lua() {
    let ws = ws_with(
        r#"
inline(lua) fn dbl(x: int) -> int = """return x * 2"""
inline(js) fn dbl(x: int) -> int = """return x + x"""

fn main() {
    let a = dbl(21)
    print("{a}")
}
"#,
    );
    let prog = crate::common::emit_program_js(&ws).expect("emit js program");
    assert!(
        prog.contains("return x + x"),
        "js body must emit verbatim:\n{prog}"
    );
    assert!(
        !prog.contains("return x * 2"),
        "lua body must NOT leak into js:\n{prog}"
    );
}

#[test]
fn portable_body_plus_inline_js_sibling_picks_js() {
    // P1: a portable Rock-bodied fn (backend Any) + a same-signature inline(js)
    // sibling. The Any body serves every target, so it is the collapse
    // representative; under the JS target codegen must still pick the js sibling.
    let ws = ws_with(
        r#"
fn dbl(x: int) -> int { return x + x }
inline(js) fn dbl(x: int) -> int = """return x * 3"""

fn main() {
    let a = dbl(21)
    print("{a}")
}
"#,
    );
    let prog = crate::common::emit_program_js(&ws).expect("emit js program");
    assert!(
        prog.contains("return x * 3"),
        "js sibling body must win over the Any rep:\n{prog}"
    );
    assert!(
        !prog.contains("x + x"),
        "portable Any body must NOT emit under js:\n{prog}"
    );
}

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
fn lua_only_fn_reached_from_js_rejects() {
    // `helper` is a plain inline(lua) with no js sibling and no host mapping ->
    // calling it from a js-reachable path must reject.
    let ws = ws_with(
        r#"
inline(lua) fn helper(x: int) -> int = """return x * 2"""

fn main() {
    let a = helper(21)
    print("{a}")
}
"#,
    );
    let err = crate::common::emit_program_js(&ws).expect_err("lua-only call from js must reject");
    assert!(
        err.contains("helper") && err.contains("not a mapped JS intrinsic"),
        "unexpected reject message: {err}"
    );
}

#[test]
fn an_unmapped_ffi_call_still_rejects() {
    let ws = ws_with(
        r#"
extern(ffi, "host") fn host_save_screenshot(path: string, width: int, height: int, max_size: int) -> bool

fn main() {
    let ok = host_save_screenshot("shot.png", 1, 1, 1)
    print("{ok}")
}
"#,
    );
    let err = crate::common::emit_program_js(&ws).expect_err("an unmapped FFI call must reject");
    assert!(
        err.contains("host_save_screenshot") && err.contains("not a mapped JS intrinsic"),
        "unexpected reject message: {err}"
    );
}

#[test]
fn coroutine_reraise_does_not_emit_lua_error_extern() {
    let ws = ws_with(
        r#"
co fn inner() -> Coroutine<int> {
    yield 1
    error("boom")
}

co fn outer() -> Coroutine<int> {
    await inner()
}

fn main() {
    let c = outer()
    c.resume()
    c.resume()
}
"#,
    );
    let prog = crate::common::emit_program_js(&ws).expect("emit js program");
    assert!(
        !prog.contains("function error("),
        "the runtime throwing `error` global must not be shadowed:\n{prog}"
    );
}

#[test]
fn debug_traceback_uses_the_declared_js_sibling() {
    let ws = ws_with(
        r#"
fn main() {
    print(__debug_traceback())
}
"#,
    );
    let prog = crate::common::emit_program_js(&ws).expect("emit js program");
    assert!(
        prog.contains("return new Error().stack || \"\";"),
        "the declared JS traceback body must own the JS implementation:\n{prog}"
    );
    let source = format!(
        "var __G = {{}};\nfunction print(value) {{ console.log(value); }}\n{}\n{}\nmain();",
        skull_codegen_js::JS_GLOBALS_RUNTIME,
        prog,
    );
    let output = node().args(["-e", &source]).output().expect("node");
    assert!(
        output.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with("Error\n"),
        "the JS traceback was empty: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

// ---------------------------------------------------------------------------
// A host-bodied decl taken as a fn VALUE
//
// The call path already refused a lua-only `extern`; the fn-value path did not,
// and lowered the declaration as if it were portable Rock. The emitted program
// then referenced a body that was never emitted, so the call answered
// `undefined` -- a wrong ANSWER rather than an unsupported program, which is
// what made `tests/closures/extern_fn_value.rock` report 7 failures instead of 7 skips.
//
// Both sides are asserted here: the shapes that must reject, and a portable Rock
// fn through the SAME path that must still be admitted. Rejecting everything
// would satisfy the first half alone.

#[test]
fn a_lua_only_extern_cannot_be_taken_as_a_fn_value() {
    // Every route to a fn value, because they are separate arms: a local, a
    // module global, through a container, and as a call argument.
    for body in [
        r#"fn main() { let g = rep; print("{g("ab", 2)}") }"#,
        r#"global R: (string, int) -> string = rep
fn main() { let g = R; print("{g("ab", 2)}") }"#,
        r#"global RS: [(string, int) -> string] = [rep]
fn main() { let g = RS[0]; print("{g("ab", 2)}") }"#,
        r#"fn apply(f: (string, int) -> string) -> string { return f("ab", 2) }
fn main() { print("{apply(rep)}") }"#,
    ] {
        let src = format!("extern(ffi, \"string\") fn rep(s: string, n: int) -> string\n{body}\n");
        let ws = ws_with(&src);
        let err = crate::common::emit_program_js(&ws)
            .expect_err(&format!("must reject a lua-only extern as a value:\n{src}"));
        assert!(
            err.contains("rep")
                && (err.contains("cannot be used as a function value")
                    || err.contains("ArtifactView")
                    || err.contains("no MIR fn-value record")),
            "rejection should name the fn and why, got: {err}"
        );
    }
}

#[test]
fn a_bare_extern_cannot_be_taken_as_a_fn_value() {
    // A bare `extern` names an existing LUA global, so it has no JS binding
    // either -- and it reaches a different arm than `extern(ffi, ..)`.
    let ws = ws_with(
        r#"
extern(lua) fn rawequal(a: int, b: int) -> bool
fn main() { let g = rawequal; print("{g(7, 7)}") }
"#,
    );
    let err =
        crate::common::emit_program_js(&ws).expect_err("must reject a bare extern as a value");
    assert!(
        err.contains("rawequal"),
        "rejection should name the fn, got: {err}"
    );
}

#[test]
fn an_inline_lua_body_cannot_be_taken_as_a_fn_value() {
    let ws = ws_with(
        r#"
inline(lua) fn twice(a: int) -> int = """return a * 2"""
fn main() { let g = twice; print("{g(3)}") }
"#,
    );
    let err = crate::common::emit_program_js(&ws)
        .expect_err("must reject an inline(lua) body as a value");
    assert!(
        err.contains("twice"),
        "rejection should name the fn, got: {err}"
    );
}

#[test]
fn a_portable_fn_is_still_admitted_as_a_fn_value() {
    // The control. This travels the same `register_fn_value_variant` path the
    // rejections above take, so a rejection that fired on everything would show
    // up here rather than passing quietly.
    let ws = ws_with(
        r#"
fn dbl(x: int) -> int { return x + x }
fn apply(f: (int) -> int) -> int { return f(21) }
fn main() { print("{apply(dbl)}") }
"#,
    );
    let prog =
        crate::common::emit_program_js(&ws).expect("a portable fn must remain usable as a value");
    assert!(
        prog.contains("dbl"),
        "the portable body should be emitted:\n{prog}"
    );
}

#[test]
fn an_inline_js_sibling_makes_the_fn_value_admissible() {
    // The rejection is about a missing JS BINDING, not about being host-bodied:
    // give the same lua-only decl a JS sibling and the value must be admitted.
    // Without this arm the rejection could be keyed on `inline_body.is_some()`
    // and nobody would notice.
    let ws = ws_with(
        r#"
inline(lua) fn twice(a: int) -> int = """return a * 2"""
inline(js) fn twice(a: int) -> int = """return a + a"""
fn apply(f: (int) -> int) -> int { return f(3) }
fn main() { print("{apply(twice)}") }
"#,
    );
    let prog =
        crate::common::emit_program_js(&ws).expect("an inline(js) sibling must admit the value");
    assert!(
        prog.contains("return a + a"),
        "js sibling body must emit:\n{prog}"
    );
}
