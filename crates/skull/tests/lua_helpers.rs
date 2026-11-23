use crate::common::{full_lua, ws_with_stdlib};

#[test]
fn lua_helpers_are_emitted_only_when_an_emitted_inline_body_names_them() {
    let hello = full_lua(&ws_with_stdlib("fn main() { print(\"hi\") }"));
    assert!(!hello.contains("function __json_record_visit("));
    assert!(!hello.contains("function __json_record_source_ws("));

    let writer = full_lua(&ws_with_stdlib(
        "struct P { x: int }\nfn main() { print(json_record_write(JsonRecordCodec {}, P { x: 1 })) }",
    ));
    // Named only in inline text: the encoder and visitor by the writer entry,
    // the field reader by the visitor.
    for helper in [
        "__json_record_encoder",
        "__json_record_visit",
        "__json_record_field_get",
    ] {
        assert!(
            writer.contains(&format!("function {helper}(")),
            "{helper} missing"
        );
    }
}
