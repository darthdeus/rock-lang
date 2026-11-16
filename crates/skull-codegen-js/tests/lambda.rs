//! Closure conversion on JS: each lambda is a hoisted sibling function reached
//! through a reusable code slot (capture-free) or a slot-bearing environment
//! record (capturing), never a frozen reference to the backend body, so an
//! overlay can swap the body. Direct calls keep their physical callee.
//! Closure behavior runs on both backends in tests/closures.

use crate::common::{emit_closure, ws_with};

#[test]
fn direct_calls_remain_direct_while_values_use_slots() {
    let ws = ws_with(
        "fn target(x: int) -> int { x + 1 }\n\
         fn direct() -> int { target(1) }\n\
         fn main() { direct() }\n",
    );
    let js = emit_closure(&ws, "main").unwrap();
    let start = js.find("function direct").expect("direct body");
    let body = js[start..].split("\n}\n").next().expect("direct body");
    assert!(
        body.contains("target_"),
        "direct call keeps its physical call: {body}"
    );
    assert!(
        !body.contains("__rock_code_slot_ref"),
        "direct call must not route through a CodeRef: {body}"
    );
}

#[test]
fn lambdas_are_hoisted_behind_code_slots() {
    for (src, root) in [
        (
            "fn capture(n: int) -> int { let g = (x: int) => x + n\n g(10) }",
            "capture",
        ),
        ("fn mk() -> fn() -> int { () => 42 }", "mk"),
    ] {
        let js = emit_closure(&ws_with(src), root).unwrap();
        assert!(
            js.contains(&format!("function {root}__lambda0(")),
            "hoisted sibling body:\n{js}"
        );
        assert!(
            js.contains("__rock_code_slot_ref("),
            "the value must resolve through a code slot:\n{js}"
        );
        assert!(
            !js.contains(&format!("__fn: {root}__lambda0")),
            "a closure must not freeze its backend body:\n{js}"
        );
    }
}
