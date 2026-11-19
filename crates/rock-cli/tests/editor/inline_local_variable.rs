use super::*;

#[test]
fn inline_use_after_unicode_on_the_same_line() {
    let source = "fn main() {\n    let x = 42\n    print(\"\u{00e9}\") print(x)\n}";
    let project = Project::single("inline_use_after_unicode_on_the_same_line", source);
    let mut doc = Document::open(project, source);
    let reply = doc.request("textDocument/codeAction", json!({"range": {"start": {"line": 1, "character": 8}, "end": {"line": 1, "character": 8}}, "context": {"diagnostics": []}}));
    assert_eq!(
        doc.apply_action(&reply, "Inline local `x`").trim_end(),
        source
            .replace("    let x = 42\n", "")
            .replace("print(x)", "print(42)")
            .trim_end()
    );
}
