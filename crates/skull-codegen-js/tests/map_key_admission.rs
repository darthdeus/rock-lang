//! The `Map` key admission runs on the JS backend, not only the Lua one.
//!
//! The check lives in `skull_mir::lower::map_key_rejection` and the Lua emitter
//! calls it from `mir_emit`. It had exactly one call site, so `rock web`
//! accepted a program `rock compile` refused -- a value-struct key emitted JS
//! that missed silently. Keying is a property of the key REPRESENTATION, and JS
//! keys a `Map` by object identity exactly as Lua keys a table by it, so both
//! backends ask the same question through one oracle.
//!
//! The admitted arms are here for the same reason: a gate that rejects
//! everything would pass the rejection assertions while making the backend
//! useless, and only a positive control can tell those apart.

use crate::common::ws_with;

fn program_result(src: &str) -> Result<String, String> {
    let ws = ws_with(src);
    let mid = crate::common::test_mid(&ws);
    match skull_codegen_js::emit::emit_program(&ws, mid) {
        Ok(p) => Ok(p.program),
        Err(e) => Err(e.message),
    }
}

fn map_source(declaration: &str, key_type: &str, key: &str) -> String {
    format!(
        "{declaration}\nfn main() {{ let m: Map<{key_type}, string> = map_new(); m.insert({key}, \"x\") }}"
    )
}

fn rejects(label: &str, src: &str, expect_fragment: &str) {
    match program_result(src) {
        Ok(js) => panic!("JS emission accepted {label}; got:\n{js}"),
        Err(msg) => assert!(
            msg.contains(expect_fragment),
            "{label}: expected `{expect_fragment}` in `{msg}`"
        ),
    }
}

fn accepts(label: &str, src: &str) {
    if let Err(msg) = program_result(src) {
        panic!("JS emission rejected {label}: {msg}");
    }
}

#[test]
fn js_map_key_admission_tracks_key_representation() {
    for (label, declaration, key_type, key, diagnostic) in [
        (
            "value struct key",
            "value VKey { a: int }",
            "VKey",
            "VKey { a: 1 }",
            "`VKey` is a `value` struct",
        ),
        (
            "tuple key",
            "",
            "(int, int)",
            "(1, 2)",
            "a tuple is an aggregate",
        ),
        (
            "payload-carrying enum key",
            "enum Ev { Tick(int), Idle }",
            "Ev",
            "Ev::Tick(1)",
            "payload-carrying variants",
        ),
        (
            "newtype over a value struct key",
            "value VKey { a: int }\nnewtype WrapValue(VKey)",
            "WrapValue",
            "WrapValue(VKey { a: 1 })",
            "wraps",
        ),
    ] {
        rejects(label, &map_source(declaration, key_type, key), diagnostic);
    }

    for (label, declaration, key_type, key) in [
        ("string key", "", "string", "\"k\""),
        ("integer key", "", "int", "1"),
        ("scalar newtype key", "newtype Id(int)", "Id", "Id(1)"),
        (
            "payload-free enum key",
            "enum Color { Red, Green, Blue }",
            "Color",
            "Color::Red",
        ),
    ] {
        accepts(label, &map_source(declaration, key_type, key));
    }
}
