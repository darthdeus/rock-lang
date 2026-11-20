use crate::common::{emit_module, function_body, run_main};

fn assert_before(body: &str, earlier: &str, later: &str) {
    let a = body
        .find(earlier)
        .unwrap_or_else(|| panic!("missing {earlier}:\n{body}"));
    let b = body
        .find(later)
        .unwrap_or_else(|| panic!("missing {later}:\n{body}"));
    assert!(a < b);
}

#[test]
fn wide_float_results_are_computed_after_an_independent_call() {
    let lua = emit_module(include_str!("../../../benchmarks/live_floats.rock"));
    let body = function_body(&lua, "membership");
    let call = body.find("b1(c1, (c2 % (8)))").expect("membership call");
    let first_wide_leaf = body.find("(1.125)").expect("first wide float leaf");
    assert!(call < first_wide_leaf);
}

const READ_ORDER: &str = r#"
struct Input { x: float }
nobox value Wide {
    f0: float, f1: float, f2: float, f3: float, f4: float,
    f5: float, f6: float, f7: float, f8: float,
    f9: float, f10: float, f11: float,
}
global MARK: int = 0

@inline fn prepare(s: Input) -> Wide {
    let x = s.x
    return Wide {
        f0: x * 1.0, f1: x * 2.0, f2: x * 3.0,
        f3: x * 4.0, f4: x * 5.0, f5: x * 6.0,
        f6: x * 7.0, f7: x * 8.0, f8: x * 9.0,
        f9: x * 10.0, f10: x * 11.0, f11: x * 12.0,
    }
}

fn sum(w: Wide) -> float {
    return w.f0 + w.f1 + w.f2 + w.f3 + w.f4 + w.f5 + w.f6 + w.f7 + w.f8 + w.f9 + w.f10 + w.f11
}

fn mutate(s: Input) -> bool {
    s.x = 10.0
    return true
}

fn sample(s: Input) -> float {
    let w = prepare(s)
    if mutate(s) { return sum(w) }
    return 0.0
}

fn mark_then_fail() -> bool {
    MARK = 1
    error("unexpected call")
    return true
}

fn sample_nullable(s: Input?) -> float {
    let w = prepare(unwrap(s))
    if mark_then_fail() { return sum(w) }
    return 0.0
}

inline(lua) fn attempt(f: () -> float) = "pcall(f)"

fn main() {
    let s = Input { x: 2.0 }
    let got = sample(s)
    let bad: Input? = null
    attempt(() => sample_nullable(bad))
    print("{got} {s.x} {MARK}")
}
"#;

#[test]
fn deferral_keeps_mutable_read_snapshots_and_throws_before_calls() {
    let lua = emit_module(READ_ORDER);
    let sample = function_body(&lua, "sample");
    assert_before(sample, "c0.x", "b0(c0)");
    assert_before(sample, "b0(c0)", "* (2.0)");
    let nullable = function_body(&lua, "sample_nullable");
    assert_before(nullable, "c0.x", "b0()");
    assert_before(nullable, "b0()", "* (2.0)");
    assert_eq!(run_main(&lua), "156 10 0");
}

const CALL_ORDER: &str = r#"
nobox value Wide {
    f0: float, f1: float, f2: float, f3: float, f4: float,
    f5: float, f6: float, f7: float, f8: float,
    f9: float, f10: float, f11: float,
}
global MARK: int = 0
global VALUE: float = 2.0

@inline fn make(x: float) -> Wide {
    return Wide {
        f0: x * 1.0, f1: x * 2.0, f2: x * 3.0,
        f3: x * 4.0, f4: x * 5.0, f5: x * 6.0,
        f6: x * 7.0, f7: x * 8.0, f8: x * 9.0,
        f9: x * 10.0, f10: x * 11.0, f11: x * 12.0,
    }
}
fn sum(w: Wide) -> float {
    return w.f0 + w.f1 + w.f2 + w.f3 + w.f4 + w.f5 + w.f6 + w.f7 + w.f8 + w.f9 + w.f10 + w.f11
}
fn first() -> float { MARK = 1; return 2.0 }
fn second() -> bool { assert_eq(MARK, 1); return true }
fn first_fails() -> float { error("first failed"); return 0.0 }
fn second_marks() -> bool { MARK = 2; return true }
fn change_global() -> bool { VALUE = 10.0; return true }
inline(lua) fn attempt(f: () -> float) = "pcall(f)"

fn ordered() -> float {
    let w = make(first())
    if second() { return sum(w) }
    return 0.0
}
fn fails_first() -> float {
    let w = make(first_fails())
    if second_marks() { return sum(w) }
    return 0.0
}
fn global_snapshot() -> float {
    let w = make(VALUE)
    if change_global() { return sum(w) }
    return 0.0
}
fn captured_snapshot() -> float {
    let x = 2.0
    let w = make(x)
    let change = () => { x = 10.0; return true }
    if change() && change() { return sum(w) }
    return 0.0
}
fn main() {
    let a = ordered()
    MARK = 0
    attempt(() => fails_first())
    let mark = MARK
    let b = global_snapshot()
    let c = captured_snapshot()
    print("{a} {mark} {b} {VALUE} {c}")
}
"#;

#[test]
fn deferral_keeps_producer_call_order_and_global_and_capture_snapshots() {
    let lua = emit_module(CALL_ORDER);
    for name in ["ordered", "fails_first"] {
        let body = function_body(&lua, name);
        assert_before(body, "b0()", "b1()");
        assert_before(body, "b1()", "* (2.0)");
    }
    let global = function_body(&lua, "global_snapshot");
    assert_before(global, "__rock_state_cell", "b0()");
    assert_before(global, "b0()", "* (2.0)");
    let captured = function_body(&lua, "captured_snapshot");
    assert_before(captured, ".v", "r1()");
    assert_before(captured, "r1()", "* (2.0)");
    assert_eq!(run_main(&lua), "156 0 156 10 156");
}
