//! P1 of docs/coroutine-frame-lambda-plan.md, delivered via the shared MIR pass
//! (S3 of docs/closure-conversion-mir-plan.md): lambdas inside a `co fn` hoist
//! exactly like non-frame lambdas -- capture-free to a bare top-level fn,
//! capturing to an env record -- with coroutine state captured through the
//! frame table `s` itself (record field `c<S> = <s>`; body reads compose
//! `<env>.c<S>.c<slot>`). Since S3 the conversion is materialized as MIR by
//! `convert_closures`, so the env param is a COLORED cell (not the literal
//! `__rock_env`) and the composed access is split across temps -- these pins
//! discriminate the via-`s` collapse (body reads THROUGH the s-field) from a
//! by-value loop-binding snapshot (body reads its own env field, never via `s`).
//! The exact per-iteration/param VALUES are pinned on luajit by
//! tests/closure_capture_semantics_cofn.rock; here we pin the emit SHAPE.

mod common;
use common::{emit_module, function_body, mangled_name};

/// Scan for `needle` followed by one-or-more ASCII digits followed by `tail`
/// (e.g. `= c` + digits + `.c` pins a frame-field read `= c<S>.c<slot>` without
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

/// The parameter names of `function <name>(...)` in the emitted Lua (post-pass a
/// hoisted body's first param is its colored env cell, then the lambda's own
/// params).
fn fn_params(lua: &str, name: &str) -> Vec<String> {
    let head = format!("function {name}(");
    let p = lua.find(&head).unwrap_or_else(|| panic!("no `function {name}(` in:\n{lua}"));
    let after = &lua[p + head.len()..];
    let close = after.find(')').expect("param list closes");
    after[..close].split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
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
    let w = mangled_name(&lua, "worker");
    let step_name = format!("{w}__step");
    let step = function_body(&lua, &step_name);
    // `s` is the step's first param cell; the frame capture collapses to it, so
    // the record field is named `c<raw(s)>` == the step's first param name.
    let s = fn_params(&lua, &step_name).swap_remove(0);
    // the co-fn lambda hoists to a sibling top-level fn taking (env, <param>).
    let lp = fn_params(&lua, &format!("{w}__step__lambda0"));
    assert_eq!(lp.len(), 2, "the hoisted lambda must take (env, n):\n{lua}");
    assert!(
        step.contains(&format!("setmetatable({{ __fn = {w}__step__lambda0, {s} =")),
        "the step must build an env record collapsing the frame capture to `s` (field `{s}`):\n{step}"
    );
    assert!(
        !step.contains("function("),
        "the step must hold no inline closure once frame lambdas hoist:\n{step}"
    );
}

/// The frame capture collapses to `s`: the hoisted body reaches the captured
/// co-fn local THROUGH the record's s field (`<env>` -> `.c<S>` -> `.c<slot>`,
/// materialized via temps), so the body reads the s-field `.c<S>`. The lambda's
/// own param stays a plain local (never a frame-field read).
#[test]
fn frame_capture_composes_env_then_frame() {
    let lua = emit_module(PARAM_CAPTURE);
    let w = mangled_name(&lua, "worker");
    let s = fn_params(&lua, &format!("{w}__step")).swap_remove(0);
    let body = function_body(&lua, &format!("{w}__step__lambda0"));
    assert!(
        body.contains(&format!(".{s}")),
        "the hoisted body must reach the frame capture through the s-field `.{s}` (via-`s` collapse):\n{body}"
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
    let w = mangled_name(&lua, "worker");
    let step = function_body(&lua, &format!("{w}__step"));
    assert!(
        lua.contains(&format!("function {w}__step__lambda0()")),
        "the capture-free co-fn lambda must hoist to a bare `{w}__step__lambda0()`:\n{lua}"
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
    let lp_name = mangled_name(&lua, "looper");
    let step = function_body(&lua, &format!("{lp_name}__step"));
    assert!(
        step.contains(&format!("setmetatable({{ __fn = {lp_name}__step__lambda")),
        "the loop lambda must build an env record:\n{step}"
    );
    // The record field value is a frame-field READ (`= c<S>.c<i>`): snapshot,
    // not a shared reference.
    assert!(
        contains_digit_bridge(step, "= c", ".c"),
        "the record must snapshot the loop binding from the frame (`= c<S>.c<N>`):\n{step}"
    );
    // The hoisted body reads its by-value snapshot as a single `.c<slot>` field
    // hop off its env, and NEVER through `s` at call time (the final-value bug
    // D4(b) avoids -- a via-`s` capture would read the s-field `.c<S>`).
    let s = fn_params(&lua, &format!("{lp_name}__step")).swap_remove(0);
    let body = function_body(&lua, &format!("{lp_name}__step__lambda"));
    assert!(
        contains_digit_bridge(body, ".c", ""),
        "the hoisted body must read a snapshot field `.c<slot>` from its env:\n{body}"
    );
    assert!(
        !body.contains(&format!(".{s}")),
        "a read-only loop binding must NOT read through the s-field `.{s}` at call time:\n{body}"
    );
}
