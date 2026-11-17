const FIELD_GET_NOBOX: &str = r#"
struct Row { pos: Vec2, tag: int }

fn read_pos(r: Row, f: ReadFieldRef<Row, Vec2>) -> float {
    return field_get(r, f).x
}

fn main() {
    let r = Row { pos: v(1.0, 2.0), tag: 0 }
    print("{read_pos(r, .pos)}")
}
"#;

const GENERIC_RETURN_NOBOX: &str = r#"
struct Box2 { p: Vec2 }

@inline inline(lua) fn unwrap_generic<V>(b: Box2) -> V = """
    return b.p
"""

fn main() {
    let a: Vec2 = unwrap_generic(Box2 { p: v(1.0, 2.0) })
    print("x={a.x}")
}
"#;

const FIELD_SET_NOBOX: &str = r#"
struct Row { pos: Vec2, tag: int }

fn write_pos(r: Row, f: WriteFieldRef<Row, Vec2>, val: Vec2) {
    field_set(r, f, val)
}

fn main() {
    let r = Row { pos: v(1.0, 2.0), tag: 0 }
    write_pos(r, .pos, v(99.0, 98.0))
    print("{r.pos.x}")
}
"#;

fn emit_err(src: &str) -> String {
    match crate::common::try_emit_module(src) {
        Ok(_) => String::new(),
        Err(e) => format!("{e}"),
    }
}

#[test]
fn a_field_get_of_a_nobox_field_is_rejected_at_the_return_boundary() {
    let err = emit_err(FIELD_GET_NOBOX);
    assert!(err.contains("cannot cross the boxed return of `field_get`") && err.contains("Vec2"));
}

#[test]
fn a_generic_nobox_return_is_rejected_at_an_opaque_boundary() {
    let err = emit_err(GENERIC_RETURN_NOBOX);
    assert!(
        err.contains("cannot cross the boxed return of `unwrap_generic`") && err.contains("Vec2")
    );
}

#[test]
fn the_write_direction_of_the_same_field_is_rejected() {
    let err = emit_err(FIELD_SET_NOBOX);
    assert!(err.contains("cannot cross boxed call boundary"));
}
