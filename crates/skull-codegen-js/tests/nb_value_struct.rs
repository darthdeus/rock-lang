//! `NativeBuffer<Vertex>` (value-struct elements) under JS.
//!
//! The vertex pool is a `NativeBuffer<Vertex>` where `Vertex` is an all-float
//! value struct (10 floats). `native_buffer_alloc<Vertex>(n)` lowers to
//! `FfiNewArray { ctype: "Vertex", count: n }`. The JS emitter previously
//! rejected any non-float/int ctype; it now flattens an all-float value struct
//! to `new Float32Array(n * k)` (k = flattened float leaf count). A value
//! struct with any non-float leaf still rejects.
use crate::common::{emit_user_err, ws_with};

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
