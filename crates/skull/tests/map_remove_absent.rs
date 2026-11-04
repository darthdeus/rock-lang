use crate::common::{emit_module, run_luajit};

// LuaJIT records `t[k] = nil` for an absent key as a new hash node, and once
// the free nodes run out (a table at 2^k-1 live entries) every such store
// rehashes the whole hash part. So removing an absent key must not store into
// the key's table at all, on every Map representation. A `__newindex`
// metamethod on that table counts the stores that reach an absent key.
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

/// Stores to absent keys of `keyed(m)` while removing 100 absent keys and then
/// one present key twice.
fn absent_key_stores(build: &str, remove: &str, keyed: &str) -> String {
    let lua = emit_module(SOURCE);
    run_luajit(
        &format!(
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
        ),
        remove,
    )
    .trim()
    .to_string()
}

#[test]
fn boxed_map_absent_remove_does_not_store() {
    assert_eq!(absent_key_stores("boxed_map", "boxed_remove", "m"), "0");
}

#[test]
fn scalar_map_absent_remove_does_not_store() {
    assert_eq!(absent_key_stores("int_map", "int_remove", "m"), "0");
}

#[test]
fn column_map_absent_remove_does_not_store() {
    assert_eq!(
        absent_key_stores("column_map", "column_remove", "m.spine"),
        "0"
    );
}
