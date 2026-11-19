use crate::common::{emit_module, function_body};

const EXTERN_VALUE_RETURN: &str = r#"
value Stats { a: int, b: int }

extern(ffi, "host") fn host_probe_stats() -> Stats

fn main() {
    let s = host_probe_stats()
    print("{s.a}")
}
"#;

#[test]
fn an_extern_value_return_does_not_hand_back_a_reference_into_its_out_param() {
    let lua = emit_module(EXTERN_VALUE_RETURN);
    let wrapper = function_body(&lua, "host_probe_stats");
    assert!(wrapper.contains("ffi.new(\"Stats[1]\")"));
    assert!(!wrapper.contains("return __out[0]"));
    assert!(wrapper.contains("ffi.new(\"Stats\", __out[0])"));
}
