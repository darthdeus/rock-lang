use crate::common::{emit_module, function_body, run_luajit};

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
        body.contains(r#"repr = "strided""#),
        "the five-leaf flat enum Vec must declare dense strided storage:\n{body}"
    );
    assert!(
        !body.contains("{__variant = "),
        "pushing a strided flat-enum element must not build an enum table:\n{body}"
    );
}

#[test]
fn wide_numeric_flat_enum_vec_round_trips_both_variants() {
    let lua = emit_module(SRC);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "flat_enum_strided"),
        "92"
    );
}

const SURFACE: &str = r#"
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

fn kind_score(kind: Kind) -> int { return match kind { Kind::A => 0, Kind::B => 1 } }

fn score(slot: Slot) -> int {
    return match slot {
        Slot::Entry(row) => int(row.remaining) * 1000 + int(row.duration) * 100 + row.level * 10 + kind_score(row.kind),
        Slot::Overflow(hidden) => hidden,
    }
}

fn main() {
    let slots: [Slot] = [
        Slot::Entry(Row { kind: Kind::B, remaining: 8.0, duration: 9.0, level: 3 }),
        Slot::Overflow(7),
        Slot::Entry(Row { kind: Kind::A, remaining: 2.0, duration: 3.0, level: 4 }),
    ]
    slots.set(1, Slot::Entry(Row { kind: Kind::A, remaining: 5.0, duration: 6.0, level: 7 }))
    slots.insert_at(1, Slot::Overflow(9))
    let removed = slots.remove_at(2)
    let swapped = slots.swap_remove(0)
    slots.push(Slot::Overflow(4))
    let popped = slots.pop()
    let copy = slots.clone()
    copy.sort(score)
    slots.clear()
    slots.push(Slot::Overflow(1))
    let spread: [Slot] = [...copy, Slot::Overflow(12)]
    print("{score(removed.unwrap())}|{score(swapped.unwrap())}|{score(popped.unwrap())}|{score(copy[0])},{score(copy[1])}|{score(slots[0])}|{score(spread[2])}|{copy.capacity()}")
}
"#;

#[test]
fn strided_vec_preserves_the_complete_mutating_surface() {
    let lua = emit_module(SURFACE);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "flat_enum_strided_surface"),
        "5670|8931|4|9,2340|1|12|3"
    );
}

const BRIDGE: &str = r#"
nobox value Row { a: int, b: int, c: int, d: int }
enum Slot { Entry(Row), Overflow(int) }

fn score(slot: Slot) -> int {
    return match slot {
        Slot::Entry(row) => row.a * 1000 + row.b * 100 + row.c * 10 + row.d,
        Slot::Overflow(hidden) => hidden,
    }
}

fn main() {
    let slots: [Slot] = [Slot::Entry(Row { a: 1, b: 2, c: 3, d: 4 }), Slot::Overflow(8)]
    let copy: [Slot] = __rock_vec_from_lua_array(__rock_vec_to_lua_array(slots), slots.len)
    print("{score(copy[0])},{score(copy[1])},{copy.len}")
}
"#;

#[test]
fn erased_array_bridge_materializes_and_reimports_strided_elements() {
    let lua = emit_module(BRIDGE);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "flat_enum_strided_bridge"),
        "1234,8,2"
    );
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
        !body.contains(r#"repr = "strided""#) && body.contains("{__variant = "),
        "a bool payload cannot use false as the inactive-lane sentinel:\n{body}"
    );
}

const DENSE_SENTINEL: &str = r#"
nobox value Row { a: int, b: int, c: int, d: int }
enum Slot { Entry(Row), Overflow(int) }

fn main() {
    let slots: [Slot] = []
    slots.push(Slot::Entry(Row { a: 1, b: 2, c: 3, d: 4 }))
    slots.push(Slot::Overflow(8))
    print("{slots.len},{slots.capacity()}")
}
"#;

#[test]
fn inactive_payload_lanes_remain_physically_dense() {
    let lua = emit_module(DENSE_SENTINEL);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "flat_enum_strided_dense"),
        "2,2"
    );
}

const EQUALITY: &str = r#"
nobox value Row { a: int, b: int, c: int, d: int }
enum Slot { Entry(Row), Overflow(int) }

fn main() {
    let a: [Slot] = [Slot::Entry(Row { a: 1, b: 2, c: 3, d: 4 }), Slot::Overflow(8)]
    let b: [Slot] = []
    b.push(Slot::Entry(Row { a: 1, b: 2, c: 3, d: 4 }))
    b.push(Slot::Overflow(8))
    let c: [Slot] = [Slot::Entry(Row { a: 1, b: 2, c: 3, d: 5 }), Slot::Overflow(8)]
    print("{a == b},{a != b},{a == c},{a != c}")
}
"#;

#[test]
fn strided_vec_equality_compares_logical_elements_not_storage_metadata() {
    let lua = emit_module(EQUALITY);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "flat_enum_strided_eq"),
        "true,false,false,true"
    );
}
