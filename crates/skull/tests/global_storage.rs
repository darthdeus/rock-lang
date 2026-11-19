use crate::common::{emit_mir, emit_module, mangled_name, run_luajit};

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
fn global_name_lookup_keeps_index_construction_out_of_the_hot_function() {
    let emitted = emit_mir(FIXTURE);
    let hot = emitted
        .lua
        .split("_G.__rock_global_entry = function(name)")
        .nth(1)
        .expect("global-name lookup function")
        .split("_G.__rock_global_decorate")
        .next()
        .expect("global-name lookup body");
    assert!(!hot.contains("local function"));
}

#[test]
fn rng_state_suffix_restore() {
    let emitted = emit_mir("fn main() {}\n");
    let program = format!(
        r#"{lua}
__rock_rng_seed(17)
local first = __rock_rng_next()
local checkpoint = __rock_rng_state_get()
local second = __rock_rng_next()
local third = __rock_rng_next()
__rock_rng_state_set(checkpoint)
local replay_second = __rock_rng_next()
local replay_third = __rock_rng_next()
print(first ~= second and second ~= third
  and second == replay_second and third == replay_third)
"#,
        lua = emitted.lua,
    );
    assert_eq!(run_luajit(&program), "true");
}

#[test]
fn the_program_opens_sized_storage_before_initializing() {
    let lua = emit_module(FIXTURE);
    let open = lua
        .lines()
        .find(|l| l.starts_with("__rock_state_cell = __rock_globals_open("))
        .expect("open line");
    let nums: Vec<usize> = open
        .trim_end_matches(')')
        .rsplit(", ")
        .take(2)
        .map(|n| n.parse().expect("capacity number"))
        .collect();
    assert_eq!(nums[1], 9);
    // The hash part also counts stdlib statics, so pin only its shape.
    assert!(nums[0] >= 16 && nums[0].is_power_of_two());
    let open_at = lua
        .find("\n__rock_state_cell = __rock_globals_open(")
        .expect("open line");
    let init = lua
        .find("\n__rock_state_initialize()\n")
        .expect("initialize call");
    assert!(open_at < init);
    assert!(lua.contains("__rock_table_new(narr, nhash)"));
}

#[test]
fn a_reloaded_program_keeps_live_globals_and_its_storage_table() {
    let lua = emit_module(FIXTURE);
    let bump = mangled_name(&lua, "bump");
    let program = format!(
        "{lua}
local storage = __rock_globals
print({bump}())
print({bump}())
{lua}
print({bump}())
print(storage == __rock_globals, __rock_globals_fresh)
"
    );
    assert_eq!(run_luajit(&program), "1\n2\n3\ntrue\tfalse");
}

#[test]
fn a_program_with_a_different_global_layout_refuses_live_storage() {
    let before = emit_mir(FIXTURE).lua;
    let after = emit_mir(&FIXTURE.replace(
        "global COUNTER: int = 0",
        "global COUNTER: int = 0\nglobal EXTRA: int = 0",
    ))
    .lua;
    assert!(!after.contains("]======]"));
    let program = format!(
        "{before}
local chunk = assert(loadstring([======[{after}]======]))
local ok, err = pcall(chunk)
print(ok, err)
"
    );
    assert_eq!(
        run_luajit(&program),
        "false\tRock global layout changed; restart required"
    );
}

#[test]
fn a_checked_global_read_without_stdlib_still_raises_at_runtime() {
    let lua = crate::common::emit_without_stdlib(
        "global LATE: int\n@nomangle fn read() -> int { LATE }\nfn main() { read() }\n",
    );
    let result = run_luajit(&format!(
        "{lua}\nlocal ok, err = pcall(read); assert(not ok); print(err)"
    ));
    assert!(result.contains("uninitialized global `LATE`"));
    assert!(result.contains("<t>:2:30"));
}
