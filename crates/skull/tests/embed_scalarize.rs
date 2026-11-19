use crate::common::{emit_module, function_body};

const EMBED_LEAVES: &str = r#"
struct Pt { x: int, y: int }
struct Holder { embed p: Pt, tag: int }

fn mk(a: int, b: int) -> Holder {
    return Holder { p: Pt { x: a, y: b }, tag: 3 }
}

fn read_leaf(h: Holder) -> int { return h.p.x + h.p.y + h.tag }

fn write_leaf(h: Holder) { h.p.x = 9 }

fn main() {
    let h = mk(1, 2)
    write_leaf(h)
    let _ = read_leaf(h)
}
"#;

#[test]
fn embed_ctor_splices_dotted_keys() {
    let lua = emit_module(EMBED_LEAVES);
    let mk = function_body(&lua, "mk");
    assert!(mk.contains(r#"["p.x"]"#) && mk.contains(r#"["p.y"]"#));
    assert!(!mk.contains(r#""Pt""#));
    assert!(mk.contains("tag = "));
}

#[test]
fn embed_leaf_read_indexes_the_parent() {
    let lua = emit_module(EMBED_LEAVES);
    let body = function_body(&lua, "read_leaf");
    assert!(body.contains(r#"["p.x"]"#) && body.contains(r#"["p.y"]"#));
    assert!(!body.contains(".p"));
}

#[test]
fn embed_leaf_write_stores_into_the_parent() {
    let lua = emit_module(EMBED_LEAVES);
    let body = function_body(&lua, "write_leaf");
    assert!(body.contains(r#"["p.x"] = "#));
    assert!(!body.contains(".p"));
}

const EMBED_NOBOX_VALUE: &str = r#"
nobox value Pt { x: int, y: int }
struct Holder { embed p: Pt?, tag: int }

fn mk(a: int, b: int) -> Holder {
    return Holder { p: Pt { x: a, y: b }, tag: 3 }
}

fn read_leaf(h: Holder) -> int { return (h.p?.x ?? 0) + (h.p?.y ?? 0) + h.tag }
fn main() { let _ = read_leaf(mk(1, 2)) }
"#;

#[test]
fn embed_nobox_value_uses_the_owned_representation_fact() {
    let lua = emit_module(EMBED_NOBOX_VALUE);
    let body = function_body(&lua, "mk");
    assert!(body.contains(r#"["p.x"]"#) && body.contains(r#"["p.y"]"#));
    assert!(!body.contains(r#""Pt""#));
}

const EMBED_NESTED: &str = r#"
struct Inner { c: int }
struct Mid { embed b: Inner, m: int }
struct Outer { embed a: Mid, o: int }

fn mk_outer(x: int) -> Outer {
    return Outer { a: Mid { b: Inner { c: x }, m: 2 }, o: 3 }
}

fn read_outer(t: Outer) -> int { return t.a.b.c }

fn main() { let _ = read_outer(mk_outer(1)) }
"#;

#[test]
fn nested_embed_composes_the_whole_path() {
    let lua = emit_module(EMBED_NESTED);
    let ctor = function_body(&lua, "mk_outer");
    assert!(ctor.contains(r#"["a.b.c"]"#));
    assert!(ctor.contains(r#"["a.m"]"#));
    assert!(!ctor.contains(r#""Mid""#) && !ctor.contains(r#""Inner""#));

    let read = function_body(&lua, "read_outer");
    assert!(read.contains(r#"["a.b.c"]"#));
}

const EMBED_HOT_LOOP: &str = r#"
struct Pt { x: int, y: int }
struct Holder { embed p: Pt, tag: int }

@inline fn mk(s: int) -> Holder { return Holder { p: Pt { x: s, y: s * 2 }, tag: s } }

fn hot(n: int) -> int {
    let acc = 0
    for i in 0..n {
        let h = mk(i)
        h.p.x = h.p.x + 1
        acc = acc + h.p.x + h.p.y + h.tag
    }
    return acc
}

fn main() { let _ = hot(4) }
"#;

#[test]
fn embed_parent_still_scalarizes() {
    let lua = emit_module(EMBED_HOT_LOOP);
    let hot = function_body(&lua, "hot");
    assert!(!hot.contains("{"));
    assert!(!hot.contains(r#"["p."#));
}

const EMBED_VALUE_PARENT: &str = r#"
value V2 { x: float, y: float }
value ValueParent { embed v: V2, w: float }

fn mkv(a: float) -> ValueParent {
    return ValueParent { v: V2 { x: a, y: a }, w: 1.0 }
}

fn main() { let _ = mkv(1.0) }
"#;

#[test]
fn value_parent_embed_introduces_no_dotted_keys() {
    let lua = emit_module(EMBED_VALUE_PARENT);
    let body = function_body(&lua, "mkv");
    assert!(!body.contains(r#"["v."#));
}

const NOBOX_REFLECTION_FIELD: &str = r#"
nobox value Pt { x: int, y: int }
struct Holder { p: Pt, tag: int }

fn main() {
    let _ = Holder { p: Pt { x: 1, y: 2 }, tag: 3 }
}
"#;

#[test]
fn type_table_uses_the_shared_plan_for_a_nobox_field() {
    let lua = emit_module(NOBOX_REFLECTION_FIELD);
    assert!(lua.contains(r#"name = "p", type = "Pt", nullable = false, embed = true"#));
}

#[test]
fn a_v1_embedded_child_is_reflected_by_its_minted_symbol() {
    let src = r#"
struct Pt { x: int, y: int }
struct Holder { embed p: Pt, tag: int }
enum Mode { Fast(string), Slow }
global MODE: Mode = Mode::Slow
inline(lua) fn embed_field_x(h: Holder) -> string = """
    local decl = _G.__ROCK_TYPE_TABLE[h.__ty]
    for i = 1, #decl.fields do
        local f = decl.fields[i]
        if f.name == "p" then
            local child = __rock_embed_view(h, "p", f.type, f.nullable)
            return child and tostring(child.x) or "nil"
        end
    end
    return "missing"
"""
fn probe() -> string { return embed_field_x(Holder { p: Pt { x: 4, y: 5 }, tag: 3 }) }
fn main() {}
"#;
    let legacy =
        crate::common::emit_module(&format!("{src}\nfn legacy_probe() {{ print(probe()) }}"));
    let legacy_probe = crate::common::mangled_name(&legacy, "legacy_probe");
    assert_eq!(
        crate::common::run_luajit(&format!("{legacy}\n{legacy_probe}()\n")),
        "4"
    );

    let v1_src = src.replace("fn probe()", "pub fn probe()");
    let emitted = crate::common::emit_v1_package("v1embed", &[("main.rock", &v1_src)]);
    let probe = crate::common::mangled_name(&emitted.lua, "probe");
    assert_eq!(
        crate::common::run_luajit(&format!("{}\nprint({probe}())\n", emitted.lua)),
        "4"
    );
    let keys: Vec<&str> = emitted
        .lua
        .lines()
        .filter_map(|l| l.strip_prefix("_G.__ROCK_ENUM_FLAT_LAYOUT[\""))
        .filter_map(|rest| rest.split('"').next())
        .filter(|key| key.contains("Mode"))
        .collect();
    assert_eq!(keys.len(), 1);
    assert!(!keys[0].contains("::"));
}
