//! Evaluation-order regressions for annotated call preflights.
use crate::common::{emit_module, require_luajit, run_luajit, run_main};

const COMMON: &str = r#"
struct Buffer { capacity: int, count: int, value: int }
fn prepare(b: Buffer) -> Buffer {
    if b.count == b.capacity { b.capacity += 1 }
    return b
}
fn tail(b: Buffer, value: int) -> bool {
    b.value = value
    b.count += 1
    return true
}
@preflight("prepare", "tail") fn append(b: Buffer, value: int) -> bool {
    return tail(prepare(b), value)
}
"#;

fn run(source: &str) -> String {
    require_luajit();
    run_main(&emit_module(&format!("{COMMON}\n{source}")))
}

#[test]
fn effectful_and_throwing_arguments_keep_their_order() {
    require_luajit();
    let lua = emit_module(&format!(
        r#"{COMMON}
fn argument(b: Buffer) -> int {{ let before = b.capacity; b.capacity = 4; return before }}
@nomangle inline(lua) fn fail() -> int = "error('argument failed')"
@nomangle fn effect(b: Buffer) {{ append(b, argument(b)) }}
@nomangle fn throws(b: Buffer) {{ append(b, fail()) }}
"#
    ));
    let driver = format!(
        r#"{lua}
local b = {{capacity=0, count=0, value=-1}}
effect(b)
assert(b.value == 0 and b.capacity == 4 and b.count == 1)
local c = {{capacity=0, count=0, value=-1}}
local ok, err = pcall(throws, c)
assert(not ok and err:find('argument failed', 1, true))
assert(c.capacity == 0 and c.count == 0 and c.value == -1)
print('ok')
"#
    );
    assert_eq!(run_luajit(&driver), "ok");
}

#[test]
fn receiver_capacity_read_is_before_growth() {
    assert_eq!(
        run(r#"
fn main() {
    let b = Buffer { capacity: 0, count: 0, value: -1 }
    append(b, b.capacity)
    print("{b.value} {b.capacity} {b.count}")
}
"#),
        "0 1 1"
    );
}

#[test]
fn captured_scalar_keeps_its_value_when_the_hook_calls_back() {
    require_luajit();
    let lua = emit_module(
        r#"
struct Buffer { capacity: int, count: int, value: int }
global HOOK: fn() = () => {}
fn prepare(b: Buffer) -> Buffer {
    if b.count == b.capacity { b.capacity += 1; HOOK() }
    return b
}
fn tail(b: Buffer, value: int) {
    b.value = value
    b.count += 1
}
@preflight("prepare", "tail") fn append(b: Buffer, value: int) {
    tail(prepare(b), value)
}
fn main() {
    let x = 7
    HOOK = () => { x = 9 }
    let b = Buffer { capacity: 0, count: 0, value: -1 }
    append(b, x + 1)
    print("{b.value} {x} {b.count}")
}
"#,
    );
    assert_eq!(run_main(&lua), "8 9 1");
}

#[test]
fn short_circuit_and_argument_block_keep_branch_scope() {
    assert_eq!(
        run(r#"
fn main() {
    let b = Buffer { capacity: 0, count: 0, value: -1 }
    let skipped = false && append(b, 5)
    print("{skipped} {b.capacity} {b.count}")
    append(b, if true { let local_value = 7; local_value + 2 } else { 3 })
    print("{b.value} {b.capacity} {b.count}")
}
"#),
        "false 0 0\n9 1 1"
    );
}

#[test]
fn receiver_runs_once_and_late_named_receiver_stays_late() {
    assert_eq!(
        run(r#"
global ORDER: int = 0
fn receiver(b: Buffer) -> Buffer { ORDER = ORDER * 10 + 2; return b }
fn arg() -> int { ORDER = ORDER * 10 + 1; return 7 }
fn plain(a: int, b: int) -> int { return a * 10 + b }
fn main() {
    let b = Buffer { capacity: 0, count: 0, value: -1 }
    append(receiver(b), 5)
    print("{ORDER} {b.value} {b.count}")
    ORDER = 0
    append(value: arg(), b: receiver(b))
    print("{ORDER} {b.value} {b.count}")
    print(plain(b: 2, a: 1))
}
"#),
        "2 5 1\n12 7 2\n12"
    );
}
