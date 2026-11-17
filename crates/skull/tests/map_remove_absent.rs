use crate::common::{emit_module, run_luajit};

const SOURCE: &str = r#"
struct Item { id: int }
nobox value Column { a: int, b: int }

@nomangle fn boxed_map(n: int) -> Map<string, Item> {
    let m: Map<string, Item> = map_new()
    for i in 0..n { m["k{i}"] = Item { id: i } }
    return m
}
@nomangle fn column_map(n: int) -> Map<string, Column> {
    let m: Map<string, Column> = map_new()
    for i in 0..n { m["k{i}"] = Column { a: i, b: i } }
    return m
}
@nomangle fn int_map(n: int) -> Map<string, int> {
    let m: Map<string, int> = map_new()
    for i in 0..n { m["k{i}"] = i }
    return m
}
@nomangle fn boxed_remove(m: Map<string, Item>, key: string) -> bool { return m.remove(key) != null }
@nomangle fn column_remove(m: Map<string, Column>, key: string) -> bool { return m.remove(key) != null }
@nomangle fn int_remove(m: Map<string, int>, key: string) -> bool { return m.remove(key) != null }
fn main() {}
"#;

fn absent_key_stores(lua: &str, build: &str, remove: &str, keyed: &str) -> String {
    run_luajit(&format!(
        r#"{lua}
local m = {build}(511)
local stores = 0
setmetatable({keyed}, {{ __newindex = function(t, k, v)
    stores = stores + 1
    rawset(t, k, v)
end }})
for i = 1, 100 do assert(not {remove}(m, "absent" .. i)) end
assert({remove}(m, "k0") and not {remove}(m, "k0"))
assert({remove}(m, "k1"))
print(stores)
"#
    ))
    .trim()
    .to_string()
}

#[test]
fn map_absent_remove_does_not_store() {
    let lua = emit_module(SOURCE);
    for (build, remove, keyed) in [
        ("boxed_map", "boxed_remove", "m"),
        ("int_map", "int_remove", "m"),
        ("column_map", "column_remove", "m.spine"),
    ] {
        assert_eq!(absent_key_stores(&lua, build, remove, keyed), "0");
    }
}
