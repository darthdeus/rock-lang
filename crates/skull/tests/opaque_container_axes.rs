//! The remaining container axes of the 2.4 trigger
//! (`docs/boundary-contracts.md`): `Arena<T>`, `FlatVec<T>`, `NativeBuffer<T>`.
//!
//! 2.4 names five container constructors; only `[T]`/`Vec<T>` and `Map<K, V>`
//! were implemented, and `TypeCtor::Nominal(_)` silently admitted the rest.
//! Measured physical forms, which is what decides each axis:
//!
//! - `Arena<G>` at a column-eligible `G` is
//!   `{generations, occupied, data = <COLUMN vec>, free_indices, occupied_count}`
//!   -- its `data` has per-leaf arrays and no `_data`, on every target.
//! - `FlatVec<G>` and `NativeBuffer<G>` differ BY TARGET, so their axes live in
//!   `skull-codegen-js/tests/opaque_container_axes_js.rs`, where the JS emitter
//!   harness is. This file is the target-independent half.
//!
//! Both answers come from `spec_key_has_target_storage`'s oracle rather than a
//! second derivation.

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

/// Bounds the Arena axis on the ELEMENT: a boxed element keeps `data._data`, so
/// the identical body must stay valid.
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

fn lua_outcome(src: &str) -> Result<String, String> {
    crate::common::try_emit_module(src).map_err(|e| e.message)
}

#[test]
fn an_arena_at_a_column_element_rejects() {
    let msg = lua_outcome(ARENA_PARAM).expect_err("must reject");
    assert!(
        msg.contains("Arena<G>") && msg.contains("probe"),
        "must name the container and the callee, got: {msg}"
    );
}

/// Bounds the axis on the ELEMENT. Without it, a change that rejected every
/// `Arena` param would leave the case above green and read as progress.
#[test]
fn an_arena_at_a_boxed_element_is_admitted() {
    assert!(
        lua_outcome(ARENA_BOXED_ELEM).is_ok(),
        "a boxed element keeps `data._data`, so the same body stays valid"
    );
}

/// The CONSTRUCTOR mirror (2.4): an opaque body whose declared RETURN is a
/// container hand-builds its physical form, so it can mint boxed storage for a
/// column-planned result. The argument rule cannot see a return.
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

/// Bounds it on the ELEMENT: the identical body is CORRECT at a boxed element,
/// which is what the boxed representation is.
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

/// The stdlib constructors are exempt BY CONSTRUCTION (routed at
/// `call.rs:172-201`) and must keep minting COLUMN storage for a column
/// element. Measured with `vec_new`'s route removed: it minted boxed storage
/// and the caller's column read died on nil, with nothing rejecting -- which is
/// the gap the mirror clause closes.
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
    let msg = lua_outcome(CTOR_RETURNS_COLUMN_VEC).expect_err("must reject");
    assert!(
        msg.contains("cannot BUILD column-stored `[G]`") && msg.contains("make"),
        "got: {msg}"
    );
}

#[test]
fn an_opaque_body_returning_a_boxed_vec_is_admitted() {
    assert!(lua_outcome(CTOR_RETURNS_BOXED_VEC).is_ok());
}

#[test]
fn the_stdlib_constructors_still_mint_column_storage() {
    let lua = crate::common::try_emit_module(STDLIB_CTORS_STILL_ROUTE).expect("must emit");
    assert_eq!(
        crate::common::run_luajit(&format!("{lua}\nmain()\n"), "ctor_symmetry"),
        "135",
        "the three constructors mint COLUMN storage, so each leaf reads back"
    );
}
