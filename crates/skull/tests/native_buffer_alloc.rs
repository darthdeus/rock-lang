//! `native_buffer_alloc<T>` intrinsic (docs/flatvec-plan.md). skull owns the
//! value-struct cdef names, so a call resolves its concrete element type at the
//! site and emits `ffi.new("<ctype>[?]", n)` wrapped in a `NativeBuffer` -- no
//! per-type `__nb_alloc_*` boilerplate. The rejection of a non-allocatable
//! element (heap struct, opaque generic `T`) is a HARD emit-time error covered
//! by `tests/errors/native_buffer_alloc_*.rock.err`; here we pin the positive
//! shape: the emitted ffi ctor carries the right ctype for value / int / float.
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
    assert!(
        body.contains(r#"ffi.new("V2[?]","#),
        "expected `ffi.new(\"V2[?]\", ...)` for the value-struct element, got:\n{body}"
    );
    // The wrapper builds a NativeBuffer around the ffi array; no per-type ctor.
    assert!(
        body.contains(r#"__ty = "NativeBuffer""#),
        "expected the result wrapped in a NativeBuffer struct, got:\n{body}"
    );
    assert!(
        !body.contains("__nb_alloc_"),
        "intrinsic must not route through a per-type `__nb_alloc_*` fn, got:\n{body}"
    );
}

const PRIM_ELEM: &str = r#"
fn nbi(n: int) -> NativeBuffer<int> { return native_buffer_alloc(n) }
fn nbf(n: int) -> NativeBuffer<float> { return native_buffer_alloc(n) }

fn main() { let _ = nbi(2); let _ = nbf(2) }
"#;

#[test]
fn primitive_elements_are_subsumed() {
    let lua = emit_module(PRIM_ELEM);
    assert!(
        function_body(&lua, "nbi").contains(r#"ffi.new("int[?]","#),
        "int element must emit `ffi.new(\"int[?]\", ...)`"
    );
    assert!(
        function_body(&lua, "nbf").contains(r#"ffi.new("float[?]","#),
        "float element must emit `ffi.new(\"float[?]\", ...)`"
    );
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
    assert!(
        lua.contains(r#"ffi.new("int[?]"#),
        "the concrete primitive instance must publish its own native-buffer ctype, got:\n{lua}"
    );
}
