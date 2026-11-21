use crate::common::{emit_module, function_body};

#[test]
fn optional_value_return_uses_a_presence_lane() {
    let lua = emit_module(include_str!(
        "../../../tests/noalloc/optional_value_wire.rock"
    ));
    let body = function_body(&lua, "mk_mix");
    assert!(body.contains("do return (true), "));
    assert!(body.contains("do return (false) end"));
}
