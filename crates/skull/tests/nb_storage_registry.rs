//! `_G.__ROCK_NB_STORAGE`: the compiler's description of concrete NativeBuffer
//! storage (`docs/runtime-world-plan.md` S1). A holder of a storage cdata asks
//! it what the bytes mean -- element ctype, element width, and the leaf views
//! minted over that element -- instead of reading a `__ty` spelling, which names
//! the wrapper and never the element.
//!
//! Both halves come from the same generator: the primitive/sized element types
//! from `NB_PRIM_ELEMS`, the value-struct ones from the layout oracle the
//! emitter itself uses. So the widths here are the widths `ffi.new` allocates,
//! and the view set is the set element access indexes.
use crate::common::emit_module;

const VALUE_ELEM: &str = r#"
value Mix { x: float, id: int, on: bool }

fn nbm(n: int) -> NativeBuffer<Mix> { return native_buffer_alloc(n) }

fn main() { let _ = nbm(4) }
"#;

#[test]
fn primitive_and_sized_element_types_are_registered() {
    let lua = emit_module("fn main() { let _ = 1 }");
    for (cty, size) in [
        ("float", 4),
        ("int", 4),
        ("bool", 1),
        ("uint8_t", 1),
        ("int8_t", 1),
        ("uint16_t", 2),
        ("int16_t", 2),
        ("uint32_t", 4),
    ] {
        let want = format!("__rock_nb_storage(\"{cty}\", {size}, ");
        assert!(
            lua.contains(&want),
            "expected `{want}...)` in the prelude; a sized buffer whose element is \
             undescribed cannot be imaged, got prelude:\n{}",
            &lua[..lua.len().min(4000)]
        );
    }
}

#[test]
fn a_value_element_registers_its_luajit_width_and_every_leaf_view() {
    let lua = emit_module(VALUE_ELEM);
    // float + int + bool with C padding: 4 + 4 + 1 -> 12 (align 4).
    let line = lua
        .lines()
        .find(|l| l.starts_with("__rock_nb_storage(\"Mix\","))
        .unwrap_or_else(|| panic!("no `Mix` storage registration in:\n{lua}"));
    assert!(line.contains("\"Mix\", 12,"), "expected `Mix` to be 12 bytes wide, got: {line}");
    for view in ["__vf = \"float*\"", "__vi = \"int*\"", "__vb = \"bool*\""] {
        assert!(
            line.contains(view),
            "a mixed-leaf element must register every view its access path uses; \
             missing `{view}` in: {line}"
        );
    }
}

/// The registration and the lookup have to agree on the ctype spelling, and only
/// LuaJIT decides what a typedef'd anonymous struct is called. Registering
/// through `ffi.typeof` is what makes them agree -- a registration keyed on the
/// Rock/C name would never match the allocation's own ctype.
#[test]
fn the_registry_is_keyed_through_ffi_typeof() {
    let lua = emit_module(VALUE_ELEM);
    assert!(
        lua.contains("_G.__ROCK_NB_STORAGE[tostring(ct)]"),
        "expected the registry keyed by `tostring(ffi.typeof(ctype .. \"[?]\"))`"
    );
}

/// The declared width is asserted against LuaJIT at load. A drift here is not a
/// wrong number in a table: it is a byte image of the wrong length copied back
/// over an allocation.
#[test]
fn a_width_that_disagrees_with_luajit_aborts_at_load() {
    let lua = emit_module(VALUE_ELEM);
    assert!(
        lua.contains("skull storage drift: sizeof("),
        "expected a load-time width cross-check against ffi.sizeof"
    );
}
