use crate::common::{emit_module, emit_v1_package, function_body, mangled_name, run_luajit};

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

const EMBED_NOBOX_VALUE: &str = r#"
nobox value Pt { x: int, y: int }
struct Holder { embed p: Pt?, tag: int }

fn mk(a: int, b: int) -> Holder {
    return Holder { p: Pt { x: a, y: b }, tag: 3 }
}

fn read_leaf(h: Holder) -> int { return (h.p?.x ?? 0) + (h.p?.y ?? 0) + h.tag }
fn main() { let _ = read_leaf(mk(1, 2)) }
"#;

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

const EMBED_VALUE_PARENT: &str = r#"
value V2 { x: float, y: float }
value ValueParent { embed v: V2, w: float }

fn mkv(a: float) -> ValueParent {
    return ValueParent { v: V2 { x: a, y: a }, w: 1.0 }
}

fn main() { let _ = mkv(1.0) }
"#;

const NOBOX_REFLECTION_FIELD: &str = r#"
nobox value Pt { x: int, y: int }
struct Holder { p: Pt, tag: int }

fn main() {
    let _ = Holder { p: Pt { x: 1, y: 2 }, tag: 3 }
}
"#;

#[test]
fn a_v1_embedded_child_is_reflected_by_its_minted_symbol() {
    let src = r#"
struct Pt { x: int, y: int }
struct Holder { embed p: Pt, tag: int }
enum Mode { Fast(string), Slow }
global MODE: Mode = Mode::Slow
inline(lua) fn embed_field_x(h: Holder) -> string = """
    local decl = __rock_field_declaration(h)
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
    let legacy = emit_module(&format!("{src}\nfn legacy_probe() {{ print(probe()) }}"));
    let legacy_probe = mangled_name(&legacy, "legacy_probe");
    assert_eq!(run_luajit(&format!("{legacy}\n{legacy_probe}()\n")), "4");

    let v1_src = src.replace("fn probe()", "pub fn probe()");
    let emitted = emit_v1_package("v1embed", &[("main.rock", &v1_src)]);
    let probe = mangled_name(&emitted.lua, "probe");
    assert_eq!(
        run_luajit(&format!("{}\nprint({probe}())\n", emitted.lua)),
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
