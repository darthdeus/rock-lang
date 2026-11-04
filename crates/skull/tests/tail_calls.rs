use crate::common::{emit_module, function_body, mangled_name, run_luajit};

fn body<'a>(lua: &'a str, name: &str) -> &'a str {
    function_body(lua, name)
}

#[test]
fn direct_tail_calls_preserve_results_and_effect_order() {
    let lua = emit_module(
        r#"
struct Output { value: int }
fn write(out: Output, x: int) { out.value = x }
fn value(x: int) -> int { return x + 1 }
fn forward_unit(out: Output, x: int) { write(out, x) }
fn forward_scalar(x: int) -> int { return value(x) }
fn discard_scalar(x: int) { value(x) }
fn work_after_call(out: Output, x: int) { write(out, x); out.value += 1 }
fn write_pair(out: Output, x: int, y: int) { out.value = x * 100 + y }
fn computed_unit(out: Output, x: int) {
    let a = x + 1
    let b = x + 2
    write_pair(out, a * a, b * b)
}
fn inspect_result(x: int) -> int { return value(x) + 1 }
inline(lua) fn raw_scalar(x: int) -> int = "return x, x + 1"
inline(lua) fn raw_unit(x: int) = "return x"
fn foreign_scalar(x: int) -> int { return raw_scalar(x) }
fn foreign_unit(x: int) { raw_unit(x) }
fn main() {
    let out = Output { value: 0 }
    forward_unit(out, 41)
    print("{out.value}")
    print("{forward_scalar(41)}")
    work_after_call(out, 41)
    print("{out.value}")
    print("{inspect_result(41)}")
    computed_unit(out, 10)
    print("{out.value}")
    discard_scalar(41)
}
"#,
    );
    for name in ["forward_unit", "forward_scalar"] {
        assert!(
            body(&lua, name)
                .lines()
                .any(|line| { line.trim_start().starts_with("do return ") && line.contains('(') }),
            "{name} retains an unnecessary caller frame:\n{}",
            body(&lua, name)
        );
    }
    for name in ["discard_scalar", "work_after_call", "inspect_result"] {
        assert!(
            !body(&lua, name).lines().any(|line| {
                line.trim_start().starts_with("do return ")
                    && (line.contains("write_") || line.contains("value_"))
            }),
            "{name} changed return arity or skipped subsequent work"
        );
    }
    let discard = mangled_name(&lua, "discard_scalar");
    let foreign_scalar = mangled_name(&lua, "foreign_scalar");
    let foreign_unit = mangled_name(&lua, "foreign_unit");
    assert_eq!(
        run_luajit(
            &format!(
                "{lua}\nmain()\nprint(select('#', {discard}(1)))\nprint(select('#', {foreign_scalar}(1)))\nprint(select('#', {foreign_unit}(1)))\n"
            ),
            "direct_tail_calls"
        ),
        "41\n42\n42\n43\n12244\n0\n1\n0"
    );
}

#[test]
fn finite_sibling_cache_queries_do_not_exhaust_tailcall_budget() {
    let lua = emit_module(include_str!("fixtures/finite_cache_siblings.rock"));
    let bench = mangled_name(&lua, "bench");
    let controls = mangled_name(&lua, "cache_controls");
    let getter = crate::common::definition_names(&lua)
        .into_iter()
        .find(|name| name.starts_with("__weak_cache_entity_get_"))
        .expect("the typed cache getter must be emitted");
    let driver = format!(
        r#"
local jit = require("jit")
local util = require("jit.util")
local vm = require("jit.vmdef")
local aborts, stops = 0, 0
local function trace(what, tr, fn, pc, err)
    if what == "stop" then stops = stops + 1 end
    if what == "abort" and vm.traceerr[err] == "loop unroll limit reached" then
        aborts = aborts + 1
    end
end
jit.flush()
jit.attach(trace, "trace")
local result = {bench}(2000)
jit.attach(trace)
assert(stops > 0, "the finite sibling path must compile")
print(result .. ":" .. aborts)
{controls}()
for pc = 1, util.funcinfo({getter}).bytecodes - 1 do
    local ins = util.funcbc({getter}, pc)
    local index = bit.band(ins, 255) * 6
    local op = vm.bcnames:sub(index + 1, index + 6)
    assert(op ~= "CALLT " and op ~= "CALLMT", "cache lookup must not tail-forward")
end
"#
    );
    assert_eq!(
        run_luajit(&format!("{lua}\n{driver}"), "finite_cache_siblings"),
        "32000:0"
    );
    assert!(
        !lua.contains("function __weak_cache_read("),
        "the single-use forwarding helper must be removed"
    );
}
