use crate::common::ws_with;

const SOURCE: &str = include_str!("../../skull/tests/fixtures/default_lambda_fields.rock");

fn check(root: &str, expected: i64) {
    let ws = ws_with(SOURCE);
    let js = crate::common::emit_closure(&ws, root).expect("emit default lambda fixture");
    let program = format!(
        "{}{js}\nconsole.log({root}(7));",
        skull_codegen_js::JS_CALL_HELPER
    );
    let out = crate::common::run_node(&program);
    let value = out.trim().parse::<i64>().expect("integer result");
    assert_eq!(value, expected, "{program}");
}

#[test]
fn parameter_after_default() {
    check("parameter_after_default", 15);
}

#[test]
fn capturing_default() {
    check("capturing_default", 28);
}

#[test]
fn nested_function() {
    check("nested_function", 26);
}

#[test]
fn multiple_defaults() {
    check("multiple_defaults", 30);
}

#[test]
fn value_default() {
    check("value_default", 30);
}

#[test]
fn nested_default() {
    check("nested_default", 17);
}
