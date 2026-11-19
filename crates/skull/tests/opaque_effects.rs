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
    assert_eq!(
        run_luajit(&format!(
            r#"{lua}
local ok, err = pcall(native_store_case)
assert(not ok and tostring(err):find('Vec index out of bounds', 1, true), tostring(err))
print('ok')
"#
        )),
        "ok"
    );
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

#[test]
fn foreign_record_callbacks_invalidate_vector_bounds() {
    let lua = emit_module(
        r#"
struct ExternalRecord { value: int? }
fn read_external(record: ExternalRecord) -> int? { return record.value }
fn write_external(record: ExternalRecord) { record.value = 1 }
@nomangle fn repeated_record_read(record: ExternalRecord) -> int {
    let a = record.value ?? 0
    let b = record.value ?? 0
    return a * 10 + b
}
@nomangle fn after_record_read(xs: [int], record: ExternalRecord) -> int {
    if xs.len() <= 0 { return -1 }
    let value = record.value
    if value == null { return -2 }
    return xs[0]
}
@nomangle fn after_record_write(xs: [int], record: ExternalRecord) -> int {
    if xs.len() <= 0 { return -1 }
    record.value = 1
    return xs[0]
}
@nomangle fn after_record_read_helper(xs: [int], record: ExternalRecord) -> int {
    if xs.len() <= 0 { return -1 }
    let value = read_external(record)
    if value == null { return -2 }
    return xs[0]
}
@nomangle fn after_record_write_helper(xs: [int], record: ExternalRecord) -> int {
    if xs.len() <= 0 { return -1 }
    write_external(record)
    return xs[0]
}
@nomangle fn after_unused_record_read(xs: [int], record: ExternalRecord) -> int {
    if xs.len() <= 0 { return -1 }
    let unused = record.value
    return xs[0]
}
@nomangle fn after_safe_record_write(xs: [int], record: ExternalRecord?) -> int {
    if xs.len() <= 0 { return -1 }
    record?.value = 1
    return xs[0]
}
@nomangle fn after_safe_record_read(xs: [int], record: ExternalRecord?) -> int {
    if xs.len() <= 0 { return -1 }
    let value = record?.value
    if value == null { return -2 }
    return xs[0]
}
value EmbeddedValue { n: int }
struct ExternalHolder { embed child: EmbeddedValue }
@nomangle fn after_embedded_read(xs: [int], record: ExternalHolder) -> int {
    if xs.len() <= 0 { return -1 }
    let value = record.child.n
    if value == 0 { return -2 }
    return xs[0]
}
@nomangle fn after_embedded_write(xs: [int], record: ExternalHolder) -> int {
    if xs.len() <= 0 { return -1 }
    record.child.n = 1
    return xs[0]
}
"#,
    );
    let output = run_luajit(&format!(
        r#"{lua}
for _, case in ipairs({{
    {{after_record_read, '__index'}},
    {{after_record_write, '__newindex'}},
    {{after_record_read_helper, '__index'}},
    {{after_record_write_helper, '__newindex'}},
    {{after_unused_record_read, '__index'}},
    {{after_safe_record_read, '__index'}},
    {{after_safe_record_write, '__newindex'}},
    {{after_embedded_read, '__index', 'child.n'}},
    {{after_embedded_write, '__newindex', 'child.n'}},
}}) do
    local xs = {{len = 1, _data = {{7}}}}
    local calls = 0
    local record = setmetatable({{}}, {{[case[2]] = function(_, key)
        assert(key == (case[3] or 'value'), key)
        calls = calls + 1
        xs.len, xs._data[1] = 0, nil
        return 1
    end}})
    local ok, err = pcall(case[1], xs, record)
    assert(calls == 1, 'field callback must run exactly once: ' .. calls)
    assert(not ok and tostring(err):find('Vec index out of bounds', 1, true), tostring(err))
end
local reads = 0
local record = setmetatable({{}}, {{__index = function()
    reads = reads + 1
    return reads
end}})
assert(repeated_record_read(record) == 12 and reads == 2, 'reads must not be combined')
print('ok')
"#
    ));
    assert_eq!(output.trim(), "ok");
}
