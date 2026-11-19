use crate::common::{emit_module, function_body, run_main};

const PAIR_ITER_LOOP: &str = r#"
struct PairIter { cur: int, limit: int }

@inline
fn next(it: PairIter) -> (bool, (int, int)) {
    if it.cur >= it.limit { return (false, phantom()) }
    let v = it.cur
    it.cur = it.cur + 1
    return (true, (v, v * 2))
}

fn hot(n: int) -> int {
    let total = 0
    let it = PairIter { cur: 0, limit: n }
    for (a, b) in it {
        total += a + b
    }
    return total
}

fn main() {
    let _ = hot(4)
}
"#;

#[test]
fn pair_iter_tuple_scalarizes() {
    let lua = emit_module(PAIR_ITER_LOOP);
    let hot = function_body(&lua, "hot");
    assert!(!hot.contains("{"));
}

const SKIP_PAIR_ITER_LOOP: &str = r#"
struct SkipShapeIter { cur: int, limit: int }

@inline
fn next(it: SkipShapeIter) -> (bool, (int, int)) {
    let i = it.cur
    if i >= it.limit { return (false, phantom()) }
    while i % 3 == 0 {
        i = i + 1
        if i >= it.limit { it.cur = i; return (false, phantom()) }
    }
    it.cur = i + 1
    return (true, (i, i * 2))
}

fn hot(n: int) -> int {
    let total = 0
    let it = SkipShapeIter { cur: 0, limit: n }
    for (a, b) in it {
        total += a + b
    }
    return total
}

fn main() {
    let _ = hot(10)
}
"#;

#[test]
fn skip_pair_iter_tuple_scalarizes() {
    let lua = emit_module(SKIP_PAIR_ITER_LOOP);
    let hot = function_body(&lua, "hot");
    assert!(!hot.contains("{"));
}

const TUPLE_WHOLE_USE: &str = r#"
fn second(p: (int, int)) -> int { return p.1 }

fn use_it() -> int {
    let p = (3, 4)
    let direct = p.0
    return second(p) + direct
}

fn main() {
    let _ = use_it()
}
"#;

#[test]
fn tuple_whole_use_keeps_table() {
    let lua = emit_module(TUPLE_WHOLE_USE);
    let body = function_body(&lua, "use_it");
    assert!(body.contains("{"));
}

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
fn arena_bare_loop_unused_entity_allocates_nothing() {
    let lua = emit_module(ARENA_BARE_LOOP_UNUSED_ENTITY);
    let hot = function_body(&lua, "hot");
    assert!(!hot.contains("{"));
}

#[test]
fn arena_bare_loop_is_a_counted_for() {
    for src in [ARENA_BARE_LOOP_UNUSED_ENTITY, ARENA_BARE_LOOP_USED_ENTITY] {
        let lua = emit_module(src);
        let hot = function_body(&lua, "hot");
        assert!(hot.contains("for ") && !hot.contains("goto") && !hot.contains("next"));
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
    assert!(!hot.contains("Entity_vt") && !hot.contains("{"));
    assert!(
        crate::common::definitions_of(&lua, "get_Entity")
            .iter()
            .any(|global| {
                let callee = crate::common::bound_callee_name(&lua, "hot", global);
                hot.contains(&format!("{callee}("))
            })
    );
}

const ARENA_SCAN_RETURN_ENTITY: &str = r#"
struct Mob { health: int, tag: int }
struct Found { entity: Entity<Mob>? }
global STORED: Found? = null

@nomangle inline(lua) fn __out(s: string) = "io.write(s, ' ')"

fn scan_return_hit(mobs: Arena<Mob>, want: int) -> Entity<Mob>? {
    for (e, m) in mobs {
        if m.tag == want { return e }
    }
    return null
}

fn main() {
    let mobs: Arena<Mob> = arena_new()
    insert(mobs, Mob { health: 10, tag: 1 })
    insert(mobs, Mob { health: 20, tag: 2 })
    let hit = scan_return_hit(mobs, 2).unwrap()
    let again = scan_return_hit(mobs, 2)
    STORED = Found { entity: scan_return_hit(mobs, 2) }
    let empty: Arena<Mob> = arena_new()
    __out("{scan_return_hit(mobs, 2) == again} {get(hit).unwrap().tag} {STORED.unwrap().entity == again}")
    __out("{scan_return_hit(mobs, 3) == null} {scan_return_hit(empty, 1) == null}")
}
"#;

#[test]
fn arena_scan_returning_entity_keeps_leaves_until_a_boxed_sink() {
    let lua = emit_module(ARENA_SCAN_RETURN_ENTITY);
    let find = function_body(&lua, "scan_return_hit");
    assert!(!find.contains("Entity_vt(") && !find.contains("Entity_ct(") && !find.contains('{'));
    assert!(!find.contains("_arena ="));
    let main = function_body(&lua, "main");
    assert_eq!(main.matches("Entity_vt(").count(), 1);
    assert_eq!(run_main(&lua), "true 2 true true true");
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
    assert!(!hot.contains("Entity_vt("));
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
    assert_eq!(pick.matches("__ty = \"Vec\"").count(), 1);
    assert_eq!(pick.matches('{').count(), 2);
    assert!(!pick.contains("__ty = \"Bag\""));
    assert_eq!(run_main(&lua), "1 1 true");
}
