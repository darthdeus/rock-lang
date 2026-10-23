use crate::common::ws_with;
use skull_mir::mir::FuncRef;

const SOURCE: &str = include_str!("../../skull/tests/fixtures/default_lambda_fields.rock");

fn check(root: &str, expected: i64) {
    let ws = ws_with(SOURCE);
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == root)
        .expect("root function");
    let js = skull_codegen_js::emit::emit_with_closure(
        &ws,
        &[(
            FuncRef {
                module: mid,
                func: fid,
            },
            root.to_string(),
        )],
    )
    .expect("emit default lambda fixture");
    let program = format!(
        "{}{js}\nconsole.log({root}(7));",
        skull_codegen_js::JS_CALL_HELPER
    );
    let out = crate::common::node()
        .arg("-e")
        .arg(&program)
        .output()
        .expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n{}\n{program}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<i64>()
        .expect("integer result");
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
