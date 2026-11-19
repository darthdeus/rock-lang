use crate::common::{emit_module, run_luajit};

const SOURCE: &str = r#"
@nomangle fn new_map() -> Map<string, bool> { return map_new() }
@nomangle fn map_size(m: Map<string, bool>) -> int { return m.len() }
@nomangle fn put(m: Map<string, bool>, key: string, value: bool) { m[key] = value }
@nomangle fn interop_map_remove(m: Map<string, bool>, key: string) { m.remove(key) }
@nomangle fn interop_map_clear(m: Map<string, bool>) { m.clear() }
"#;

#[test]
fn map_length_observes_lua_mutations_and_external_tables() {
    let lua = emit_module(SOURCE);
    let output = run_luajit(&format!(
        r#"{lua}
jit.on()
local function check(m, expected)
    local n = 0
    for _ in next, m do n = n + 1 end
    assert(n == expected)
    assert(map_size(m) == expected)
end
for _, m in ipairs({{new_map(), {{}}}}) do
    local alias = m
    check(m, 0)
    put(m, 'first', false)
    check(m, 1)
    put(alias, 'first', true)
    check(m, 1)
    alias.external = false
    check(m, 2)
    rawset(alias, 'raw', true)
    check(m, 3)
    rawset(alias, 'first', nil)
    check(m, 2)
    alias.external = nil
    check(m, 1)
    interop_map_remove(m, 'missing')
    check(m, 1)
    interop_map_remove(alias, 'raw')
    interop_map_remove(alias, 'raw')
    check(m, 0)
    put(m, 'count', false)
    put(m, 'occupied_count', false)
    put(m, '__count', false)
    check(m, 3)
    interop_map_clear(alias)
    check(m, 0)
    for i = 1, 2000 do
        m.live = false
        check(m, 1)
        rawset(alias, 'live', nil)
        check(m, 0)
    end
end
print('ok')
"#
    ));
    assert_eq!(output, "ok");
}
