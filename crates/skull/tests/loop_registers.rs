use crate::common::{emit_module, function_body, run_main};

#[test]
fn loop_temporaries_keep_their_values_after_register_reuse() {
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
    assert!(body.contains(" for "));
    assert_eq!(run_main(&lua), "288");
}
