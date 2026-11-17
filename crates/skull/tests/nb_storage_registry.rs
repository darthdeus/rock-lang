use crate::common::emit_module;

const VALUE_ELEM: &str = r#"
value Mix { x: f32, id: int, on: bool }

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
        assert!(lua.contains(&want));
    }
}

#[test]
fn a_value_element_registers_its_luajit_width_and_every_leaf_view() {
    let lua = emit_module(VALUE_ELEM);
    // f32 + int + bool with C padding: 4 + 4 + 1 -> 12 (align 4).
    let line = lua
        .lines()
        .find(|l| l.starts_with("__rock_nb_storage(\"Mix\","))
        .unwrap_or_else(|| panic!("no `Mix` storage registration in:\n{lua}"));
    assert!(line.contains("\"Mix\", 12,"));
    for view in ["__vf = \"float*\"", "__vi = \"int*\"", "__vb = \"bool*\""] {
        assert!(line.contains(view));
    }
}

#[test]
fn the_registry_is_keyed_through_ffi_typeof() {
    let lua = emit_module(VALUE_ELEM);
    assert!(lua.contains("_G.__ROCK_NB_STORAGE[tostring(ct)]"));
}

#[test]
fn a_width_that_disagrees_with_luajit_aborts_at_load() {
    let lua = emit_module(VALUE_ELEM);
    assert!(lua.contains("skull storage drift: sizeof("));
}
