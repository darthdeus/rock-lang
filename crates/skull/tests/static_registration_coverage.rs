//! Every emitted `static` reaches the compiler-owned StateSchema.
//!
//! The old process-global static registry was a second membership oracle. The
//! schema is now the one publication consumed by the Lua Session, so this file
//! compares emitted Session keys with the compiler inventory instead.

use crate::common::emit_mir;
use skull_mir::state_schema::StateKind;

fn assert_static_schema_is_live(source: &str, what: &str) {
    let emit = emit_mir(source);
    let entries: Vec<_> = emit
        .state_schema
        .entries
        .iter()
        .filter(|entry| entry.kind == StateKind::Static)
        .collect();
    assert!(!entries.is_empty(), "{what}: static population is zero");
    for entry in entries {
        assert_eq!(entry.physical.len(), 2, "{what}: {} is not value+done", entry.identity);
        for key in skull_mir::state_schema::entry_state_keys(entry) {
            assert!(
                emit.lua.contains(&format!("{key:?}")),
                "{what}: Session key {key} for {} is absent from emitted code",
                entry.identity
            );
        }
        assert!(
            emit.lua.contains("__rock_state_prepare_static"),
            "{what}: no per-Session initializer preparation was emitted"
        );
    }
}

#[test]
fn an_ordinary_functions_static_is_published_to_state_schema() {
    assert_static_schema_is_live(
        r#"
global RESULT: int = 0
fn hits() -> int {
    static n = 0
    n = n + 1
    return n
}
fn main() { RESULT = hits() }
"#,
        "ordinary static",
    );
}

#[test]
fn a_bounded_generics_static_is_published_through_its_call_sites() {
    assert_static_schema_is_live(
        r#"
global RESULT: int = 0
fn tally<I: Iterator<int>>(it: I) -> int {
    static calls = 0
    calls = calls + 1
    let n = 0
    for x in it { n = n + x }
    return n + calls
}
fn main() { RESULT = tally([1, 2, 3].iter()) }
"#,
        "bounded generic static",
    );
}

#[test]
fn a_bounded_generics_static_behind_a_wrapper_is_published_once() {
    let emit = emit_mir(
        r#"
global RESULT: int = 0
fn tally<I: Iterator<int>>(it: I) -> int {
    static calls = 0
    calls = calls + 1
    let n = 0
    for x in it { n = n + x }
    return n + calls
}
fn wrapper(a: int, b: int) -> int { return tally([a, b].iter()) }
fn main() { RESULT = wrapper(1, 2) }
"#,
    );
    let entries: Vec<_> = emit
        .state_schema
        .entries
        .iter()
        .filter(|entry| entry.kind == StateKind::Static)
        .collect();
    assert_eq!(entries.len(), 1, "one owner must yield one logical static");
    assert_static_schema_is_live(
        r#"
global RESULT: int = 0
fn tally<I: Iterator<int>>(it: I) -> int {
    static calls = 0
    calls = calls + 1
    let n = 0
    for x in it { n = n + x }
    return n + calls
}
fn wrapper(a: int, b: int) -> int { return tally([a, b].iter()) }
fn main() { RESULT = wrapper(1, 2) }
"#,
        "wrapped bounded generic static",
    );
}

#[test]
fn a_specialized_generics_static_is_published() {
    assert_static_schema_is_live(
        r#"
value V2 { x: float, y: float }
global RESULT: float = 0.0
@inline fn tagged<T>(v: T) -> T {
    static seen = 0
    seen = seen + 1
    return v
}
fn use_it(a: V2) -> float {
    let r = tagged(a)
    return r.x
}
fn main() { RESULT = use_it(V2 { x: 1.0, y: 2.0 }) }
"#,
        "specialized generic static",
    );
}

#[test]
fn a_program_without_statics_publishes_a_schema_without_static_entries() {
    let emit = emit_mir("global RESULT: int = 0\nfn main() { RESULT = 1 }\n");
    assert!(emit.lua.contains("_G.__ROCK_STATE_SCHEMA = {"));
    assert_eq!(
        emit
            .state_schema
            .entries
            .iter()
            .filter(|entry| entry.kind == StateKind::Static)
            .count(),
        0
    );
}
