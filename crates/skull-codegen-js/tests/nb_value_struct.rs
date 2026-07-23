//! stone-web arc 2 P3: `NativeBuffer<Vertex>` (all-float value struct) under JS.
//!
//! The vertex pool is a `NativeBuffer<Vertex>` where `Vertex` is an all-float
//! value struct (10 floats). `native_buffer_alloc<Vertex>(n)` lowers to
//! `FfiNewArray { ctype: "Vertex", count: n }`. The JS emitter previously
//! rejected any non-float/int ctype; it now flattens an all-float value struct
//! to `new Float32Array(n * k)` (k = flattened float leaf count). A value
//! struct with any non-float leaf still rejects.

mod common;
use common::{emit_user, emit_user_err, ws_with};

const VERTEX: &str = r#"
value Vertex {
    px: float, py: float,
    u: float, v: float,
    r: float, g: float, b: float, a: float,
    pivot_y: float, entity_id: float
}
"#;

#[test]
fn all_float_value_struct_nb_lowers_to_float32array() {
    let mut src = String::from(VERTEX);
    src.push_str("fn make_pool(n: int) -> NativeBuffer<Vertex> { return native_buffer_alloc(n) }\n");
    let ws = ws_with(&src);
    let js = emit_user(&ws, "make_pool");
    // k = 10 floats per Vertex -> the flat backing is `n * 10`.
    assert!(
        js.contains("new Float32Array(") && js.contains("* 10"),
        "all-float value-struct NativeBuffer must back onto a flat Float32Array(n*10):\n{js}"
    );
}

#[test]
fn value_struct_with_int_leaf_still_rejects() {
    let src = r#"
value Mixed { x: float, tag: int }
fn make_mixed(n: int) -> NativeBuffer<Mixed> { return native_buffer_alloc(n) }
"#;
    let ws = ws_with(src);
    let err = emit_user_err(&ws, "make_mixed");
    assert!(
        err.contains("Mixed") || err.contains("float"),
        "a non-all-float value struct must reject with a clear message:\n{err}"
    );
}
