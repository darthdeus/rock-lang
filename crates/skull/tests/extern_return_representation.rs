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
newtype Boxed<T>(T)
newtype Pair<T>(T)
newtype Deep(Pair<Pair<int>>)
newtype Wrap<T>(Boxed<T>)

extern(ffi, "m") fn ret_int() -> int
extern(ffi, "m") fn ret_handle() -> Handle
extern(ffi, "m") fn ret_alias() -> Alias
extern(ffi, "m") fn ret_boxed() -> Boxed<int>
extern(ffi, "m") fn ret_deep() -> Deep
extern(ffi, "m") fn ret_wrap() -> Wrap<int>
extern(ffi, "m") fn ret_ratio() -> Ratio
extern(ffi, "m") fn ret_boxed_float() -> Boxed<float>
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

fn reject(src: &str) -> String {
    let err = crate::common::try_emit_module(src)
        .expect_err("this return has no C representation to give");
    format!("{err:?}")
}

/// Both sides, which is what makes either one evidence: the declarations whose C
/// representation can arrive boxed are coerced, and the ones that cannot are
/// left alone rather than paying for a conversion that means nothing.
///
/// A newtype APPLICATION substitutes: `Boxed<int>` is an `int` and
/// `Boxed<float>` is a `float`, from the same declaration. Reading the
/// declaration's own `T` without the argument would either reject valid source
/// or coerce a float.
#[test]
fn an_int_representation_is_coerced_through_every_alias_shape() {
    let lua = emit_module(SRC);

    for name in ["ret_int", "ret_handle", "ret_alias", "ret_boxed", "ret_deep", "ret_wrap"] {
        let w = wrapper(&lua, name);
        assert!(
            w.contains(&format!("return tonumber(m.{name}(")),
            "`{name}` bottoms out at an int representation, so its return must be a \
             Rock number and not whatever the C ABI handed back:\n{w}"
        );
    }

    // `float` and `bool` never arrive as cdata from LuaJIT, so coercing them
    // would be cost with no property behind it -- and a blanket `tonumber` would
    // silently turn a `bool` return into 1/0. `ret_boxed_float` is the arm that
    // separates "substitutes correctly" from "coerces every newtype".
    for name in ["ret_ratio", "ret_boxed_float", "ret_flag"] {
        let w = wrapper(&lua, name);
        assert!(
            w.contains(&format!("return m.{name}(")) && !w.contains("tonumber"),
            "`{name}` bottoms out at a representation LuaJIT already hands back as a \
             Lua value; it must cross unchanged:\n{w}"
        );
    }

    let w = wrapper(&lua, "ret_void");
    assert!(!w.contains("return"), "a void extern returns nothing:\n{w}");
}

/// A generic is unrealizable only when nothing BOUND it. `Boxed<T>` on a generic
/// extern has no argument to substitute, so it lands on the same rejection a
/// bare `-> T` gets -- and this is the arm that keeps the substitution from
/// being an unconditional "assume it resolves".
#[test]
fn an_unbound_generic_representation_rejects() {
    let msg = reject(
        r#"
newtype Boxed<T>(T)
extern(ffi, "m") fn ret_open<T>() -> Boxed<T>
fn main() { print("x") }
"#,
    );
    assert!(
        msg.contains("ret_open") && msg.contains("unbound generic parameter"),
        "an unbound generic must reject however it is wrapped, got:\n{msg}"
    );
}

/// `newtype A(B); newtype B(A)` COMPILES, so the alias walk has to terminate on
/// its own rather than trusting the typechecker to have refused the shape.
/// Termination is by visited identity and not by a step cap, which is why
/// `ret_deep` above -- `Pair<Pair<int>>`, legitimately visiting `Pair` twice
/// with different arguments -- still resolves.
#[test]
fn a_self_referential_alias_rejects_instead_of_spinning() {
    let msg = reject(
        r#"
newtype A(B)
newtype B(A)
extern(ffi, "m") fn ret_cycle() -> A
fn main() { print("x") }
"#,
    );
    assert!(
        msg.contains("ret_cycle") && msg.contains("contains itself"),
        "a newtype cycle must reject as a cycle, got:\n{msg}"
    );
}
