//! A nullable-Entity Vec must keep one representation across field defaults,
//! field assignment, mutation, and separately emitted consumers.

use crate::common::{run_luajit, try_emit_module};

const SRC: &str = r#"
struct Mob { hp: int }

struct EntitySlots {
    slots: [Entity<Mob>?] = [null, null, null, null]
}

struct BoxedSlots {
    slots: [string] = ["seed"]
}

value ColumnCell { x: int, y: int }

fn nullable_len(slots: EntitySlots) -> int { return slots.slots.len }

fn nullable_get(slots: EntitySlots, index: int) -> bool {
    let entity = slots.slots.get(index) ?? return false
    return entity.get() != null
}

fn nullable_iter(slots: EntitySlots) -> int {
    let total = 0
    for entity in slots.slots {
        if entity != null {
            let mob = entity.unwrap().get() ?? return -3
            total += mob.hp
        }
    }
    return total
}

fn boxed_len(slots: BoxedSlots) -> int { return slots.slots.len }

fn column_sum(slots: [ColumnCell]) -> int {
    let total = 0
    for cell in slots { total += cell.x + cell.y }
    return total
}

fn main() {
    let arena: Arena<Mob> = arena_new()
    let first = arena.insert(Mob { hp: 7 })
    let second = arena.insert(Mob { hp: 9 })
    let slots = EntitySlots {}
    let default_state = nullable_len(slots) * 10 + if slots.slots[0] == null { 1 } else { 0 }

    let typed: [Entity<Mob>?] = [first, second]
    let literal_slots = EntitySlots { slots: [first, second] }
    slots.slots = [first, second]
    slots.slots.push(first)

    let boxed = BoxedSlots {}
    boxed.slots.push("live")
    let columns: [ColumnCell] = [ColumnCell { x: 2, y: 3 }]
    columns.push(ColumnCell { x: 4, y: 5 })

    print("{default_state} {nullable_len(slots)} {nullable_get(slots, 0)} {nullable_get(slots, 1)} {nullable_iter(slots)} {typed.len} {nullable_len(literal_slots)} {boxed_len(boxed)} {column_sum(columns)}")
}
"#;

#[test]
fn nullable_entity_field_assignment_and_consumers_round_trip() {
    let lua = try_emit_module(SRC).expect("fixture must emit");
    assert!(lua.contains(r#"slots = ({_data ="#), "nullable Entity assignment is not boxed:\n{}", lua);
    assert!(lua.contains(r#"repr = "columns", elem = "ColumnCell""#), "column control stopped using columns:\n{}", lua);
    assert!(!lua.contains(r#"repr = "columns", elem = "Entity""#), "nullable Entity assignment selected columns:\n{}", lua);
    let out = run_luajit(&format!("{lua}\nmain()\n"), "optional_entity_vec");
    assert_eq!(out, "41 3 true true 23 2 2 2 14");
}
