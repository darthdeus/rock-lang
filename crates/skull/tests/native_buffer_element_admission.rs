use crate::common::{emit_module, try_emit_module};

const NATIVEBUFFER_STRING: &str = r#"
fn bad(n: int) -> NativeBuffer<string> { return native_buffer_alloc(n) }
fn main() { let b = bad(2); print("{b.len}") }
"#;

const FLATVEC_STRING: &str = r#"
fn bad(n: int) -> FlatVec<string> { return flat_vec_new(n) }
fn main() { let b = bad(2); print("{b.len}") }
"#;

const GC_VALUE: &str = r#"
value GcValue { text: string }
fn bad(n: int) -> NativeBuffer<GcValue> { return native_buffer_alloc(n) }
fn main() { let b = bad(2); print("{b.len}") }
"#;

const VALID_CONTROLS: &str = r#"
value Pair { x: float, y: int }
fn int_buffer(n: int) -> NativeBuffer<int> { return native_buffer_alloc(n) }
fn uint_buffer(n: int) -> NativeBuffer<uint> { return native_buffer_alloc(n) }
fn float_buffer(n: int) -> NativeBuffer<float> { return native_buffer_alloc(n) }
fn bool_buffer(n: int) -> NativeBuffer<bool> { return native_buffer_alloc(n) }
fn pair_buffer(n: int) -> NativeBuffer<Pair> { return native_buffer_alloc(n) }
fn main() {
    let _ = int_buffer(1)
    let _ = uint_buffer(1)
    let _ = float_buffer(1)
    let _ = bool_buffer(1)
    let _ = pair_buffer(1)
}
"#;

#[test]
fn non_c_elements_reject_at_the_planner_boundary() {
    for (src, element) in [
        (NATIVEBUFFER_STRING, "string"),
        (FLATVEC_STRING, "string"),
        (GC_VALUE, "GcValue"),
    ] {
        let error = try_emit_module(src).expect_err(element);
        let msg = &error.message;
        assert!(
            msg.contains("requires a valid native-buffer element ctype") && msg.contains(element)
        );
        assert!(!error.labels.is_empty());
    }
}

#[test]
fn native_buffer_primitives_and_ffi_value_are_valid_controls() {
    emit_module(VALID_CONTROLS);
}
