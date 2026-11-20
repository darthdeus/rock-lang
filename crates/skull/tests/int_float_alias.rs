use crate::common::{emit_module, function_body, run_main};

fn register_moves(body: &str) -> Vec<&str> {
    body.lines()
        .map(str::trim)
        .filter(|line| {
            let Some((dst, src)) = line.split_once(" = ") else {
                return false;
            };
            let register = |s: &str| {
                s.strip_prefix('r')
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            };
            register(dst) && register(src)
        })
        .collect()
}

const YIELD_ACROSS: &str = r#"
global seed: int = 3

co fn scaled(n: int) -> Coroutine<float> {
    let x = seed * n
    let y = x as float
    yield y * y + y
    seed = 100
    yield y * 2.0 + y
    yield (x + 1) as float
}

fn main() {
    let c = scaled(2)
    print(c.resume())
    print(c.resume())
    print(c.resume())
}
"#;

#[test]
fn reused_int_to_float_across_a_yield_aliases_and_resumes_its_value() {
    let lua = emit_module(YIELD_ACROSS);
    let step = function_body(&lua, "scaled_int__step");
    assert_eq!(register_moves(step), Vec::<&str>::new());
    assert_eq!(run_main(&lua), "42\n18\n7");
}

const SELECTION: &str = r#"
fn min_param(x: int, b: float) -> float {
    let a = x as float
    return if a < b { a } else { b }
}
fn min_register(x: int, b: float) -> float {
    let s = x * 3
    let a = s as float
    return if a < b { a } else { b } + a + (s as float)
}
fn main() {
    print(min_param(2, 2.5))
    print(min_param(3, 2.5))
    print(min_register(1, 2.5))
    print(min_register(0, 2.5))
}
"#;

#[test]
fn aliased_int_to_float_selection_operand_keeps_native_minmax() {
    let lua = emit_module(SELECTION);
    let body = function_body(&lua, "min_param_int_float");
    assert!(body.contains("math.min(c0, c1)"));
    let body = function_body(&lua, "min_register_int_float");
    assert!(body.contains("math.min("));
    assert_eq!(register_moves(body), Vec::<&str>::new());
    assert_eq!(run_main(&lua), "2\n2.5\n8.5\n0");
}

#[test]
fn prelude_identity_conversions_return_their_arguments_directly() {
    let lua = emit_module(
        r#"
fn conversions(n: int, s: string) -> (int, float, float, float, string) {
    return (int(n), float(n), int_to_float(n), to_float(n), to_string(s))
}
fn main() { print(conversions(-3, "hello")) }
"#,
    );
    let body = function_body(&lua, "conversions_int_string");
    assert!(body.contains("return c0, c0, c0, c0, c1"));
}
