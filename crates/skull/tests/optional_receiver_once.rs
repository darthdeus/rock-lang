use crate::common::{emit_module, function_body, run_main};

const SOURCE: &str = r#"
value Row { text: string, n: int }
struct Counter { calls: int = 0 }
@nomangle fn next_row(c: Counter) -> Row? {
    c.calls += 1
    if c.calls == 1 { return Row { text: "first", n: 7 } }
    return null
}
@nomangle fn read_once(c: Counter) -> int { return next_row(c)?.n ?? -1 }
fn main() {
    let c = Counter {}
    print("{read_once(c)} {c.calls}")
}
"#;

#[test]
fn optional_receiver_fallback_does_not_emit_a_discarded_call() {
    let lua = emit_module(SOURCE);
    let body = function_body(&lua, "read_once");
    assert_eq!(body.matches("next_row(").count(), 1, "{body}");
    assert_eq!(run_main(&lua), "7 1");
}
