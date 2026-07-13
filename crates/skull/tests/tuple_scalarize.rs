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
struct SkipIter { cur: int, limit: int }

@inline
fn next(it: SkipIter) -> (bool, (int, int)) {
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
    let it = SkipIter { cur: 0, limit: n }
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
fn arena_bare_loop_used_entity_no_pair_tuple() {
    let lua = emit_module(ARENA_BARE_LOOP_USED_ENTITY);
    let hot = function_body(&lua, "hot");
    assert!(
        hot.contains("__ty = \"Entity\""),
        "used entity escapes into get() and must keep its table:\n{hot}"
    );
    // The Entity ctor is the ONLY allowed table: the pair tuple itself must
    // still scalarize (a `{` not part of the Entity ctor is the pair).
    let non_entity_braces =
        hot.matches('{').count() - hot.matches("({__ty = \"Entity\"").count();
    assert_eq!(
        non_entity_braces, 0,
        "pair tuple survived alongside the Entity ctor:\n{hot}"
    );
}
