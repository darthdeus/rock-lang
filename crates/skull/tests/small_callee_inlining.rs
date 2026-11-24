use crate::common::{bound_callee_names, emit_full_of, function_body, run_luajit, ws_with_stdlib};

fn emit(src: &str) -> String {
    let ws = ws_with_stdlib(src);
    emit_full_of(&ws, "emit_full").0.lua
}

fn run_counting_aborts(src: &str, reason: &str) -> String {
    let lua = emit(src);
    let driver = format!(
        r#"
local traceerr = require("jit.vmdef").traceerr
local aborts = 0
local function on_trace(what, _, _, _, err)
    if what == "abort" and traceerr[err] == "{reason}" then
        aborts = aborts + 1
    end
end
jit.attach(on_trace, "trace")
main()
jit.attach(on_trace)
print(aborts)
"#
    );
    run_luajit(&format!("{lua}\n{driver}"))
}

#[test]
fn small_loop_free_callees_are_spliced_and_others_stay_calls() {
    let lua = emit(
        "fn scale(a: int, k: int) -> int { return a * k + 1 }
fn total(xs: [int]) -> int {
    let s = 0
    for x in xs { s += x }
    return s
}
fn top(xs: [int], a: int) -> int { return scale(a, 3) + total(xs) }
fn main() { print(\"{top([1, 2], 4)}\") }
",
    );
    let body = function_body(&lua, "top");
    let callees: Vec<String> = bound_callee_names(&lua, "top")
        .into_iter()
        .map(|(_, target)| target)
        .collect();
    assert!(
        callees.iter().any(|c| c.starts_with("total"))
            && !callees.iter().any(|c| c.starts_with("scale")),
        "expected only the looping callee to stay a call:\n{body}"
    );
}

/// LuaJIT counts every tail call a trace records against `loopunroll` (15),
/// so a hot loop through a longer chain of forwarding calls never compiles.
/// Splicing small callees removes the calls instead of trading the tail call
/// for a frame that counts against the trace's slot limit.
#[test]
fn small_forwarding_chains_stay_within_the_trace_unroll_limit() {
    let mut src = String::from(
        "struct Acc { total: int, hits: int }
fn step(acc: Acc, x: int, k: int) -> int {
    acc.hits += 1
    acc.total += x * k
    return x + k
}
fn count_down(n: int, acc: int) -> int {
    if n == 0 { return acc }
    return count_down(n - 1, acc + 1)
}
fn link20(acc: Acc, x: int) -> int { return step(acc, x, 6) }
fn main() {
    let acc = Acc { total: 0, hits: 0 }
    let s = 0
    for i in 0..1000 { s += link0(acc, i) }
    print(\"{s} {acc.total} {acc.hits} {count_down(200000, 0)}\")
}
",
    );
    for i in 0..20 {
        src += &format!(
            "fn link{i}(acc: Acc, x: int) -> int {{ return link{}(acc, step(acc, x, {})) }}\n",
            i + 1,
            i % 7
        );
    }
    assert_eq!(
        run_counting_aborts(&src, "loop unroll limit reached"),
        "562500 33316500 21000 200000\n0"
    );
}

/// A trace holds at most 250 stack slots, so wide forwarding frames kept
/// live under their callees abort it as "trace too deep"; forwards that stay
/// calls remain tail calls that reuse the frame.
#[test]
fn wide_forwarding_frames_stay_within_the_trace_slot_limit() {
    let params = (0..24)
        .map(|i| format!("a{i}: int"))
        .collect::<Vec<_>>()
        .join(", ");
    let args = (1..24)
        .map(|i| format!("a{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let literals = (1..24)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut src = format!(
        "struct Acc {{ total: int }}
fn wide12(acc: Acc, {params}) -> int {{
    acc.total += a0
    return a0 + a23
}}
fn main() {{
    let acc = Acc {{ total: 0 }}
    let s = 0
    for i in 0..1000 {{ s += wide0(acc, i, {literals}) }}
    print(\"{{s}} {{acc.total}}\")
}}
"
    );
    for i in 0..12 {
        src += &format!(
            "fn wide{i}(acc: Acc, {params}) -> int {{ return wide{}(acc, a0 + 1, {args}) }}\n",
            i + 1
        );
    }
    assert_eq!(
        run_counting_aborts(&src, "trace too deep"),
        "534500 511500\n0"
    );
}
