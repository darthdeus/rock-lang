//! A read or call with one use in the same block renders inside that use, so
//! its value never occupies a Lua local: no register, no copy into the cell or
//! argument that consumes it, and no `nil` kill afterwards. The fold must keep
//! every read ordered against the writes it could observe. A call whose result
//! nothing reads is a bare call statement.

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

#[test]
fn unused_call_results_run_as_bare_statements() {
    let src = r#"
global marks: int = 0
fn bump() -> int { marks += 1; return marks }
@inline inline(lua) fn checked(x: int) -> int = "x + 1"
fn run(n: int) {
    bump()
    checked(n)
    bump()
}
fn main() {
    run(1)
    print("{marks}")
}
"#;
    let lua = emit_module(src);
    let body = function_body(&lua, "run");
    assert!(
        !body.contains(" local r") && !body.contains(" r0"),
        "an unused call result took a register:\n{body}"
    );
    assert!(body.contains("local _ = "), "{body}");
    assert_eq!(compile_and_run(src), "2");
}

#[test]
fn folded_calls_stay_ordered_against_global_reads_and_empty_branches() {
    let src = r#"
global marks: int = 0
fn bump(n: int) -> int { marks += 1; return n }
fn maybe(n: int) -> int? { marks += 10; return n }
fn global_after_call() -> int {
    marks = 0
    let r = bump(5) + 1
    return r * 100 + marks
}
fn call_in_dropped_branch() -> int {
    marks = 0
    let unused = maybe(1) ?? -1
    return marks
}
fn main() {
    print("{global_after_call()} {call_in_dropped_branch()}")
}
"#;
    assert_eq!(compile_and_run(src), "601 10");
}

#[test]
fn a_read_of_a_cell_nothing_rewrites_renders_as_the_cell() {
    let src = r#"
fn pick(a: int, b: int) -> int {
    let m = a
    if b > a {
        m = b * 2
    }
    let n = m
    if n > 10 {
        return n - 10
    }
    return n * n + n
}
fn stepper(k: int) -> int {
    let x = k
    let total = 0
    while x > 0 {
        let y = x
        x = x - 3
        total = total + y * y
    }
    return total
}
fn main() {
    print("{pick(1, 2)} {pick(3, 1)} {pick(1, 9)} {stepper(7)}")
}
"#;
    let lua = emit_module(src);
    let pick = function_body(&lua, "pick");
    assert!(!pick.contains("local r"), "{pick}");
    let stepper = function_body(&lua, "stepper");
    assert!(stepper.contains("r0 = c0"), "{stepper}");
    assert_eq!(compile_and_run(src), "20 12 8 66");
}
