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

const BOOL_PAYLOAD: &str = r#"
nobox value Row { active: bool, a: int, b: int, c: int }
enum Slot { Entry(Row), Overflow(int) }
fn make() -> [Slot] { return [Slot::Entry(Row { active: false, a: 1, b: 2, c: 3 })] }
fn main() { let _ = make() }
"#;
