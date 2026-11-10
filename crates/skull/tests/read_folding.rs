//! A read or call with one use in the same block renders inside that use, so
//! its value never occupies a Lua local: no register, no copy into the cell or
//! argument that consumes it, and no `nil` kill afterwards. The fold must keep
//! every read ordered against the writes it could observe.

use crate::common::{compile_and_run, emit_module, function_body};

const SHAPES: &str = r#"
struct P { x: int, y: int }
fn twice(n: int) -> int { return n * 2 }
fn sum(p: P) -> int { return p.x + p.y }
fn scaled(p: P) -> int { return twice(p.x) + 1 }
fn main() {
    let p = P { x: 3, y: 4 }
    print("{sum(p)} {scaled(p)}")
}
"#;

#[test]
fn single_use_reads_and_calls_render_in_their_consumer() {
    let lua = emit_module(SHAPES);
    for name in ["sum", "scaled"] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains(" local r"),
            "`{name}` gave a single-use read or call a register:\n{body}"
        );
    }
    assert!(
        function_body(&lua, "scaled").contains("(c0.x)"),
        "{}",
        function_body(&lua, "scaled")
    );
    assert_eq!(compile_and_run(SHAPES), "7 7");
}

#[test]
fn folded_reads_stay_ordered_against_writes() {
    let src = r#"
struct P { x: int, y: int }
global marks: int = 0
fn bump() -> int { marks += 1; return marks }
fn change(p: P) { p.x = 50 }
fn field_before_store(p: P) -> int {
    let old = p.x
    p.x = 9
    return old * 100 + p.x
}
fn field_before_call(p: P) -> int {
    let old = p.x
    change(p)
    return old * 100 + p.x
}
fn cell_before_store(a: int) -> int {
    let x = a
    let old = x
    x = x + 10
    return old * 100 + x
}
fn length_before_push() -> int {
    let v: [int] = []
    v.push(v.len())
    v.push(v.len())
    v.push(v[1] + v.len())
    return v[0] * 100 + v[1] * 10 + v[2]
}
fn call_once_in_product() -> uint {
    let a = bump() as uint
    return a * 3 as uint
}
fn main() {
    let p = P { x: 1, y: 2 }
    let q = P { x: 1, y: 2 }
    let product = call_once_in_product()
    print("{field_before_store(p)} {field_before_call(q)} {cell_before_store(1)} {length_before_push()} {product} {marks}")
}
"#;
    assert_eq!(compile_and_run(src), "109 150 111 13 3 1");
}

