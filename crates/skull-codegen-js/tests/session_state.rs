use crate::common::ws_with;
use skull_mir::mir::FuncRef;

const FIXTURE: &str = r#"
global COUNTER: int = 0

fn bump() -> int {
    COUNTER = COUNTER + 1
    return COUNTER
}

fn main() {
    bump()
}
"#;

#[test]
fn compiler_generated_mutable_global_accesses_require_the_active_session() {
    let ws = ws_with(FIXTURE);
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let emitted = skull_codegen_js::emit::emit_program(&ws, mid).expect("JS emit");
    assert!(
        emitted.program.contains("__rock_state_get("),
        "mutable global reads must use the active Session"
    );
    assert!(
        emitted.program.contains("__rock_state_set("),
        "mutable global writes must use the active Session"
    );
}

fn node_available() -> bool {
    crate::common::node()
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn emit_root(src: &str, root: &str) -> String {
    let ws = ws_with(src);
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find_map(|(fid, function)| (function.name.as_str() == root).then_some(fid))
        .expect("root function");
    let program = skull_codegen_js::emit::emit_program_with_roots(
        &ws,
        &[(FuncRef { module: mid, func: fid }, root.to_string())],
    )
    .expect("JS emit");
    format!(
        "{}\nvar __G = {{}};\n{}\n",
        skull_codegen_js::JS_SESSION_RUNTIME,
        program
    )
}

fn run_node(program: &str) -> String {
    let output = crate::common::node()
        .arg("-e")
        .arg(program)
        .output()
        .expect("spawn node");
    assert!(
        output.status.success(),
        "node failed:\n{program}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn two_js_sessions_initialize_and_isolate_mutable_globals() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let mut program = emit_root(FIXTURE, "bump");
    program.push_str(
        "let a = __rock_active_session; let schema = __rock_state_schema_id; "
    );
    program.push_str("console.log(bump()); let b = __rock_session_new(schema); ");
    program.push_str("__rock_session_activate(b); __rock_state_initialize(); console.log(bump()); ");
    program.push_str("__rock_session_activate(a); console.log(bump());\n");
    assert_eq!(run_node(&program), "1\n1\n2");
}

#[test]
fn static_value_and_done_state_are_session_local_in_js() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let source = r#"
fn bump_static() -> int {
    static value = 10
    value = value + 1
    return value
}
"#;
    let mut program = emit_root(source, "bump_static");
    program.push_str("let a = __rock_active_session; let schema = __rock_state_schema_id; ");
    program.push_str("console.log(bump_static()); let b = __rock_session_new(schema); ");
    program.push_str("__rock_session_activate(b); console.log(bump_static()); ");
    program.push_str("__rock_session_activate(a); console.log(bump_static());\n");
    assert_eq!(run_node(&program), "11\n11\n12");
}
