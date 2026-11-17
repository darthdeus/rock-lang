use crate::common::ws_with;

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

fn emit_main(src: &str) -> Result<String, skull_parser::CompilerError> {
    let ws = ws_with(src);
    let root = crate::common::test_fn_ref(&ws, "main");
    skull_codegen_js::emit::emit_with_closure(&ws, &[(root, "main".to_string())])
}

fn reject(src: &str) -> skull_parser::CompilerError {
    emit_main(src).expect_err("invalid native-buffer element must reject")
}

fn assert_planner_rejection(error: &skull_parser::CompilerError, label: &str, element: &str) {
    let msg = &error.message;
    assert!(
        msg.contains("requires a valid native-buffer element ctype") && msg.contains(element),
        "{label}: {msg}"
    );
    assert!(
        !error.labels.is_empty(),
        "{label}: native-buffer rejection must be located"
    );
}

#[test]
fn invalid_native_buffer_elements_reject_at_the_planner_boundary() {
    for (label, source, element) in [
        ("NativeBuffer<string>", NATIVEBUFFER_STRING, "string"),
        ("FlatVec<string>", FLATVEC_STRING, "string"),
        ("NativeBuffer<GcValue>", GC_VALUE, "GcValue"),
    ] {
        let error = reject(source);
        assert_planner_rejection(&error, label, element);
    }
}

#[test]
fn native_buffer_primitives_and_ffi_value_are_valid_controls() {
    let js = emit_main(VALID_CONTROLS).expect("valid c-mappable elements must emit");
    assert!(js.contains("Int32Array") && js.contains("Uint32Array") && js.contains("Float32Array"));
    assert!(
        js.contains("Uint8Array"),
        "bool must have a concrete JS byte storage"
    );
}

#[test]
fn unaligned_value_element_rejects_instead_of_reinterpreting_bytes() {
    let error = emit_main(
        r#"
value Tiny { a: bool, b: bool }
fn make_tiny(n: int) -> NativeBuffer<Tiny> { return native_buffer_alloc(n) }
fn main() {
    let buffer = make_tiny(1)
    print("{buffer.len}")
}
"#,
    )
    .expect_err("a two-byte C element has no aligned TypedArray backing");
    assert!(error.message.contains("multiple of 4"), "{}", error.message);
}
