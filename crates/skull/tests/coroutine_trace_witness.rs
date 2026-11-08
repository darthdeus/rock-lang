//! Issue #44: nested coroutine steps abort LuaJIT traces with `NYI: return to
//! lower frame`. Each resume runs its step under `pcall`, and the recorder
//! cannot return through a pcall frame that was entered below the trace
//! start (`lj_record_ret`). Side traces that start inside a nested step, root
//! traces at a step's entry and loop traces inside a step all end by returning
//! from that step into its pcall frame, so the abort is inherent to keeping
//! per-resume error trapping. These tests pin where the crossing is, so a new
//! emitted shape that also aborts there is caught.

use crate::common::{definition_names, emit_module, function_body, mangled_name, run_luajit};

const FIXTURE: &str = include_str!("fixtures/nested_coroutine_trace.rock");

#[test]
fn each_resume_traps_its_step_in_a_pcall_frame() {
    let lua = emit_module(FIXTURE);
    for co in ["leaf", "mid", "top"] {
        let resume = definition_names(&lua)
            .into_iter()
            .find(|name| name.starts_with(co) && name.ends_with("__resume"))
            .unwrap_or_else(|| panic!("no `{co}` resume in:\n{lua}"));
        let step = resume.replace("__resume", "__step");
        assert!(
            function_body(&lua, resume).contains(&format!("pcall({step}, s, arg)")),
            "`{resume}` must run `{step}` directly under pcall:\n{}",
            function_body(&lua, resume)
        );
    }
}

#[test]
fn return_to_lower_frame_aborts_only_at_step_returns() {
    let lua = emit_module(FIXTURE);
    let bench = mangled_name(&lua, "bench");
    let driver = format!(
        r#"
local jit = require("jit")
local util = require("jit.util")
local vm = require("jit.vmdef")
local steps = {{}}
for name, value in pairs(_G) do
    if type(name) == "string" and name:match("__step$") then steps[value] = true end
end
local crossings, elsewhere = 0, {{}}
local function trace(what, tr, func, pc, err)
    if what == "abort" and vm.traceerr[err] == "NYI: return to lower frame" then
        if steps[func] then
            crossings = crossings + 1
        else
            elsewhere[#elsewhere + 1] = util.funcinfo(func, pc).loc
        end
    end
end
jit.flush()
jit.attach(trace, "trace")
local result = {bench}(40)
jit.attach(trace)
print(result .. ":" .. table.concat(elsewhere, ","))
assert(crossings > 0, "LuaJIT returned through a pcall frame below the trace start; revisit #44")
"#
    );
    assert_eq!(
        run_luajit(&format!("{lua}\n{driver}"), "nested_coroutine_trace"),
        "459180:"
    );
}
