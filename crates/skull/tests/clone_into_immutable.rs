use crate::common::{emit_module, function_body, require_luajit, run_luajit};

// `clone_into` gives the destination owner its own copy. On Lua a value with
// no mutable heap storage is already that copy, so the call must be the
// argument itself: the generic body allocated a `seen` table per call even
// for a string. Vec, Map and boxed values keep the deep copy.
const SRC: &str = r#"
enum Mood { Calm, Angry }
nobox value Pos { x: int, y: int }
nobox value Tag { name: string, count: int, weight: float, on: bool, mood: Mood, at: Pos, alias: string? }
struct Bag { label: string, items: [int] }
nobox value Wide { a: int, b: int, c: int, d: int, e: int }
nobox value Mixed { a: int, b: int, c: int, enabled: bool, label: string? }
nobox value Mutable { a: int, b: int, c: int, d: int, items: [int] }
global ORDER: string = ""
fn source_rows(xs: [Wide]) -> [Wide] { ORDER = ORDER + "v"; return xs }
fn target_storage() -> Storage { ORDER = ORDER + "s"; return OUT.storage }
@nomangle fn copy_mixed(xs: [Mixed]) -> [Mixed] { return xs.clone_into(OUT.storage) }
@nomangle fn clone_edges() -> string {
    let empty: [Wide] = vec_new_with_capacity(100)
    let empty_copy = empty.clone_into(OUT.storage)
    let xs: [Wide] = [Wide { a: 1, b: 2, c: 3, d: 4, e: 5 }]
    ORDER = ""
    let copy = source_rows(xs).clone_into(target_storage())
    let order = ORDER
    copy.push(Wide { a: 6, b: 7, c: 8, d: 9, e: 10 })
    let mixed: [Mixed] = [Mixed { a: 1, b: 2, c: 3, enabled: false, label: null }]
    let mixed_copy = copy_mixed(mixed)
    return "{empty_copy.len()} {empty_copy.capacity()} {order} {xs.len()} {copy.len()} {mixed_copy[0].enabled} {mixed_copy[0].label ?? "nil"}"
}
@nomangle fn clone_mutable_wide() -> string {
    let xs: [Mutable] = [Mutable { a: 1, b: 2, c: 3, d: 4, items: [10] }]
    let copy = xs.clone_into(OUT.storage)
    xs[0].items.push(20)
    return "{xs[0].items.len()} {copy[0].items.len()}"
}

inline(lua) fn tail_count(xs: [Wide]) -> int = """
    local n = 0
    for k, _ in pairs(xs._data) do
        if type(k) == "number" and k > xs.len * xs["@vec"].stride then n = n + 1 end
    end
    return n
"""


global OUT: [string] = []

@nomangle fn clone_string(s: string) -> string { return s.clone_into(OUT.storage) }
@nomangle fn clone_int(n: int) -> int { return n.clone_into(OUT.storage) }
@nomangle fn clone_maybe(s: string?) -> string? { return s.clone_into(OUT.storage) }
@nomangle fn clone_tag(name: string, n: int) -> string {
    let tag = Tag { name, count: n, weight: 1.5, on: true, mood: Mood::Angry, at: Pos { x: n, y: 2 }, alias: null }
    let copy = tag.clone_into(OUT.storage)
    return "{copy.name} {copy.count} {copy.weight} {copy.on} {copy.at.x} {copy.alias ?? "-"}"
}
@nomangle fn clone_list() -> string {
    let xs: [int] = [1, 2]
    let copy = xs.clone_into(OUT.storage)
    xs.push(3)
    return "{xs.len()} {copy.len()}"
}
@nomangle fn copy_wide(xs: [Wide]) -> [Wide] { return xs.clone_into(OUT.storage) }
@nomangle fn make_rows() -> [Wide] {
    let xs: [Wide] = vec_new_with_capacity(1000)
    for i in 0..1000 { xs.push(Wide { a: i, b: 2, c: 3, d: 4, e: 5 }) }
    xs.clear()
    xs.push(Wide { a: 7, b: 2, c: 3, d: 4, e: 5 })
    return xs
}
@nomangle fn clone_wide() -> string {
    let xs = make_rows()
    let old_tail = tail_count(xs)
    let copy = copy_wide(xs)
    let copied_tail = tail_count(copy)
    xs[0] = Wide { a: 9, b: 2, c: 3, d: 4, e: 5 }
    return "{old_tail} {copied_tail} {copy[0].a} {copy.len()} {copy.capacity()}"
}
@nomangle fn clone_bag() -> string {
    let bag = Bag { label: "b", items: [1] }
    let copy = bag.clone_into(OUT.storage)
    bag.items.push(2)
    return "{bag.items.len()} {copy.items.len()}"
}
"#;

#[test]
fn clone_into_is_the_argument_for_values_without_mutable_storage() {
    let lua = emit_module(SRC);
    for name in ["clone_string", "clone_int", "clone_maybe", "clone_tag"] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("clone_into") && !body.contains("__rock_deepcopy_value"),
            "`{name}` still copies an immutable value:\n{body}"
        );
    }
    for name in ["clone_list", "clone_bag"] {
        let body = function_body(&lua, name);
        assert!(
            body.contains("clone_into"),
            "`{name}` must keep the deep copy of mutable storage:\n{body}"
        );
    }
}

#[test]
fn clone_into_keeps_values_and_deep_copies_mutable_storage() {
    require_luajit();
    let mut lua = emit_module(SRC);
    lua.push_str(
        "\nprint(clone_string(\"s\"), clone_int(7), clone_maybe(nil), clone_maybe(\"m\"), \
         clone_tag(\"t\", 4), clone_list(), clone_bag())\n",
    );
    assert_eq!(
        run_luajit(&lua, "clone_into_immutable"),
        "s\t7\tnil\tm\tt 4 1.5 true 4 -\t3 2\t2 1"
    );
}

#[test]
fn clone_into_strided_copies_only_live_rows() {
    require_luajit();
    let lua = emit_module(SRC);
    assert_eq!(run_luajit(&format!("{lua}\nprint(clone_wide())\n"), "clone_into_strided_live"),
        "4995 0 7 1 1000");
}

#[test]
fn clone_into_strided_edges_and_mutable_fallback() {
    require_luajit();
    let lua = emit_module(SRC);
    assert!(function_body(&lua, "copy_wide").contains("__rock_stride_copy"));
    assert!(function_body(&lua, "copy_mixed").contains("__rock_stride_copy"));
    assert!(function_body(&lua, "clone_mutable_wide").contains("clone_into"));
    assert_eq!(run_luajit(&format!("{lua}\nprint(clone_edges()); print(clone_mutable_wide())\n"),
        "clone_into_strided_edges"), "0 100 vs 1 2 false nil\n2 1");
}
