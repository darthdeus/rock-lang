//! A `<local> = nil` kill keeps a dead slot out of LuaJIT exit snapshots. A
//! kill with only a call-free return after it changes no snapshot: LuaJIT
//! drops every slot a return does not read. A kill before a return that still
//! calls something stays, since the frame lives through that call.

use crate::common::{emit_module, function_body, run_main};

const SRC: &str = r#"
inline(lua) fn pick(a: int, b: int) -> int = "if a > b then return a end\nreturn b"
inline(lua) fn bump(a: int) -> int = "return a + 1"
fn plain(a: int, b: int) -> int {
    let m = pick(a, b)
    return m * 2
}
fn calls(a: int, b: int) -> int {
    let m = pick(a, b)
    return bump(m) * 2
}
fn joined(a: int, b: int) -> int {
    let m = 0
    if a > b {
        m = pick(a, b)
    } else {
        m = bump(b)
    }
    return m
}
fn main() {
    print("{plain(3, 4)} {calls(5, 1)} {joined(2, 1)} {joined(1, 2)}")
}
"#;

#[test]
fn a_kill_before_a_call_free_return_is_dropped() {
    let lua = emit_module(SRC);
    let body = function_body(&lua, "plain");
    assert!(!body.contains(" = nil"));
    let body = function_body(&lua, "calls");
    assert!(body.contains(" = nil"));
    // An arm's kills after its last call, with the join only returning.
    let body = function_body(&lua, "joined");
    assert!(!body.contains(" = nil\n else"));
    assert!(!body.contains(" = nil\n end"));
    assert_eq!(run_main(&lua), "8 12 2 3");
}
