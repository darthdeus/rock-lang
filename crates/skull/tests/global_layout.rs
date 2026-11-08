//! The mutable-global layout a program opens its storage with, and the static
//! storage keys a reload resets by.

use crate::common::emit_mir;

const FIXTURE: &str = r#"
value Boxed { x: int, y: int }
nobox value Flat { x: int, y: int }

global BOXED: Boxed = Boxed { x: 1, y: 2 }
global FLAT: Flat = Flat { x: 3, y: 4 }
global RESULT: int = 0

fn with_static() -> int {
    static initialized = 7
    static lazy = 2
    return initialized + lazy
}

@inline fn generic_static<T>(value: T) -> T {
    static calls = 0
    calls = calls + 1
    return value
}

co fn frame_source() -> Coroutine<int> {
    let live = 11
    yield live
    yield live + 1
}

fn main() {
    let _ = generic_static(5)
    let _frame = frame_source()
    RESULT = with_static() + BOXED.x + FLAT.x
}
"#;

/// The `__rock_static_prepare` value keys a program registers.
fn static_keys(lua: &str) -> std::collections::BTreeSet<String> {
    lua.lines()
        .filter_map(|line| line.strip_prefix("__rock_static_prepare(\""))
        .map(|rest| rest.split('"').next().unwrap().to_string())
        .collect()
}

#[test]
fn every_static_including_a_generic_ones_is_registered_by_logical_identity() {
    let keys = static_keys(&emit_mir(FIXTURE).lua);
    for name in ["with_static", "generic_static"] {
        assert!(
            keys.iter().any(|key| key.starts_with("static:")
                && key.contains(name)
                && key.ends_with("::value")),
            "static of {name} is not registered: {keys:?}"
        );
    }
}

#[test]
fn global_layout_ignores_bodies_statics_and_source_order_but_tracks_globals_and_types() {
    let baseline = emit_mir(FIXTURE);
    let body_edit = FIXTURE.replace("live + 1", "live + 2");
    assert_eq!(baseline.global_layout, emit_mir(&body_edit).global_layout);
    let static_body_edit = FIXTURE.replace(
        "fn with_static() -> int {\n    static initialized",
        "fn with_static() -> int {\n    let unrelated = 0\n    static initialized",
    );
    let static_edit = emit_mir(&static_body_edit);
    assert_eq!(baseline.global_layout, static_edit.global_layout);
    assert_eq!(
        static_keys(&baseline.lua),
        static_keys(&static_edit.lua),
        "an unrelated statement before a static must not rename its storage key"
    );
    let static_added = FIXTURE.replace(
        "    static lazy = 2\n",
        "    static lazy = 2\n    static more = 3\n",
    );
    assert_eq!(
        baseline.global_layout,
        emit_mir(&static_added).global_layout
    );

    let reordered = FIXTURE.replace(
        "global BOXED: Boxed = Boxed { x: 1, y: 2 }\nglobal FLAT: Flat = Flat { x: 3, y: 4 }",
        "global FLAT: Flat = Flat { x: 3, y: 4 }\nglobal BOXED: Boxed = Boxed { x: 1, y: 2 }",
    );
    assert_eq!(baseline.global_layout, emit_mir(&reordered).global_layout);

    let added = FIXTURE.replace(
        "global RESULT: int = 0",
        "global EXTRA: int = 9\nglobal RESULT: int = 0",
    );
    assert_ne!(baseline.global_layout, emit_mir(&added).global_layout);

    let reshaped = FIXTURE
        .replace(
            "value Boxed { x: int, y: int }",
            "value Boxed { x: int, z: float }",
        )
        .replace("Boxed { x: 1, y: 2 }", "Boxed { x: 1, z: 2.0 }");
    assert_ne!(baseline.global_layout, emit_mir(&reshaped).global_layout);
}

/// The initializer keys (`type@hash`) a program's statics register.
fn initializer_keys(lua: &str) -> Vec<String> {
    lua.lines()
        .filter_map(|line| line.strip_prefix("__rock_static_prepare("))
        .map(|rest| rest.split('"').nth(5).unwrap().to_string())
        .collect()
}

/// A reload keeps a static's cell only while its initializer means the same
/// thing, so every part of the initializer that changes its value -- match
/// and `if let` patterns, format specs -- must change its key.
#[test]
fn static_initializer_key_tracks_patterns_and_format_specs() {
    let src = r#"
global MODE: int = 1
global N: int? = 1
fn pick() -> string {
    static by_match = match MODE { 1 => 10, _ => 0 }
    static by_if_let = if let 1 = N { 1 } else { 0 }
    static by_spec = "{1.5:.2f}"
    return "{by_match} {by_if_let} {by_spec}"
}
fn main() { print(pick()) }
"#;
    let baseline = initializer_keys(&emit_mir(src).lua);
    assert_eq!(baseline.len(), 3, "{baseline:?}");
    for (i, (from, to)) in [
        ("{ 1 => 10", "{ 2 => 10"),
        ("if let 1 = N", "if let 2 = N"),
        ("{1.5:.2f}", "{1.5:.3f}"),
    ]
    .into_iter()
    .enumerate()
    {
        let edited = initializer_keys(&emit_mir(&src.replace(from, to)).lua);
        assert_ne!(
            baseline[i], edited[i],
            "editing `{from}` to `{to}` kept the initializer key"
        );
    }
}
