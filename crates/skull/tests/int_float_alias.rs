use crate::common;

fn function_body<'a>(lua: &'a str, name: &str) -> &'a str {
    let body = lua
        .split(&format!("function {name}("))
        .nth(1)
        .expect("function");
    &body[..body.find("\nend\n").expect("function end")]
}

/// `rN = rM` lines: a register copied unchanged from another. (`rN = cM` is
/// also how a mutable cell is read, so it is not a copy here.)
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

// `y` is read before and after each yield, and `x` after the last one: both
// cross a suspension, so the step must keep them in frame slots.
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
    let lua = common::try_emit_module(YIELD_ACROSS).expect("emit");
    let step = function_body(&lua, "scaled_int__step");
    assert_eq!(register_moves(step), Vec::<&str>::new(), "{step}");
    assert_eq!(
        common::run_luajit(&format!("{lua}\nmain()\n"), "int_float_alias_yield"),
        "42\n18\n7"
    );
}

// `a` is one conversion, read by the comparison, by an arm and (in
// `min_register`) after the selection.
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
    let lua = common::try_emit_module(SELECTION).expect("emit");
    let body = function_body(&lua, "min_param_int_float");
    assert!(body.contains("math.min(c0, c1)"), "{body}");
    let body = function_body(&lua, "min_register_int_float");
    assert!(body.contains("math.min("), "{body}");
    assert_eq!(register_moves(body), Vec::<&str>::new(), "{body}");
    assert_eq!(
        common::run_luajit(&format!("{lua}\nmain()\n"), "int_float_alias_selection"),
        "2\n2.5\n8.5\n0"
    );
}
