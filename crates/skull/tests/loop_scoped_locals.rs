use crate::common::{emit_module, function_body};

#[test]
fn loop_body_temps_declared_inside_for() {
    let src = r#"
fn consume(a: float, b: float, c: float, d: float) -> float { return a + b + c + d }

fn hot(x: float, y: float, n: int) -> float {
    let acc = 0.0
    for i in 0..n {
        let t1 = x * 2.0 + y
        let t2 = y * 3.0 + x
        acc = acc + consume(t1, t2, t1 * t2, t1 - t2)
    }
    return acc
}

fn main() { print("{hot(1.0, 2.0, 8)}") }
"#;
    let lua = emit_module(src);
    let body = function_body(&lua, "hot");
    let for_pos = body
        .find(" for ")
        .unwrap_or_else(|| panic!("no for loop in:\n{body}"));
    let after_for = &body[for_pos..];
    let decl = after_for.lines().nth(1).unwrap_or_default();
    assert!(decl.trim_start().starts_with("local r"));
}
