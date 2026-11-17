use crate::common::{compile_and_run, emit_module, function_body, run_luajit, run_main};

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
    // Two host property reads must retain their order; the first is staged.
    assert!(function_body(&lua, "sum").contains(" local r"));
    let body = function_body(&lua, "scaled");
    assert!(!body.contains(" local r"));
    assert!(function_body(&lua, "scaled").contains("(c0.x)"));
    assert_eq!(run_main(&lua), "7 7");
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
    assert!(!body.contains(" local r") && !body.contains(" r0"));
    assert!(body.contains("local _ = "));
    assert_eq!(run_main(&lua), "2");
}

#[test]
fn folded_calls_stay_ordered_against_global_reads_and_empty_branches() {
    let src = r#"
global marks: int = 1
struct Row { value: int }
@nomangle fn bump() -> int { marks = 2; return 7 }
@nomangle fn probe(row: Row) -> int { marks = 1; return row.value + marks }
@nomangle fn via_map(values: Map<string, int>) -> int { marks = 1; return values["value"] + marks }
@nomangle fn via_vec(values: [int?]) -> (int?, int) { marks = 1; return (values[0], marks) }
@nomangle fn control() -> int { marks = 1; return bump() + marks }
fn main() {}
"#;
    let lua = emit_module(src);
    assert_eq!(
        run_luajit(&format!(
            r#"{lua}
local row = setmetatable({{}}, {{ __index = function() return bump() end }})
local values = setmetatable({{}}, {{ __index = function() return bump() end }})
local xs = {{ len = 1, _data = values }}
local value, snapshot = via_vec(xs)
print(probe(row), via_map(values), value + snapshot, control())
"#
        )),
        "9\t9\t9\t9"
    );

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
    assert!(!pick.contains("local r"));
    let stepper = function_body(&lua, "stepper");
    assert!(stepper.contains("r0 = c0"));
    assert_eq!(run_main(&lua), "20 12 8 66");
}

#[test]
fn a_value_stored_into_a_dead_cell_is_computed_in_that_cell() {
    let src = r#"
fn pick(a: int, b: int) -> int {
    let m = 0
    let t = a * 3
    if t > b {
        m = t
    } else {
        m = b - t
    }
    return m * 2
}
fn clampish(a: int, lo: int) -> int {
    let m = 0
    let t = a * 3
    if t < lo {
        m = lo - 1
    } else {
        m = t
    }
    return m + t
}
fn swapper(a: int, b: int) -> int {
    let x = a
    let y = b
    let i = 0
    while i < 3 {
        let t = x + 1
        x = y
        y = t
        i = i + 1
    }
    return x * 100 + y
}
fn main() {
    print("{pick(1, 5)} {pick(4, 5)} {clampish(1, 5)} {clampish(4, 5)} {swapper(1, 2)}")
}
"#;
    let lua = emit_module(src);
    let pick = function_body(&lua, "pick");
    assert!(!pick.contains("local r"));
    assert!(function_body(&lua, "clampish").contains("local r0"));
    assert_eq!(run_main(&lua), "4 24 7 24 303");
}

#[test]
fn a_cell_copied_from_a_dying_cell_shares_its_local() {
    let src = r#"
fn h(n: int) -> int {
    let u = n * 2
    let t = 0
    let i = 0
    while i < n {
        t = t + u
        u = u - 1
        i = i + 1
    }
    let s = t
    let k = 0
    while k < n {
        s = s + k
        k = k + 1
    }
    return s
}
fn main() {
    print("{h(3)}")
}
"#;
    let lua = emit_module(src);
    let body = function_body(&lua, "h");
    let copies = body.lines().filter(|l| {
        let l = l.trim();
        let mut sides = l.split(" = ");
        matches!((sides.next(), sides.next(), sides.next()),
            (Some(a), Some(b), None) if a.starts_with('c') && b.starts_with('c')
                && !b.contains(' ') && !b.contains('('))
    });
    assert_eq!(copies.count(), 0);
    assert_eq!(run_main(&lua), "18");
}

#[test]
fn a_call_bound_to_underscore_runs_as_a_bare_statement() {
    let src = r#"
fn f(x: int) -> int {
    print("f{x}")
    return x + 1
}
fn h(n: int) {
    let _ = f(n)
    let _ = f(n * 2)
}
fn main() {
    h(3)
}
"#;
    let lua = emit_module(src);
    let body = function_body(&lua, "h");
    assert!(!body.contains(" = nil"));
    assert_eq!(run_main(&lua), "f3\nf6");
}

#[test]
fn a_discarded_call_through_a_local_runs_as_a_bare_statement() {
    let src = r#"
fn h(cb: fn() -> int, n: int) {
    let i = 0
    while i < n {
        cb()
        i = i + 1
    }
}
fn main() {
    let t = 0
    h(() => {
        t = t + 1
        print("t{t}")
        return t
    }, 2)
}
"#;
    let lua = emit_module(src);
    let body = function_body(&lua, "h");
    assert!(!body.contains(" = nil"));
    assert_eq!(run_main(&lua), "t1\nt2");
}
