use crate::common::emit_rejection;

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
@lua_named struct Box2 { p: Vec2 }

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

#[test]
fn nobox_values_are_rejected_at_boxed_generic_boundaries() {
    for (src, needles) in [
        (
            FIELD_GET_NOBOX,
            &["cannot cross the boxed return of `field_get`", "Vec2"][..],
        ),
        (
            GENERIC_RETURN_NOBOX,
            &["cannot cross the boxed return of `unwrap_generic`", "Vec2"][..],
        ),
        (FIELD_SET_NOBOX, &["cannot cross boxed call boundary"][..]),
    ] {
        let err = emit_rejection(src);
        for needle in needles {
            assert!(err.contains(needle), "{err}");
        }
    }
}
