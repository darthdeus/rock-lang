//! `NativeBuffer<Vertex>` (value-struct elements) under JS.
//!
//! The vertex pool is a `NativeBuffer<Vertex>` where `Vertex` is an all-f32
//! value struct (10 four-byte leaves). `native_buffer_alloc<Vertex>(n)` lowers to
//! `FfiNewArray { ctype: "Vertex", count: n }`. The JS emitter previously
//! rejected any non-primitive ctype; it now backs value structs using their C
//! size and overlays typed views for the leaves. An all-f32 value occupies one
//! Float32Array slot per leaf; mixed f32/int values use the same C layout.
use crate::common::{emit_user, emit_user_err, ws_with};

const VERTEX: &str = r#"
value Vertex {
    px: f32, py: f32,
    u: f32, v: f32,
    r: f32, g: f32, b: f32, a: f32,
    pivot_y: f32, entity_id: f32
}
"#;

#[test]
fn all_float_value_struct_nb_lowers_to_float32array() {
    let mut src = String::from(VERTEX);
    src.push_str(
        "fn make_pool(n: int) -> NativeBuffer<Vertex> { return native_buffer_alloc(n) }\n",
    );
    let ws = ws_with(&src);
    let js = emit_user(&ws, "make_pool");
    assert!(
        js.contains("new Float32Array(") && js.contains("* 10"),
        "all-f32 value-struct NativeBuffer must back onto a flat Float32Array(n*10):\n{js}"
    );
}

#[test]
fn value_struct_with_int_leaf_now_backs_onto_typed_views() {
    let src = r#"
value Mixed { x: f32, tag: int }
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

#[test]
fn float_record_uses_a_float64_view_over_its_c_sized_backing() {
    let src = r#"
value Wide { x: float, y: float }
fn make_wide(n: int) -> NativeBuffer<Wide> { return native_buffer_alloc(n) }
"#;
    let ws = ws_with(src);
    let js = emit_user(&ws, "make_wide");
    assert!(
        js.contains("new Float32Array(") && js.contains("* 4"),
        "two double fields need a 16-byte C element backed by four float32 slots:\n{js}"
    );
    assert!(
        js.contains("new Float64Array(") && js.contains(".buffer"),
        "double leaves need a Float64Array view over the backing buffer:\n{js}"
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
