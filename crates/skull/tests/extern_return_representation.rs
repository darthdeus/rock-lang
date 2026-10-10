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
newtype Outer<T>(Pair<Pair<T>>)
newtype Twice<A, B>(Pair<B>)

extern(ffi, "m") fn ret_int() -> int
extern(ffi, "m") fn ret_handle() -> Handle
extern(ffi, "m") fn ret_alias() -> Alias
extern(ffi, "m") fn ret_boxed() -> Boxed<int>
extern(ffi, "m") fn ret_deep() -> Deep
extern(ffi, "m") fn ret_wrap() -> Wrap<int>
extern(ffi, "m") fn ret_outer() -> Outer<int>
extern(ffi, "m") fn ret_twice() -> Twice<float, int>
extern(ffi, "m") fn ret_outer_float() -> Outer<float>
extern(ffi, "m") fn ret_ratio() -> Ratio
extern(ffi, "m") fn ret_boxed_float() -> Boxed<float>
extern(ffi, "m") fn ret_flag() -> Flag
extern(ffi, "m") fn ret_bool() -> bool
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

#[test]
fn a_newtype_return_cannot_smuggle_a_gc_value_across_extern() {
    let err = reject(
        r#"
value Bag { items: [int] }
newtype Wrap<T>(T)
newtype BagAlias(Wrap<Bag>)
extern(ffi, "m") fn ret_bag() -> BagAlias
fn main() { print("x") }
"#,
    );
    assert!(
        err.contains("Bag") && err.contains("GC-carrying `value` struct"),
        "the resolved return layout is a GC-carrying Bag and must be rejected, got:\n{err}"
    );

    let ffi_clean = crate::common::try_emit_module(
        r#"
value Point { x: int, y: float }
newtype Wrap<T>(T)
newtype PointAlias(Wrap<Point>)
extern(ffi, "m") fn ret_point() -> PointAlias
fn main() { print("x") }
"#,
    );
    assert!(
        ffi_clean.is_ok(),
        "an FFI-clean value layout remains legal through its newtype: {ffi_clean:?}"
    );
}

/// Both sides, which is what makes either one evidence: the declarations whose C
/// representation can arrive boxed are coerced, and the ones that cannot are
/// left alone rather than paying for a conversion that means nothing.
///
/// A newtype APPLICATION substitutes, and the substitution has to COMPOSE.
/// `Boxed<int>` is an `int` and `Boxed<float>` a `float` from one declaration;
/// `Outer<T>(Pair<Pair<T>>)` at `Outer<int>` hands the inner `Pair` an argument
/// that still mentions `Outer`'s `T`, so a binding that does not carry the
/// environment its argument was written in loses it and rejects a concrete type
/// as unbound. `Deep` does NOT cover that -- its `int` is written concretely
/// inside the declaration -- which is why `ret_outer` is here. `Twice<A, B>`
/// pins that binding is positional and an unused parameter does not leak.
#[test]
fn an_int_representation_is_coerced_through_every_alias_shape() {
    let lua = emit_module(SRC);

    for name in [
        "ret_int", "ret_handle", "ret_alias", "ret_boxed", "ret_deep", "ret_wrap",
        "ret_outer", "ret_twice",
    ] {
        let w = wrapper(&lua, name);
        assert!(
            w.contains(&format!("local __rock_ret = tonumber(m.{name}(")) && w.contains("return __rock_ret"),
            "`{name}` bottoms out at an int representation, so its return must be a \
             Rock number and not whatever the C ABI handed back:\n{w}"
        );
    }

    // `float` never arrives as cdata from LuaJIT, so coercing it would be cost
    // with no property behind it -- and a blanket `tonumber` would silently turn
    // a `bool` return into 1/0. `ret_boxed_float` is the arm that separates
    // "substitutes correctly" from "coerces every newtype".
    for name in ["ret_ratio", "ret_boxed_float", "ret_outer_float"] {
        let w = wrapper(&lua, name);
        assert!(
            w.contains(&format!("local __rock_ret = m.{name}(")) && !w.contains("tonumber"),
            "`{name}` bottoms out at a representation LuaJIT already hands back as a \
             Lua value; it must cross unchanged:\n{w}"
        );
    }

    let w = wrapper(&lua, "ret_void");
    assert!(!w.contains("return"), "a void extern returns nothing:\n{w}");
}

/// A `-> bool` extern's C return is declared `uint8_t`, so the wrapper owes the
/// declaration a LUA BOOLEAN and the byte has to be compared. This is the same
/// rule `Int` follows, not a second one: the `cdef` is opaque, the declaration
/// is the contract, and the wrapper pays whatever the contract costs.
///
/// The `uint8_t` spelling is forced by LuaJIT: `crec_call` can only record a
/// `bool`-returning C call with a Lua frame inside the trace above it, which a
/// trace rooted at this one-line wrapper never has, so every such wrapper
/// aborted `NYI: unsupported C function type` on becoming a trace root.
///
/// `ret_flag` is here and not with the pass-through group because a `newtype`
/// is a representation ALIAS: `newtype Flag(bool)` is a `bool` at the boundary,
/// and a better-typed declaration must not be the loophole that reintroduces
/// the raw byte.
#[test]
fn a_bool_representation_is_compared_to_the_byte_the_cdef_returns() {
    let lua = emit_module(SRC);

    for name in ["ret_bool", "ret_flag"] {
        let w = wrapper(&lua, name);
        assert!(
            w.contains(&format!("return m.{name}(")) && w.trim_end().ends_with("~= 0"),
            "`{name}` bottoms out at a bool representation, so its wrapper must turn \
             the `uint8_t` the cdef returns into a Lua boolean:\n{w}"
        );
        assert!(
            !w.contains("tonumber"),
            "`{name}` is a boolean, not a number:\n{w}"
        );
    }
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

/// A generic alias cycle must be caught by IDENTITY, not by running out of
/// steps: `X<T>(Y<T>); Y<U>(X<U>)` revisits `X` with the same instantiation, and
/// the visited key is built from a normalized form precisely so that repeat is
/// recognizable across arenas.
#[test]
fn a_generic_alias_cycle_is_recognized_as_a_cycle() {
    let msg = reject(
        r#"
newtype X<T>(Y<T>)
newtype Y<U>(X<U>)
extern(ffi, "m") fn ret_gcycle() -> X<int>
fn main() { print("x") }
"#,
    );
    assert!(
        msg.contains("ret_gcycle") && msg.contains("contains itself"),
        "a generic cycle must be reported as a cycle rather than as an exhausted \
         alias chain, got:\n{msg}"
    );
}

/// A wrapper whose body is `return lib.f(..)` is a `CALLT` into C, and a root
/// trace that leaves the function through it aborts `leaving loop in root
/// trace` (`rec_ret` requires a real `RET`). Every value-returning wrapper
/// binds the result to a local and returns that instead; measured 166 -> 0
/// aborts on the shipped LuaJIT (docs/backends.md, "Wrapper return").
#[test]
fn every_value_returning_wrapper_returns_through_a_local() {
    let lua = emit_module(SRC);
    for name in [
        "ret_int", "ret_handle", "ret_alias", "ret_boxed", "ret_deep", "ret_wrap",
        "ret_outer", "ret_twice", "ret_ratio", "ret_boxed_float", "ret_outer_float",
    ] {
        let w = wrapper(&lua, name);
        let tail = w.lines().any(|l| {
            let t = l.trim_start();
            t.starts_with("return m.") || t.starts_with("return tonumber(") || t.starts_with("return ffi.new(")
        });
        assert!(!tail, "`{name}` still returns through a tail call into C:\n{w}");
        assert!(w.contains("return __rock_ret"), "`{name}` must return the bound local:\n{w}");
    }
}

#[test]
fn an_omitted_extern_literal_default_is_passed_and_filled_in_the_wrapper() {
    let lua = emit_module(
        r#"
extern(ffi, "m") fn poll(id: int, wait: bool = false) -> int
fn go() -> int { poll(-1) }
"#,
    );
    let w = wrapper(&lua, "poll");
    assert!(
        w.contains("if wait == nil then wait = false end"),
        "the FFI wrapper must fill an omitted literal default before the C call:\n{w}"
    );
    let go = crate::common::function_body(&lua, "go");
    assert!(
        go.contains("poll((-1), (false))"),
        "a direct omitted call must pass the literal default:\n{go}"
    );
}

/// A modules-v1 extern is registered under its DefKey, but its wrapper is the
/// published surface name (call sites spell it that way) and its C symbol is
/// the declared name. Spelling either with the key emits `function
/// pkg::ret_int()`, which is not Lua.
#[test]
fn a_v1_extern_wrapper_uses_the_declared_symbol() {
    let emitted = crate::common::emit_v1_package(
        "v1extern",
        &[crate::common::V1Fragment {
            file: "main.rock",
            src: r#"
extern(ffi, "m") fn ret_int() -> int
extern(ffi, "m") fn ret_bool() -> bool
pub fn probe() -> string { return "{ret_int()} {ret_bool()}" }
fn main() {}
"#,
        }],
    );
    let out = crate::common::run_luajit(
        &format!(
            "m = {{ ret_int = function() return 7 end, ret_bool = function() return 1 end }}\n{}\nprint(ret_int(), ret_bool())\n",
            emitted.lua
        ),
        "v1_extern_wrapper",
    );
    assert_eq!(out, "7\ttrue");
}

/// A v1 extern returning a value struct allocates its out-parameter by the
/// struct's C typedef, which is the minted type symbol, not the DefKey
/// (`ffi.new("pkg::Pair[1]")` is not a C declaration).
#[test]
fn a_v1_extern_value_return_allocates_by_the_type_symbol() {
    let emitted = crate::common::emit_v1_package(
        "v1extval",
        &[crate::common::V1Fragment {
            file: "main.rock",
            src: r#"
pub value Pair { a: float, b: float }
extern(ffi, "m") fn get_pair() -> Pair
pub @nomangle fn probe() -> string {
    let p = get_pair()
    return "{p.a + p.b}"
}
fn main() {}
"#,
        }],
    );
    let out = crate::common::run_luajit(
        &format!(
            "m = {{ get_pair = function(out) out[0].a = 3; out[0].b = 4 end }}\n{}\nprint(probe())\n",
            emitted.lua
        ),
        "v1_extern_value_return",
    );
    assert_eq!(out, "7");
}
