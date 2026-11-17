use crate::support::lint_only;
use bone_lints::{EscapeClass, EscapeKind, Finding};
use rock_core::errors::CompilerErrorLevel;

fn lint(src: &str) -> Vec<Finding> {
    lint_only(&[("<row_escape>", src)], bone_lints::ROW_ESCAPE)
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

fn findings(body: &str) -> Vec<Finding> {
    lint(&format!("{PRELUDE}\n{body}\n"))
}

fn kind_of(finding: &Finding) -> EscapeKind {
    use EscapeKind::*;
    let msg = &finding.error.message;
    if msg.contains("is returned here") {
        return Return;
    }
    [
        StoreGlobal,
        StoreStatic,
        StoreField,
        StoreCollection,
        ClosureCapture,
        AnyErasure,
        AcrossYield,
        FfiArg,
    ]
    .into_iter()
    .find(|k| msg.contains(&format!("({})", k.as_str())))
    .unwrap_or_else(|| panic!("no escape kind in {msg:?}"))
}

fn kinds(body: &str) -> Vec<EscapeKind> {
    let mut ks: Vec<EscapeKind> = findings(body).iter().map(kind_of).collect();
    ks.sort();
    ks
}

#[test]
fn prelude_alone_is_clean() {
    let fs = lint(PRELUDE);
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
        (
            "suspension_on_one_branch_taints_the_merge",
            "co fn f(e: Entity<Mob>, c: bool) -> Coroutine<int> {\n    let m = unwrap(e.get())\n    if c {\n        yield 1\n    }\n    m.hp -= 1\n}",
            EscapeKind::AcrossYield,
        ),
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

#[test]
fn returning_a_row_is_propagation_not_a_hazard() {
    let fs = findings("fn accessor(e: Entity<Mob>) -> Mob? {\n    return e.get()\n}");
    assert_eq!(fs.len(), 1, "{fs:?}");
    assert_eq!(kind_of(&fs[0]), EscapeKind::Return);
    assert_eq!(fs[0].level(), CompilerErrorLevel::Advice);
    assert!(fs[0].error.message.contains("Mob"));
    assert!(fs[0].error.message.contains("return"));
    assert_eq!(kind_of(&fs[0]).class(), EscapeClass::Propagation);
    assert!(
        fs[0].error.message.contains("caller"),
        "the diagnostic must point at the callers, not accuse the accessor: {}",
        fs[0].error.message
    );
}

#[test]
fn correct_row_handling_is_clean() {
    let cases: &[&str] = &[
        "co fn f(e: Entity<Mob>) -> Coroutine<int> {\n    for _i in 0..3 {\n        let m = unwrap(e.get())\n        m.hp -= 1\n        yield 1\n    }\n}",
        "co fn f(e: Entity<Mob>) -> Coroutine<int> {\n    let m = unwrap(e.get())\n    yield 1\n    m = unwrap(e.get())\n    m.hp -= 1\n}",
        "co fn f(e: Entity<int>) -> Coroutine<int> {\n    let n = unwrap(e.get())\n    yield 1\n    let _u = n + 1\n}",
        "co fn f(e: Entity<Mob>) -> Coroutine<int> {\n    let h = e\n    yield 1\n    let _m = unwrap(h.get())\n}",
        "global KEEP: Entity<Mob>? = null\nfn main() {\n    let e = some_entity()\n    KEEP = e\n}",
        "global HP: int = 0\nfn main() {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    HP = m.hp\n}",
        "fn main() {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    m.hp -= 10\n}",
        "fn hurt(m: Mob, n: int) { m.hp -= n }\nfn main() {\n    let e = some_entity()\n    let m = unwrap(e.get())\n    hurt(m, 10)\n}",
        "co fn good(e: Entity<Mob>) -> Coroutine<int> {\n    yield 1\n    let m = unwrap(e.get())\n    m.hp -= 10\n}",
        "co fn fine(e: Entity<Mob>) -> Coroutine<int> {\n    let m = unwrap(e.get())\n    m.hp -= 10\n    yield 1\n}",
        "fn take(f: () -> int) -> int { return f() }\nfn main() -> int {\n    let e = some_entity()\n    return take(() => unwrap(e.get()).hp)\n}",
        "fn main() -> int {\n    let t = 0\n    for m in MOBS {\n        t += m.hp\n    }\n    return t\n}",
    ];
    for src in cases {
        let ks = kinds(src);
        assert!(ks.is_empty(), "{src}: {ks:?}");
    }
}

#[test]
fn a_trailing_row_in_a_unit_fn_is_clean() {
    let ks = kinds("fn drop_it(e: Entity<Mob>) {\n    e.get()\n}");
    assert!(
        ks.is_empty(),
        "a discarded tail in a unit fn returns nothing: {ks:?}"
    );
}

#[test]
fn default_tier_does_not_report_row_escape() {
    let src = format!("{PRELUDE}\nfn leak(e: Entity<Mob>) -> Mob? {{\n    return e.get()\n}}\n");
    let default_tier = crate::lint(&[("<row_escape>", src.as_str())], &Default::default(), true);
    assert!(
        !default_tier
            .iter()
            .any(|f| f.lint == bone_lints::ROW_ESCAPE),
        "a build must not see row_escape: {:?}",
        default_tier.iter().map(|f| f.lint).collect::<Vec<_>>()
    );

    let named = lint(&src);
    assert_eq!(
        named
            .iter()
            .filter(|f| f.lint == bone_lints::ROW_ESCAPE)
            .count(),
        1,
        "naming it runs it: {named:?}"
    );
}
