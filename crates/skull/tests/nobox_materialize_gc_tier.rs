use crate::common::try_emit_module;

#[test]
fn live_implicit_materialization_in_a_hoisted_closure_is_rejected() {
    let source = r#"
nobox value NV2 { x: float, y: float }
fn with_check(check: fn() -> ()) { for _ in 0..1 { check() } }
fn main() {
    with_check(() => {
        let t = (NV2 { x: 1.0, y: 2.0 }, 1.0)
        print("{t}")
    })
}
"#;
    let err = try_emit_module(source).expect_err("live closure boxes must still reject");
    assert!(
        err.message
            .contains("survived lowering as an implicit materialization")
    );
    assert!(err.message.contains("lambda"));
    assert!(err.location.line > 0);
}
