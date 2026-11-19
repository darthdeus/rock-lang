use crate::common::{compile_and_run, emit_module, emit_rejection};

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
    return {_data = {}, len = 0, __ty = "Vec"}
"""

fn main() {
    let xs: [G] = make()
    print("{xs.len}")
}
"#;

const CTOR_RETURNS_BOXED_VEC: &str = r#"
struct B { s: string }

@nomangle inline(lua) fn make() -> [B] = """
    return {_data = {}, len = 0, __ty = "Vec"}
"""

fn main() {
    let xs: [B] = make()
    print("{xs.len}")
}
"#;

const STDLIB_CTORS_STILL_ROUTE: &str = r#"
value G { x: float, y: float }

fn main() {
    let a: [G] = vec_new()
    a.push(G { x: 1.0, y: 2.0 })
    let b: [G] = vec_new_with_capacity(4)
    b.push(G { x: 3.0, y: 4.0 })
    let m: Map<string, G> = map_new()
    m["k"] = G { x: 5.0, y: 6.0 }
    print("{a[0].x}{b[0].x}{m[\"k\"].x}")
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

#[test]
fn the_stdlib_constructors_still_mint_column_storage() {
    assert_eq!(compile_and_run(STDLIB_CTORS_STILL_ROUTE), "135");
}
