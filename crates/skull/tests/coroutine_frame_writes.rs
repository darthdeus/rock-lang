use crate::common;

const FRAME_WRITES: &str = r#"
enum FrameYield { Empty, Pair(int, int) }
struct UserBox { value: int? = null }
global calls: int = 0

fn next_frame_value(i: int) -> FrameYield {
    calls += 1
    if i % 2 == 0 { return FrameYield::Pair(calls, calls + 10) }
    return FrameYield::Empty
}

co fn frame_values() -> Coroutine<FrameYield> {
    for i in 0..4 { yield next_frame_value(i) }
}

inline(lua) fn frame_payload_empty(c: Coroutine<FrameYield>) -> bool = """
    return c.__y1 == nil and c.__y2 == nil
"""
inline(lua) fn restore_frame(c: Coroutine<FrameYield>) -> Coroutine<FrameYield> = """
    local copy = {}
    for k, v in pairs(c) do copy[k] = v end
    return copy
"""
inline(lua) fn observe_user_writes(b: UserBox) = """
    b.__hits = 0
    setmetatable(b, { __newindex = function(t, k, v)
        t.__hits = t.__hits + 1
        rawset(t, k, v)
    end })
"""
inline(lua) fn user_writes(b: UserBox) -> int = """
    return b.__hits
"""
fn clear_user_box(b: UserBox) { b.value = null }

fn main() {
    let c = frame_values()
    print(c.resume() == FrameYield::Pair(1, 11))
    print(c.resume() == FrameYield::Empty)
    print(frame_payload_empty(c))
    c = restore_frame(c)
    print(c.resume() == FrameYield::Pair(3, 13))
    print(c.resume() == FrameYield::Empty)
    print(frame_payload_empty(c))
    print(calls)
    let b = UserBox {}
    observe_user_writes(b)
    clear_user_box(b)
    clear_user_box(b)
    print(user_writes(b))
}
"#;

#[test]
fn frame_stores_evaluate_rhs_once_clear_payloads_and_preserve_user_writes() {
    let lua = common::try_emit_module(FRAME_WRITES).expect("frame writes emit");
    assert_eq!(common::run_luajit(&format!("{lua}\nmain()\n"), "frame_nil_writes"),
        "true\ntrue\ntrue\ntrue\ntrue\ntrue\n4\n2");
}
