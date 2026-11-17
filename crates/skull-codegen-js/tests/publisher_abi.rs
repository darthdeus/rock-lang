use crate::common::ws_with;

fn reject(source: &str, expected: &str) {
    let ws = ws_with(source);
    match crate::common::emit_closure(&ws, "main") {
        Ok(js) => panic!("expected publisher rejection for `{expected}`, emitted:\n{js}"),
        Err(message) => assert!(
            message.contains(expected),
            "expected `{expected}` in publisher rejection: {message}"
        ),
    }
}

#[test]
fn malformed_inline_publishers_are_rejected() {
    for (body, expected) in [
        ("return __rock_publish(false);", "2 result lanes"),
        ("return __rock_publish(false, v, v);", "2 result lanes"),
        ("return false;", "__rock_publish"),
        (
            "if (v > 0) { return __rock_publish(true, v); } return __rock_publish(false, v);",
            "one final",
        ),
        ("__rock_ret1 = v; return false;", "scratch bank"),
        (
            "var __rock_lane0 = v; return __rock_publish(false, v);",
            "compiler-owned",
        ),
    ] {
        reject(
            &format!(
                "inline(js) fn probe(v: float) -> (bool, float) = \"{body}\"\n\
                 fn main() {{ let (ok, value) = probe(1.0) }}"
            ),
            expected,
        );
    }

    reject(
        "inline(js) fn probe(v: float) -> float = \"return __rock_publish(v, v);\"\n\
         fn main() { let value = probe(1.0) }",
        "1 result lane",
    );
}
