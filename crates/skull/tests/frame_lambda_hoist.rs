//! P1 of docs/coroutine-frame-lambda-plan.md: lambdas inside a `co fn` hoist
//! exactly like non-frame lambdas -- capture-free to a bare top-level fn,
//! capturing to an env record -- with coroutine state captured through the
//! frame table `s` itself (record field `c<S> = c<S>`; body reads compose
//! `__rock_env.c<S>.c<N>`). The lambda's own params/locals stay plain locals
//! of the hoisted body, which is what fixes the nil-param miscompile pinned in
//! tests/coroutine_lambda_param_sentinel.rock.

mod common;
use common::{emit_module, function_body};

/// Scan for `needle` followed by one-or-more ASCII digits followed by `tail`
/// (e.g. `__rock_env.c` + digits + `.c` pins the via-`s` composed read without
/// hardcoding the s cell's raw id).
fn contains_digit_bridge(hay: &str, needle: &str, tail: &str) -> bool {
    let mut rest = hay;
    while let Some(p) = rest.find(needle) {
        let after = &rest[p + needle.len()..];
        let ndig = after.bytes().take_while(|b| b.is_ascii_digit()).count();
        if ndig > 0 && after[ndig..].starts_with(tail) {
            return true;
        }
        rest = &rest[p + needle.len()..];
    }
    false
}

const PARAM_CAPTURE: &str = r#"
enum Ev { Tick(int) }
global R: int = 0

co fn worker() -> Coroutine<Ev> {
    let counter = 100
    let add = (n: int) => n + counter
    R = add(5)
    yield Ev::Tick(1)
}

fn main() {
    let c = worker()
    let _ = c.resume()
}
"#;

/// A capturing lambda with a param inside a co fn hoists to a sibling
/// top-level fn taking `(__rock_env, <param>)`; the step builds an env record.
/// The step body must hold NO inline closure (`function(` with no name).
#[test]
fn param_lambda_hoists_as_record_under_frame() {
    let lua = emit_module(PARAM_CAPTURE);
    let step = function_body(&lua, "worker__step");
    assert!(
        lua.contains("function worker__step__lambda0(__rock_env, c"),
        "the co-fn lambda must hoist to `worker__step__lambda0(__rock_env, c<param>)`:\n{lua}"
    );
    assert!(
        step.contains("setmetatable({ __fn = worker__step__lambda0"),
        "the step must build an env record for the capturing lambda:\n{step}"
    );
    assert!(
        !step.contains("function("),
        "the step must hold no inline closure once frame lambdas hoist:\n{step}"
    );
}

/// The frame capture collapses to `s`: the hoisted body reads the captured
/// co-fn local through the record's s field (`__rock_env.c<S>.c<N>`), and the
/// lambda's own param stays a plain local (never a frame-field read).
#[test]
fn frame_capture_composes_env_then_frame() {
    let lua = emit_module(PARAM_CAPTURE);
    let body = function_body(&lua, "worker__step__lambda0");
    assert!(
        contains_digit_bridge(body, "__rock_env.c", ".c"),
        "the hoisted body must read the frame capture as `__rock_env.c<S>.c<N>`:\n{body}"
    );
}

const CAPTURE_FREE: &str = r#"
enum Ev { Tick(int) }
global R: int = 0

co fn worker() -> Coroutine<Ev> {
    let f = () => 42
    R = f()
    yield Ev::Tick(1)
}

fn main() {
    let c = worker()
    let _ = c.resume()
}
"#;

/// A capture-free lambda in a co fn hoists to a bare top-level fn referenced
/// by name -- no record, no setmetatable, no inline closure.
#[test]
fn capture_free_lambda_hoists_bare_under_frame() {
    let lua = emit_module(CAPTURE_FREE);
    let step = function_body(&lua, "worker__step");
    assert!(
        lua.contains("function worker__step__lambda0()"),
        "the capture-free co-fn lambda must hoist to a bare `worker__step__lambda0()`:\n{lua}"
    );
    assert!(
        !step.contains("setmetatable") && !step.contains("function("),
        "capture-free: the step references the bare name (no record, no inline closure):\n{step}"
    );
}

const LOOP_BINDING: &str = r#"
enum Ev { Tick(int) }
global FS: [fn() -> int] = []

co fn looper() -> Coroutine<Ev> {
    for i in 0..3 {
        FS.push(() => i)
        yield Ev::Tick(i)
    }
}

fn main() {
    let c = looper()
    let _ = c.resume()
}
"#;

/// D4(b): a read-only captured loop binding snapshots BY VALUE into the record
/// at creation (`c<N> = c<S>.c<N>` -- a frame-field read in the field value),
/// and the hoisted body reads the snapshot from its own env, NOT through `s`.
#[test]
fn loop_binding_snapshots_by_value_into_record() {
    let lua = emit_module(LOOP_BINDING);
    let step = function_body(&lua, "looper__step");
    assert!(
        step.contains("setmetatable({ __fn = looper__step__lambda"),
        "the loop lambda must build an env record:\n{step}"
    );
    // The record field value is a frame-field READ (`= c<S>.c<i>`): snapshot,
    // not a shared reference.
    assert!(
        contains_digit_bridge(step, "= c", ".c"),
        "the record must snapshot the loop binding from the frame (`= c<S>.c<N>`):\n{step}"
    );
    // The hoisted body reads its by-value env field directly -- no `.c` hop
    // after the env access for the binding read.
    let body = function_body(&lua, "looper__step__lambda");
    assert!(
        body.contains("__rock_env.c"),
        "the hoisted body must read the snapshot from its env:\n{body}"
    );
    assert!(
        !contains_digit_bridge(body, "__rock_env.c", ".c"),
        "a read-only loop binding must NOT read through `s` at call time (that is \
         the final-value bug):\n{body}"
    );
}
