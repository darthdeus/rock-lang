//! Codegen-shape contract: a tuple whose only uses are positional reads must
//! NOT survive as a Lua table after inlining. Mirrors `plain_struct_scalarize.rs`
//! for `Inst::MakeTuple`.
//!
//! The load-bearing case is a 2-tuple-element iterator (`next -> (bool, (A, B))`):
//! the pair protocol that lets Arena/enumerate/zip iterators satisfy the
//! `Iterator<T>` bound. `for (a, b) in it` construct-then-destructures the pair
//! every iteration; the tuple must be split into scalars at codegen -- LuaJIT
//! SINK only fires on compiled traces, and other backends have no SINK at all.

mod common;
use common::{emit_module, function_body};

// Straight-line pair iterator (the EnumIter/ZipIter shape): @inline `next`
// builds a fresh 2-tuple per iteration, the `for` header destructures it.
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
    assert!(
        !hot.contains("{"),
        "per-iteration pair tuple survived as a table ctor (any `{{` in this body is a tuple) in hot():\n{hot}"
    );
}

// Skip-shape pair iterator (the ArenaPairIter shape): the done paths store
// `phantom()` (nil) into the element cell alongside the tuple ctor stores.
// SROA's every-store-is-a-ctor consistency check must tolerate the nil store,
// not bail on the whole cell.
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
    assert!(
        !hot.contains("{"),
        "skip-shape pair tuple survived as a table ctor in hot():\n{hot}"
    );
}

// A tuple used as a WHOLE (passed by value to a non-inlined fn) escapes; the
// cell must keep its table -- a half-rewrite would read never-stored field
// cells. The positional read alongside forces the candidate shape.
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
    assert!(
        body.contains("{"),
        "a tuple passed by value must keep its table:\n{body}"
    );
}

// The option-B gate (docs/iterator-terminal-ops-plan.md): bare
// `for (e, v) in arena` desugars through the 2-tuple `pairs()` protocol, and
// the per-iteration `(Entity<T>, T)` tuple must be removed by the compiler --
// parity with the deleted 3-value `ArenaIter`. With `e` unused, the Entity
// ctor must DCE away too: the loop body allocates NOTHING per iteration.
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
    assert!(
        !hot.contains("{"),
        "bare arena loop with unused entity must not allocate per iteration \
         (no pair tuple, no Entity table):\n{hot}"
    );
}

// `Entity<T>` is a `value` struct (GC-carrying tier). An entity passed to a
// scalar-ABI callee (`get(e)`) scalarizes into per-field leaf args -- NO
// Entity table materializes even though the entity is "used". This is the full
// two-tier win: the arena pair scan allocates nothing per iteration.
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
    // The entity threads to `get` as scalar leaves (arena, index, generation) --
    // no `Entity_vt` materialization, no table, no surviving pair tuple.
    assert!(
        !hot.contains("Entity_vt") && !hot.contains("{"),
        "used entity passed to a scalar-ABI callee must scalarize (no table):\n{hot}"
    );
    assert!(
        hot.contains("get_Entity"),
        "entity should thread into the scalarized get() call:\n{hot}"
    );
}

// The door_at shape: a `for (e, d)` pair scan that RETURNS the matching entity.
// The non-returning iterations scalarize away (no per-iteration table); only
// the escaping return materializes, and it must go through `Entity_vt` -- the
// tier-correct ctor carrying `__ty` + the value-`__eq` metatable (a raw
// `MakeStruct` here silently broke cross-scan `==`, the item-6 fallout).
const ARENA_SCAN_RETURN_ENTITY: &str = r#"
struct Mob { health: int, tag: int }

fn scan_return_hit(mobs: Arena<Mob>, want: int) -> Entity<Mob>? {
    for (e, m) in mobs {
        if m.tag == want { return e }
    }
    return null
}

fn main() {
    let mobs: Arena<Mob> = arena_new()
    insert(mobs, Mob { health: 10, tag: 1 })
    let _ = scan_return_hit(mobs, 1)
}
"#;

#[test]
fn arena_scan_returning_entity_materializes_via_vt() {
    let lua = emit_module(ARENA_SCAN_RETURN_ENTITY);
    let find = function_body(&lua, "scan_return_hit");
    assert!(
        find.contains("Entity_vt("),
        "the escaping returned entity must materialize via the tier-correct \
         Entity_vt ctor (metatable-carrying), not a raw table:\n{find}"
    );
    // No un-tagged raw Entity table leaked (the lower_for_arena MakeStruct bug).
    assert!(
        !find.contains("_arena ="),
        "no raw metatable-less Entity table may be built in the scan:\n{find}"
    );
    // Item 8 escape-site placement: the ctor moved OUT of the per-iteration yield
    // block into the hit/return path. Exactly one Entity_vt SITE, and it sits on
    // a `return` line -- so a MISS runs zero ctors, the scalars (arena/index/gen)
    // ride loop-carried cells and rebuild only on the hit.
    assert_eq!(
        find.matches("Entity_vt(").count(),
        1,
        "expected exactly one Entity_vt pack site (the hit-path rebuild):\n{find}"
    );
    let ctor_line = find
        .lines()
        .find(|l| l.contains("Entity_vt("))
        .expect("Entity_vt line");
    assert!(
        ctor_line.contains("return"),
        "the Entity_vt ctor must be on the hit-path return, not the yield block \
         (item 8: misses allocate nothing):\n{ctor_line}"
    );
}

// Gate-stays-put pin (item 8): a NULLABLE value binding (`let v: V? = ...`) is
// never flagged for escape-rematerialization -- so its escaping return stays a
// boxed load, NOT a rebuild-from-field-cells (which would reconstruct a bogus
// non-null value on the nil path). A future "simplification" of the lowering
// gate goes red here. The `Bag`-vt ctor appears exactly ONCE (the `if` literal);
// a second occurrence would mean the return got rematerialized.
const NULLABLE_VALUE_RETURN: &str = r#"
value Bag { xs: [int] }

fn pick(c: bool) -> Bag? {
    let v: Bag? = null
    if c { v = Bag { xs: [1] } }
    return v
}

fn main() {
    let _ = pick(true)
}
"#;

#[test]
fn nullable_value_binding_return_stays_boxed() {
    let lua = emit_module(NULLABLE_VALUE_RETURN);
    let pick = function_body(&lua, "pick");
    assert_eq!(
        pick.matches("Bag_vt(").count(),
        1,
        "a nullable value binding's return must stay a boxed load (one ctor, the \
         `if` literal) -- a second Bag_vt means the escape was wrongly rebuilt:\n{pick}"
    );
}
