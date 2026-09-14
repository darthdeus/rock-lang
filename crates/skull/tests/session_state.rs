use crate::common::{emit_mir, mangled_name, run_luajit, ws_with_stdlib};
use skull::mir_emit::{emit_full_with_shapes, ShapeOverrides};
use skull_codegen_lua::CalleeBindingMode;
use ustr::Ustr;

/// The fixture emitted under an explicit callee-binding shape and Session-cell
/// switch, so one process can pin both spellings of a mutable-global access.
fn emit_shaped(src: &str, binding: CalleeBindingMode, state_cell: Option<bool>) -> String {
    let ws = ws_with_stdlib(src);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let shapes = ShapeOverrides { callee_binding: Some(binding), state_cell, ..ShapeOverrides::default() };
    emit_full_with_shapes(&ws, mid, shapes).expect("emit").0.lua
}

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
fn lua_global_session_slots_are_dense_numeric_realizations() {
    let emitted = emit_mir(FIXTURE);
    let global_walk = emit_shaped(FIXTURE, CalleeBindingMode::Global, None);
    assert!(
        global_walk.contains("__rock_active_session.__rock_state[5]"),
        "the fixture global must use its compact numeric Lua Session slot"
    );
    assert!(
        emitted.lua.contains(
            "identity=\"global:<test>::COUNTER\",fingerprint=\"type=int;representation=opaque;storage=None;column=false;nobox=false;lanes=1\",physical={\"COUNTER\"},session_keys={5}"
        ),
        "StateSchema must map the logical identity to the same numeric Lua slot"
    );
    assert!(
        !emitted
            .lua
            .contains("__rock_active_session.__rock_state[\"global:<test>::COUNTER::value\"]"),
        "logical StateSchema identities must not remain hot Lua table keys"
    );
}

#[test]
fn lua_global_session_slot_order_is_declaration_independent() {
    let first = emit_mir(
        "global ZED: int = 0\n\
         global ALPHA: int = 0\n\
         fn main() { ZED = ALPHA }\n",
    );
    let second = emit_mir(
        "global ALPHA: int = 0\n\
         global ZED: int = 0\n\
         fn main() { ZED = ALPHA }\n",
    );
    for emitted in [&first.lua, &second.lua] {
        assert!(
            emitted.contains(
                "identity=\"global:<test>::ALPHA\",fingerprint=\"type=int;representation=opaque;storage=None;column=false;nobox=false;lanes=1\",physical={\"ALPHA\"},session_keys={5}"
            ),
            "ALPHA must keep the first user-global Lua slot"
        );
        assert!(
            emitted.contains(
                "identity=\"global:<test>::ZED\",fingerprint=\"type=int;representation=opaque;storage=None;column=false;nobox=false;lanes=1\",physical={\"ZED\"},session_keys={6}"
            ),
            "ZED must keep the second user-global Lua slot"
        );
    }
}

#[test]
fn generated_state_accesses_follow_mid_call_session_swaps() {
    let lua = emit_shaped(FIXTURE, CalleeBindingMode::Global, None);
    assert!(
        !lua.contains("local __rock_active_state = __rock_active_session.__rock_state"),
        "state-using functions must not cache a Session table across activation"
    );
    assert!(
        lua.contains("__rock_active_session.__rock_state[5]"),
        "mutable global reads and writes must follow the active Session"
    );
    assert!(
        lua.contains("__rock_active_session.__rock_initialized[5] = true"),
        "mutable global writes must mark the active Session initialized"
    );
}

#[test]
fn state_name_lookup_keeps_index_construction_out_of_the_hot_function() {
    let emitted = emit_mir(FIXTURE);
    let hot = emitted
        .lua
        .split("_G.__rock_state_entry_for_name = function(name)")
        .nth(1)
        .expect("state-name lookup function")
        .split("_G.__rock_state_entry_for_identity")
        .next()
        .expect("state-name lookup body");
    assert!(
        !hot.contains("local function"),
        "the traced state-name lookup must not carry the cold index-builder closure"
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
fn rng_state_suffix_restore_and_session_isolation() {
    let emitted = emit_mir("fn main() {}\n");
    let program = format!(
        r#"{lua}
local a = __rock_active_session
__rock_rng_seed(17)
local first = __rock_rng_next()
local checkpoint = __rock_rng_state_get()
local second = __rock_rng_next()
local third = __rock_rng_next()
__rock_rng_state_set(checkpoint)
local replay_second = __rock_rng_next()
local replay_third = __rock_rng_next()
local b = __rock_session_new(__rock_state_schema_id)
__rock_session_activate(b)
__rock_rng_seed(99)
local b_first = __rock_rng_next()
local b_checkpoint = __rock_rng_state_get()
local b_second = __rock_rng_next()
__rock_session_activate(a)
local a_fourth = __rock_rng_next()
__rock_session_activate(b)
__rock_rng_state_set(b_checkpoint)
local replay_b_second = __rock_rng_next()
print(first ~= second and second ~= third
  and second == replay_second and third == replay_third
  and a_fourth ~= b_first and b_second == replay_b_second)
"#,
        lua = emitted.lua,
    );
    assert_eq!(run_luajit(&program, "rng_suffix_and_sessions"), "true");
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
local entry = __rock_state_entry_for_name("COUNTER")
local key = entry.session_keys[1]
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

// --- Session cells under the bound shape (docs/backends.md) -----------------
//
// Under `CalleeBindingMode::Upvalue` a mutable-global access reads the active
// Session's tables through two chunk-level locals, `__rock_state_cell` and
// `__rock_initialized_cell`, instead of walking
// `_G.__rock_active_session.__rock_state` (three guarded hash lookups that
// LuaJIT re-executes after every store to the state table). Every writer of the
// active Session publishes to the cells through `__rock_session_publish`, so
// the mid-call-swap rule above still holds.

#[test]
fn bound_emission_reads_state_through_the_session_cells() {
    let lua = emit_shaped(FIXTURE, CalleeBindingMode::Upvalue, None);
    assert!(
        lua.contains("local __rock_state_cell, __rock_initialized_cell"),
        "the prelude must declare the cells as chunk-level locals:\n{}",
        &lua[..lua.len().min(4000)]
    );
    assert!(lua.contains("__rock_state_cell[5]"), "reads and writes go through the state cell");
    assert!(
        lua.contains("__rock_initialized_cell[5] = true"),
        "writes mark the initialized cell"
    );
    assert!(
        !lua.contains("__rock_active_session.__rock_state[5]"),
        "no access may keep the `_G` walk under the bound shape"
    );
    assert!(
        !lua.contains("local __rock_active_state = __rock_active_session.__rock_state"),
        "state-using functions must not cache a Session table across activation"
    );
}

#[test]
fn global_emission_keeps_the_session_walk() {
    let lua = emit_shaped(FIXTURE, CalleeBindingMode::Global, None);
    assert!(lua.contains("__rock_active_session.__rock_state[5]"));
    assert!(
        !lua.contains("__rock_state_cell["),
        "an overlay-capable build must never read a chunk-level cell"
    );
}

#[test]
fn state_cell_switch_off_keeps_the_session_walk_under_bound_emission() {
    let lua = emit_shaped(FIXTURE, CalleeBindingMode::Upvalue, Some(false));
    assert!(lua.contains("__rock_active_session.__rock_state[5]"));
    assert!(!lua.contains("__rock_state_cell["));
}

/// Every path that swaps the active Session -- `__rock_session_activate`, the
/// direct publisher the state-image code uses, and a transaction abort -- must
/// reach the cells, or a bound function would read the previous Session.
#[test]
fn session_cells_follow_every_session_writer() {
    let lua = emit_shaped(FIXTURE, CalleeBindingMode::Upvalue, None);
    let bump = mangled_name(&lua, "bump");
    let program = format!(
        "{lua}
local a = __rock_active_session
local schema = __rock_state_schema_id
print({bump}())
local b = __rock_session_new(schema)
__rock_session_activate(b)
__rock_state_initialize()
print({bump}())
__rock_session_publish(a)
print({bump}())
__rock_state_transaction_begin()
__rock_session_activate(b)
__rock_state_transaction_abort()
print({bump}())
"
    );
    assert_eq!(run_luajit(&program, "session_cells_writers"), "1\n1\n2\n3");
}

/// The rule `generated_state_accesses_follow_mid_call_session_swaps` pins, on
/// the cell shape at runtime: a callee swaps the Session in the middle of the
/// caller, and the caller's NEXT read sees the new Session.
#[test]
fn session_cells_follow_a_mid_call_swap() {
    let source = r#"
global COUNTER: int = 0

@nomangle inline(lua) fn swap_to_b() = """
    __rock_session_publish(_G.__rock_test_session_b)
"""

fn probe() -> int {
    let before = COUNTER
    swap_to_b()
    return before * 100 + COUNTER
}
"#;
    let lua = emit_shaped(source, CalleeBindingMode::Upvalue, None);
    let probe = mangled_name(&lua, "probe");
    let program = format!(
        "{lua}
local a = __rock_active_session
local schema = __rock_state_schema_id
local b = __rock_session_new(schema)
__rock_session_activate(b)
__rock_state_initialize()
__rock_active_session.__rock_state[5] = 41
_G.__rock_test_session_b = b
__rock_session_activate(a)
__rock_active_session.__rock_state[5] = 7
print({probe}())
"
    );
    assert_eq!(run_luajit(&program, "session_cells_mid_call"), "741");
}

// --- Session tables sized from the schema (docs/backends.md, "Session cells") --
//
// LuaJIT specialises a constant-key read to the table's CURRENT array/hash
// split (`ULE asize` / `ABC` guard). `__rock_state` used to start as `{}` and
// grow lazily, and every rehash (one per hash-part overflow as statics fill in)
// recomputed the array part from the populated keys, so a trace recorded before
// a move carried a permanently failing guard: a doomed side exit with the
// caller's floats live, 468 aborts a run at one site, plus the interpreted
// remainder of every execution after. The emitter now publishes the schema's
// rehash fixpoint and the prelude sizes every Session's tables to it.

#[test]
fn the_emitter_publishes_a_state_capacity_and_reserves_it_before_initializing() {
    let lua = emit_shaped(FIXTURE, CalleeBindingMode::Upvalue, None);
    // COUNTER is slot 5 (`lua_global_session_slots_are_dense_numeric_realizations`):
    // the array fixpoint is the next power of two above the highest slot, plus
    // one for index 0; the hash part has a 16-node floor.
    let line = lua
        .lines()
        .find(|l| l.starts_with("_G.__ROCK_STATE_CAPACITY = {"))
        .expect("capacity line");
    let nums: Vec<usize> = line
        .trim_start_matches("_G.__ROCK_STATE_CAPACITY = {")
        .trim_end_matches('}')
        .split(", ")
        .map(|n| n.parse().expect("capacity number"))
        .collect();
    assert_eq!(nums[0], 9, "array fixpoint of slot 5 is 8 + 1: {line}");
    // The hash size counts the stdlib's own statics too, so pin its shape, not
    // its value: a power of two no smaller than the 16-node floor.
    assert!(nums[1] >= 16 && nums[1].is_power_of_two(), "hash capacity shape: {line}");
    let cap = lua.find("_G.__ROCK_STATE_CAPACITY = ").expect("capacity line");
    let reserve = lua.find("\n__rock_state_reserve_active()\n").expect("reserve call");
    let init = lua.find("\n__rock_state_initialize()\n").expect("initialize call");
    assert!(cap < reserve && reserve < init, "capacity, then reserve, then initialize");
    assert!(
        lua.contains("__rock_table_new(narr, nhash)"),
        "sessions must be built with table.new, not `{{}}`"
    );
}

#[test]
fn every_session_carries_the_reserved_capacity_and_keeps_its_values() {
    let lua = emit_shaped(FIXTURE, CalleeBindingMode::Upvalue, None);
    let bump = mangled_name(&lua, "bump");
    let program = format!(
        "{lua}
local a = __rock_active_session
local schema = __rock_state_schema_id
print({bump}())
local b = __rock_session_new(schema)
__rock_session_activate(b)
__rock_state_initialize()
print({bump}())
__rock_session_activate(a)
print({bump}())
print(a.__rock_state_capacity[1], b.__rock_state_capacity[1],
  a.__rock_state_capacity[2] == b.__rock_state_capacity[2] and a.__rock_state_capacity[2] >= 16)
"
    );
    assert_eq!(run_luajit(&program, "session_capacity"), "1\n1\n2\n9\t9\ttrue");
}
