//! Every emitted `static` registers its storage keys for reload.
//!
//! The old process-global static registry was a second membership oracle. The
//! program's `__rock_static_prepare` registrations are now the one inventory a
//! reload resets by, so this file compares them with the keys the emitted code
//! reads.

use crate::common::emit_mir;

/// The (value, done) storage keys of every registered static.
fn registered_statics(lua: &str) -> Vec<(String, String)> {
    lua.lines()
        .filter_map(|line| line.strip_prefix("__rock_static_prepare("))
        .map(|args| {
            let mut quoted = args.split('"').skip(1).step_by(2);
            (
                quoted.next().unwrap().to_string(),
                quoted.next().unwrap().to_string(),
            )
        })
        .collect()
}

fn assert_static_registration_is_live(source: &str, what: &str) {
    let emit = emit_mir(source);
    let statics = registered_statics(&emit.lua);
    assert!(!statics.is_empty(), "{what}: static population is zero");
    for (value, done) in statics {
        for key in [&value, &done] {
            assert!(
                emit.lua.matches(&format!("{key:?}")).count() > 1,
                "{what}: storage key {key} is registered but never read by emitted code"
            );
        }
    }
}

#[test]
fn an_ordinary_functions_static_is_registered() {
    assert_static_registration_is_live(
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
fn a_bounded_generics_static_is_registered_through_its_call_sites() {
    assert_static_registration_is_live(
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
fn a_bounded_generics_static_behind_a_wrapper_is_registered_once() {
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
    assert_eq!(
        registered_statics(&emit.lua).len(),
        1,
        "one owner must yield one logical static"
    );
    assert_static_registration_is_live(
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
fn a_specialized_generics_static_is_registered() {
    assert_static_registration_is_live(
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
fn a_program_without_statics_registers_none() {
    let emit = emit_mir("global RESULT: int = 0\nfn main() { RESULT = 1 }\n");
    assert!(emit.lua.contains("_G.__rock_globals_publish({"));
    assert!(registered_statics(&emit.lua).is_empty());
}
