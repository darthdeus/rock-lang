use crate::common::{emit_without_stdlib, run_luajit, ws_of};

const SRC: &str = "global bar: int = 7\nfn main() {}\n";

fn metamethod_body<'a>(lua: &'a str, name: &str) -> &'a str {
    let head = format!("\n  mt.{name} = function(");
    let at = lua
        .find(&head)
        .unwrap_or_else(|| panic!("no `mt.{name}` assignment in the emitted prelude"));
    let rest = &lua[at + 1..];
    let end = rest
        .find("\n  end\n")
        .unwrap_or_else(|| panic!("`mt.{name}` body is unterminated"));
    &rest[..end]
}

#[test]
fn the_bridge_miss_path_is_not_a_tail_call() {
    let lua = emit_without_stdlib(SRC);
    assert!(!lua.contains("old_read") && !lua.contains("old_write"));
    let read = metamethod_body(&lua, "__index");
    assert!(read.trim_end().ends_with("return nil"));
    let write = metamethod_body(&lua, "__newindex");
    assert!(write.trim_end().ends_with("rawset(t, key, value)"));
}

#[test]
fn every_fallback_shape_and_the_state_path_survive_a_reinstall() {
    let mut prog = emit_without_stdlib(SRC);
    prog.push_str(
        r#"
local function check(label, got, want)
  if got ~= want then
    print("FAIL " .. label .. ": got " .. tostring(got) .. " want " .. tostring(want))
  else
    print("ok " .. label)
  end
end

-- 1. A Rock state global resolves through the storage, on the shape installed
--    by the chunk itself.
check("state read", _G.bar, 7)
_G.bar = 11
check("state write", _G.bar, 11)
check("state stays out of the raw table", rawget(_G, "bar"), nil)
check("plain miss", _G.no_such_global_at_all, nil)

-- 2. Reinstalling over a metatable whose `__index`/`__newindex` are FUNCTIONS.
local seen = nil
setmetatable(_G, {
  __index = function(t, k) seen = k; return "fn:" .. tostring(k) end,
  __newindex = function(t, k, v) rawset(t, "captured_" .. tostring(k), v) end,
})
_G.__rock_globals_install_bridge()
check("function fallback read", _G.absent_one, "fn:absent_one")
check("function fallback saw the key", seen, "absent_one")
_G.absent_two = 3
check("function fallback write", rawget(_G, "captured_absent_two"), 3)
check("state read past a function fallback", _G.bar, 11)
_G.bar = 13
check("state write past a function fallback", _G.bar, 13)

-- 3. Reinstalling over a metatable whose `__index` is a TABLE.
setmetatable(_G, { __index = { from_table = "tab" } })
_G.__rock_globals_install_bridge()
check("table fallback read", _G.from_table, "tab")
check("table fallback miss", _G.absent_three, nil)
_G.absent_four = 5
check("table fallback write is raw", rawget(_G, "absent_four"), 5)
check("state read past a table fallback", _G.bar, 13)

-- 4. Reinstalling over a bare metatable: no fallback at all.
setmetatable(_G, {})
_G.__rock_globals_install_bridge()
check("bare reinstall miss", _G.absent_five, nil)
_G.absent_six = 7
check("bare reinstall write is raw", rawget(_G, "absent_six"), 7)
check("state read past a bare reinstall", _G.bar, 13)
_G.bar = 17
check("state write past a bare reinstall", _G.bar, 17)
"#,
    );
    let out = run_luajit(&prog);
    assert!(!out.contains("FAIL"));
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 17);
}

#[test]
fn the_emit_reports_where_the_bridge_install_ends() {
    let ws = ws_of(SRC);
    let mid = skull::user_module_id(&ws).unwrap();
    let emit = skull::mir_emit::emit_full(&ws, mid).expect("emit_full").0;
    let at = emit
        .globals_bridge_end
        .expect("a full emit installs the bridge");
    assert!(emit.lua[..at].ends_with(skull::mir_emit::GLOBALS_BRIDGE_LINE));
    assert_eq!(
        emit.lua
            .matches(skull::mir_emit::GLOBALS_BRIDGE_LINE)
            .count(),
        1
    );
}

#[test]
fn full_and_incremental_programs_reflect_nullable_global_leaves() {
    let ws =
        ws_of("nobox value Point { x: int, y: int }\nglobal POINT: Point? = null\nfn main() {}");
    let mid = skull::user_module_id(&ws).unwrap();
    let (full, mut cache) = skull::mir_emit::FullEmit::program()
        .with_global_accessors()
        .run(&ws, mid)
        .unwrap();
    let incremental = skull::mir_emit::emit_incremental(&ws, &mut cache, &[]).unwrap();
    for emit in [full, incremental] {
        let mut lua = emit.lua;
        lua.push_str(
            r#"
local point = assert(__rock_global_entry("POINT"))
assert(__rock_global_get(point) == nil)
__rock_global_set(point, {x=3,y=5})
local value = __rock_global_get(point)
assert(value.x == 3 and value.y == 5)
__rock_global_set(point, nil)
assert(__rock_global_get(point) == nil)
for _, slot in ipairs(point.slots) do assert(not __rock_globals[slot]) end
print("ok")
"#,
        );
        assert_eq!(run_luajit(&lua), "ok");
    }
}
