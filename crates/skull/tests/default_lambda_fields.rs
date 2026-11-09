use crate::common::compile_and_run;

const SOURCE: &str = include_str!("fixtures/default_lambda_fields.rock");

fn check(root: &str, expected: &str) {
    let source = format!("{SOURCE}\nfn main() {{ print(\"{{{root}(7)}}\") }}\n");
    assert_eq!(compile_and_run(&source), expected);
}

#[test]
fn parameter_after_default() {
    check("parameter_after_default", "15");
}

#[test]
fn capturing_default() {
    check("capturing_default", "28");
}

#[test]
fn nested_function() {
    check("nested_function", "26");
}

#[test]
fn multiple_defaults() {
    check("multiple_defaults", "30");
}

#[test]
fn value_default() {
    check("value_default", "30");
}

#[test]
fn nested_default() {
    check("nested_default", "17");
}
