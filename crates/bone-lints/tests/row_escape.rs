//! `row_escape` over bone HIR + inference: a `T` read out of an `Arena<T>` or a
//! `Scene<T>` root reaching storage that outlives the borrow. Reads inference
//! (a row source is recognized by its RECEIVER type, not its callee name --
//! `get` is a name a dozen unrelated types have), so the harness runs a full
//! `semantic_pass`. `Arena`/`Entity`/`Scene` are declared locally with the same
//! shapes `stdlib/unified.rock` gives them; the lint never looks at the stdlib.

use bone_lints::{filter_suppressed, row_escapes, EscapeKind, LintFinding, RowEscape};
use bone_semantic::prelude::*;
use rock_core::errors::CompilerErrorLevel;

fn parse(src: &str) -> skull_parser::Ast {
    skull_parser::parse_module_clean(src, Ustr::from("<row_escape>")).expect("parse")
}

const PRELUDE: &str = r#"
inline(lua) fn phantom<T>() -> T = "return nil"

value Arena<T> { _scene_id: int, _arena_id: int }
value Entity<T> { _scene_id: int, _arena_id: int, _slot: int, _generation: int }
value Scene<T> { _scene_id: int }

struct Mob { hp: int }
struct Holder { mob: Mob? }

inline(lua) fn get<T>(entity: Entity<T>) -> T? = "return nil"
inline(lua) fn get<T>(arena: Arena<T>, entity: Entity<T>) -> T? = "return nil"
inline(lua) fn scene_root<T>(scene: Scene<T>) -> T? = "return nil"
inline(lua) fn unwrap<T>(x: T?) -> T = "return x"

struct ArenaValIter<T> { i: int }
fn values<T>(a: Arena<T>) -> ArenaValIter<T> { return ArenaValIter { i: 0 } }
fn iter<T>(a: Arena<T>) -> ArenaValIter<T> { return values(a) }
fn next<T>(it: ArenaValIter<T>) -> (bool, T) { return (false, phantom()) }

global MOBS: Arena<Mob> = Arena { _scene_id: 0, _arena_id: 0 }
global CACHED: Mob? = null
global BAG: [Mob] = []
global HOLDER: Holder = Holder { mob: null }

fn some_entity() -> Entity<Mob> {
    return Entity { _scene_id: 0, _arena_id: 0, _slot: 0, _generation: 0 }
}
"#;

fn findings(body: &str) -> Vec<RowEscape> {
    let src = format!("{PRELUDE}\n{body}\n");
    let ws = semantic_pass(&mut [parse(&src)]).expect("clean program typechecks");
    row_escapes(&ws)
}

fn kinds(body: &str) -> Vec<EscapeKind> {
    let mut ks: Vec<EscapeKind> = findings(body).iter().map(|f| f.kind).collect();
    ks.sort();
    ks
}

// --- the prelude itself is a control -------------------------------------
//
// Every test below reuses it, so a finding that originates HERE would inflate
// every other assertion. `values`/`iter`/`next` are row-adjacent by
// construction (`next` returns a `T` out of an arena iterator), which is
// exactly the shape most at risk of a spurious hit.
#[test]
fn prelude_alone_is_clean() {
    let ws = semantic_pass(&mut [parse(PRELUDE)]).expect("prelude typechecks");
    let fs = row_escapes(&ws);
    assert!(fs.is_empty(), "prelude must contribute no findings: {fs:?}");
}

// --- store classes --------------------------------------------------------

#[test]
fn store_into_global_is_flagged() {
    assert_eq!(
        kinds("fn main() {\n    let e = some_entity()\n    CACHED = e.get()\n}"),
        vec![EscapeKind::StoreGlobal]
    );
}

#[test]
fn store_into_struct_field_is_flagged() {
    assert_eq!(
        kinds("fn main() {\n    let e = some_entity()\n    HOLDER.mob = e.get()\n}"),
        vec![EscapeKind::StoreField]
    );
}

#[test]
fn store_into_struct_literal_field_is_flagged() {
    assert_eq!(
        kinds(
            "fn main() {\n    let e = some_entity()\n    let m = e.get()\n    HOLDER = Holder { mob: m }\n}"
        ),
        vec![EscapeKind::StoreField]
    );
}

#[test]
fn push_into_collection_is_flagged() {
    assert_eq!(
        kinds("fn main() {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    BAG.push(m)\n}"),
        vec![EscapeKind::StoreCollection]
    );
}

#[test]
fn index_store_into_collection_is_flagged() {
    assert_eq!(
        kinds("fn main() {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    BAG[0] = m\n}"),
        vec![EscapeKind::StoreCollection]
    );
}

#[test]
fn static_local_store_is_flagged() {
    assert_eq!(
        kinds("fn main() {\n    let e = some_entity()\n    static keep: Mob? = e.get()\n    let _u = keep\n}"),
        vec![EscapeKind::StoreStatic]
    );
}

// --- return, capture, erasure --------------------------------------------

#[test]
fn returning_a_row_is_flagged() {
    assert_eq!(
        kinds("fn leak(e: Entity<Mob>) -> Mob? {\n    return e.get()\n}"),
        vec![EscapeKind::Return]
    );
}

#[test]
fn closure_capturing_a_row_is_flagged() {
    assert_eq!(
        kinds(
            "fn take(f: () -> int) -> int { return f() }\nfn main() -> int {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    return take(() => m.hp)\n}"
        ),
        vec![EscapeKind::ClosureCapture]
    );
}

#[test]
fn erasing_a_row_into_any_is_flagged() {
    assert_eq!(
        kinds(
            "fn sink(x: any) -> int { return 0 }\nfn main() -> int {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    return sink(m)\n}"
        ),
        vec![EscapeKind::AnyErasure]
    );
}

#[test]
fn passing_a_row_to_an_extern_is_flagged() {
    assert_eq!(
        kinds(
            "extern fn host_take(m: Mob) -> int\nfn main() -> int {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    return host_take(m)\n}"
        ),
        vec![EscapeKind::FfiArg]
    );
}

// --- suspension -----------------------------------------------------------

#[test]
fn row_held_across_a_yield_is_flagged() {
    assert_eq!(
        kinds(
            "co fn bad(e: Entity<Mob>) -> Coroutine<int> {\n    let m = unwrap(e.get())\n    yield 1\n    m.hp -= 10\n}"
        ),
        vec![EscapeKind::AcrossYield]
    );
}

// --- the row sources ------------------------------------------------------

#[test]
fn arena_get_is_a_row_source() {
    assert_eq!(
        kinds("fn main() {\n    let e = some_entity()\n    CACHED = MOBS.get(e)\n}"),
        vec![EscapeKind::StoreGlobal]
    );
}

#[test]
fn arena_iteration_binding_is_a_row_source() {
    assert_eq!(
        kinds("fn main() {\n    for m in MOBS {\n        BAG.push(m)\n    }\n}"),
        vec![EscapeKind::StoreCollection]
    );
}

#[test]
fn scene_root_is_a_row_source() {
    // P1's standing limitation: `Scene<T>.get()` hands out an ordinary `T?`
    // that dies with the Scene, so a stored root is the same defect shape.
    assert_eq!(
        kinds(
            "global ROOT: Mob? = null\nfn main() {\n    let s: Scene<Mob> = Scene { _scene_id: 1 }\n    ROOT = scene_root(s)\n}"
        ),
        vec![EscapeKind::StoreGlobal]
    );
}

#[test]
fn a_row_aliased_through_a_local_still_escapes() {
    assert_eq!(
        kinds(
            "fn main() {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    let alias = m\n    BAG.push(alias)\n}"
        ),
        vec![EscapeKind::StoreCollection]
    );
}

// --- negative controls ----------------------------------------------------
//
// Each is the CORRECT rewrite of one flagged shape above. A lint that reports
// these is one nobody will run twice, and a census built on it is noise.

#[test]
fn storing_the_entity_handle_is_clean() {
    let ks = kinds(
        "global KEEP: Entity<Mob>? = null\nfn main() {\n    let e = some_entity()\n    KEEP = e\n}",
    );
    assert!(ks.is_empty(), "the handle is the thing you are SUPPOSED to store: {ks:?}");
}

#[test]
fn copying_a_scalar_out_of_a_row_is_clean() {
    let ks = kinds(
        "global HP: int = 0\nfn main() {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    HP = m.hp\n}",
    );
    assert!(ks.is_empty(), "a scalar read out of a row is a copy, not a row: {ks:?}");
}

#[test]
fn reading_and_mutating_a_row_in_place_is_clean() {
    let ks = kinds(
        "fn main() {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    m.hp -= 10\n}",
    );
    assert!(ks.is_empty(), "read/mutate in place is the whole point of .get(): {ks:?}");
}

#[test]
fn passing_a_row_to_an_ordinary_helper_is_clean() {
    let ks = kinds(
        "fn hurt(m: Mob, n: int) { m.hp -= n }\nfn main() {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    hurt(m, 10)\n}",
    );
    assert!(ks.is_empty(), "a Rock-bodied helper taking a row by value is not an escape: {ks:?}");
}

#[test]
fn refetching_after_the_yield_is_clean() {
    let ks = kinds(
        "co fn good(e: Entity<Mob>) -> Coroutine<int> {\n    yield 1\n    let m = unwrap(e.get())\n    m.hp -= 10\n}",
    );
    assert!(ks.is_empty(), "a row bound AFTER the last suspension is the correct form: {ks:?}");
}

#[test]
fn row_used_before_the_yield_is_clean() {
    let ks = kinds(
        "co fn fine(e: Entity<Mob>) -> Coroutine<int> {\n    let m = unwrap(e.get())\n    m.hp -= 10\n    yield 1\n}",
    );
    assert!(ks.is_empty(), "liveness, not scope -- finished with before the suspension: {ks:?}");
}

#[test]
fn closure_capturing_the_entity_and_refetching_is_clean() {
    // `mag/glyphs.rock`'s `move_to` / `trace` idiom: capture the handle, get()
    // inside the callback each frame.
    let ks = kinds(
        "fn take(f: () -> int) -> int { return f() }\nfn main() -> int {\n    let e = some_entity()\n    return take(() => unwrap(e.get()).hp)\n}",
    );
    assert!(ks.is_empty(), "capturing the handle and re-getting inside is correct: {ks:?}");
}

#[test]
fn a_trailing_row_in_a_unit_fn_is_clean() {
    // `stone`'s `unload_sound` and bitgun's `despawn_mob` both end on
    // `handle.remove()`, whose `T?` result is discarded because the function
    // returns nothing. A tail rule that does not read the DECLARED return type
    // calls that a leaked row, and it inflated the production census by two.
    let ks = kinds("fn drop_it(e: Entity<Mob>) {\n    e.get()\n}");
    assert!(ks.is_empty(), "a discarded tail in a unit fn returns nothing: {ks:?}");
}

#[test]
fn a_trailing_row_in_a_returning_fn_is_still_flagged() {
    // The other side of the same rule: without an explicit `return`, the tail
    // IS the returned value. Narrowing the rule above must not disarm this.
    assert_eq!(
        kinds("fn leak(e: Entity<Mob>) -> Mob? {\n    e.get()\n}"),
        vec![EscapeKind::Return]
    );
}

#[test]
fn iterating_an_arena_without_storing_is_clean() {
    let ks = kinds("fn main() -> int {\n    let t = 0\n    for m in MOBS {\n        t += m.hp\n    }\n    return t\n}");
    assert!(ks.is_empty(), "a bulk scan that keeps nothing is the common correct loop: {ks:?}");
}

// --- catalog wiring -------------------------------------------------------

#[test]
fn finding_reports_at_advice_level() {
    // A census lint, not a gate: the escape POPULATION is the input to the
    // P6 build-or-delete decision (`docs/scene-system-plan.md`), so it must be
    // possible to run it over a tree it would otherwise fail.
    let fs = findings("fn leak(e: Entity<Mob>) -> Mob? {\n    return e.get()\n}");
    assert_eq!(fs.len(), 1, "expected the one return finding: {fs:?}");
    assert_eq!(fs[0].to_diagnostic().level, CompilerErrorLevel::Advice);
}

#[test]
fn diagnostic_names_the_row_type_and_the_kind() {
    let fs = findings("fn leak(e: Entity<Mob>) -> Mob? {\n    return e.get()\n}");
    assert_eq!(fs.len(), 1, "{fs:?}");
    let msg = fs[0].to_diagnostic().message;
    assert!(msg.contains("Mob"), "names the row type: {msg}");
    assert!(msg.contains("return"), "names the escape kind: {msg}");
}

#[test]
fn allow_comment_above_suppresses() {
    let body =
        "fn leak(e: Entity<Mob>) -> Mob? {\n    // rock: allow(row_escape)\n    return e.get()\n}";
    assert_eq!(kinds(body), vec![EscapeKind::Return], "raw finding still exists");
    let src = format!("{PRELUDE}\n{body}\n");
    let kept = filter_suppressed(findings(body), &src);
    assert!(kept.is_empty(), "allow comment above silences it: {kept:?}");
}

#[test]
fn default_tier_does_not_report_row_escape() {
    // The property the opt-in tier exists for, asserted on a program that DOES
    // escape: a migration lint firing on every pre-existing arena consumer would
    // bury the rest of the catalog on every build. Membership in OPT_IN_LINTS is
    // not the same claim -- this drives the runner both ways.
    let src = format!("{PRELUDE}\nfn leak(e: Entity<Mob>) -> Mob? {{\n    return e.get()\n}}\n");
    let ws = semantic_pass(&mut [parse(&src)]).expect("typechecks");
    let mut sources = FxHashMap::default();
    sources.insert(Ustr::from("<row_escape>"), src.as_str());

    let default_tier =
        bone_lints::lint_workspace(&ws, &FxHashSet::default(), true, &sources, &Default::default());
    assert!(
        !default_tier.iter().any(|f| f.lint == bone_lints::ROW_ESCAPE),
        "a build must not see row_escape: {:?}",
        default_tier.iter().map(|f| f.lint).collect::<Vec<_>>()
    );

    let named = bone_lints::lint_workspace(
        &ws,
        &FxHashSet::default(),
        true,
        &sources,
        &bone_lints::LintOptions::only([bone_lints::ROW_ESCAPE.to_string()]),
    );
    assert_eq!(
        named.iter().filter(|f| f.lint == bone_lints::ROW_ESCAPE).count(),
        1,
        "naming it runs it: {named:?}"
    );
}

#[test]
fn row_escape_is_opt_in() {
    // A migration lint with a finding on nearly every pre-existing arena
    // consumer would bury the rest of the catalog on every build.
    assert!(bone_lints::OPT_IN_LINTS.contains(&bone_lints::ROW_ESCAPE));
    assert!(bone_lints::KNOWN_LINTS.contains(&bone_lints::ROW_ESCAPE));
}
