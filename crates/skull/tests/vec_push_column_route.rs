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

fn reaches_boxed_body(lua: &str) -> bool {
    // `main`'s body: the always-emitted `vec_push` definition contains `._data`
    // too, so a module-wide match would answer yes for any program.
    function_body(lua, "main").contains("._data")
}

#[test]
fn a_column_receiver_never_reaches_the_boxed_body() {
    let lua = emit_module(COLUMN_RECEIVER);
    assert!(!reaches_boxed_body(&lua));
    assert_eq!(run_main(&lua), "7");
}

#[test]
fn a_boxed_receiver_does_reach_the_boxed_body() {
    let lua = emit_module(BOXED_RECEIVER);
    assert!(reaches_boxed_body(&lua));
    let defs: Vec<&str> = lua
        .match_indices("function vec_push")
        .filter_map(|(i, _)| lua[i..].lines().next())
        .collect();
    assert_eq!(defs.len(), 1);
    assert_eq!(run_main(&lua), "7");
}

#[test]
fn a_column_payload_reaches_the_boxed_body_and_is_correct() {
    let lua = emit_module(COLUMN_PAYLOAD);
    assert!(reaches_boxed_body(&lua));
    assert_eq!(run_main(&lua), "1");
}
