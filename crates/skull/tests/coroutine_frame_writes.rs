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

/// A v1 enum has one runtime tag. The erased `resume()` rebuilds the yielded
/// value from the frame slots, and that box must carry the same `__ty` as a
/// constructed value (the spelling `__ROCK_TYPE_TABLE` is keyed on), not the
/// enum's definition key.
#[test]
fn a_resumed_v1_enum_box_carries_the_types_runtime_tag() {
    let emitted = common::emit_v1_package(
        "v1yield",
        &[common::V1Fragment {
            file: "main.rock",
            src: r#"
enum Step { Go(string), Done }
co fn steps() -> Coroutine<Step> { yield Step::Go("one") }
inline(lua) fn erased_resume_tag(c: Coroutine<Step>) -> string = "return c.__resume(c).__ty"
inline(lua) fn unit_tag(v: Step) -> string = "return v.__ty"
pub fn probe() -> string {
    return "{erased_resume_tag(steps())} {unit_tag(Step::Done)}"
}
fn main() {}
"#,
        }],
    );
    let probe = common::mangled_name(&emitted.lua, "probe");
    let out = common::run_luajit(&format!("{}\nprint({probe}())\n", emitted.lua), "v1_resume_tag");
    let (resumed, built) = out.split_once(' ').expect("two tags");
    assert_eq!(resumed, built, "the erased resume box and a constructed value disagree on the tag");
    assert!(!resumed.contains("::"), "a runtime tag is the minted symbol, not the DefKey: {resumed}");
}
