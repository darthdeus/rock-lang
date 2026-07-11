//! Reserved lexical namespace: skull emits `r<N>` value temps, `c<N>` cell
//! locals, and `__rock_*` runtime names as bare locals into every function body,
//! while `Inst::Global` reads/writes user globals by their bare name. So a
//! top-level def whose emitted name matches one of those forms is silently
//! shadowed by the local inside any body that also uses temps -- the
//! env-param-`e` class (`5c378c810`). Live symptom (pre-fix): `global r1` +
//! a fn reading it crashed at runtime ("arithmetic on local 'r1' (a nil value)").
//!
//! These names are legal bone identifiers -- only skull's codegen makes them
//! collide -- so the rejection is emit-time, tested here (bone's `tests/errors`
//! never runs skull emit). The reserved set is the *body-local* namespace only
//! (`r<N>`/`v<N>` temps, `c<N>` cells, the `__rock_env` env param). The `__rock_`
//! prefix at large is NOT reserved: the stdlib AND stone engine legitimately
//! define top-level `@nomangle __rock_*` fns (`__rock_create_struct`,
//! `__rock_install_serialize_value`, ...), which are globals, never body locals,
//! so a `__rock_foo` def compiles -- only the exact `__rock_env` name collides.

mod common;
use common::{emit_module, try_emit_module};

fn reject_msg(src: &str) -> String {
    let err = try_emit_module(src).err().expect("emit must reject the reserved-name def");
    assert!(
        err.message.contains("reserved codegen identifier"),
        "the rejection must name the reserved-identifier class; got: {}",
        err.message
    );
    err.message
}

/// The live-crash case: `global r1` shadowed by a body's `local r1` temp.
#[test]
fn global_r1_rejected() {
    let msg = reject_msg(
        r#"
global r1: int = 100
fn use_it(a: int) -> int { return a + r1 }
fn main() { print("{use_it(7)}") }
"#,
    );
    assert!(msg.contains("r1"), "names the colliding global; got: {msg}");
}

/// A `c<N>` cell-local collision.
#[test]
fn global_c0_rejected() {
    let msg = reject_msg(
        r#"
global c0: int = 5
fn use_it(a: int) -> int { return a + c0 }
fn main() { print("{use_it(1)}") }
"#,
    );
    assert!(msg.contains("c0"), "names the colliding global; got: {msg}");
}

/// The `__rock_env` closure-env param -- the sole `__rock_*` name emitted as a
/// body local. A user global by that name is shadowed inside every lambda body
/// (the env-param-`e` class, `5c378c810`).
#[test]
fn global_rock_env_rejected() {
    let msg = reject_msg(
        r#"
global __rock_env: int = 9
fn use_it(a: int) -> int { return a + __rock_env }
fn main() { print("{use_it(1)}") }
"#,
    );
    assert!(msg.contains("__rock_env"), "names the colliding global; got: {msg}");
}

/// A `@nomangle` fn whose EMITTED name is `r2` (mangling would otherwise make it
/// `r2__ret_int`, which can't collide). The check runs against the emitted name.
#[test]
fn nomangle_fn_r2_rejected() {
    let msg = reject_msg(
        r#"
@nomangle fn r2() -> int { return 3 }
fn main() { print("{r2()}") }
"#,
    );
    assert!(msg.contains("r2"), "names the colliding fn; got: {msg}");
}

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
/// prefix, which the stdlib and stone engine legitimately extend
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
    assert!(lua.contains("function main"), "stdlib compiles:\n{}", &lua[..lua.len().min(200)]);
}
