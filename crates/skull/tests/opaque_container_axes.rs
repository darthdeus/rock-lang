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
