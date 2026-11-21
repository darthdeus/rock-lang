use crate::common::{emit_module, emit_rejection};

const ARENA_PARAM: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe(a: Arena<G>) -> int = """
    return a.data.len
"""

fn main() {
    let a: Arena<G> = arena_new()
    print("{probe(a)}")
}
"#;

const ARENA_BOXED_ELEM: &str = r#"
struct B { s: string }

@nomangle inline(lua) fn probe(a: Arena<B>) -> int = """
    return a.data.len
"""

fn main() {
    let a: Arena<B> = arena_new()
    print("{probe(a)}")
}
"#;

const CTOR_RETURNS_COLUMN_VEC: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn make() -> [G] = """
    return {_data = {}, len = 0}
"""

fn main() {
    let xs: [G] = make()
    print("{xs.len}")
}
"#;

const CTOR_RETURNS_BOXED_VEC: &str = r#"
struct B { s: string }

@nomangle inline(lua) fn make() -> [B] = """
    return {_data = {}, len = 0}
"""

fn main() {
    let xs: [B] = make()
    print("{xs.len}")
}
"#;

#[test]
fn opaque_bodies_reject_column_storage_and_admit_boxed_storage() {
    for (src, needles) in [
        (ARENA_PARAM, ["Arena<G>", "probe"]),
        (
            CTOR_RETURNS_COLUMN_VEC,
            ["cannot BUILD column-stored `[G]`", "make"],
        ),
    ] {
        let msg = emit_rejection(src);
        assert!(needles.iter().all(|n| msg.contains(n)), "{msg}");
    }
    emit_module(ARENA_BOXED_ELEM);
    emit_module(CTOR_RETURNS_BOXED_VEC);
}
