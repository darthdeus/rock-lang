use crate::common::{emit_module, function_body, run_main};

const COLUMN_RECEIVER: &str = r#"
value PushCell { a: int, b: int }

fn main() {
    let xs: [PushCell] = []
    xs.push(PushCell { a: 7, b: 8 })
    print("{xs[0].a}")
}
"#;

const BOXED_RECEIVER: &str = r#"
fn main() {
    let ns: [int] = []
    ns.push(7)
    print("{ns[0]}")
}
"#;

const COLUMN_PAYLOAD: &str = r#"
value PushCell { a: int, b: int }

fn main() {
    let outer: [[PushCell]] = []
    let inner: [PushCell] = [PushCell { a: 1, b: 2 }]
    outer.push(inner)
    print("{outer[0][0].a}")
}
"#;

#[test]
fn only_boxed_receivers_and_column_payloads_reach_the_boxed_push_body() {
    for (src, boxed, expected) in [
        (COLUMN_RECEIVER, false, "7"),
        (BOXED_RECEIVER, true, "7"),
        (COLUMN_PAYLOAD, true, "1"),
    ] {
        let lua = emit_module(src);
        assert_eq!(
            function_body(&lua, "main").contains("._data"),
            boxed,
            "{src}"
        );
        assert!(lua.matches("function vec_push").count() <= 1, "{src}");
        assert_eq!(run_main(&lua), expected, "{src}");
    }
}
