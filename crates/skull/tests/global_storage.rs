use crate::common::{emit_mir, mangled_name, run_luajit, ws_with_stdlib};
use ustr::Ustr;

fn emit_program(src: &str) -> String {
    let ws = ws_with_stdlib(src);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_full(&ws, mid).expect("emit").0.lua
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
fn lua_global_slots_are_dense_numeric_realizations() {
    let emitted = emit_mir(FIXTURE);
    let lua = emit_program(FIXTURE);
    assert!(
        lua.contains("__rock_state_cell[5]"),
        "the fixture global must use its compact numeric Lua slot"
    );
    assert!(
        emitted.lua.contains(
            "{name=\"COUNTER\",identity=\"global:<test>::COUNTER\",physical={\"COUNTER\"},slots={5}}"
        ),
        "the global directory must map the logical identity to the same numeric Lua slot"
    );
    assert!(
        !emitted.lua.contains("[\"global:<test>::COUNTER::value\"]"),
        "logical global identities must not remain hot Lua table keys"
    );
}

#[test]
fn lua_global_slot_order_is_declaration_independent() {
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
            emitted.contains("identity=\"global:<test>::ALPHA\",physical={\"ALPHA\"},slots={5}"),
            "ALPHA must keep the first user-global Lua slot"
        );
        assert!(
            emitted.contains("identity=\"global:<test>::ZED\",physical={\"ZED\"},slots={6}"),
            "ZED must keep the second user-global Lua slot"
        );
    }
}

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
    assert!(
        !hot.contains("local function"),
        "the traced global-name lookup must not carry the cold index-builder closure"
    );
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
    assert_eq!(run_luajit(&program, "rng_suffix"), "true");
}

// --- The storage cell -----------------
//
// A mutable-global access reads the global storage through one chunk-level
// local, `__rock_state_cell`, instead of the
// `_G.__rock_globals` lookup. The table is never replaced, so the cell never
// needs updating after the program assigns it.

#[test]
fn emission_reads_globals_through_the_storage_cell() {
    let lua = emit_program(FIXTURE);
    assert!(
        lua.contains("\nlocal __rock_state_cell\n"),
        "the prelude must declare the cell as a chunk-level local:\n{}",
        &lua[..lua.len().min(4000)]
    );
    assert!(
        lua.contains("__rock_state_cell[5]"),
        "reads and writes go through the storage cell"
    );
    assert!(
        !lua.contains("__rock_globals[5]"),
        "no access may keep the `_G` lookup"
    );
}

// --- The storage table sized at creation --
//
// LuaJIT specialises a constant-key read to the table's CURRENT array/hash
// split (`ULE asize` / `ABC` guard). A table that starts as `{}` and grows
// lazily rehashes on every hash-part overflow as statics fill in, and each
// rehash recomputed the array part from the populated keys, so a trace recorded
// before a move carried a permanently failing guard: a doomed side exit with
// the caller's floats live, 468 aborts a run at one site, plus the interpreted
// remainder of every execution after. The emitter opens the storage at its
// rehash fixpoint instead.

#[test]
fn the_program_opens_sized_storage_before_initializing() {
    let lua = emit_program(FIXTURE);
    // COUNTER is slot 5 (`lua_global_slots_are_dense_numeric_realizations`):
    // the array fixpoint is the next power of two above the highest slot, plus
    // one for index 0; the hash part has a 16-node floor.
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
    assert_eq!(nums[1], 9, "array fixpoint of slot 5 is 8 + 1: {open}");
    // The hash size counts the stdlib's own statics too, so pin its shape, not
    // its value: a power of two no smaller than the 16-node floor.
    assert!(
        nums[0] >= 16 && nums[0].is_power_of_two(),
        "hash capacity shape: {open}"
    );
    let open_at = lua
        .find("\n__rock_state_cell = __rock_globals_open(")
        .expect("open line");
    let init = lua
        .find("\n__rock_state_initialize()\n")
        .expect("initialize call");
    assert!(
        open_at < init,
        "the storage opens before any initializer runs"
    );
    assert!(
        lua.contains("__rock_table_new(narr, nhash)"),
        "the storage must be built with table.new, not `{{}}`"
    );
}

/// A program loaded again over its own live storage, as a reload does, keeps
/// every global the running program wrote: no initializer runs a second time,
/// and the table is the one the first load sized.
#[test]
fn a_reloaded_program_keeps_live_globals_and_its_storage_table() {
    let lua = emit_program(FIXTURE);
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
    assert_eq!(
        run_luajit(&program, "global_reload"),
        "1\n2\n3\ntrue\tfalse"
    );
}

/// A program whose mutable-global layout differs from the live storage's
/// cannot load into it, because a slot would name a different global.
#[test]
fn a_program_with_a_different_global_layout_refuses_live_storage() {
    let before = emit_mir(FIXTURE).lua;
    let after = emit_mir(&FIXTURE.replace(
        "global COUNTER: int = 0",
        "global COUNTER: int = 0\nglobal EXTRA: int = 0",
    ))
    .lua;
    assert!(
        !after.contains("]======]"),
        "the program must fit in the long string"
    );
    let program = format!(
        "{before}
local chunk = assert(loadstring([======[{after}]======]))
local ok, err = pcall(chunk)
print(ok, err)
"
    );
    assert_eq!(
        run_luajit(&program, "global_layout_guard"),
        "false\tRock global layout changed; restart required"
    );
}
