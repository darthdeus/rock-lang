use crate::common::{definition_names, emit_module, function_body, run_luajit, run_main};

const FIXTURE: &str = include_str!("fixtures/traceable_loops.rock");

fn uncovered_backward_jumps(lua: &str) -> String {
    let driver = r#"
local util = require("jit.util")
local vm = require("jit.vmdef")
local bit = require("bit")
local function op(f, pc)
    local ins = util.funcbc(f, pc)
    local o = bit.band(ins, 0xff)
    return vm.bcnames:sub(o * 6 + 1, o * 6 + 6):gsub(" ", ""), pc + 1 + bit.rshift(ins, 16) - 0x8000
end
local found, seen = {}, {}
local function walk(f)
    if seen[f] then return end
    seen[f] = true
    local info = util.funcinfo(f)
    for pc = 1, info.bytecodes - 1 do
        local name, target = op(f, pc)
        if name == "JMP" and target <= pc and op(f, pc - 1) ~= "LOOP" then
            local covered = false
            for p = target, pc - 1 do
                local n, exit = op(f, p)
                if n == "LOOP" and exit > pc then covered = true end
            end
            if not covered then
                found[#found + 1] = info.linedefined .. ":" .. pc
            end
        end
    end
    for i = 1, info.gcconsts do
        local k = util.funck(f, -i)
        if type(k) == "proto" then walk(k) end
    end
end
walk(assert(loadstring(CHUNK)))
print(table.concat(found, " "))
"#;
    run_luajit(&format!("local CHUNK = {}\n{driver}", lua_long_string(lua)))
}

fn lua_long_string(s: &str) -> String {
    let mut eq = String::new();
    while s.contains(&format!("]{eq}]")) {
        eq.push('=');
    }
    format!("[{eq}[\n{s}]{eq}]")
}

#[test]
fn every_backward_jump_carries_a_loop_instruction() {
    let lua = emit_module(FIXTURE);
    assert_eq!(uncovered_backward_jumps(&lua), "");
}

#[test]
fn the_check_sees_a_nested_backward_goto() {
    let nested = "local i = 0\n::w1::\ni = i + 1\nif i < 9 then goto w1 end\n";
    assert_eq!(uncovered_backward_jumps(nested), "0:6");
    let level = "local i = 0\n::w1::\ni = i + 1\nif i >= 9 then return end\ngoto w1\n";
    assert_eq!(uncovered_backward_jumps(level), "");
}

#[test]
fn a_step_loop_that_does_not_cross_a_yield_prints_structured() {
    let lua = emit_module(FIXTURE);
    let name = definition_names(&lua)
        .into_iter()
        .find(|n| n.starts_with("sum_then_yield") && n.ends_with("__step"))
        .expect("step of `sum_then_yield`");
    let step = function_body(&lua, name);
    assert!(!step.contains("::bb"));
}

#[test]
fn loops_keep_their_results() {
    let lua = emit_module(FIXTURE);
    assert_eq!(run_main(&lua), "153\n52\n4315\n4316\n-1\n16\n42");
}
