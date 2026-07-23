//! A same-name inline(js)/extern(js) sibling is a BACKEND VARIANT, not an
//! overload: the Lua backend can never emit it, so it must not flip the shared
//! name into the mangled scheme. Pinned because adding web siblings for
//! previously-unique stdlib/engine fns silently renamed every Lua definition
//! and call site (caught by the match3 byte-identity gate, stone-web arc 2).

mod common;
use common::emit_module;

#[test]
fn inline_js_sibling_does_not_mangle_lua_name() {
    let lua = emit_module(
        r#"
fn dbl(x: int) -> int { return x + x }
inline(js) fn dbl(x: int) -> int = """return x * 3"""

fn use_it() -> int { return dbl(21) }
"#,
    );
    assert!(
        lua.contains("function dbl(") && !lua.contains("dbl__int_ret_int"),
        "a js sibling must not mangle the lua emission:\n{}",
        &lua[lua.find("function dbl").map(|i| i.saturating_sub(200)).unwrap_or(0)..][..400.min(lua.len())]
    );
}

#[test]
fn real_lua_overload_still_mangles() {
    let lua = emit_module(
        r#"
fn pick(x: int) -> int { return x }
fn pick(x: float) -> float { return x }

fn use_both() -> float { return float(pick(1)) + pick(2.0) }
"#,
    );
    assert!(
        lua.contains("pick__int_ret_int") && lua.contains("pick__float_ret_float"),
        "true overloads must still mangle:\n{lua}"
    );
}
