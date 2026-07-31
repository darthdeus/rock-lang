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

/// A MIXED-width element was the old domain limit ("only int, float, and
/// all-float value structs map"). Leaf addressing widened it: the storage is
/// still one `Float32Array` sized by the C layout, and each leaf C type gets its
/// own TypedArray view over the same `ArrayBuffer`.
#[test]
fn value_struct_with_int_leaf_now_backs_onto_typed_views() {
    let src = r#"
value Mixed { x: float, tag: int }
fn make_mixed(n: int) -> NativeBuffer<Mixed> { return native_buffer_alloc(n) }
"#;
    let ws = ws_with(src);
    let js = emit_user(&ws, "make_mixed");
    // `Mixed` is float(4) + int(4) = 8 bytes -> 2 four-byte slots per element.
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
