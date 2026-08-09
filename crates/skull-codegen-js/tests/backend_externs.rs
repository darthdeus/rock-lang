//! stone-web arc 1: JS-side emission of per-backend decls.
//!
//! - a same-name `inline(lua)`+`inline(js)` pair emits the JS body VERBATIM and
//!   never the lua body (the collapse + resolve_funcref backend swap picks the JS
//!   variant);
//! - `extern(js)` emits a bare driver-/runtime-provided call, no definition;
//! - a lua-only non-host-mapped fn reached from JS still rejects.
use crate::common::ws_with;


fn emit_program_js(ws: &bone_semantic::inference::Workspace) -> Result<String, String> {
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    skull_codegen_js::emit::emit_program(ws, mid)
        .map(|p| p.program)
        .map_err(|e| e.message)
}

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
    let prog = emit_program_js(&ws).expect("emit js program");
    assert!(prog.contains("return x + x"), "js body must emit verbatim:\n{prog}");
    assert!(!prog.contains("return x * 2"), "lua body must NOT leak into js:\n{prog}");
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
    let prog = emit_program_js(&ws).expect("emit js program");
    assert!(prog.contains("return x * 3"), "js sibling body must win over the Any rep:\n{prog}");
    assert!(!prog.contains("x + x"), "portable Any body must NOT emit under js:\n{prog}");
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
    let prog = emit_program_js(&ws).expect("emit js program");
    assert!(prog.contains("js_now("), "extern(js) must emit a bare call:\n{prog}");
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
    let err = emit_program_js(&ws).expect_err("lua-only call from js must reject");
    assert!(
        err.contains("helper") && err.contains("not a mapped JS intrinsic"),
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
    let prog = emit_program_js(&ws).expect("emit js program");
    assert!(
        !prog.contains("function error("),
        "the runtime throwing `error` global must not be shadowed:\n{prog}"
    );
}
