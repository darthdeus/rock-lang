//! The `_G` state bridge's MISS path.
//!
//! `__rock_globals_install_bridge` puts a metatable on `_G` so a read or
//! write of a Rock state global resolves through the global storage. Every other global
//! that is absent from the raw table reaches the same metamethod -- and a global
//! whose value is genuinely `nil` is absent every time, because `rawset(_G, k,
//! nil)` cannot make it present. So the miss path runs on every such read, and
//! it used to end in `return old_read(t, key)`: a Lua-level TAIL CALL, which
//! spends one of LuaJIT's 15 `loopunroll` slots in every trace that records it.
//! Measured on an application's combat bench, that link was 31.7% of the remaining
//! tail-call budget.
//!
//! Which of the three pre-bridge fallback shapes applies -- function, table, or
//! none -- cannot change between two calls of the metamethod, so it is resolved
//! ONCE at install. `_G` normally has no metatable at all, so the resolved miss
//! path is a plain `return nil` with no call in it.

use crate::common::{run_luajit, ws_of};

const SRC: &str = "global bar: int = 7\nfn main() {}\n";

fn program() -> String {
    let ws = ws_of(SRC);
    let mid = skull::user_module_id(&ws).unwrap();
    skull::mir_emit::emit_full(&ws, mid).expect("emit_full").0.lua
}

/// The body of `mt.<name> = function(..)` inside the installer, up to the
/// matching `end` at that indentation.
fn metamethod_body<'a>(lua: &'a str, name: &str) -> &'a str {
    let head = format!("\n  mt.{name} = function(");
    let at = lua
        .find(&head)
        .unwrap_or_else(|| panic!("no `mt.{name}` assignment in the emitted prelude"));
    let rest = &lua[at + 1..];
    let end = rest.find("\n  end\n").unwrap_or_else(|| panic!("`mt.{name}` body is unterminated"));
    &rest[..end]
}

/// The miss path resolves to a value, not to another Lua call. `old_read` and
/// `old_write` are gone: they existed only to re-decide, per miss, a question
/// the installer already knows the answer to.
#[test]
fn the_bridge_miss_path_is_not_a_tail_call() {
    let lua = program();
    assert!(
        !lua.contains("old_read") && !lua.contains("old_write"),
        "the per-miss fallback helpers are still emitted; the installer is supposed to \
         resolve the fallback shape once instead"
    );
    let read = metamethod_body(&lua, "__index");
    assert!(
        read.trim_end().ends_with("return nil"),
        "`mt.__index` must fall off to a plain `return nil` when `_G` had no previous \
         metatable; got:\n{read}"
    );
    let write = metamethod_body(&lua, "__newindex");
    assert!(
        write.trim_end().ends_with("rawset(t, key, value)"),
        "`mt.__newindex` must fall off to a plain `rawset` when `_G` had no previous \
         metatable; got:\n{write}"
    );
}

/// All four paths through the installed metamethods, driven under a real LuaJIT:
/// a Rock state global, and each of the three pre-bridge fallback shapes, each
/// captured by a REINSTALL, which is what a hot reload performs.
#[test]
fn every_fallback_shape_and_the_state_path_survive_a_reinstall() {
    let mut prog = program();
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
    let out = run_luajit(&prog, "state_global_bridge");
    assert!(!out.contains("FAIL"), "bridge behaviour changed:\n{out}");
    assert_eq!(
        out.lines().filter(|l| l.starts_with("ok ")).count(),
        17,
        "not every case reported; the program stopped early:\n{out}"
    );
}
