use crate::common::{emit_module, function_body, run_main};

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
fn arena_bare_loop_used_entity_scalarizes_into_call() {
    let lua = emit_module(ARENA_BARE_LOOP_USED_ENTITY);
    let hot = function_body(&lua, "hot");
    assert!(!hot.contains("Entity_vt(") && !hot.contains("Entity_ct("));
    assert!(
        crate::common::definitions_of(&lua, "get_Entity")
            .iter()
            .any(|global| {
                let callee = crate::common::bound_callee_name(&lua, "hot", global);
                hot.contains(&format!("{callee}("))
            })
    );
}

#[test]
fn arena_scan_only_boxes_at_the_stored_boundary() {
    let lua = emit_module(include_str!("../../../tests/noalloc/arena_scan.rock"));
    let body = function_body(&lua, "scan_returning_an_entity");
    assert_eq!(body.matches("Entity_ct(").count(), 1);
}

const ARENA_ENTITY_IN_ENUM: &str = r#"
struct Mob { health: int }

enum Target {
    Player,
    Mob(Entity<Mob>),
}

fn score(target: Target) -> int {
    return match target {
        Target::Mob(found) => if get(found) != null { 1 } else { 0 },
        Target::Player => 0,
    }
}

fn hot(mobs: Arena<Mob>) -> int {
    let total = 0
    for (e, _m) in mobs {
        total += score(Target::Mob(e))
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
fn arena_entity_threads_through_enum_payload_without_boxing() {
    let lua = emit_module(ARENA_ENTITY_IN_ENUM);
    let hot = function_body(&lua, "hot");
    assert!(!hot.contains("Entity_vt(") && !hot.contains("Entity_ct("));
}

const NULLABLE_VALUE_RETURN: &str = r#"
value Bag { xs: [int] }

@nomangle inline(lua) fn __out(s: string) = "io.write(s, ' ')"

fn pick(c: bool) -> Bag? {
    let v: Bag? = null
    if c { v = Bag { xs: [1] } }
    return v
}

fn main() {
    let present = pick(true).unwrap()
    __out("{present.xs.len} {present.xs[0]} {pick(false) == null}")
}
"#;

#[test]
fn nullable_value_binding_return_keeps_presence_and_leaves() {
    let lua = emit_module(NULLABLE_VALUE_RETURN);
    let pick = function_body(&lua, "pick");
    assert_eq!(pick.matches("Bag_vt(").count(), 0);
    assert!(!pick.contains("Bag_ct("));
    assert_eq!(pick.matches("_data = {").count(), 1);
    assert_eq!(pick.matches('{').count(), 2);
    assert!(!pick.contains("__rock_box_mts[\"Bag\"]"));
    assert_eq!(run_main(&lua), "1 1 true");
}

#[test]
fn a_boxed_handle_parent_packs_without_an_ffi_aggregate_initializer() {
    let lua = emit_module(
        r#"
value Token { handle: Entity<int> }
struct Pick { token: Token, speed: float }

@nomangle fn pick_token(tokens: [Token], index: int) -> Pick? {
    let token = tokens.get(index)
    if token == null { return null }
    return Pick { token: token, speed: 1.0 }
}
"#,
    );
    let body = function_body(&lua, "pick_token");
    assert!(body.contains("Token_lct("));
    assert!(!body.contains("Token_ct("));
    assert!(!body.contains("Entity_ct("));
}
