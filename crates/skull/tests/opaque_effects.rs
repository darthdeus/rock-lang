use crate::common::{emit_module, run_luajit};

const SOURCE: &str = r#"
fn stringify_external(value: any) -> string { return tostring(value) }

@nomangle fn after_tostring(xs: [int], value: any) -> int {
    if xs.len() <= 0 { return -1 }
    let text = tostring(value)
    if text == "" { return -2 }
    return xs[0]
}

@nomangle fn after_stringify_helper(xs: [int], value: any) -> int {
    if xs.len() <= 0 { return -1 }
    let text = stringify_external(value)
    if text == "" { return -2 }
    return xs[0]
}

@nomangle fn after_print(xs: [int], value: any) -> int {
    if xs.len() <= 0 { return -1 }
    print(value)
    return xs[0]
}

@nomangle fn after_type_tag(xs: [int], value: any) -> int {
    if xs.len() <= 0 { return -1 }
    let tag = __rock_get_type(value)
    if tag == null { return -2 }
    return xs[0]
}
"#;

#[test]
fn host_logging_callbacks_invalidate_vector_bounds() {
    let lua = emit_module(
        r#"
@nomangle fn after_log_level(xs: [int]) -> int {
    if xs.len() <= 0 { return -1 }
    let level = log_get_level()
    if level < 0 { return -2 }
    return xs[0]
}
@nomangle fn after_log_once(xs: [int]) -> int {
    if xs.len() <= 0 { return -1 }
    log_once("effects", "message", "effect-test")
    return xs[0]
}
"#,
    );
    let output = run_luajit(&format!(
        r#"{lua}
for _, case in ipairs({{
    {{after_log_level, '__rock_log_level_source'}},
    {{after_log_once, '__rock_log_sink'}},
}}) do
    local xs = {{len = 1, _data = {{7}}}}
    local calls = 0
    _G[case[2]] = function()
        calls = calls + 1
        xs.len, xs._data[1] = 0, nil
        return 2
    end
    local ok, err = pcall(case[1], xs)
    _G[case[2]] = nil
    assert(calls == 1, 'logging must invoke the callback once')
    assert(not ok and tostring(err):find('Vec index out of bounds', 1, true), tostring(err))
end
print('ok')
"#
    ));
    assert_eq!(output.trim(), "ok");
}

#[test]
fn external_string_conversion_invalidates_vector_bounds() {
    let lua = emit_module(SOURCE);
    let output = run_luajit(&format!(
        r#"{lua}
for _, f in ipairs({{after_tostring, after_stringify_helper, after_print}}) do
    local xs = {{len = 1, _data = {{7}}}}
    local calls = 0
    local value = setmetatable({{}}, {{__tostring = function()
        calls = calls + 1
        xs.len, xs._data[1] = 0, nil
        return 'converted'
    end}})
    local ok, err = pcall(f, xs, value)
    assert(calls == 1, 'string conversion must invoke the callback once')
    assert(not ok and tostring(err):find('Vec index out of bounds', 1, true), tostring(err))
end
print('ok')
"#
    ));
    assert_eq!(output.trim(), "converted\nok");
}

#[test]
fn external_type_tag_lookup_invalidates_vector_bounds() {
    let lua = emit_module(SOURCE);
    let output = run_luajit(&format!(
        r#"{lua}
local xs = {{len = 1, _data = {{7}}}}
local value = setmetatable({{}}, {{__index = function(_, key)
    assert(key == '__ty')
    xs.len, xs._data[1] = 0, nil
    return 'Foreign'
end}})
local ok, err = pcall(after_type_tag, xs, value)
assert(not ok and tostring(err):find('Vec index out of bounds', 1, true), tostring(err))
print('ok')
"#
    ));
    assert_eq!(output.trim(), "ok");
}

#[test]
fn checked_native_store_invalidates_element_value_facts() {
    let lua = emit_module(
        r#"
@nomangle fn changed_native_index(xs: [int], indices: NativeBuffer<int>) -> int {
    if indices[0] < 0 || indices[0] >= xs.len() { return -1 }
    __nb_checked_set(indices, 0, 99)
    return xs[indices[0]]
}
@nomangle fn native_store_case() -> int {
    let indices = native_buffer_int(1)
    indices[0] = 1
    return changed_native_index([11, 22], indices)
}
"#,
    );
    let output = run_luajit(&format!(
        r#"{lua}
local ok, err = pcall(native_store_case)
assert(not ok and tostring(err):find('Vec index out of bounds', 1, true), tostring(err))
print('ok')
"#
    ));
    assert_eq!(output.trim(), "ok");
}

#[test]
fn foreign_equality_helper_invalidates_vector_bounds() {
    let lua = emit_module(
        r#"
fn external_equal(a: any, b: any) -> bool { return a == b }
@nomangle fn after_equal(xs: [int], a: any, b: any) -> int {
    if xs.len() <= 0 { return -1 }
    if external_equal(a, b) { return xs[0] }
    return -2
}
"#,
    );
    let output = run_luajit(&format!(
        r#"{lua}
local xs = {{len = 1, _data = {{7}}}}
local calls = 0
local mt = {{__eq = function()
    calls = calls + 1
    xs.len, xs._data[1] = 0, nil
    return true
end}}
local ok, err = pcall(after_equal, xs, setmetatable({{}}, mt), setmetatable({{}}, mt))
assert(calls == 1, 'equality must invoke the callback once')
assert(not ok and tostring(err):find('Vec index out of bounds', 1, true), tostring(err))
print('ok')
"#
    ));
    assert_eq!(output.trim(), "ok");
}
