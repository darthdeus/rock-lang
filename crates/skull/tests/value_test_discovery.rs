use crate::common::{module_id, ws_with_stdlib};

#[test]
fn invalid_test_annotations_preserve_diagnostics() {
    for (annotation, message) in [
        ("returns = 42, expect_failure = true", "expect_failure"),
        (
            "returns = \"str\"",
            "returns expects an int/float/bool literal",
        ),
        ("returns = foo", "returns expects an int/float/bool literal"),
        (
            "output = \"Vec { 1, 2 }\"",
            "output expects a plain string literal",
        ),
        ("output = 42", "output expects a plain string literal"),
        ("backends = \"lua, luajit\"", "unknown backend `luajit`"),
    ] {
        let ws = ws_with_stdlib(&format!(
            "@test({annotation})\nfn a() -> int {{ return 1 }}"
        ));
        let error = skull::module_tests(&ws, module_id(&ws, "<test>")).unwrap_err();
        assert!(error.message.contains(message), "{error:?}");
    }
}
