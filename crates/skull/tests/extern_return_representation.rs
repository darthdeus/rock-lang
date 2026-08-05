//! An `extern(ffi, ..)` wrapper gives the callee's return the representation
//! its DECLARED Rock type promises.
//!
//! The `ffi.cdef` lives inside an opaque `inline(lua)` body, so the declaration
//! is the only contract the compiler has. LuaJIT hands back a BOXED cdata for a
//! 64-bit C integer return, and every Stone resource handle is `uint64_t`, so a
//! `-> int` extern that passed its return through produced a cdata where the
//! declaration said number. That is not cosmetic: LuaJIT keys a table by cdata
//! IDENTITY however its `__eq` compares, so a handle used as a `Map<int, V>` key
//! only ever hit on the box that was inserted
//! (`games/testbed/tests/sims/b1_ffi_handle.rock` is the runtime half).
//!
//! A `newtype` is a representation ALIAS, not a distinct C type, so it must be
//! followed: `newtype Handle(int)` is an `int` at the boundary. That matters
//! prospectively -- B3 wants stronger handle newtypes, and not following one
//! would make the better-typed declaration the loophole that reintroduces the
//! boxed cdata.

use crate::common::emit_module;

const SRC: &str = r#"
newtype Handle(int)
newtype Ratio(float)
newtype Flag(bool)
newtype Alias(Handle)

extern(ffi, "m") fn ret_int() -> int
extern(ffi, "m") fn ret_handle() -> Handle
extern(ffi, "m") fn ret_alias() -> Alias
extern(ffi, "m") fn ret_ratio() -> Ratio
extern(ffi, "m") fn ret_flag() -> Flag
extern(ffi, "m") fn ret_void()

fn main() {
    print("{ret_int()} {ret_handle().0} {ret_alias().0.0} {ret_ratio().0} {ret_flag().0}")
    ret_void()
}
"#;

/// The emitted body of `function <name>(...)`, without its `end`.
fn wrapper(lua: &str, name: &str) -> String {
    let body = lua
        .lines()
        .skip_while(|l| !l.starts_with(&format!("function {name}(")))
        .take_while(|l| *l != "end")
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!body.is_empty(), "no `{name}` wrapper was emitted:\n{lua}");
    body
}

/// Both sides, which is what makes either one evidence: the types whose C
/// representation can arrive boxed are coerced, and the ones that cannot are
/// left alone rather than paying for a conversion that means nothing.
#[test]
fn an_int_return_is_coerced_through_any_newtype_alias() {
    let lua = emit_module(SRC);

    for name in ["ret_int", "ret_handle", "ret_alias"] {
        let w = wrapper(&lua, name);
        assert!(
            w.contains(&format!("return tonumber(m.{name}(")),
            "`{name}` declares an int representation, so its return must be a Rock \
             number and not whatever the C ABI handed back:\n{w}"
        );
    }

    // `float` and `bool` never arrive as cdata from LuaJIT, so coercing them
    // would be cost with no property behind it -- and a blanket `tonumber` would
    // silently turn a `bool` return into 1/0.
    for name in ["ret_ratio", "ret_flag"] {
        let w = wrapper(&lua, name);
        assert!(
            w.contains(&format!("return m.{name}(")) && !w.contains("tonumber"),
            "`{name}` wraps a representation LuaJIT already hands back as a Lua \
             value; it must cross unchanged:\n{w}"
        );
    }

    let w = wrapper(&lua, "ret_void");
    assert!(
        !w.contains("return"),
        "a void extern returns nothing:\n{w}"
    );
}

/// A newtype whose representation is a generic parameter has no C
/// representation either, so following the alias must arrive at the same
/// rejection a bare `-> T` gets. Pinned here rather than only in
/// `tests/errors/extern_ffi_generic_return.rock.err`, because that file covers
/// the DIRECT spelling and this is the one that arrives through the new
/// alias-following loop.
#[test]
fn a_newtype_over_a_generic_has_no_representation_either() {
    let src = r#"
newtype Boxed<T>(T)
extern(ffi, "m") fn ret_boxed() -> Boxed<int>
fn main() { print("x") }
"#;
    let err = crate::common::try_emit_module(src)
        .expect_err("a newtype over a generic has no C representation to give");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("ret_boxed") && msg.contains("generic parameter"),
        "a newtype over a generic must reject like a bare generic return, got:\n{msg}"
    );
}
