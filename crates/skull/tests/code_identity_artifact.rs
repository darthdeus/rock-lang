mod common;

use common::emit_mir;

#[test]
fn emitted_lua_artifact_has_a_total_escaping_function_census() {
    let emit = emit_mir(
        r#"
fn named(x: int) -> int { return x }

fn named_value() -> fn(int) -> int { return named }

fn capture_value() -> fn() -> int {
    let captured = 3
    return () => captured
}

fn free_value() -> fn() -> int { return () => 7 }

fn main() {
    let f = named_value()
    let a = capture_value()
    let b = free_value()
    print("{f(1)} {a()} {b()}")
}
"#,
    );
    let census = &emit.code_artifact.census;
    assert!(census.examined > 0, "the fixture must expose escaping function values: {census:?}");
    assert!(census.named_values > 0, "the named-value population must be nonzero: {census:?}");
    assert!(census.lambda_values >= 2, "both lambda populations must be nonzero: {census:?}");
    assert!(census.capturing_lambdas > 0, "the capturing lambda population must be nonzero: {census:?}");
    assert!(census.capture_free_lambdas > 0, "the capture-free lambda population must be nonzero: {census:?}");
    assert!(census.is_total(), "emitted artifact census is not self-validating: {census:?}");
}
