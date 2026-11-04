//! Findings 3+4+5: every synthetic
//! top-level Lua name codegen mints -- `<fn>__lambda<k>`, and the coroutine
//! machinery `<co>__step`/`__resume`/`__advance`/`__defer<k>` +
//! `__on_killed`/`__on_resume`/`__on_hot_reload` -- is minted through ONE
//! `CodegenCtx::mint_synthetic` choke point that rejects a collision with a user
//! fn OR global (a spanned `CompilerError`, not a panic / silent clobber).
//!
//! The REJECTION cases (a user `foo_0__lambda0` fn/global, a `worker_0__on_killed`
//! hook name, a `worker_0__step`) now live in
//! `tests/errors/synthetic_name_collision.rock.err`, run through the error-test
//! harness's skull-emit extension. What stays HERE is what a rejection suite
//! can't express: the POSITIVE CONTROLS -- a look-alike name mints NO synthetic
//! when the emitting shape is ABSENT (no co-fn hook -> no `__on_killed`; no
//! lambdas -> no `__lambda0`), proven by the user name surviving into emitted
//! Lua.
use crate::common::emit_module;

/// POSITIVE CONTROL: `fn worker_0__on_killed` next to a co fn `worker` with NO
/// hooks mints no `__on_killed` synthetic name -> no collision -> compiles.
#[test]
fn fn_named_like_hook_without_hook_compiles() {
    let src = r#"
@nomangle fn worker_0__on_killed() -> int { return 42 }
co fn worker() -> Coroutine<int> {
    yield 1
}
fn main() { print("{worker_0__on_killed()}") }
"#;
    let lua = emit_module(src); // must not reject
    assert!(
        lua.contains("function worker_0__on_killed"),
        "the user fn is emitted:\n{lua}"
    );
}

/// POSITIVE CONTROL: a fn named `<name>__lambda0` in a file whose enclosing fn
/// `bar` has NO lambdas mints no `bar_0__lambda0` -> no collision -> compiles.
#[test]
fn fn_named_like_lambda_without_any_lambda_compiles() {
    let src = r#"
@nomangle fn bar_0__lambda0() -> int { return 3 }
fn bar() -> int { return 1 }
fn main() { print("{bar_0__lambda0()}") }
"#;
    let lua = emit_module(src); // must not reject
    assert!(
        lua.contains("function bar_0__lambda0"),
        "the user fn is emitted:\n{lua}"
    );
}
