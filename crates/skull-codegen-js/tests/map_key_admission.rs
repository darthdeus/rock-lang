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

fn rejects(src: &str, expect_fragment: &str) {
    match program_result(src) {
        Ok(js) => panic!("JS emission accepted an identity-keyed Map key; got:\n{js}"),
        Err(msg) => assert!(
            msg.contains(expect_fragment),
            "rejected, but not for the key: expected `{expect_fragment}` in `{msg}`"
        ),
    }
}

fn accepts(src: &str) {
    if let Err(msg) = program_result(src) {
        panic!("JS emission rejected an admitted Map key: {msg}");
    }
}

#[test]
fn js_rejects_a_value_struct_map_key() {
    rejects(
        r#"
        value VKey { a: int }
        fn main() {
            let m: Map<VKey, string> = map_new()
            m.insert(VKey { a: 1 }, "x")
        }
        "#,
        "`VKey` is a `value` struct",
    );
}

#[test]
fn js_rejects_a_tuple_map_key() {
    rejects(
        r#"
        fn main() {
            let m: Map<(int, int), string> = map_new()
            m.insert((1, 2), "x")
        }
        "#,
        "a tuple is an aggregate",
    );
}

#[test]
fn js_rejects_a_payload_carrying_enum_map_key() {
    rejects(
        r#"
        enum Ev { Tick(int), Idle }
        fn main() {
            let m: Map<Ev, string> = map_new()
            m.insert(Ev::Tick(1), "x")
        }
        "#,
        "payload-carrying variants",
    );
}

/// The escape hatch the old diagnostic recommended. A newtype is keyed as what
/// it wraps, so this one is still cdata.
#[test]
fn js_rejects_a_newtype_over_a_value_struct_map_key() {
    rejects(
        r#"
        value VKey { a: int }
        newtype WrapValue(VKey)
        fn main() {
            let m: Map<WrapValue, string> = map_new()
            m.insert(WrapValue(VKey { a: 1 }), "x")
        }
        "#,
        "wraps",
    );
}

#[test]
fn js_accepts_scalar_and_scalar_newtype_map_keys() {
    accepts(
        r#"
        newtype Id(int)
        fn main() {
            let a: Map<string, int> = map_new()
            a.insert("k", 1)
            let b: Map<int, string> = map_new()
            b.insert(1, "v")
            let c: Map<Id, string> = map_new()
            c.insert(Id(1), "v")
        }
        "#,
    );
}

/// A payload-free enum lowers to an int ordinal, so it is genuinely value-keyed
/// and must survive. Rejecting the whole `Nominal` class would take this with it.
#[test]
fn js_accepts_a_payload_free_enum_map_key() {
    accepts(
        r#"
        enum Color { Red, Green, Blue }
        fn main() {
            let m: Map<Color, string> = map_new()
            m.insert(Color::Red, "r")
        }
        "#,
    );
}
