//! An `@inline inline(lua)` call splices its body into the caller. A parameter
//! the body only reads takes the caller's local in place of a copy of it, so a
//! body that declares nothing needs no `do` block. A pure expression the body
//! reads at most once moves to that read. A parameter the body
//! assigns, declares again or captures keeps its own local.

use crate::common::{compile_and_run, emit_module, function_body};

const SRC: &str = r#"
struct Box { n: int }
@inline inline(lua) fn bump(b: Box, by: int) = "b.n = b.n + by"
@inline inline(lua) fn twice(x: int) -> int = "return x .. x"
@inline inline(lua) fn reassign(x: int) -> int = "x = x + 1\nreturn x"
@inline inline(lua) fn scoped(x: int) -> int = "local y = x * 2\nreturn y + x"
@inline inline(lua) fn captured(x: int) -> int = "local f = function() return x end\nreturn f()"
@inline inline(lua) fn half(v: int) -> int = "return v * 2"
@inline inline(lua) fn same(v: int) -> int = "return v"
@inline inline(lua) fn quoted(x: int) -> string = "return 'x' .. tostring(x) -- x"
fn run(b: Box, k: int) -> string {
    bump(b, k)
    bump(b, 1)
    let a = reassign(k)
    let s = scoped(k)
    let c = captured(k)
    let h = half(k + 1)
    let i = same(h)
    return "{twice(b.n)} {a} {k} {s} {c} {quoted(k)} {i}"
}
fn main() {
    print(run(Box { n: 1 }, 4))
}
"#;

#[test]
fn read_only_parameters_take_the_callers_local() {
    let lua = emit_module(SRC);
    let body = function_body(&lua, "run");
    assert!(!body.contains("local b = "), "{body}");
    assert!(!body.contains("local by = "), "{body}");
    assert!(body.contains("c0.n = c0.n + c1"), "{body}");
    assert!(body.contains("local x = "), "{body}");
    assert!(body.contains("'x' .. tostring(c1) -- x"), "{body}");
    assert!(!body.contains("local v = "), "{body}");
    assert!(body.contains("(c1 + (1)) * 2"), "{body}");
    assert!(body.contains("local x = (c0.n)"), "{body}");
    assert_eq!(compile_and_run(SRC), "66 5 4 12 4 x4 10");
}
