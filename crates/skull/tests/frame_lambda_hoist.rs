use crate::common::{emit_module, function_body, function_params, mangled_name};

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

fn state_capture_field(step: &str, state_param: &str) -> Option<String> {
    let start = step.find("setmetatable({")? + "setmetatable({".len();
    let record = &step[start..];
    let end = record.find("}, __rock_fn_mt)")?;
    record[..end].split(',').find_map(|field| {
        let (name, value) = field.split_once('=')?;
        (value.trim() == state_param).then(|| name.trim().to_string())
    })
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

#[test]
fn param_lambda_hoists_as_record_under_frame() {
    let lua = emit_module(PARAM_CAPTURE);
    let w = mangled_name(&lua, "worker");
    let step_name = format!("{w}__step");
    let step = function_body(&lua, &step_name);
    // `s` is the step's first param cell. Its environment field keeps the
    // stable emit-cell key, while the field value is the colored param name.
    let s = function_params(&lua, &step_name)[0];
    let state_field = state_capture_field(step, s);
    // the co-fn lambda hoists to a sibling top-level fn taking (env, <param>).
    let lp = function_params(&lua, &format!("{w}__step__lambda0"));
    assert_eq!(lp.len(), 2);
    assert!(step.contains("__slot = __rock_code_slot_ref(") && state_field.is_some());
    assert!(!step.contains("function("));
}

#[test]
fn frame_capture_composes_env_then_frame() {
    let lua = emit_module(PARAM_CAPTURE);
    let w = mangled_name(&lua, "worker");
    let step = function_body(&lua, &format!("{w}__step"));
    let s = function_params(&lua, &format!("{w}__step"))[0];
    let state_field = state_capture_field(step, s)
        .unwrap_or_else(|| panic!("the step must capture its state param `{s}`:\n{step}"));
    let body = function_body(&lua, &format!("{w}__step__lambda0"));
    assert!(body.contains(&format!(".{state_field}")) && contains_digit_bridge(body, ".c", ""));
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

#[test]
fn capture_free_lambda_hoists_bare_under_frame() {
    let lua = emit_module(CAPTURE_FREE);
    let w = mangled_name(&lua, "worker");
    let step = function_body(&lua, &format!("{w}__step"));
    assert!(lua.contains(&format!("function {w}__step__lambda0()")));
    assert!(step.contains("__rock_code_slot_ref(") && !step.contains("function("));
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

#[test]
fn loop_binding_snapshots_by_value_into_record() {
    let lua = emit_module(LOOP_BINDING);
    let lp_name = mangled_name(&lua, "looper");
    let step = function_body(&lua, &format!("{lp_name}__step"));
    assert!(step.contains("__slot = __rock_code_slot_ref("));
    // The record field value is a frame-field READ (`= c<S>.c<i>`): snapshot,
    // not a shared reference.
    assert!(contains_digit_bridge(step, "= c", ".c"));
    // The hoisted body reads its by-value snapshot as a single `.c<slot>` field
    // hop off its env, and NEVER through `s` at call time (the final-value bug
    // D4(b) avoids -- a via-`s` capture would need another field hop).
    let body = function_body(&lua, &format!("{lp_name}__step__lambda"));
    assert!(contains_digit_bridge(body, ".c", ""));
    assert_eq!(body.matches(".c").count(), 1);
}
