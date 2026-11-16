//! The typed-JSON type table: emitted only for record conversions, limited to
//! the types they reach, with the field factories those types call.
use crate::common::ws_with;

#[test]
fn record_conversion_emits_reachable_types_and_their_factories() {
    let ws = ws_with(
        r#"
struct Inner {
    @json_default
    label: string = "none",
}
struct Outer { inner: Inner, count: int }
struct Unrelated { flag: bool }

fn main() {
    let codec = JsonRecordCodec {}
    let wire = json_record_encode(codec, Outer { inner: Inner {}, count: 1 })
    let other = Unrelated { flag: true }
    print("{json_encode(wire)} {other.flag}")
}
"#,
    );
    let prog = crate::common::emit_program_js(&ws).expect("emit js program");
    let table = prog
        .split("var __rock_json_types = ")
        .nth(1)
        .and_then(|rest| rest.split("\n};\n").next())
        .unwrap_or_else(|| panic!("record encoding must emit the type table:\n{prog}"));
    assert!(table.contains("\"Outer\": {kind:") && table.contains("\"Inner\": {kind:"));
    assert!(
        !table.contains("\"Unrelated\""),
        "a type no record conversion reaches stays out of the table:\n{table}"
    );
    let factory = table
        .split("json_default: ")
        .nth(1)
        .and_then(|rest| rest.split(&[',', '}'][..]).next())
        .unwrap_or_else(|| panic!("a defaulted field names its factory:\n{table}"));
    assert!(
        prog.contains(&format!("function {factory}(")),
        "the factory the table names must be emitted:\n{prog}"
    );
}

#[test]
fn programs_without_record_conversion_emit_no_type_table() {
    let ws = ws_with(
        r#"
struct Point { x: int }

fn main() {
    let p = Point { x: 1 }
    print("{json_encode(JsonValue::Int(p.x))}")
}
"#,
    );
    let prog = crate::common::emit_program_js(&ws).expect("emit js program");
    assert!(!prog.contains("__rock_json_types"), "{prog}");
}
