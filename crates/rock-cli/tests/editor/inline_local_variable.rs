use super::*;

#[test]
fn inline_simple_variable() {
    let source = r#"fn main() {
    let x = 42
    print(x)
}"#;
    let project = Project::single("inline_simple_variable", source);
    let mut doc = Document::open(project, source);
    let reply = doc.request("textDocument/codeAction", json!({"range": {"start": {"line": 1, "character": 8}, "end": {"line": 1, "character": 8}}, "context": {"diagnostics": []}}));
    assert_eq!(
        doc.apply_action(&reply, "Inline local `x`").trim_end(),
        (&source.replace(
            r#"    let x = 42
    print(x)
"#,
            "    print(42)\n"
        ))
            .trim_end()
    );
}

#[test]
fn inline_variable_multiple_uses() {
    let source = r#"fn main() {
    let value = 10 + 20
    print(value)
    let result = value * 2
    return result
}"#;
    let project = Project::single("inline_variable_multiple_uses", source);
    let mut doc = Document::open(project, source);
    let reply = doc.request("textDocument/codeAction", json!({"range": {"start": {"line": 1, "character": 8}, "end": {"line": 1, "character": 8}}, "context": {"diagnostics": []}}));
    assert_eq!(
        doc.apply_action(&reply, "Inline local `value`").trim_end(),
        (&source.replace(
            r#"    let value = 10 + 20
    print(value)
    let result = value * 2
"#,
            r#"    print(10 + 20)
    let result = (10 + 20) * 2
"#
        ))
            .trim_end()
    );
}

#[test]
fn inline_variable_with_complex_expression() {
    let source = r#"fn calculate(x: int) -> int {
    let temp = x * 2 + 1
    return temp * temp
}"#;
    let project = Project::single("inline_variable_with_complex_expression", source);
    let mut doc = Document::open(project, source);
    let reply = doc.request("textDocument/codeAction", json!({"range": {"start": {"line": 1, "character": 8}, "end": {"line": 1, "character": 8}}, "context": {"diagnostics": []}}));
    assert_eq!(
        doc.apply_action(&reply, "Inline local `temp`").trim_end(),
        (&source.replace(
            r#"    let temp = x * 2 + 1
    return temp * temp
"#,
            "    return (x * 2 + 1) * (x * 2 + 1)\n"
        ))
            .trim_end()
    );
}

#[test]
fn inline_variable_in_function_call() {
    let source = r#"fn process(value: int) -> int {
    return value * 2
}

fn main() {
    let num = 42
    let result = process(num)
    print(result)
}"#;
    let project = Project::single("inline_variable_in_function_call", source);
    let mut doc = Document::open(project, source);
    let reply = doc.request("textDocument/codeAction", json!({"range": {"start": {"line": 5, "character": 8}, "end": {"line": 5, "character": 8}}, "context": {"diagnostics": []}}));
    assert_eq!(
        doc.apply_action(&reply, "Inline local `num`").trim_end(),
        (&source.replace(
            r#"    let num = 42
    let result = process(num)
"#,
            "    let result = process(42)\n"
        ))
            .trim_end()
    );
}

#[test]
fn inline_variable_with_side_effects() {
    let source = r#"fn get_value() -> int {
    print("Getting value")
    return 42
}

fn main() {
    let x = get_value()
    print(x)
    print(x)
}"#;
    let project = Project::single("inline_variable_with_side_effects", source);
    let mut doc = Document::open(project, source);
    let reply = doc.request("textDocument/codeAction", json!({"range": {"start": {"line": 6, "character": 8}, "end": {"line": 6, "character": 8}}, "context": {"diagnostics": []}}));
    assert!(
        reply.is_null()
            || array(&reply)
                .iter()
                .all(|action| action["title"] != "Inline local `x`"),
        "{reply}"
    );
}

#[test]
fn inline_side_effecting_initializer_with_one_use() {
    let source = r#"fn get_value() -> int {
    print("Getting value")
    return 42
}

fn main() {
    let x = get_value()
    print(x)
}"#;
    let project = Project::single("inline_side_effecting_initializer_with_one_use", source);
    let mut doc = Document::open(project, source);
    let reply = doc.request("textDocument/codeAction", json!({"range": {"start": {"line": 6, "character": 8}, "end": {"line": 6, "character": 8}}, "context": {"diagnostics": []}}));
    assert_eq!(
        doc.apply_action(&reply, "Inline local `x`").trim_end(),
        (&source.replace(
            r#"    let x = get_value()
    print(x)
"#,
            "    print(get_value())\n"
        ))
            .trim_end()
    );
}

#[test]
fn no_inline_of_a_side_effecting_initializer_used_in_a_loop() {
    let source = r#"fn get_value() -> int {
    print("Getting value")
    return 42
}

fn main() {
    let x = get_value()
    for i in 0..3 {
        print(x + i)
    }
}"#;
    let project = Project::single(
        "no_inline_of_a_side_effecting_initializer_used_in_a_loop",
        source,
    );
    let mut doc = Document::open(project, source);
    let reply = doc.request("textDocument/codeAction", json!({"range": {"start": {"line": 6, "character": 8}, "end": {"line": 6, "character": 8}}, "context": {"diagnostics": []}}));
    assert!(
        reply.is_null()
            || array(&reply)
                .iter()
                .all(|action| action["title"] != "Inline local `x`"),
        "{reply}"
    );
}

#[test]
fn no_inline_of_a_side_effecting_initializer_used_in_a_closure() {
    let source = r#"fn get_value() -> int {
    print("Getting value")
    return 42
}

fn main() {
    let x = get_value()
    let f = () => x
    print(f())
}"#;
    let project = Project::single(
        "no_inline_of_a_side_effecting_initializer_used_in_a_closure",
        source,
    );
    let mut doc = Document::open(project, source);
    let reply = doc.request("textDocument/codeAction", json!({"range": {"start": {"line": 6, "character": 8}, "end": {"line": 6, "character": 8}}, "context": {"diagnostics": []}}));
    assert!(
        reply.is_null()
            || array(&reply)
                .iter()
                .all(|action| action["title"] != "Inline local `x`"),
        "{reply}"
    );
}

#[test]
fn no_inline_of_an_indexing_initializer_with_two_uses() {
    let source = r#"fn main() {
    let xs = [1, 2, 3]
    let x = xs[0]
    print(x)
    print(x)
}"#;
    let project = Project::single("no_inline_of_an_indexing_initializer_with_two_uses", source);
    let mut doc = Document::open(project, source);
    let reply = doc.request("textDocument/codeAction", json!({"range": {"start": {"line": 2, "character": 8}, "end": {"line": 2, "character": 8}}, "context": {"diagnostics": []}}));
    assert!(
        reply.is_null()
            || array(&reply)
                .iter()
                .all(|action| action["title"] != "Inline local `x`"),
        "{reply}"
    );
}

#[test]
fn inline_pure_field_arithmetic_with_two_uses() {
    let source = r#"struct Point {
    x: int
    y: int
}

fn main() {
    let p = Point { x: 1, y: 2 }
    let s = p.x + p.y
    print(s)
    print(s)
}"#;
    let project = Project::single("inline_pure_field_arithmetic_with_two_uses", source);
    let mut doc = Document::open(project, source);
    let reply = doc.request("textDocument/codeAction", json!({"range": {"start": {"line": 7, "character": 8}, "end": {"line": 7, "character": 8}}, "context": {"diagnostics": []}}));
    assert_eq!(
        doc.apply_action(&reply, "Inline local `s`").trim_end(),
        (&source.replace(
            r#"    let s = p.x + p.y
    print(s)
    print(s)
"#,
            r#"    print(p.x + p.y)
    print(p.x + p.y)
"#
        ))
            .trim_end()
    );
}

#[test]
fn inline_variable_in_if_condition() {
    let source = r#"fn main() {
    let condition = true
    if condition {
        print("Yes")
    }
}"#;
    let project = Project::single("inline_variable_in_if_condition", source);
    let mut doc = Document::open(project, source);
    let reply = doc.request("textDocument/codeAction", json!({"range": {"start": {"line": 1, "character": 8}, "end": {"line": 1, "character": 8}}, "context": {"diagnostics": []}}));
    assert_eq!(
        doc.apply_action(&reply, "Inline local `condition`")
            .trim_end(),
        (&source.replace(
            r#"    let condition = true
    if condition {
"#,
            "    if true {\n"
        ))
            .trim_end()
    );
}

#[test]
fn inline_string_variable() {
    let source = r#"fn main() {
    let message = "Hello, World!"
    print(message)
}"#;
    let project = Project::single("inline_string_variable", source);
    let mut doc = Document::open(project, source);
    let reply = doc.request("textDocument/codeAction", json!({"range": {"start": {"line": 1, "character": 8}, "end": {"line": 1, "character": 8}}, "context": {"diagnostics": []}}));
    assert_eq!(
        doc.apply_action(&reply, "Inline local `message`")
            .trim_end(),
        (&source.replace(
            r#"    let message = "Hello, World!"
    print(message)
"#,
            "    print(\"Hello, World!\")\n"
        ))
            .trim_end()
    );
}

#[test]
fn inline_variable_nested_scope() {
    let source = r#"fn main() {
    let outer = 10
    {
        let inner = outer + 5
        print(inner)
    }
}"#;
    let project = Project::single("inline_variable_nested_scope", source);
    let mut doc = Document::open(project, source);
    let reply = doc.request("textDocument/codeAction", json!({"range": {"start": {"line": 1, "character": 8}, "end": {"line": 1, "character": 8}}, "context": {"diagnostics": []}}));
    assert_eq!(
        doc.apply_action(&reply, "Inline local `outer`").trim_end(),
        (&source.replace("    let outer = 10\n", "").replace(
            "        let inner = outer + 5\n",
            "        let inner = 10 + 5\n"
        ))
            .trim_end()
    );
}

#[test]
fn inline_use_after_unicode_on_the_same_line() {
    let source = "fn main() {\n    let x = 42\n    print(\"\u{00e9}\") print(x)\n}";
    let project = Project::single("inline_use_after_unicode_on_the_same_line", source);
    let mut doc = Document::open(project, source);
    let reply = doc.request("textDocument/codeAction", json!({"range": {"start": {"line": 1, "character": 8}, "end": {"line": 1, "character": 8}}, "context": {"diagnostics": []}}));
    assert_eq!(
        doc.apply_action(&reply, "Inline local `x`").trim_end(),
        (&source
            .replace("    let x = 42\n", "")
            .replace("print(x)", "print(42)"))
            .trim_end()
    );
}
