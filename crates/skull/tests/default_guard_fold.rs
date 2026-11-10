//! An inlined callee's default prologue tests a parameter the caller already
//! stored. When that store is a constant in the same block, the test is
//! decided, so neither it nor its fill may survive. The values must not move:
//! omitted gets the default, a passed value stays, an explicit null stays null.

use crate::common::{compile_and_run, emit_module, function_body};

const SHAPES: &str = r#"
nobox value C { r: float, g: float, b: float, a: float }
@inline @pure fn rgb(r: float, g: float, b: float, a: float = 1.0) -> C {
    return C { r: r, g: g, b: b, a: a }
}
@inline fn seeded(seed: int? = 42) -> int { return seed ?? -1 }
fn alphas(x: float) -> float {
    let total = rgb(x, 0.3, 0.3, 0.9).a + rgb(x, 0.1, 0.2).a
    for i in 0..3 {
        total += rgb(x, 0.0, 0.0).a + rgb(x, 0.0, 0.0, 0.25).a
    }
    let c = rgb(x, 0.0, 0.0, 0.5)
    if x > 1.0 {
        c = rgb(x, 1.0, 1.0)
    }
    return total + c.a
}
fn seeds() -> int { return seeded() * 100 + seeded(7) * 10 + seeded(null) }
fn main() {
    print("{alphas(2.0)} {alphas(0.5)} {seeds()}")
}
"#;

#[test]
fn constant_arguments_decide_inlined_default_tests() {
    let lua = emit_module(SHAPES);
    for name in ["alphas_float", "seeds"] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("== (nil)") && !body.contains("__rock_omitted"),
            "`{name}` kept a decided default test:\n{body}"
        );
    }
    assert_eq!(compile_and_run(SHAPES), "6.65 6.15 4269");
}

/// A test whose argument reaches it along two paths, or around a loop, is not
/// decided by the literal on one of them.
#[test]
fn joined_and_looped_arguments_keep_their_default_tests() {
    let src = r#"
@inline fn scaled(x: int, by: int? = 3) -> int { return x * (by ?? 10) }
@inline fn bump(x: int, by: int = 1) -> int { return x + by }
fn joined(flag: bool) -> int {
    let by: int? = 2
    if flag {
        by = null
    }
    return scaled(5, by)
}
fn looped(n: int) -> int {
    let by: int? = null
    let total = 0
    for i in 0..n {
        total += scaled(1, by)
        by = i
    }
    return total
}
fn labelled(n: int) -> int {
    let total = 0
    let i = 0
    while i < n {
        i += 1
        if i == 2 {
            continue
        }
        total = bump(total)
        if i > 3 {
            break
        }
        total = bump(total, i)
    }
    return total
}
fn main() {
    print("{joined(true)} {joined(false)} {looped(4)} {labelled(6)}")
}
"#;
    assert_eq!(compile_and_run(src), "50 10 13 7");
}
