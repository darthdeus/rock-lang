//! Findings 3+4+5 (docs/closure-env-records-plan.md review): every synthetic
//! top-level Lua name codegen mints -- `<fn>__lambda<k>`, and the coroutine
//! machinery `<co>__step`/`__resume`/`__advance`/`__defer<k>` +
//! `__on_killed`/`__on_resume`/`__on_hot_reload` -- is now minted through ONE
//! `CodegenCtx::mint_synthetic` choke point that rejects a collision with a user
//! fn OR global (a spanned `CompilerError`, not a panic / silent clobber).
//!
//! Pre-fix these were three separate defects: a hook name collision SILENTLY ran
//! the hook body for the user's call (F3), a global collision CLOBBERED the
//! hoisted lambda with the global's value (F4), and a fn-vs-lambda collision
//! PANICKED the compiler (F5, `assert!`). The `__step`/`__defer`/`__advance`/
//! `__resume` class was unguarded entirely on master -- same silent clobber.
//!
//! These are CODEGEN-level rejections (bone accepts every case -- the names are
//! valid identifiers; only skull's mangling makes them collide), so they are
//! tested on the emit path, not the bone-only `tests/errors` suite. The positive
//! controls pin that a synthetic name is minted ONLY when the shape that emits it
//! is present (no co-fn hooks -> no `__on_killed`; no lambdas -> no `__lambda0`).

mod common;
use common::{emit_module, try_emit_module};

fn reject_msg(src: &str) -> String {
    let err = try_emit_module(src).err().expect("emit must reject the collision");
    assert!(
        err.message.contains("compiler-reserved synthetic name"),
        "the rejection must name the reserved-suffix class; got: {}",
        err.message
    );
    err.message
}

/// F5: a user fn named like an enclosing fn's hoisted lambda. Pre-fix: `assert!`
/// PANIC (no span, killed `rock stone`/`lsp serve`). Now a spanned rejection.
#[test]
fn fn_vs_lambda_rejected() {
    let src = r#"
fn foo__lambda0() -> int { return 7 }
fn foo() -> fn() -> int { return () => 3 }
fn main() { let f = foo() print("{f()}") }
"#;
    let msg = reject_msg(src);
    assert!(msg.contains("foo__lambda0"), "names the colliding synthetic name; got: {msg}");
}

/// F4: a user GLOBAL named like a hoisted lambda. Pre-fix: the global's value
/// clobbered the fn at init ("attempt to call a number value"). The collision set
/// must include globals, not only fn names.
#[test]
fn global_vs_lambda_rejected() {
    let src = r#"
global foo__lambda0: int = 5
fn foo() -> fn() -> int { return () => 3 }
fn main() { let f = foo() print("{f()}") }
"#;
    let msg = reject_msg(src);
    assert!(msg.contains("foo__lambda0"), "names the colliding synthetic name; got: {msg}");
}

/// F3: a user fn named like a co fn's hook. Pre-fix: TWO top-level defs; the
/// user's call ran the hook body (returned nil silently).
#[test]
fn fn_vs_hook_rejected() {
    let src = r#"
@nomangle fn worker__on_killed() -> int { return 999 }
co fn worker() -> Coroutine<int> {
    on_killed { }
    yield 1
}
fn main() { print("{worker__on_killed()}") }
"#;
    let msg = reject_msg(src);
    assert!(msg.contains("worker__on_killed"), "names the colliding hook name; got: {msg}");
}

/// The pre-existing (master-unguarded) class: a user fn named like a co fn's
/// `__step`. Same silent clobber; now rejected by the same guard.
#[test]
fn fn_vs_step_rejected() {
    let src = r#"
@nomangle fn worker__step() -> int { return 1 }
co fn worker() -> Coroutine<int> {
    yield 1
    yield 2
}
fn main() { print("{worker__step()}") }
"#;
    let msg = reject_msg(src);
    assert!(msg.contains("worker__step"), "names the colliding step name; got: {msg}");
}

/// POSITIVE CONTROL: `fn worker__on_killed` next to a co fn `worker` with NO
/// hooks mints no `__on_killed` synthetic name -> no collision -> compiles.
#[test]
fn fn_named_like_hook_without_hook_compiles() {
    let src = r#"
@nomangle fn worker__on_killed() -> int { return 42 }
co fn worker() -> Coroutine<int> {
    yield 1
}
fn main() { print("{worker__on_killed()}") }
"#;
    let lua = emit_module(src); // must not reject
    assert!(lua.contains("function worker__on_killed"), "the user fn is emitted:\n{lua}");
}

/// POSITIVE CONTROL: a fn named `<name>__lambda0` in a file whose enclosing fn
/// `bar` has NO lambdas mints no `bar__lambda0` -> no collision -> compiles.
#[test]
fn fn_named_like_lambda_without_any_lambda_compiles() {
    let src = r#"
fn bar__lambda0() -> int { return 3 }
fn bar() -> int { return 1 }
fn main() { print("{bar__lambda0()}") }
"#;
    let lua = emit_module(src); // must not reject
    assert!(lua.contains("function bar__lambda0"), "the user fn is emitted:\n{lua}");
}
