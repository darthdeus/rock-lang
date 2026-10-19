//! Reserved lexical namespace: skull emits `r<N>` value temps, `c<N>` cell
//! locals, and `__rock_*` runtime names as bare locals into every function body,
//! while `Inst::Global` reads/writes user globals by their bare name. So a
//! top-level def whose emitted name matches one of those forms is silently
//! shadowed by the local inside any body that also uses temps -- the
//! env-param-`e` class (`5c378c810`).
//!
//! The REJECTION cases (a `global r1` / `c0` / `__rock_env` / `@nomangle fn r2`
//! that must be refused at emit time) now live in
//! `tests/errors/reserved_namespace.rock.err`, run through the error-test
//! harness's skull-emit extension. What stays HERE is what a rejection suite
//! can't express: the POSITIVE CONTROLS -- a name that only LOOKS reserved still
//! compiles, AND the exact emitted-Lua substring proves the user name survived
//! codegen. The reserved set is the *body-local* namespace only (`r<N>`/`v<N>`
//! temps, `c<N>` cells, the `__rock_env` env param). The `__rock_` prefix at
//! large is NOT reserved: the stdlib AND host engine define top-level
//! `@nomangle __rock_*` fns (`__rock_create_struct`, ...), which are globals,
//! never body locals, so a `__rock_foo` def compiles.
use crate::common::emit_module;

/// POSITIVE CONTROL: `r` alone (no digits) is not a temp form.
#[test]
fn global_r_bare_compiles() {
    let lua = emit_module(
        r#"
global r: int = 1
fn use_it(a: int) -> int { return a + r }
fn main() { print("{use_it(1)}") }
"#,
    );
    assert!(lua.contains("r = (1)") || lua.contains("r ="), "the user global is emitted:\n{lua}");
}

/// POSITIVE CONTROL: `rx1` -- a digit not immediately after the prefix letter.
#[test]
fn global_rx1_compiles() {
    let lua = emit_module(
        r#"
global rx1: int = 1
fn use_it(a: int) -> int { return a + rx1 }
fn main() { print("{use_it(1)}") }
"#,
    );
    assert!(lua.contains("rx1"), "the user global is emitted:\n{lua}");
}

/// POSITIVE CONTROL: `r1x` -- trailing non-digit, so not `^r[0-9]+$`.
#[test]
fn global_r1x_compiles() {
    let lua = emit_module(
        r#"
global r1x: int = 1
fn use_it(a: int) -> int { return a + r1x }
fn main() { print("{use_it(1)}") }
"#,
    );
    assert!(lua.contains("r1x"), "the user global is emitted:\n{lua}");
}

/// POSITIVE CONTROL: an ordinary name starting with the reserved prefix letter.
#[test]
fn fn_named_radius_compiles() {
    let lua = emit_module(
        r#"
fn radius(a: int) -> int { return a * 2 }
fn main() { print("{radius(3)}") }
"#,
    );
    assert!(lua.contains("radius"), "the user fn is emitted:\n{lua}");
}

/// POSITIVE CONTROL: a user `__rock_*` global that is NOT `__rock_env` names no
/// body local, so it compiles. Guards against re-banning the whole `__rock_`
/// prefix, which the stdlib and host engine legitimately extend
/// (`__rock_install_serialize_value`, ...) and which broke every real compile.
#[test]
fn global_rock_prefixed_non_env_compiles() {
    let lua = emit_module(
        r#"
global __rock_foo: int = 9
fn use_it(a: int) -> int { return a + __rock_foo }
fn main() { print("{use_it(1)}") }
"#,
    );
    assert!(lua.contains("__rock_foo"), "the user global is emitted:\n{lua}");
}

/// The stdlib defines `@nomangle __rock_*` fns and is loaded into every emit
/// here; a trivial program compiling confirms none of them trip the guard.
#[test]
fn stdlib_rock_prefixed_defs_compile() {
    let lua = emit_module(r#"fn main() { print("ok") }"#);
    assert!(
        !crate::common::definitions_matching(&lua, "main").is_empty(),
        "stdlib compiles:\n{}",
        &lua[..lua.len().min(200)]
    );
}
