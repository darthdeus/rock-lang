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

@nomangle fn direct_target(x: int) -> int { return x + 1 }
@nomangle fn direct_call() -> int { return direct_target(1) }
"#,
    );
    let census = &emit.code_artifact.census;
    assert!(census.examined > 0, "the fixture must expose escaping function values: {census:?}");
    assert!(census.named_values > 0, "the named-value population must be nonzero: {census:?}");
    assert!(census.lambda_values >= 2, "both lambda populations must be nonzero: {census:?}");
    assert!(census.capturing_lambdas > 0, "the capturing lambda population must be nonzero: {census:?}");
    assert!(census.capture_free_lambdas > 0, "the capture-free lambda population must be nonzero: {census:?}");
    let direct_body = emit
        .lua
        .split("function direct_call()")
        .nth(1)
        .and_then(|body| body.split("\nend").next())
        .expect("direct call body");
    assert!(direct_body.contains("direct_target("), "direct calls retain their direct target: {direct_body}");
    assert!(!direct_body.contains("__rock_code_slot_ref"), "direct calls must not pay the slot indirection: {direct_body}");
    assert!(census.is_total(), "emitted artifact census is not self-validating: {census:?}");
}

#[test]
fn generated_decorator_function_values_use_logical_slots() {
    let emit = emit_mir(
        r#"
global SYSTEMS: [fn() -> ()] = []
deco init system { SYSTEMS.push(transmute(target.func)) }
@system
fn alpha() { }
fn main() { }
"#,
    );
    let application = emit
        .lua
        .lines()
        .find(|line| line.contains("__deco_system({"))
        .expect("decorator application");
    assert!(
        application.contains("__rock_code_slot_ref(\"named:<test>::alpha<>\")"),
        "generated decorator target must be a CodeRef: {application}"
    );
    assert!(
        !application.contains("func = alpha_ret_unit"),
        "generated decorator target must not freeze the backend body: {application}"
    );
}
