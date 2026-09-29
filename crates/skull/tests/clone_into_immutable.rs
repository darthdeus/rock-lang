use crate::common::{emit_module, function_body, run_luajit};

// `clone_into` gives the destination owner its own copy. On Lua a value with
// no mutable heap storage is already that copy, so the call must be the
// argument itself: the generic body allocated a `seen` table per call even
// for a string. Vec, Map and boxed values keep the deep copy.
const SRC: &str = r#"
enum Mood { Calm, Angry }
nobox value Pos { x: int, y: int }
nobox value Tag { name: string, count: int, weight: float, on: bool, mood: Mood, at: Pos, alias: string? }
struct Bag { label: string, items: [int] }

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
    if std::process::Command::new("luajit").arg("-v").output().is_err() {
        eprintln!("luajit not found; skipping execution check");
        return;
    }
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
