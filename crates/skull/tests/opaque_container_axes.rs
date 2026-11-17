use crate::common::emit_rejection;

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

#[test]
fn an_arena_at_a_column_element_rejects() {
    let msg = emit_rejection(ARENA_PARAM);
    assert!(msg.contains("Arena<G>") && msg.contains("probe"));
}

#[test]
fn an_arena_at_a_boxed_element_is_admitted() {
    assert!(crate::common::try_emit_module(ARENA_BOXED_ELEM).is_ok());
}

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
fn an_opaque_body_returning_a_column_vec_rejects() {
    let msg = emit_rejection(CTOR_RETURNS_COLUMN_VEC);
    assert!(msg.contains("cannot BUILD column-stored `[G]`") && msg.contains("make"));
}

#[test]
fn an_opaque_body_returning_a_boxed_vec_is_admitted() {
    assert!(crate::common::try_emit_module(CTOR_RETURNS_BOXED_VEC).is_ok());
}

#[test]
fn the_stdlib_constructors_still_mint_column_storage() {
    let lua = crate::common::try_emit_module(STDLIB_CTORS_STILL_ROUTE).expect("must emit");
    assert_eq!(crate::common::run_main(&lua), "135");
}
