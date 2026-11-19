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

#[test]
fn cross_loop_temp_stays_at_function_top() {
    let src = r#"
fn consume(a: float, b: float) -> float { return a + b }

fn hot(x: float, y: float, n: int) -> float {
    let acc = 0.0
    let pre = x * 7.0 + y
    for i in 0..n {
        let t1 = x * 2.0 + y
        acc = acc + consume(t1, t1 * 2.0)
    }
    return acc + pre * pre
}

fn main() { print("{hot(1.0, 2.0, 8)}") }
"#;
    let lua = emit_module(src);
    let body = function_body(&lua, "hot");
    let top_decl = body
        .lines()
        .find(|l| l.trim_start().starts_with("local "))
        .expect("top local line")
        .to_string();
    let loop_end = body.rfind("\n end").expect("loop end");
    let tail = &body[loop_end..];
    for tok in tail.split(|c: char| !c.is_alphanumeric()) {
        if tok.starts_with('r') && tok[1..].chars().all(|c| c.is_ascii_digit()) && tok.len() > 1 {
            assert!(top_decl.contains(tok));
        }
    }
}
