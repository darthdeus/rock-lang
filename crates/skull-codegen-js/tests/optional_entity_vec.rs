//! A nullable-Entity Vec keeps the destination field's boxed representation
//! when a non-empty literal is assigned to it.

use crate::common::ws_with;

const SRC: &str = r#"
struct Mob { hp: int }

struct EntitySlots {
    slots: [Entity<Mob>?] = [null, null, null, null]
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
            let mob = entity.get() ?? return -3
            total += mob.hp
        }
    }
    return total
}

fn column_sum(slots: [ColumnCell]) -> int {
    let total = 0
    for cell in slots { total += cell.x + cell.y }
    return total
}

fn main() -> int {
    let arena: Arena<Mob> = arena_new()
    let first = arena.insert(Mob { hp: 7 })
    let second = arena.insert(Mob { hp: 9 })
    let slots = EntitySlots {}
    let default_state = slots.slots.len * 10 + if slots.slots[0] == null { 1 } else { 0 }

    let typed: [Entity<Mob>?] = [first, second]
    let literal_slots = EntitySlots { slots: [first, second] }
    slots.slots = [first, second]
    slots.slots.push(first)

    let columns: [ColumnCell] = [ColumnCell { x: 2, y: 3 }]
    columns.push(ColumnCell { x: 4, y: 5 })
    let first_live = if nullable_get(slots, 0) { 10 } else { 0 }
    let second_live = if nullable_get(slots, 1) { 1 } else { 0 }
    return default_state * 1000 + nullable_len(slots) * 100 + first_live + second_live + nullable_iter(slots) + typed.len * 10 + literal_slots.slots.len + column_sum(columns)
}
"#;

fn emit_js() -> String {
    let ws = ws_with(SRC);
    crate::common::emit_closure(&ws, "main")
        .unwrap_or_else(|e| panic!("nullable Entity Vec must emit: {e}"))
}

#[test]
fn nullable_entity_vec_uses_boxed_assignment_and_column_control_survives() {
    let js = emit_js();
    assert!(
        js.contains("slots: ({_data:"),
        "nullable field default is not boxed:\n{js}"
    );
    assert!(
        js.contains("\"@vec\": {repr: \"columns\", elem: \"ColumnCell\""),
        "column control stopped using columns:\n{js}"
    );
    assert!(
        !js.contains("repr: \"columns\", elem: \"Entity\""),
        "nullable Entity assignment selected columns:\n{js}"
    );
}

#[test]
fn nullable_entity_vec_round_trips_through_node() {
    let js = emit_js();
    let program = format!(
        "{}{}{}{}{}{}\n{js}\nconsole.log(main());",
        skull_codegen_js::JS_GLOBALS_RUNTIME,
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_TABLE_PRIM,
        skull_codegen_js::JS_STRUCTURAL_EQ,
        skull_codegen_js::JS_STRING_RUNTIME,
    );
    let out = crate::common::run_node(&program);
    assert_eq!(out.trim(), "41370");
}
