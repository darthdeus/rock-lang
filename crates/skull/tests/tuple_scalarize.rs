use crate::common::{emit_module, function_body};

const ARENA_BARE_LOOP_UNUSED_ENTITY: &str = r#"
struct Mob { health: int }

fn hot(mobs: Arena<Mob>) -> int {
    let total = 0
    for (e, m) in mobs {
        total += m.health
    }
    return total
}

fn main() {
    let mobs: Arena<Mob> = arena_new()
    insert(mobs, Mob { health: 10 })
    let _ = hot(mobs)
}
"#;

#[test]
fn arena_bare_loop_is_an_allocation_free_counted_for() {
    for src in [ARENA_BARE_LOOP_UNUSED_ENTITY, ARENA_BARE_LOOP_USED_ENTITY] {
        let lua = emit_module(src);
        let hot = function_body(&lua, "hot");
        assert!(hot.contains("for ") && !hot.contains("goto") && !hot.contains("next"));
        assert!(!hot.contains("{"));
    }
}

const ARENA_BARE_LOOP_USED_ENTITY: &str = r#"
struct Mob { health: int }

fn hot(mobs: Arena<Mob>) -> int {
    let alive = 0
    for (e, m) in mobs {
        if get(e) != null {
            alive += 1
        }
    }
    return alive
}

fn main() {
    let mobs: Arena<Mob> = arena_new()
    insert(mobs, Mob { health: 10 })
    let _ = hot(mobs)
}
"#;

#[test]
fn arena_scan_only_boxes_at_the_stored_boundary() {
    let lua = emit_module(include_str!("../../../tests/noalloc/arena_scan.rock"));
    let body = function_body(&lua, "scan_returning_an_entity");
    assert_eq!(body.matches("Entity_ct(").count(), 1);
}
