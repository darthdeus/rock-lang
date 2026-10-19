//! Finding 1: a hoisted CAPTURING
//! lambda takes an env-record param. Named literally `e`, it shadowed any user
//! global (or fn) named `e` read inside ANY capturing lambda body -- `Inst::Global`
//! emits the bare name, so the global resolved to the env record (a table) and
//! arithmetic on it crashed at runtime. The param is now the reserved name
//! `__rock_env`, which no user identifier can legally collide with.
use crate::common::{emit_module, run_luajit};

/// The repro: a global `e` read alongside a captured param inside a capturing
/// lambda. Pre-fix this crashed ("arithmetic on a table value"); the env record
/// bound `e`. make(1) => 1 + 5 == 6.
#[test]
fn global_e_read_in_capturing_lambda() {
    let src = r#"
global e: int = 5
fn make(x: int) -> fn() -> int { return () => x + e }
fn main() { let f = make(1) print("{f()}") }
"#;
    let lua = emit_module(src);
    let out = run_luajit(&format!("{lua}\nmain()\n"), "env_shadow_direct");
    assert_eq!(out, "6", "global `e` must read the user global, not the env record; got: {out}");
}

/// A NESTED (transitive) capture body also reads the global `e`: the inner lambda
/// captures the outer's `x` through its OWN env, and reads global `e` too. Both
/// env params must be reserved names, or the inner body's `e` shadows the global.
/// make(2) => inner reads x(=2) + e(=5) == 7.
#[test]
fn global_e_read_in_nested_capture_body() {
    let src = r#"
global e: int = 5
fn make(x: int) -> fn() -> fn() -> int {
    return () => {
        return () => x + e
    }
}
fn main() { let f = make(2) let g = f() print("{g()}") }
"#;
    let lua = emit_module(src);
    let out = run_luajit(&format!("{lua}\nmain()\n"), "env_shadow_nested");
    assert_eq!(out, "7", "global `e` in a nested capture body must not resolve to an env record; got: {out}");
}

/// Control: a LOCAL named `e` captured normally still works (it becomes an env
/// FIELD, addressed through the reserved param -- the field name is unaffected by
/// the param rename). make(3) => captured local e == 3.
#[test]
fn local_e_captured_normally() {
    let src = r#"
fn make(x: int) -> fn() -> int {
    let e = x
    return () => e
}
fn main() { let f = make(3) print("{f()}") }
"#;
    let lua = emit_module(src);
    let out = run_luajit(&format!("{lua}\nmain()\n"), "env_shadow_local");
    assert_eq!(out, "3", "a captured local `e` reads its own value; got: {out}");
}
