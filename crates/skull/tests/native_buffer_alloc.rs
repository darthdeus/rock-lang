use crate::common::{emit_module, function_body};

const VALUE_ELEM: &str = r#"
value V2 { x: float, y: float }

fn native_buffer_v2(n: int) -> NativeBuffer<V2> {
    return native_buffer_alloc(n)
}

fn main() { let _ = native_buffer_v2(4) }
"#;

#[test]
fn value_struct_element_emits_ctype_ffi_new() {
    let lua = emit_module(VALUE_ELEM);
    let body = function_body(&lua, "native_buffer_v2");
    assert!(body.contains(r#"__rock_nb_alloc("V2","#));
    assert!(body.contains(r#"__rock_box_mts["NativeBuffer"]"#));
    assert!(!body.contains("__nb_alloc_"));
}

const PRIM_ELEM: &str = r#"
fn nbi(n: int) -> NativeBuffer<int> { return native_buffer_alloc(n) }
fn nbf(n: int) -> NativeBuffer<float> { return native_buffer_alloc(n) }

fn main() { let _ = nbi(2); let _ = nbf(2) }
"#;

#[test]
fn primitive_elements_are_subsumed() {
    let lua = emit_module(PRIM_ELEM);
    assert!(function_body(&lua, "nbi").contains(r#"__rock_nb_alloc("int","#));
    assert!(function_body(&lua, "nbf").contains(r#"__rock_nb_alloc("float","#));
}

const GENERIC_PRIMITIVE_ELEM: &str = r#"
fn generic_native_buffer<T>(cap: int) -> NativeBuffer<T> {
    return native_buffer_alloc(cap)
}

fn main() {
    let b: NativeBuffer<int> = generic_native_buffer(8)
    print("{b.len}")
}
"#;

#[test]
fn complete_generic_primitive_instance_owns_native_buffer_ctype() {
    let lua = emit_module(GENERIC_PRIMITIVE_ELEM);
    assert!(lua.contains(r#"__rock_nb_alloc("int"#));
}
