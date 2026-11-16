//! Evaluation-order regressions for annotated call preflights that need a Lua
//! driver; the rest run from tests/annotations/preflight.rock.
use crate::common::{emit_module, require_luajit, run_luajit};

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
