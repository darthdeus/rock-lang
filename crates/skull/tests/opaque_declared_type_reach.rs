use crate::common::emit_rejection;

#[test]
fn opaque_foreign_boundaries_reject_column_storage_in_every_argument_form() {
    for (generics, parameters, arguments) in [
        ("T", "sink: [T], token: int", "xs, 0"),
        ("T", "sink: [T], token: int", "token: 0, sink: xs"),
        ("T", "sink: [T], token: int = 0", "xs"),
        ("T, U", "sink: [T]", "xs"),
        ("T", "sink: [T]? = null", "xs"),
        ("T", "pair: ([T], int)", "(xs, 0)"),
    ] {
        let source = format!(
            r#"
value G {{ x: float, y: float }}
@nomangle inline(lua) fn probe<{generics}>({parameters}) -> int = "return 0"
fn main() {{
    let xs: [G] = [G {{ x: 1.0, y: 2.0 }}]
    print(probe({arguments}))
}}
"#
        );
        let error = emit_rejection(&source);
        assert!(error.contains("column-stored `[G]`") && error.contains("probe"));
    }
    let error = emit_rejection(
        r#"
value G { x: float, y: float }
@nomangle inline(lua) fn make<T>() -> T = "return {_data = {}, len = 0}"
fn main() { let xs: [G] = make(); print(xs.len) }
"#,
    );
    assert!(error.contains("cannot BUILD column-stored `[G]`") && error.contains("make"));
}
