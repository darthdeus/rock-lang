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

#[test]
fn nobox_leaves_and_aliases_are_session_local() {
    let source = r#"
global ROOT: [int] = [1]
global ALIAS: [int] = ROOT

fn mutate_through_root() -> int {
    ROOT[0] = ROOT[0] + 1
    return ALIAS[0]
}
"#;
    let emitted = emit_mir(source);
    let mutate = mangled_name(&emitted.lua, "mutate_through_root");
    let program = format!(
        "{}\nlocal a = __rock_active_session\nlocal schema = __rock_state_schema_id\nprint({mutate}())\nlocal b = __rock_session_new(schema)\n__rock_session_activate(b)\n__rock_state_initialize()\nprint({mutate}())\n__rock_session_activate(a)\nprint({mutate}())\n",
        emitted.lua
    );
    assert_eq!(run_luajit(&program, "two_sessions_alias"), "2\n2\n3");
}

#[test]
fn failed_state_publication_restores_active_session_and_slots() {
    let source = r#"
global COUNTER: int = 0

fn bump_static() -> int {
    static value = 10
    value = value + 1
    return value
}
"#;
    let emitted = emit_mir(source);
    let bump_static = mangled_name(&emitted.lua, "bump_static");
    let program = format!(
        r#"{lua}
local global_entry
local static_entry
for _, entry in ipairs(__ROCK_STATE_SCHEMA) do
  if entry.identity:match("COUNTER$") then global_entry = entry end
  if entry.kind == "static" then static_entry = entry end
end
local global_key = global_entry.session_keys[1]
local static_value_key = static_entry.session_keys[1]
local static_done_key = static_entry.session_keys[2]
local a = __rock_active_session
local schema = __rock_state_schema_id
__rock_state_set(global_key, 7)
local first = {bump_static}()
local b = __rock_session_new(schema)
__rock_session_activate(b)
__rock_state_initialize()
local b_first = {bump_static}()
__rock_session_activate(a)
local before_static = __rock_state_get(static_value_key)
local old_schema = __ROCK_STATE_SCHEMA
__rock_state_transaction_begin()
__ROCK_STATE_SCHEMA = {{}}
__rock_state_set(global_key, 99)
__rock_state_set(static_value_key, 88)
__rock_state_set(static_done_key, false)
__rock_session_activate(b)
local ok = pcall(function() error("late publication") end)
__rock_state_transaction_abort()
print(ok == false and __rock_active_session == a
  and __ROCK_STATE_SCHEMA == old_schema
  and __rock_state_get(global_key) == 7
  and __rock_state_get(static_value_key) == before_static
  and __rock_state_has(static_done_key)
  and __rock_state_get(static_done_key) == true
  and b_first == 11)
"#,
        lua = emitted.lua,
    );
    assert_eq!(run_luajit(&program, "failed_state_publication"), "true");
}

#[test]
fn lifecycle_roots_run_against_the_active_session() {
    let source = r#"
global COUNTER: int = 0

@lifecycle(phase="init", owner="session")
deco init tick {}

@tick
fn lifecycle_bump() {
    COUNTER = COUNTER + 1
}
"#;
    let emitted = emit_mir(source);
    let program = format!(
        r#"{lua}
local run = __ROCK_LIFECYCLE_PLAN.init._data[1].func
local key = "global:<test>::COUNTER::value"
local a = __rock_active_session
run()
local b = __rock_session_new(__rock_state_schema_id)
__rock_session_activate(b)
__rock_state_initialize()
run()
local b_value = __rock_state_get(key)
__rock_session_activate(a)
run()
print(b_value .. ":" .. tostring(__rock_state_get(key)))
"#,
        lua = emitted.lua,
    );
    assert_eq!(run_luajit(&program, "lifecycle_session"), "1:2");
}
