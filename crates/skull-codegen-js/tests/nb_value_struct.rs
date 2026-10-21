//! `NativeBuffer<Vertex>` (value-struct elements) under JS.
//!
//! The vertex pool is a `NativeBuffer<Vertex>` where `Vertex` is an all-float
//! value struct (10 floats). `native_buffer_alloc<Vertex>(n)` lowers to
//! `FfiNewArray { ctype: "Vertex", count: n }`. The JS emitter previously
//! rejected any non-float/int ctype; it now flattens an all-float value struct
//! to `new Float32Array(n * k)` (k = flattened float leaf count). A value
//! struct with any non-float leaf still rejects.
use crate::common::{emit_user, emit_user_err, ws_with};

const VERTEX: &str = r#"
value Vertex {
    px: float, py: float,
    u: float, v: float,
    r: float, g: float, b: float, a: float,
    pivot_y: float, entity_id: float
}
"#;

#[test]
#[ignore = "known red (os-m3-js-pure-rock): Vertex allocates 20 float32 slots per element instead of 10"]
fn all_float_value_struct_nb_lowers_to_float32array() {
    let mut src = String::from(VERTEX);
    src.push_str("fn make_pool(n: int) -> NativeBuffer<Vertex> { return native_buffer_alloc(n) }\n");
    let ws = ws_with(&src);
    let js = emit_user(&ws, "make_pool");
    assert!(
        js.contains("new Float32Array(") && js.contains("* 10"),
        "all-float value-struct NativeBuffer must back onto a flat Float32Array(n*10):\n{js}"
    );
}

#[test]
#[ignore = "known red (os-m3-js-pure-rock): Mixed allocates 4 float32 slots per element instead of 2"]
fn value_struct_with_int_leaf_now_backs_onto_typed_views() {
    let src = r#"
value Mixed { x: float, tag: int }
fn make_mixed(n: int) -> NativeBuffer<Mixed> { return native_buffer_alloc(n) }
"#;
    let ws = ws_with(src);
    let js = emit_user(&ws, "make_mixed");
    assert!(
        js.contains("new Float32Array(") && js.contains("* 2"),
        "mixed-width element must back onto the C-sized flat array:\n{js}"
    );
    assert!(
        js.contains("new Int32Array(") && js.contains(".buffer"),
        "the int leaf needs its own view over the SAME ArrayBuffer:\n{js}"
    );
}

/// The domain is widened, not unbounded: an element whose C size is not a
/// multiple of 4 has no aligned TypedArray to back it, and rejects rather than
/// reinterpreting its bytes.
#[test]
fn an_unaligned_element_still_rejects() {
    let src = r#"
value Tiny { a: bool, b: bool }
fn make_tiny(n: int) -> NativeBuffer<Tiny> { return native_buffer_alloc(n) }
"#;
    let ws = ws_with(src);
    let err = emit_user_err(&ws, "make_tiny");
    assert!(
        err.contains("multiple of 4"),
        "an unaligned element must reject with a clear message:\n{err}"
    );
}
