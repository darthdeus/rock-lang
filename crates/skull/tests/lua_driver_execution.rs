use crate::common;
use std::collections::HashMap;

fn run(src: &str, driver: &str) -> String {
    let ws = common::ws_with_stdlib(src);
    let mid = common::module_id(&ws, "<test>");
    let lua = skull::mir_emit::emit_workspace_mir(&ws, mid)
        .expect("emit")
        .lua;
    let functions = &ws.module(mid).functions;
    let mut counts: HashMap<_, usize> = HashMap::new();
    for (_, f) in functions.iter() {
        *counts.entry(f.name).or_default() += 1;
    }
    let mut aliases = String::new();
    for (_, f) in functions.iter() {
        let mangled = skull_mir::mangle::mangle_fn(&ws, f);
        let source_name = bone_core::modules::display_name(f.name.as_str());
        if counts[&f.name] == 1 && mangled != source_name {
            aliases.push_str(&format!("_G[\"{source_name}\"] = {mangled}\n"));
        }
    }
    common::run_luajit(&format!("{lua}\n{aliases}print({driver})\n"))
}

#[test]
fn exec_uint_checks_preserve_ranges_and_single_evaluation() {
    let src = r#"
fn cast(value: float) -> uint { value as uint }
fn from_int(value: int) -> uint { value as uint }
fn left(value: uint, count: uint) -> uint { value << count }
fn right(value: uint, count: uint) -> uint { value >> count }
fn take(values: [uint]) -> uint {
    let value = values[0]
    values[0] += 1
    value
}
fn cast_once(values: [uint]) -> uint { (take(values) as float) as uint }
fn left_once(values: [uint]) -> uint { take(values) << take(values) }
fn right_once(values: [uint]) -> uint { take(values) >> take(values) }
"#;
    let driver = r#"(function()
local function rejects(fn, message, ...)
  local ok, err = pcall(fn, ...)
  assert(not ok and tostring(err):sub(-#message) == message, tostring(err))
end
assert(cast(0) == 0 and cast(3.9) == 3 and cast(4294967295) == 4294967295)
assert(1 / cast(-0.0) == -math.huge)
assert(from_int(0) == 0 and from_int(4294967295) == 4294967295)
for _, value in ipairs({-1, 4294967296}) do
  rejects(from_int, 'uint conversion out of range', value)
end
for _, value in ipairs({-0.1, 4294967295.5, 0/0, math.huge, -math.huge}) do
  rejects(cast, 'uint conversion out of range', value)
end
assert(left(1, 0) == 1 and left(1, 31) == 2147483648)
assert(right(4294967295, 0) == 4294967295 and right(4294967295, 31) == 1)
for _, fn in ipairs({left, right}) do
  for _, count in ipairs({32, 4294967295}) do
    rejects(fn, 'uint shift count out of range', 0, count)
  end
end
local values = {_data={1}, len=1}
assert(cast_once(values) == 1 and values._data[1] == 2)
values._data[1] = 1
assert(left_once(values) == 4 and values._data[1] == 3)
values._data[1] = 1
assert(right_once(values) == 0 and values._data[1] == 3)
return true
end)()"#;
    assert_eq!(run(src, driver), "true");
}

#[test]
fn exec_uint_entry_values_are_normalized() {
    let src = "fn root(a: uint) -> uint { a }";
    assert_eq!(run(src, "root(-1)"), "4294967295");
}

#[test]
fn map_remove_can_mutate_a_vector_index_through_lua_metatable() {
    let src = "
fn held(xs: [int], indices: [int], m: Map<string, [int]>) -> int {
    if indices[0] < 0 || indices[0] >= xs.len() { return -1 }
    m.remove(\"held\")
    return xs[indices[0]]
}

fn removed(xs: [int], m: Map<string, int>) -> int {
    if m[\"key\"] < 0 || m[\"key\"] >= xs.len() { return -1 }
    m.remove(\"key\")
    return xs[m[\"key\"]]
}
fn shrunk(xs: [int], m: Map<string, int>) -> int {
    if xs.len() <= 0 { return -1 }
    m.remove(\"missing\")
    return xs[0]
}";
    assert_eq!(
        run(
            src,
            r#"(function()
                local xs = { len = 2, _data = { 11, 22 } }
                local indices = { len = 1, _data = { 1 } }
                local m = { held = indices }
                assert(held(xs, indices, m) == 22 and m.held == nil)
                local ok, err = pcall(removed, xs, { key = 1 })
                assert(not ok and tostring(err):find("attempt to compare nil", 1, true), tostring(err))
                local changed = { len = 1, _data = { 1 } }
                local foreign = setmetatable({}, { __index = function()
                    changed._data[1] = 99
                    return nil
                end })
                ok, err = pcall(held, xs, changed, foreign)
                assert(not ok and tostring(err):find("Vec index out of bounds", 1, true), tostring(err))
                local target = { len = 1, _data = { 7 } }
                local shrinking = setmetatable({}, { __index = function()
                    target.len = 0
                    target._data[1] = nil
                    return nil
                end })
                ok, err = pcall(shrunk, target, shrinking)
                assert(not ok and tostring(err):find("Vec index out of bounds", 1, true), tostring(err))
                return "ok"
            end)()"#,
        ),
        "ok"
    );
}

#[test]
fn callee_binding_ignores_calls_removed_by_cleanup() {
    let _inline_off = common::inline_off_guard();
    let src = "fn discarded(a: int) -> int { a + 2 }\nfn live(a: int) -> int { a + 1 }\nfn caller(a: int) -> int { if false { discarded(a) } else { live(a) } }";
    assert_eq!(run(src, "caller(7)"), "8");
}

#[test]
fn callee_binding_ignores_discarded_uint_helpers() {
    let _inline_off = common::inline_off_guard();
    let src = "fn checked(value: float, count: uint) -> uint { if false { (value as uint) << count } else { count } }";
    assert_eq!(run(src, "checked(-1, 3)"), "3");
}

#[test]
fn callee_binding_short_names_skip_public_globals() {
    let _inline_off = common::inline_off_guard();
    let src = "fn leaf(a: int) -> int { a + 1 }\n\
               @nomangle fn b0(a: int) -> int { a + 2 }\n\
               fn caller(a: int) -> int { leaf(a) + b0(a) }";
    assert_eq!(run(src, "caller(3)"), "9");
}

#[test]
fn callee_binding_aliases_skip_names_inline_lua_reads() {
    let _inline_off = common::inline_off_guard();
    let src = "fn leaf(a: int) -> int { a + 1 }\n\
               @inline inline(lua) fn raw(a: int) -> int = \"return b0(a)\"\n\
               fn caller(a: int) -> int { leaf(a) + raw(a) }";
    assert_eq!(
        run(
            src,
            "(function() b0 = function(a) return a + 10 end; return caller(3) end)()"
        ),
        "17"
    );
}

#[test]
fn aliased_captures_around_a_spliced_body_in_a_loop_run_correctly() {
    let _inline_off = common::inline_off_guard();
    let src = "fn leaf(a: int) -> int { a + 1 }\n\
               fn other(a: int) -> int { a * 3 }\n\
               @inline inline(lua) fn twice(a: int) -> int = \"local b0 = a * 2\\nreturn b0\"\n\
               fn caller(n: int) -> int {\n\
                 let s = 0\n\
                 for i in 0..n { s = s + leaf(twice(i)) + other(i) }\n\
                 return s\n\
               }";
    // sum over i < 4 of (2i + 1) + 3i
    assert_eq!(run(src, "caller(4)"), "34");
}

#[test]
fn callee_binding_caps_at_the_lua_upvalue_limit() {
    let _inline_off = common::inline_off_guard();
    let count = skull_codegen_lua::MAX_BOUND_CALLEES + 7;
    let mut src = String::new();
    for i in 0..count {
        src.push_str(&format!("fn leaf{i:03}(a: int) -> int {{ a + {i} }}\n"));
    }
    src.push_str("fn caller(a: int) -> int {\n  let t = 0\n");
    for i in 0..count {
        src.push_str(&format!("  t = t + leaf{i:03}(a)\n"));
    }
    src.push_str("  t\n}\n");
    let expected: i64 = (0..count as i64).map(|i| 7 + i).sum();
    assert_eq!(run(&src, "caller(7)"), expected.to_string());
}

#[test]
fn foreign_metamethods_can_shrink_a_guarded_vector() {
    let src = r#"
fn after_get(xs: [int], m: Map<string, int>) -> int {
    if xs.len() <= 0 { return -1 }
    let old = m.get("missing")
    if old != null { return old }
    return xs[0]
}
fn after_table_set(xs: [int], t: any) -> int {
    if xs.len() <= 0 { return -1 }
    table_set(t, "missing", 1)
    return xs[0]
}
fn after_table_unset(xs: [int], t: Map<string, int>) -> int {
    if xs.len() <= 0 { return -1 }
    table_unset(t, "missing")
    return xs[0]
}
fn after_vec_data_set(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 { return -1 }
    __vec_data_set(ys, 0, 1)
    return xs[0]
}
fn after_vec_data_unset(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 { return -1 }
    __vec_data_unset(ys, 0)
    return xs[0]
}
fn after_table_get(xs: [int], t: any) -> int {
    if xs.len() <= 0 { return -1 }
    let value: int = cast(table_get(t, "missing"))
    return xs[0] + value
}
fn after_vec_data_get(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 { return -1 }
    let value: int = cast(__vec_data_get(ys, 0))
    return xs[0] + value
}
fn map_read(m: Map<string, int>) -> int { return m["missing"] }
fn after_map_read(xs: [int], m: Map<string, int>) -> int {
    if xs.len() <= 0 { return -1 }
    let value = m["missing"]
    return xs[0] + value
}
fn after_nested_map_read(xs: [int], m: Map<string, int>) -> int {
    if xs.len() <= 0 { return -1 }
    let value = map_read(m)
    return xs[0] + value
}
fn after_map_write(xs: [int], m: Map<string, int>) -> int {
    if xs.len() <= 0 { return -1 }
    m["missing"] = 1
    return xs[0]
}
fn after_repeated_map_read(xs: [int], m: Map<string, int>) -> int {
    if xs.len() <= 0 { return -1 }
    let first = xs[0]
    let value = m["missing"]
    return xs[0] + first + value
}
fn read_nullable(ys: [int?]) -> int? { return ys[0] }
fn write_nullable(ys: [int?]) { ys[0] = 1 }
fn after_nullable_read(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 { return -1 }
    let value = ys[0]
    if value != null { return -2 }
    return xs[0]
}
fn after_nested_nullable_read(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 { return -1 }
    let value = read_nullable(ys)
    if value != null { return -2 }
    return xs[0]
}
fn after_nullable_write(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 { return -1 }
    ys[0] = 1
    return xs[0]
}
fn after_nested_nullable_write(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 { return -1 }
    write_nullable(ys)
    return xs[0]
}
fn after_discarded_nullable_read(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 || ys.len() <= 0 { return -1 }
    ys[0]
    return xs[0]
}
fn after_push(xs: [int], ys: [int]) -> int {
    if xs.len() <= 0 { return -1 }
    ys.push(1)
    return xs[0]
}
fn after_vec_push(xs: [int], ys: [int]) -> int {
    if xs.len() <= 0 { return -1 }
    vec_push(ys, 1)
    return xs[0]
}
fn external_equal(a: any, b: any) -> bool { return a == b }
fn after_equal(xs: [int], a: any, b: any) -> int {
    if xs.len() <= 0 { return -1 }
    let equal = external_equal(a, b)
    if equal { return xs[0] }
    return -2
}
"#;
    let out = run(
        src,
        r#"(function()
            __op_eq = function(a, b) return a == b end
            local function table(mt) return setmetatable({}, mt) end
            local function vec(mt) return { len = 1, _data = setmetatable({}, mt) } end
            local function empty_vec(mt) return { len = 0, _data = setmetatable({}, mt) } end
            local function pair(mt) return setmetatable({}, mt), setmetatable({}, mt) end
            local function check(name, f, wrap, key, result)
                local xs = { len = 1, _data = { 7 } }
                local mt = { [key] = function()
                    xs.len = 0
                    xs._data[1] = nil
                    return result
                end }
                local ok, err = pcall(f, xs, wrap(mt))
                assert(not ok and tostring(err):find("Vec index out of bounds", 1, true), name .. ": " .. tostring(err))
            end
            check("get", after_get, table, "__index", nil)
            check("table_set", after_table_set, table, "__newindex")
            check("table_unset", after_table_unset, table, "__newindex")
            check("vec_data_set", after_vec_data_set, vec, "__newindex")
            check("vec_data_unset", after_vec_data_unset, vec, "__newindex")
            check("table_get", after_table_get, table, "__index", 1)
            check("vec_data_get", after_vec_data_get, vec, "__index", 1)
            check("map_read", after_map_read, table, "__index", 1)
            check("nested_map_read", after_nested_map_read, table, "__index", 1)
            check("map_write", after_map_write, table, "__newindex", 1)
            check("repeated_map_read", after_repeated_map_read, table, "__index", 1)
            check("nullable_read", after_nullable_read, vec, "__index", nil)
            check("nested_nullable_read", after_nested_nullable_read, vec, "__index", nil)
            check("nullable_write", after_nullable_write, vec, "__newindex", nil)
            check("nested_nullable_write", after_nested_nullable_write, vec, "__newindex", nil)
            check("discarded_nullable_read", after_discarded_nullable_read, vec, "__index", nil)
            check("push", after_push, empty_vec, "__newindex")
            check("vec_push", after_vec_push, empty_vec, "__newindex")
            check("equal", after_equal, pair, "__eq", true)
            return "ok"
        end)()"#,
    );
    assert_eq!(out, "ok");
}

#[test]
fn exec_spliced_inline_lua_keeps_memory_argument_snapshot() {
    let src = r#"
struct SnapshotState { i: int }
inline(lua) fn snapshot_build(state: SnapshotState) -> any = """
return setmetatable({}, {
  __index = function()
    state.i = 2
    return {11, 22}
  end,
})
"""
@inline inline(lua) fn snapshot_pick(t: any, i: int) -> int = "return t.values[i]"
fn snapshot_run() -> int {
    let state = SnapshotState {i: 1}
    let t = snapshot_build(state)
    snapshot_pick(t, state.i)
}
"#;
    assert_eq!(run(src, "snapshot_run()"), "11");
}
