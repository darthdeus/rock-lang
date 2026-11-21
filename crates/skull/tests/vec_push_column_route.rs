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
