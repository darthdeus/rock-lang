use crate::common::{emit_mir, mangled_name, run_luajit};

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
    let emitted = emit_mir(FIXTURE);
    assert!(
        emitted.lua.contains("__rock_state_get("),
        "mutable global reads must use the active Session"
    );
    assert!(
        emitted.lua.contains("__rock_state_set("),
        "mutable global writes must use the active Session"
    );
}

#[test]
fn two_lua_sessions_initialize_and_isolate_mutable_globals() {
    let emitted = emit_mir(FIXTURE);
    let bump = mangled_name(&emitted.lua, "bump");
    let program = format!(
        "{}\nlocal a = __rock_active_session\nlocal schema = __rock_state_schema_id\nprint({bump}())\nlocal b = __rock_session_new(schema)\n__rock_session_activate(b)\n__rock_state_initialize()\nprint({bump}())\n__rock_session_activate(a)\nprint({bump}())\n",
        emitted.lua
    );
    assert_eq!(run_luajit(&program, "two_sessions_global"), "1\n1\n2");
}

#[test]
fn static_value_and_done_state_are_session_local() {
    let source = r#"
fn bump_static() -> int {
    static value = 10
    value = value + 1
    return value
}
"#;
    let emitted = emit_mir(source);
    let bump = mangled_name(&emitted.lua, "bump_static");
    let program = format!(
        "{}\nlocal a = __rock_active_session\nlocal schema = __rock_state_schema_id\nprint({bump}())\nlocal b = __rock_session_new(schema)\n__rock_session_activate(b)\nprint({bump}())\n__rock_session_activate(a)\nprint({bump}())\n",
        emitted.lua
    );
    assert_eq!(run_luajit(&program, "two_sessions_static"), "11\n11\n12");
}
