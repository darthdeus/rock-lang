use crate::common::try_emit_module;

const POLYMORPHIC_RECURSION: &str = r#"
fn rec<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    return rec([x], n - 1)
}

fn main() {
    print("{rec(1, 3)}")
}
"#;

#[test]
fn user_fn_emit_failure_is_a_hard_error() {
    let err = try_emit_module(POLYMORPHIC_RECURSION)
        .expect_err("an unemittable user fn must reject, not silently skip");
    assert!(err.message.contains("rec"));
    assert!(err.message.contains("polymorphic recursion"));
}
