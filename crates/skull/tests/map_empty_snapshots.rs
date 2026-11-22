use crate::common::{emit_module, run_luajit};

const SOURCE: &str = r#"
@lua_named struct Item { id: int }
nobox value Column { a: int, b: int, c: int, d: int }
newtype Label(string)

@nomangle fn empty_boxed_map() -> Map<string, Item> { return map_new() }
@nomangle fn empty_column_map() -> Map<string, Column> { return map_new() }
@nomangle fn empty_keys() -> [string] { return [] }
@nomangle fn empty_boxed_values() -> [Item] { return [] }
@nomangle fn empty_column_values() -> [Column] { return [] }
@nomangle fn empty_boxed_pairs() -> [(string, Item)] { return [] }
@nomangle fn empty_column_pairs() -> [(string, Column)] { return [] }
@nomangle fn boxed_keys(m: Map<string, Item>) -> [string] { return m.keys() }
@nomangle fn column_keys(m: Map<string, Column>) -> [string] { return m.keys() }
@nomangle fn boxed_keys_into(m: Map<string, Item>, out: [string]) -> [string] {
    return m.keys_into(out)
}
@nomangle fn column_keys_into(m: Map<string, Column>, out: [string]) -> [string] {
    return m.keys_into(out)
}
@nomangle fn boxed_values(m: Map<string, Item>) -> [Item] { return m.values() }
@nomangle fn column_values(m: Map<string, Column>) -> [Column] { return m.values() }
@nomangle fn boxed_pairs(m: Map<string, Item>) -> [(string, Item)] { return m.to_array() }
@nomangle fn column_pairs(m: Map<string, Column>) -> [(string, Column)] { return m.to_array() }
@nomangle fn label_keys(m: Map<Label, int>) -> [Label] { return m.keys() }
@nomangle fn number_keys(m: Map<int, int>) -> [int] { return m.keys() }
@nomangle fn mixed_keys(m: Map<any, int>) -> [any] { return m.keys() }

@nomangle fn sorted_snapshots() {
    let m: Map<string, Item> = map_new()
    m["z"] = Item { id: 30 }
    m["a"] = Item { id: 10 }
    m["m"] = Item { id: 20 }
    let keys = m.keys()
    let values = m.values()
    let pairs = m.to_array()
    let reused = ["stale"]
    m.keys_into(reused)
    assert(keys.len == 3 && keys[0] == "a" && keys[1] == "m" && keys[2] == "z", "sorted keys")
    assert(reused.len == 3 && reused[0] == "a" && reused[1] == "m" && reused[2] == "z",
        "sorted reused keys")
    assert(values[0].id == 10 && values[1].id == 20 && values[2].id == 30,
        "values follow sorted keys")
    assert(pairs[0].0 == "a" && pairs[1].0 == "m" && pairs[2].0 == "z",
        "pairs follow sorted keys")
    for item in values { m.clear() }
    assert(values.len == 3 && values[1].id == 20 && pairs[2].1.id == 30,
        "boxed results survive map mutation")

    let columns: Map<string, Column> = map_new()
    columns["z"] = Column { a: 30, b: 31, c: 32, d: 33 }
    columns["a"] = Column { a: 10, b: 11, c: 12, d: 13 }
    columns["m"] = Column { a: 20, b: 21, c: 22, d: 23 }
    let column_keys = columns.keys()
    let column_values = columns.values()
    let column_pairs = columns.to_array()
    columns.keys_into(reused)
    assert(column_keys.len == 3 && column_keys[0] == "a" && column_keys[1] == "m"
        && column_keys[2] == "z", "sorted column keys")
    assert(reused.len == 3 && reused[0] == "a" && reused[1] == "m" && reused[2] == "z",
        "sorted reused column keys")
    assert(column_values[0].a == 10 && column_values[1].b == 21 && column_values[2].d == 33,
        "column values follow sorted keys")
    assert(column_pairs[0].0 == "a" && column_pairs[1].0 == "m" && column_pairs[2].0 == "z",
        "column pairs follow sorted keys")
    for item in column_values { columns.clear() }
    columns["a"] = Column { a: 99, b: 99, c: 99, d: 99 }
    assert(column_values.len == 3 && column_values[0].a == 10 && column_pairs[2].1.d == 33,
        "column results survive map mutation and slot reuse")
}
"#;

#[test]
fn map_snapshots_skip_empty_work_and_preserve_sort_order() {
    let lua = emit_module(SOURCE);
    let output = run_luajit(&format!(
        r#"{lua}
jit.off()
jit.flush()
local boxed, column = empty_boxed_map(), empty_column_map()
local cases = {{
    {{boxed_keys, empty_keys, boxed, 'boxed keys'}},
    {{column_keys, empty_keys, column, 'column keys'}},
    {{boxed_values, empty_boxed_values, boxed, 'boxed values'}},
    {{column_values, empty_column_values, column, 'column values'}},
    {{boxed_pairs, empty_boxed_pairs, boxed, 'boxed pairs'}},
    {{column_pairs, empty_column_pairs, column, 'column pairs'}}
}}
local sink
local function growth(f, map)
    for i = 1, 32 do sink = f(map) end
    collectgarbage('collect')
    collectgarbage('stop')
    local before = collectgarbage('count') * 1024
    for i = 1, 1024 do sink = f(map) end
    local bytes = collectgarbage('count') * 1024 - before
    collectgarbage('restart')
    return bytes
end
for _, case in ipairs(cases) do
    local baseline = growth(case[2], case[3])
    local actual = growth(case[1], case[3])
    assert(actual <= baseline + 1024,
        case[4] .. ' allocated beyond its result: ' .. (actual - baseline) .. ' bytes')
end
jit.on()
local sorters = {{}}
for name, f in pairs(_G) do
    if name:match('^__sort_map_keys') then sorters[f] = true end
end
assert(next(sorters), 'map key sorter was not emitted')
sorters[table.sort] = true
debug.sethook(function()
    assert(not sorters[debug.getinfo(2, 'f').func], 'empty snapshot sorted')
end, 'c')
for _, case in ipairs(cases) do
    local first, second = case[1](case[3]), case[1](case[3])
    assert((first[0] or first.len) == 0 and (second[0] or second.len) == 0, case[4])
    assert(not rawequal(first, second), 'snapshot result was shared')
    if first[0] ~= nil then
        first[1] = 'probe'
        assert(second[1] == nil, 'snapshot storage was shared')
    end
end
for _, case in ipairs({{{{boxed_keys_into, boxed}}, {{column_keys_into, column}}}}) do
    local out = empty_keys()
    out[1], out[0] = 'stale', 1
    assert(rawequal(case[1](case[2], out), out))
    assert(out[0] == 0 and out[1] == nil)
end
debug.sethook()
print('ok')
"#
    ));
    assert_eq!(output, "ok");

    let output = run_luajit(&format!(
        r#"{lua}
sorted_snapshots()
local boxed, column = empty_boxed_map(), empty_column_map()
boxed['10'], boxed['2'] = {{ id = 10 }}, {{ id = 2 }}
column.spine['10'], column.spine['2'] = 1, 2
column.occupied_count = 2
local labels, numbers = {{}}, {{}}
labels['10'], labels['2'] = 10, 2
numbers[10], numbers[2] = 10, 2
local mixed = {{ [10] = 10, ['10'] = 10, ['2'] = 2 }}
local tied = {{}}
for key in pairs(mixed) do
    if tostring(key) == '10' then tied[#tied + 1] = key end
end
local a, b, c = boxed_keys(boxed), column_keys(column), label_keys(labels)
assert(a[1] == '10' and b[1] == '10' and c[1] == '10')
local d, e = number_keys(numbers), mixed_keys(mixed)
assert(d[1] == 10 and d[2] == 2, 'numeric keys keep tostring order')
assert(e[0] == 3 and e[3] == '2', 'mixed keys keep tostring order')
assert(e[1] == tied[1] and e[2] == tied[2], 'equal key text keeps traversal order')
print('ok')
"#
    ));
    assert_eq!(output, "ok");
}

#[test]
fn map_sort_reuses_scratch_and_releases_keys() {
    let lua = emit_module(SOURCE);
    let output = run_luajit(&format!(
        r#"{lua}
jit.off()
local sort = assert(__sort_map_keys)
local keys = {{ [0] = 32 }}
local source = {{}}
for i = 1, keys[0] do source[i] = ("%02d"):format(keys[0] + 1 - i) end
local function sweep()
    for i = 1, keys[0] do keys[i] = source[i] end
    sort(keys)
    assert(keys[1] == "01" and keys[32] == "32")
end
for i = 1, 10 do sweep() end
collectgarbage("collect")
collectgarbage("stop")
local before = collectgarbage("count") * 1024
for i = 1, 64 do sweep() end
local growth = collectgarbage("count") * 1024 - before
collectgarbage("restart")
assert(growth <= 1024, ("repeated stable merge requested new scratch, %.0f heap bytes"):format(growth))

local inner = {{ [0] = 17 }}
local outer = {{ [0] = 17 }}
local nested = 0
local key = setmetatable({{}}, {{ __tostring = function()
    for i = 1, inner[0] do inner[i] = ("%02d"):format(inner[0] + 1 - i) end
    sort(inner)
    assert(inner[1] == "01" and inner[17] == "17")
    nested = nested + 1
    return "09"
end }})
local weak = setmetatable({{ key }}, {{ __mode = "v" }})
for i = 1, outer[0] do outer[i] = ("%02d"):format(outer[0] + 1 - i) end
outer[9] = key
sort(outer)
assert(nested > 0 and outer[9] == key)
outer, key = nil, nil
collectgarbage("collect")
assert(weak[1] == nil, "sort keeps caller keys alive")
print("ok")
"#
    ));
    assert_eq!(output, "ok");
}
