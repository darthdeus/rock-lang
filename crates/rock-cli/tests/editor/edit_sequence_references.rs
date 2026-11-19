use super::*;

#[test]
fn references_shift_after_typing_and_removing_line_in_other_fn() {
    let source = r#"fn alpha() -> int {
    return 1
}

fn beta() -> int {
    let count = 2
    return count + count
}"#;
    let mut doc = Document::open(
        Project::single(
            "references_shift_after_typing_and_removing_line_in_other_fn",
            source,
        ),
        source,
    );
    let reply = doc.request(
        "textDocument/references",
        json!({"position": {"line": 5, "character": 8}, "context": {"includeDeclaration": true}}),
    );
    reference_starts(&reply, &[(5, 8), (6, 11), (6, 19)]);
    doc.type_at(
        1,
        12,
        r#"
    let pad = 0"#,
    );
    let reply = doc.request(
        "textDocument/references",
        json!({"position": {"line": 6, "character": 8}, "context": {"includeDeclaration": true}}),
    );
    reference_starts(&reply, &[(6, 8), (7, 11), (7, 19)]);
    doc.delete_at(2, 15, 16);
    let reply = doc.request(
        "textDocument/references",
        json!({"position": {"line": 5, "character": 8}, "context": {"includeDeclaration": true}}),
    );
    reference_starts(&reply, &[(5, 8), (6, 11), (6, 19)]);
}

#[test]
fn enum_variant_references_shift_after_body_edit() {
    let step_1 = r#"enum Color {
    Red,
    Green,
}

fn alpha() -> int {
    return 1
}

fn beta(c: Color) -> int {
    match c {
        Color::Red => 1,
        Color::Green => 2,
    }
}"#;
    let source = step_1;
    let mut doc = Document::open(
        Project::single("enum_variant_references_shift_after_body_edit", source),
        source,
    );
    let reply = doc.request(
        "textDocument/references",
        json!({"position": {"line": 11, "character": 15}, "context": {"includeDeclaration": true}}),
    );
    reference_starts(&reply, &[(1, 4), (11, 15)]);
    doc.replace(&step_1.replace(
        "    return 1\n",
        r#"    let pad = 0
    return 1 + pad
"#,
    ));
    let reply = doc.request(
        "textDocument/references",
        json!({"position": {"line": 12, "character": 15}, "context": {"includeDeclaration": true}}),
    );
    reference_starts(&reply, &[(1, 4), (12, 15)]);
}

#[test]
fn call_site_references_shift_after_edit_in_other_fn() {
    let step_1 = r#"fn alpha() -> int {
    return 1
}

fn beta() -> int {
    return alpha() + alpha()
}"#;
    let source = step_1;
    let mut doc = Document::open(
        Project::single("call_site_references_shift_after_edit_in_other_fn", source),
        source,
    );
    let reply = doc.request(
        "textDocument/references",
        json!({"position": {"line": 5, "character": 12}, "context": {"includeDeclaration": true}}),
    );
    reference_starts(&reply, &[(0, 3), (5, 11), (5, 21)]);
    doc.replace(&step_1.replace(
        "    return 1\n",
        r#"    let pad = 0
    return 1 + pad
"#,
    ));
    let reply = doc.request(
        "textDocument/references",
        json!({"position": {"line": 6, "character": 12}, "context": {"includeDeclaration": true}}),
    );
    reference_starts(&reply, &[(0, 3), (6, 11), (6, 21)]);
}
