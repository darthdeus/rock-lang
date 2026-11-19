use super::*;

#[test]
fn completion_during_editing_with_syntax_error() {
    let source = r#"fn add(a: int, b: int) -> int {
    return a + b
}

fn main() {
    let x = 42
    let y = add(
    // Syntax error: unclosed function call
}"#;
    let project = Project::single("completion_during_editing_with_syntax_error", source);
    let reply = project.cli("completion", Some((6, 16)), &[]);
    completion_labels(&reply, &["x", "y", "add"]);
}

#[test]
fn completion_after_valid_then_broken() {
    let source = "struct Point {\n    x: float,\n    y: float,\n}\n\nfn create_point() -> Point {\n    return Point { x: 1.0, y: 2.0 }\n}\n\nfn get_point() -> Point {\n    \n}\n\nfn main() {\n    let invalid_syntax = broken\n    // This line has invalid syntax but completion should still work\n}";
    let project = Project::single("completion_after_valid_then_broken", source);
    let reply = project.cli("completion", Some((10, 4)), &[]);
    completion_labels(&reply, &["create_point", "Point"]);
}

#[test]
fn hover_during_editing_with_error() {
    let source = r#"fn calculate(num: int) -> int {
    return num * 2
}

fn get_value() -> int {
    return calculate(21)
}

fn main() {
    let broken = this is invalid syntax
}"#;
    let project = Project::single("hover_during_editing_with_error", source);
    let reply = project.cli("hover", Some((1, 12)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int\n".trim()
    );
}

#[test]
fn inlay_hints_with_partial_compilation() {
    let source = r#"fn process(value: int) -> string {
    return value.to_string()
}

fn get_result() -> string {
    let result = process(123)
    return result
}

fn main() {
    let broken = invalid syntax here!!!
}"#;
    let project = Project::single("inlay_hints_with_partial_compilation", source);
    let reply = project.cli("inlay-hints", None, &[]);
    contains_items(
        &reply["hints"],
        &[
            json!({"position": {"line": 5}, "label": {"text": "string", "strip_prefix": ":"}}),
            json!({"position": {"line": 5}, "label": {"text": "value:", "strip_prefix": ":"}}),
        ],
    );
}
