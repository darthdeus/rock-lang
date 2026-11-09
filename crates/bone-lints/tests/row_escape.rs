//! `row_escape` over bone HIR + inference: a `T` read out of an `Arena<T>` or a
//! `Scene<T>` root reaching storage that outlives the borrow. Reads inference
//! (a row source is recognized by its RECEIVER type, not its callee name --
//! `get` is a name a dozen unrelated types have), so the harness runs a full
//! `semantic_pass`. `Arena`/`Entity`/`Scene` are declared locally with the same
//! shapes `stdlib/unified.rock` gives them; the lint never looks at the stdlib.

use bone_lints::{EscapeClass, EscapeKind, LintFinding, RowEscape, filter_suppressed, row_escapes};
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

#[test]
fn each_escaping_shape_reports_its_kind() {
    let cases: &[(&str, &str, EscapeKind)] = &[
        (
            "store_into_global_is_flagged",
            "fn main() {\n    let e = some_entity()\n    CACHED = e.get()\n}",
            EscapeKind::StoreGlobal,
        ),
        (
            "store_into_struct_field_is_flagged",
            "fn main() {\n    let e = some_entity()\n    HOLDER.mob = e.get()\n}",
            EscapeKind::StoreField,
        ),
        (
            "store_into_struct_literal_field_is_flagged",
            "fn main() {\n    let e = some_entity()\n    let m = e.get()\n    HOLDER = Holder { mob: m }\n}",
            EscapeKind::StoreField,
        ),
        (
            "push_into_collection_is_flagged",
            "fn main() {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    BAG.push(m)\n}",
            EscapeKind::StoreCollection,
        ),
        (
            "index_store_into_collection_is_flagged",
            "fn main() {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    BAG[0] = m\n}",
            EscapeKind::StoreCollection,
        ),
        (
            "static_local_store_is_flagged",
            "fn main() {\n    let e = some_entity()\n    static keep: Mob? = e.get()\n    let _u = keep\n}",
            EscapeKind::StoreStatic,
        ),
        (
            "closure_capturing_a_row_is_flagged",
            "fn take(f: () -> int) -> int { return f() }\nfn main() -> int {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    return take(() => m.hp)\n}",
            EscapeKind::ClosureCapture,
        ),
        (
            "erasing_a_row_into_any_is_flagged",
            "fn sink(x: any) -> int { return 0 }\nfn main() -> int {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    return sink(m)\n}",
            EscapeKind::AnyErasure,
        ),
        (
            "passing_a_row_to_an_extern_is_flagged",
            "extern(lua) fn host_take(m: Mob) -> int\nfn main() -> int {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    return host_take(m)\n}",
            EscapeKind::FfiArg,
        ),
        (
            "row_held_across_a_yield_is_flagged",
            "co fn bad(e: Entity<Mob>) -> Coroutine<int> {\n    let m = unwrap(e.get())\n    yield 1\n    m.hp -= 10\n}",
            EscapeKind::AcrossYield,
        ),
        // Only the then-arm suspends, but the read after the `if` is reachable
        // through it, so the binding is stale on one path.
        (
            "suspension_on_one_branch_taints_the_merge",
            "co fn f(e: Entity<Mob>, c: bool) -> Coroutine<int> {\n    let m = unwrap(e.get())\n    if c {\n        yield 1\n    }\n    m.hp -= 1\n}",
            EscapeKind::AcrossYield,
        ),
        // Textually the read PRECEDES the yield, so a source-offset rule sees
        // nothing. The back edge is the suspension: from iteration two on, the
        // binding was fetched before a `yield` that already ran.
        (
            "read_before_the_loops_yield_is_flagged_via_the_back_edge",
            "co fn f(e: Entity<Mob>) -> Coroutine<int> {\n    let m = unwrap(e.get())\n    for _i in 0..3 {\n        m.hp -= 1\n        yield 1\n    }\n}",
            EscapeKind::AcrossYield,
        ),
        (
            "arena_get_is_a_row_source",
            "fn main() {\n    let e = some_entity()\n    CACHED = MOBS.get(e)\n}",
            EscapeKind::StoreGlobal,
        ),
        (
            "arena_iteration_binding_is_a_row_source",
            "fn main() {\n    for m in MOBS {\n        BAG.push(m)\n    }\n}",
            EscapeKind::StoreCollection,
        ),
        // Current limitation: `Scene<T>.get()` hands out an ordinary `T?`
        // that dies with the Scene, so a stored root is the same defect shape.
        (
            "scene_root_is_a_row_source",
            "global ROOT: Mob? = null\nfn main() {\n    let s: Scene<Mob> = Scene { _scene_id: 1 }\n    ROOT = scene_root(s)\n}",
            EscapeKind::StoreGlobal,
        ),
        (
            "a_row_aliased_through_a_local_still_escapes",
            "fn main() {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    let alias = m\n    BAG.push(alias)\n}",
            EscapeKind::StoreCollection,
        ),
        // The other side of the same rule: without an explicit `return`, the tail
        // IS the returned value. Narrowing the rule above must not disarm this.
        (
            "a_trailing_row_in_a_returning_fn_is_still_flagged",
            "fn leak(e: Entity<Mob>) -> Mob? {\n    e.get()\n}",
            EscapeKind::Return,
        ),
    ];
    for (name, src, kind) in cases {
        assert_eq!(kinds(src), vec![*kind], "{name}");
    }
}

// --- return, capture, erasure --------------------------------------------

#[test]
fn returning_a_row_is_propagation_not_a_hazard() {
    // An accessor hands a row to its caller. A `return` runs nothing, so it cannot invalidate the
    // slot -- what crosses it is the reference, never the validity window. It is
    // reported only because it is where an INTRAPROCEDURAL check stops.
    let fs = findings("fn accessor(e: Entity<Mob>) -> Mob? {\n    return e.get()\n}");
    assert_eq!(fs.len(), 1, "{fs:?}");
    assert_eq!(fs[0].kind, EscapeKind::Return);
    assert_eq!(fs[0].kind.class(), EscapeClass::Propagation);
    assert!(
        fs[0].to_diagnostic().message.contains("caller"),
        "the diagnostic must point at the callers, not accuse the accessor: {}",
        fs[0].to_diagnostic().message
    );
}

#[test]
fn escape_classes_partition_the_kinds() {
    use EscapeKind::*;
    // A row outliving an invalidation point unconditionally.
    for k in [
        StoreGlobal,
        StoreStatic,
        StoreField,
        StoreCollection,
        AcrossYield,
    ] {
        assert_eq!(k.class(), EscapeClass::Hazard, "{k:?}");
    }
    // A hazard only if the consumer RETAINS -- which no retain summary proves
    // today, so these are reported without claiming a defect.
    for k in [ClosureCapture, FfiArg, AnyErasure] {
        assert_eq!(k.class(), EscapeClass::Unprovable, "{k:?}");
    }
    assert_eq!(Return.class(), EscapeClass::Propagation);
}

#[test]
fn correct_row_handling_is_clean() {
    // Each is the CORRECT rewrite of a flagged shape. A lint that reports
    // these is one nobody will run twice, and a census built on it is noise.
    let cases: &[&str] = &[
        // Re-fetched at the top of each iteration and finished with before the
        // suspension -- the correct loop shape.
        "co fn f(e: Entity<Mob>) -> Coroutine<int> {\n    for _i in 0..3 {\n        let m = unwrap(e.get())\n        m.hp -= 1\n        yield 1\n    }\n}",
        // The fix the sims apply: `m = e.get().unwrap()` after each suspension.
        // A rebind must clear `crossed`, or the correct program stays flagged.
        "co fn f(e: Entity<Mob>) -> Coroutine<int> {\n    let m = unwrap(e.get())\n    yield 1\n    m = unwrap(e.get())\n    m.hp -= 1\n}",
        // `Entity<int>` hands back the value itself -- nothing points at the arena,
        // so holding it across a suspension is safe.
        "co fn f(e: Entity<int>) -> Coroutine<int> {\n    let n = unwrap(e.get())\n    yield 1\n    let _u = n + 1\n}",
        // the handle is what you are supposed to hold
        "co fn f(e: Entity<Mob>) -> Coroutine<int> {\n    let h = e\n    yield 1\n    let _m = unwrap(h.get())\n}",
        // the handle is the thing you are SUPPOSED to store
        "global KEEP: Entity<Mob>? = null\nfn main() {\n    let e = some_entity()\n    KEEP = e\n}",
        // a scalar read out of a row is a copy, not a row
        "global HP: int = 0\nfn main() {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    HP = m.hp\n}",
        // read/mutate in place is the whole point of .get()
        "fn main() {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    m.hp -= 10\n}",
        // a Rock-bodied helper taking a row by value is not an escape
        "fn hurt(m: Mob, n: int) { m.hp -= n }\nfn main() {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    hurt(m, 10)\n}",
        // a row bound AFTER the last suspension is the correct form
        "co fn good(e: Entity<Mob>) -> Coroutine<int> {\n    yield 1\n    let m = unwrap(e.get())\n    m.hp -= 10\n}",
        // liveness, not scope -- finished with before the suspension
        "co fn fine(e: Entity<Mob>) -> Coroutine<int> {\n    let m = unwrap(e.get())\n    m.hp -= 10\n    yield 1\n}",
        // Capture the handle and fetch the row inside the callback each frame.
        "fn take(f: () -> int) -> int { return f() }\nfn main() -> int {\n    let e = some_entity()\n    return take(() => unwrap(e.get()).hp)\n}",
        // a bulk scan that keeps nothing is the common correct loop
        "fn main() -> int {\n    let t = 0\n    for m in MOBS {\n        t += m.hp\n    }\n    return t\n}",
    ];
    for src in cases {
        let ks = kinds(src);
        assert!(ks.is_empty(), "{src}: {ks:?}");
    }
}

#[test]
fn a_trailing_row_in_a_unit_fn_is_clean() {
    // A unit function discards its tail value. Reading only the expression
    // type would incorrectly report a returned row.
    let ks = kinds("fn drop_it(e: Entity<Mob>) {\n    e.get()\n}");
    assert!(
        ks.is_empty(),
        "a discarded tail in a unit fn returns nothing: {ks:?}"
    );
}

// --- catalog wiring -------------------------------------------------------

#[test]
fn finding_reports_at_advice_level() {
    // A census must be runnable over a tree with escapes without failing it.
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
    assert_eq!(
        kinds(body),
        vec![EscapeKind::Return],
        "raw finding still exists"
    );
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

    let default_tier = bone_lints::lint_workspace(
        &ws,
        &FxHashSet::default(),
        true,
        &sources,
        &Default::default(),
    );
    assert!(
        !default_tier
            .iter()
            .any(|f| f.lint == bone_lints::ROW_ESCAPE),
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
        named
            .iter()
            .filter(|f| f.lint == bone_lints::ROW_ESCAPE)
            .count(),
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
