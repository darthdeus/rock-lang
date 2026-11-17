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
    assert_eq!(run_luajit(&format!("{lua}\nprint('PASS')")), "PASS");
}

#[test]
fn typed_definition_catalog_lookup_is_fail_closed_and_counts_population() {
    let lua = emit_module("fn main() { let f = () => 1; print(f()) }\n");
    let driver = r#"
local missing_plan = not pcall(function() __rock_definition_catalog("widgets") end)
__rock_code_slot_begin()
__rock_definition_plan_stage({
  widgets = { len = 1, _data = { [1] = { key = "alpha", factory = function() return { value = 1 } end } } },
  empty = { len = 0, _data = {} },
})
__rock_code_slot_publish()
local missing_catalog = not pcall(function() __rock_definition_catalog("missing") end)
local empty = __rock_definition_catalog("empty")
local nonempty = __rock_definition_catalog("widgets")
if missing_plan and missing_catalog and empty.len == 0 and nonempty.len == 1 then
  print("4/4 PASS")
else
  print("0/4 FAIL")
end
"#;
    assert_eq!(run_luajit(&format!("{lua}{driver}")), "4/4 PASS");
}

#[test]
fn prelude_defines_shared_fn_metatable() {
    let lua = emit_module("fn main() { let f = () => 1; print(f()) }\n");
    assert!(
        lua.contains("if not _G.__rock_fn_mt then")
            && lua.contains("invalid Rock CodeRef environment")
            && lua.contains("e.__slot.current_body(e, ...)")
    );
    assert!(lua.contains("if not _G.__rock_fn_mt then"));
}

#[test]
fn record_is_callable_via_call_metamethod() {
    let lua = emit_module("fn main() { let f = () => 1; print(f()) }\n");
    let driver = "\n\
        __rock_code_slot_begin()\n\
        __rock_code_slot_stage(\"record\", function(e, y) return e.x + y end, \"record-abi\")\n\
        __rock_code_slot_publish()\n\
        local rec = setmetatable({ __slot = __rock_code_slot_ref(\"record\"), x = 100 }, _G.__rock_fn_mt)\n\
        print(rec(5))\n";
    let out = run_luajit(&format!("{lua}{driver}"));
    assert_eq!(out, "105");
}

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
    let out = run_luajit(&script);
    assert_eq!(out, "PASS");
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
    let out = run_luajit(&format!("{lua}{driver}"));
    assert_eq!(out, "PASS");
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
    let out = run_luajit(&format!("{lua}{driver}"));
    assert_eq!(out, "PASS");
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
    let out = run_luajit(&format!("{lua}{driver}"));
    assert_eq!(out, "PASS");
}

#[test]
fn c_struct_registry_rows_reset_the_ctype_memo_after_themselves() {
    let lua = emit_module(
        "struct P { x: float, y: float }\nfn main() { let p = P { x: 1.0, y: 2.0 }; print(inspect(p)) }\n",
    );
    let last_row = lua
        .rfind("_G.__ROCK_C_STRUCT[\"")
        .expect("a value struct registers a __ROCK_C_STRUCT row");
    let reset = lua
        .find("_G.__ROCK_C_STRUCT_BY_CTYPE = {}\n")
        .expect("the registry emitter resets the ctype memo");
    assert!(reset > last_row);
    assert_eq!(lua.matches("_G.__ROCK_C_STRUCT_BY_CTYPE = {}\n").count(), 1);
}
