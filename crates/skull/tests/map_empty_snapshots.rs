use crate::common::{emit_module, run_luajit};

const SOURCE: &str = r#"
struct Item { id: int }
nobox value Column { a: int, b: int, c: int, d: int }

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
fn empty_snapshots_allocate_only_the_returned_list() {
    let lua = emit_module(SOURCE);
    let output = run_luajit(&format!(
        r#"{lua}
jit.off()
jit.flush()
local boxed, column = empty_boxed_map(), empty_column_map()
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
for _, case in ipairs({{
    {{boxed_keys, empty_keys, boxed, 'boxed keys'}},
    {{column_keys, empty_keys, column, 'column keys'}},
    {{boxed_values, empty_boxed_values, boxed, 'boxed values'}},
    {{column_values, empty_column_values, column, 'column values'}},
    {{boxed_pairs, empty_boxed_pairs, boxed, 'boxed pairs'}},
    {{column_pairs, empty_column_pairs, column, 'column pairs'}}
}}) do
    local baseline = growth(case[2], case[3])
    local actual = growth(case[1], case[3])
    assert(actual <= baseline + 1024,
        case[4] .. ' allocated beyond its result: ' .. (actual - baseline) .. ' bytes')
end
print('ok')
"#
    ));
    assert_eq!(output, "ok");
}

#[test]
fn empty_snapshots_skip_sorting_and_keys_into_clears_its_result() {
    let lua = emit_module(SOURCE);
    let output = run_luajit(&format!(
        r#"{lua}
local boxed, column = empty_boxed_map(), empty_column_map()
table.sort = function() error('empty snapshot sorted') end
for _, case in ipairs({{
    {{boxed_keys, boxed}}, {{column_keys, column}},
    {{boxed_values, boxed}}, {{column_values, column}},
    {{boxed_pairs, boxed}}, {{column_pairs, column}}
}}) do
    assert(case[1](case[2]).len == 0)
end
for _, case in ipairs({{{{boxed_keys_into, boxed}}, {{column_keys_into, column}}}}) do
    local out = empty_keys()
    out._data[1], out.len = 'stale', 1
    assert(rawequal(case[1](case[2], out), out))
    assert(out.len == 0 and out._data[1] == nil)
end
print('ok')
"#
    ));
    assert_eq!(output, "ok");
}

#[test]
fn empty_snapshots_return_fresh_lists() {
    let lua = emit_module(SOURCE);
    let output = run_luajit(&format!(
        r#"{lua}
local boxed, column = empty_boxed_map(), empty_column_map()
for _, case in ipairs({{
    {{boxed_keys, boxed}}, {{column_keys, column}},
    {{boxed_values, boxed}}, {{column_values, column}},
    {{boxed_pairs, boxed}}, {{column_pairs, column}}
}}) do
    local first, second = case[1](case[2]), case[1](case[2])
    assert(first.len == 0 and second.len == 0)
    assert(not rawequal(first, second), 'snapshot result was shared')
    if first._data then
        assert(not rawequal(first._data, second._data), 'snapshot storage was shared')
    end
end
print('ok')
"#
    ));
    assert_eq!(output, "ok");
}

#[test]
fn nonempty_snapshots_stay_sorted_and_survive_map_mutation() {
    let lua = emit_module(SOURCE);
    assert_eq!(
        run_luajit(&format!("{lua}\nsorted_snapshots()\nprint('ok')")),
        "ok"
    );
}
