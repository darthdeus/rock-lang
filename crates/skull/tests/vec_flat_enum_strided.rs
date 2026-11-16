use crate::common::{emit_module, function_body, run_main};

const SRC: &str = r#"
enum Kind { A, B }

nobox value Row {
    kind: Kind,
    remaining: float,
    duration: float,
    level: int,
}

enum Slot {
    Entry(Row),
    Overflow(int),
}

fn exercise() -> int {
    let slots: [Slot] = []
    slots.push(Slot::Entry(Row {
        kind: Kind::B,
        remaining: 8.0,
        duration: 9.0,
        level: 3,
    }))
    slots.push(Slot::Overflow(7))
    let checksum = 0
    for slot in slots {
        checksum += match slot {
            Slot::Entry(row) => int(row.remaining) + int(row.duration) + row.level,
            Slot::Overflow(hidden) => hidden * 10,
        }
    }
    return checksum + slots.len()
}

fn main() { print("{exercise()}") }
"#;

#[test]
fn wide_numeric_flat_enum_vec_uses_dense_strided_storage_without_element_boxes() {
    let lua = emit_module(SRC);
    let body = function_body(&lua, "exercise");
    assert!(
        crate::common::has_vec_header(body, "strided", None),
        "the five-leaf flat enum Vec must declare dense strided storage:\n{body}"
    );
    assert!(
        !body.contains("{__variant = "),
        "pushing a strided flat-enum element must not build an enum table:\n{body}"
    );
    assert_eq!(run_main(&lua), "92");
}

const BOOL_PAYLOAD: &str = r#"
nobox value Row { active: bool, a: int, b: int, c: int }
enum Slot { Entry(Row), Overflow(int) }
fn make() -> [Slot] { return [Slot::Entry(Row { active: false, a: 1, b: 2, c: 3 })] }
fn main() { let _ = make() }
"#;

#[test]
fn false_capable_payload_keeps_the_existing_boxed_representation() {
    let lua = emit_module(BOOL_PAYLOAD);
    let body = function_body(&lua, "make");
    assert!(
        !crate::common::has_vec_header(body, "strided", None) && body.contains("{__variant = "),
        "a bool payload cannot use false as the inactive-lane sentinel:\n{body}"
    );
}
