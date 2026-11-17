use crate::common::emit_module;

const EXTERN_VALUE_RETURN: &str = r#"
value Stats { a: int, b: int }

extern(ffi, "host") fn host_probe_stats() -> Stats

fn main() {
    let s = host_probe_stats()
    print("{s.a}")
}
"#;

#[test]
fn an_extern_value_return_does_not_hand_back_a_reference_into_its_out_param() {
    let lua = emit_module(EXTERN_VALUE_RETURN);
    assert!(!crate::common::definitions_matching(&lua, "host_probe_stats").is_empty());
    let wrapper = crate::common::function_body(&lua, "host_probe_stats");
    assert!(wrapper.contains("ffi.new(\"Stats[1]\")"));
    assert!(!wrapper.contains("return __out[0]"));
    assert!(wrapper.contains("ffi.new(\"Stats\", __out[0])"));
}

#[test]
fn a_reference_into_a_collected_parent_is_use_after_free() {
    let probe = r#"
local ffi = require("ffi")
ffi.cdef[[ typedef struct { int a, b; } RefProbe; ]]

local freed_while_referenced = false
local function ref_into_out()
    local out = ffi.new("RefProbe[1]")
    ffi.gc(out, function() freed_while_referenced = true end)
    out[0].a = 7
    return out[0]
end

local function owning_copy()
    local out = ffi.new("RefProbe[1]")
    ffi.gc(out, function() end)
    out[0].a = 7
    return ffi.new("RefProbe", out[0])
end

local ref = ref_into_out()
collectgarbage("collect")
collectgarbage("collect")
io.write(tostring(freed_while_referenced), " ", tostring(ref ~= nil), " ", tostring(owning_copy().a))
"#;
    assert_eq!(crate::common::run_luajit(probe), "true true 7");
}
