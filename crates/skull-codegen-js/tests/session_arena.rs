//! A generic instance added by an edit must be callable from the live overlay.
//! Removing and reintroducing it must keep live behavior in step with a fresh
//! emission.

use crate::common::{overlay_of_fn, run_node, ws_with};

const BEFORE: &str = "\
fn pick<T>(x: T, y: T) -> T { x }\n\
@nomangle fn hot() -> float { pick(1.0, 2.0) }\n\
fn main() { hot() }\n";

const AFTER: &str = "\
fn pick<T>(x: T, y: T) -> T { x }\n\
@nomangle fn hot() -> float {\n\
    let n = pick(2, 3)\n\
    pick(1.0, 2.0) + int_to_float(n)\n\
}\n\
fn main() { hot() }\n";

fn emit_program(src: &str) -> skull_codegen_js::emit::JsProgram {
    let ws = ws_with(src);
    let mid = crate::common::test_mid(&ws);
    skull_codegen_js::emit::emit_program(&ws, mid).expect("fresh JS emit")
}

fn run_hot(program: &skull_codegen_js::emit::JsProgram) -> String {
    let mut js = skull_codegen_js::assemble_prelude(
        &program.program,
        program.has_code_refs(),
        skull_codegen_js::JS_NODE_HOST,
        &[],
    );
    js.push_str(&program.program);
    js.push_str("\nconsole.log(hot());");
    run_node(&js).trim().to_string()
}

#[test]
fn generic_instance_edits_match_fresh_runtime_results() {
    let before = ws_with(BEFORE);
    let mid = crate::common::test_mid(&before);
    let (initial, mut cache) =
        skull_codegen_js::emit::emit_program_cached(&before, mid).expect("cached JS emit");

    let mut overlay_run = skull_codegen_js::assemble_prelude(
        &initial.program,
        initial.has_code_refs(),
        skull_codegen_js::JS_NODE_HOST,
        &[],
    );
    overlay_run.push_str(&initial.program);
    overlay_run.push_str("\nconsole.log(hot());");

    let stages = [AFTER, BEFORE, AFTER];
    for source in stages {
        let ws = ws_with(source);
        let hot = crate::common::test_fn_ref(&ws, "hot");
        let overlay = overlay_of_fn(&ws, mid, &mut cache, hot);
        overlay_run.push_str(&format!("\n(0, eval)({overlay:?});\nconsole.log(hot());"));
    }

    let overlay_output = run_node(&overlay_run).trim().to_string();
    let fresh_results = [BEFORE, AFTER, BEFORE, AFTER].map(|source| run_hot(&emit_program(source)));
    assert_eq!(fresh_results, ["1", "3", "1", "3"]);
    assert_eq!(overlay_output, fresh_results.join("\n"));
}
