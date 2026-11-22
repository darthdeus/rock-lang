use crate::common::{emit_module, emit_v1_package, mangled_name, run_luajit};

#[test]
fn a_v1_embedded_child_is_reflected_by_its_minted_symbol() {
    let src = r#"
struct Pt { x: int, y: int }
struct Holder { embed p: Pt, tag: int }
enum Mode { Fast(string), Slow }
global MODE: Mode = Mode::Slow
inline(lua) fn embed_field_x(@repr(opaque) h: Holder) -> string = """
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
