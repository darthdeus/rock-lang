use crate::common::{compile_and_run, emit_module, function_body, run_main};

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
    assert!(!body.contains("local b = "));
    assert!(!body.contains("local by = "));
    assert!(body.contains("c0.n = c0.n + c1"));
    assert!(body.contains("local x = "));
    assert!(body.contains("'x' .. tostring(c1) -- x"));
    assert!(!body.contains("local v = "));
    assert!(body.contains("(c1 + (1)) * 2"));
    assert!(body.contains("local x = (c0.n)"));
    assert_eq!(run_main(&lua), "66 5 4 12 4 x4 10");
}

const ORDER_SRC: &str = r#"
struct Box { n: int }
struct Holder { b: Box, n: int }
inline(lua) fn tick(h: Holder) -> int = "h.b.n = h.b.n + 10\nh.n = h.n + 1\nreturn h.n"
@inline inline(lua) fn ignore(v: int) = ""
@inline inline(lua) fn next_of(b: Box) -> int = "return b.n + 1"
@inline inline(lua) fn sum(n: int, k: int) -> int = "return n + k"
@inline inline(lua) fn show(n: int) -> string = "return tostring(n)"
fn run(h: Holder) -> string {
    ignore(tick(h))
    show(h.n)
    let a = next_of(h.b)
    let s = sum(h.n, tick(h))
    return "{a} {s} {h.n}"
}
fn main() {
    print(run(Holder { b: Box { n: 5 }, n: 0 }))
}
"#;

#[test]
fn a_splice_preserves_read_order_without_empty_blocks() {
    let lua = emit_module(ORDER_SRC);
    let body = function_body(&lua, "run");
    assert!(!body.contains("local v = "));
    assert!(!body.contains(" do\n end"));
    // A discarded call result needs no local to bind it.
    assert!(body.contains("tostring("));
    assert!(!body.contains("local _"));
    assert!(body.contains("local b = (c0.b)"));
    assert!(!body.contains(".b).n + 1"));
    assert_eq!(run_main(&lua), "16 3 2");
}

#[test]
fn a_moved_read_never_opens_a_statement() {
    // A parenthesized operand at the start of a line would continue the
    // previous statement as a call.
    let src = r#"
struct Box { n: int }
struct Holder { b: Box, n: int }
@inline inline(lua) fn set_n(b: Box, v: int) = "b.n = v"
fn run(h: Holder, v: int) -> int {
    h.n = v
    set_n(h.b, v + 1)
    return h.b.n
}
fn main() {
    print(run(Holder { b: Box { n: 5 }, n: 0 }, 7))
}
"#;
    assert_eq!(compile_and_run(src), "8");
}
