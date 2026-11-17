//! Issue #44: nested coroutine steps must remain traceable through resume.
//! Resumes call their step directly, without a pcall frame below the trace
//! start, so returning from a nested step must not abort with
//! `NYI: return to lower frame`.

use crate::common::{definition_names, emit_module, function_body, mangled_name, run_luajit};

const FIXTURE: &str = include_str!("fixtures/nested_coroutine_trace.rock");

#[test]
fn each_resume_calls_its_step_directly() {
    let lua = emit_module(FIXTURE);
    for co in ["leaf", "mid", "top"] {
        let resume = definition_names(&lua)
            .into_iter()
            .find(|name| name.starts_with(co) && name.ends_with("__resume"))
            .unwrap_or_else(|| panic!("no `{co}` resume in:\n{lua}"));
        let step = resume.replace("__resume", "__step");
        assert!(
            function_body(&lua, resume).contains(&format!("{step}(s, arg)"))
                && !function_body(&lua, resume).contains("pcall(")
        );
    }
}

#[test]
fn nested_resumes_do_not_abort_with_return_to_lower_frame() {
    let lua = emit_module(FIXTURE);
    let bench = mangled_name(&lua, "bench");
    let driver = format!(
        r#"
local jit = require("jit")
local util = require("jit.util")
local vm = require("jit.vmdef")
local aborts, locations = 0, {{}}
local function trace(what, tr, func, pc, err)
    if what == "abort" and vm.traceerr[err] == "NYI: return to lower frame" then
        aborts = aborts + 1
        locations[#locations + 1] = util.funcinfo(func, pc).loc
    end
end
jit.flush()
jit.attach(trace, "trace")
local result = {bench}(40)
jit.attach(trace)
print(result .. ":" .. aborts)
assert(aborts == 0, "LuaJIT aborted a trace with `NYI: return to lower frame`: " .. table.concat(locations, ","))
"#
    );
    assert_eq!(run_luajit(&format!("{lua}\n{driver}")), "459180:0");
}
