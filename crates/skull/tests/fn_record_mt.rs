//! P2 step 4 of docs/closure-env-records-plan.md: the `__rock_fn_mt` runtime
//! helper -- a shared metatable making an env record `{ __slot = <CodeRef>, <captures> }`
//! callable as `f(...)` via `__call`, initialized once at load with the
//! preservation idiom so pre- and post-reload records share metatable identity.
//!
//! It lives in the runtime prelude (`skull::runtime_prelude`) alongside its
//! siblings `__ROCK_C_STRUCT` / `__rock_on_hot_reload` / the RNG -- the home for
//! unconditional, load-time, reload-preserved runtime tables. (The plan said
//! "stdlib inline(lua)"; an inline(lua) fn doesn't auto-run at load and a
//! metatable is a table, not a callable, so the prelude bare-global form is what
//! the design's `setmetatable({..}, __rock_fn_mt)` reference needs.)
use crate::common::{emit_module, run_luajit};

#[test]
fn no_code_ref_values_omit_code_ref_runtime() {
    let lua = emit_module("fn main() {}\n");
    assert!(!lua.contains("if not _G.__rock_code_slot_mt then"));
}

#[test]
fn programs_without_definition_roles_do_not_stage_an_empty_plan() {
    let lua = emit_module("fn main() {}\n");
    assert!(!lua.contains("__rock_definition_plan_stage("));
    assert_eq!(run_luajit(&format!("{lua}\nprint('PASS')"), "empty_definition_plan"), "PASS");
}

/// An escaping Rock function value enables the shared CodeRef runtime.
#[test]
fn prelude_defines_shared_fn_metatable() {
    let lua = emit_module("fn main() { let f = () => 1; print(f()) }\n");
    assert!(
        lua.contains("if not _G.__rock_fn_mt then")
            && lua.contains("invalid Rock CodeRef environment")
            && lua.contains("e.__slot.current_body(e, ...)") ,
        "the prelude must define the shared CodeRef-aware __rock_fn_mt metatable:\n(searched the prelude)"
    );
    assert!(
        lua.contains("if not _G.__rock_fn_mt then"),
        "__rock_fn_mt must use the reload-preservation guard"
    );
}

/// A hand-built CodeRef record routes a plain `f(args)` call through `__call`
/// to the current slot body, reading its captured field.
#[test]
fn record_is_callable_via_call_metamethod() {
    let lua = emit_module("fn main() { let f = () => 1; print(f()) }\n");
    let driver = "\n\
        __rock_code_slot_begin()\n\
        __rock_code_slot_stage(\"record\", function(e, y) return e.x + y end, \"record-abi\")\n\
        __rock_code_slot_publish()\n\
        local rec = setmetatable({ __slot = __rock_code_slot_ref(\"record\"), x = 100 }, _G.__rock_fn_mt)\n\
        print(rec(5))\n";
    let out = run_luajit(&format!("{lua}{driver}"), "call");
    assert_eq!(out, "105", "f(5) must route through the CodeRef slot reading rec.x; got: {out}");
}

/// Metatable identity is preserved across a reload: a record built before the
/// re-`dofile` and one built after share the SAME metatable (the `or`/`if not`
/// guard skips re-creation), so `getmetatable` compares equal -- required for
/// snapshot deep-copy to recognize records by `getmetatable(x) == __rock_fn_mt`.
#[test]
fn metatable_identity_preserved_across_reload() {
    let prog = emit_module("fn main() { let f = () => 1; print(f()) }\n");
    let script = format!(
        "_G.HOT_RELOAD = false\n{prog}\n\
         local mt_before = _G.__rock_fn_mt\n\
         __rock_code_slot_begin()\n\
         __rock_code_slot_stage(\"record\", function(e) return 1 end, \"record-abi\")\n\
         __rock_code_slot_publish()\n\
         local rec_before = setmetatable({{ __slot = __rock_code_slot_ref(\"record\") }}, _G.__rock_fn_mt)\n\
         _G.HOT_RELOAD = true\n{prog}\n\
         local rec_after = setmetatable({{ __slot = __rock_code_slot_ref(\"record\") }}, _G.__rock_fn_mt)\n\
         if getmetatable(rec_before) == getmetatable(rec_after) and _G.__rock_fn_mt == mt_before then\n\
           print(\"PASS\")\n\
         else print(\"FAIL\") end\n"
    );
    let out = run_luajit(&script, "reload");
    assert_eq!(out, "PASS", "records built across a reload must share metatable identity; got: {out}");
}

#[test]
fn code_slot_publish_is_transactional_and_preserves_slot_identity() {
    let lua = emit_module("fn main() { let f = () => 1; print(f()) }\n");
    let driver = r#"
__rock_code_slot_begin()
__rock_code_slot_stage("probe", function() return 1 end, "abi")
__rock_code_slot_publish()
local before = __rock_code_slot_ref("probe")
local ok = pcall(function()
  __rock_code_slot_begin()
  __rock_code_slot_stage("probe", function() return 2 end, "changed-abi")
  __rock_code_slot_publish()
end)
local after = __rock_code_slot_ref("probe")
if not ok and before == after and after() == 1 then print("PASS") else print("FAIL") end
"#;
    let out = run_luajit(&format!("{lua}{driver}"), "slot_transaction");
    assert_eq!(out, "PASS", "a failed compatibility stage must not publish or replace its slot: {out}");
}

#[test]
fn lifecycle_plan_publishes_in_the_code_slot_transaction() {
    let lua = emit_module("fn main() { let f = () => 1; print(f()) }\n");
    let driver = r#"
__rock_code_slot_begin()
__rock_code_slot_stage("probe", function() return 1 end, "abi")
__rock_lifecycle_plan_stage({ update = { len = 1 } })
__rock_code_slot_publish()
local old_plan = _G.__ROCK_LIFECYCLE_PLAN
local ok = pcall(function()
  __rock_code_slot_begin()
  __rock_lifecycle_plan_stage({ update = { len = 2 } })
  __rock_code_slot_stage("probe", function() return 2 end, "changed-abi")
  __rock_code_slot_publish()
end)
__rock_code_slot_abort()
if not ok and _G.__rock_lifecycle_plan_pending == nil
   and _G.__ROCK_LIFECYCLE_PLAN == old_plan and old_plan.update.len == 1
   and __rock_code_slot_ref("probe")() == 1 then print("PASS") else print("FAIL") end
"#;
    let out = run_luajit(&format!("{lua}{driver}"), "lifecycle_transaction");
    assert_eq!(out, "PASS", "failed staging must retain both old plan membership and old bodies: {out}");
}

#[test]
fn definition_plan_publishes_and_aborts_with_the_code_slot_transaction() {
    let lua = emit_module("fn main() { let f = () => 1; print(f()) }\n");
    let driver = r#"
__rock_code_slot_begin()
__rock_code_slot_stage("probe", function() return 1 end, "abi")
__rock_definition_plan_stage({ panels = { len = 1, _data = { [1] = { key = "old" } } } })
__rock_code_slot_publish()
local old_plan = _G.__ROCK_DEFINITION_PLAN
local ok = pcall(function()
  __rock_code_slot_begin()
  __rock_definition_plan_stage({ panels = { len = 2, _data = {} } })
  __rock_code_slot_stage("probe", function() return 2 end, "changed-abi")
  __rock_code_slot_publish()
end)
__rock_code_slot_abort()
if not ok and _G.__rock_definition_plan_pending == nil
   and _G.__ROCK_DEFINITION_PLAN == old_plan
   and old_plan.panels.len == 1
   and __rock_code_slot_ref("probe")() == 1 then print("PASS") else print("FAIL") end
"#;
    let out = run_luajit(&format!("{lua}{driver}"), "definition_transaction");
    assert_eq!(out, "PASS", "failed staging must retain the old definition plan and clear pending state: {out}");
}
