//! Codegen-shape contract: an all-constant `MakeValueStruct` is hoisted to ONE
//! function-owned module-scope constant, not rebuilt at each use.
//!
//! The mag HUD/text path's `make_text_vertex` writes a nested
//! `outline_color: Vec2 { 0.0, 0.0 }` into every glyph vertex slot. opt #3 does
//! not recurse into nested value-struct fields, so this becomes a literal
//! `slot.outline_color = Vec2_ct(0.0, 0.0)` per store. On a trace it sinks; in the
//! interpreter (mag's aborted HUD loops) it allocates a fresh cdata every store --
//! the entire skull-vs-rock interpreter-path gap on the text loop (skull 8.25 vs
//! rock 6.70 KB per draw_text call; `benchmarks/opt_showcase/07_text_interp_alloc.rock`).
//! rock hoists an all-constant value-struct literal to a module-scope constant
//! (`__rock_const_Vec2_...`); skull emits `__skull_kc_<fn>_<g> = <Name>_ct(..)`
//! once and references it. Value structs are immutable, so one shared cdata is
//! sound, built once at chunk load (0 alloc on BOTH the traced and interpreted
//! paths -- unlike a per-call function-entry local, which defeats SINK).
//!
//! Emitted in the function's OWN chunk (not shared scaffolding), so an incremental
//! body-edit overlay carries the def with the function it serves.

use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn emit_module(src: &str) -> String {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).expect("emit").lua
}

fn function_body<'a>(lua: &'a str, name: &str) -> &'a str {
    let head = format!("function {name}");
    let start = lua.find(&head).unwrap_or_else(|| panic!("no `{head}` in:\n{lua}"));
    let rest = &lua[start..];
    let end = rest.find("\nend\n").map(|e| e + 5).unwrap_or(rest.len());
    &rest[..end]
}

fn count(hay: &str, needle: &str) -> usize {
    hay.matches(needle).count()
}

// Two nested const Vec2 fields, both a DISTINCTIVE `Vec2 { 3.0, 7.0 }`, written
// into a NativeBuffer<Outer> slot. opt #3 lowers the store to
// `slot.a = Vec2_ct(3, 7); slot.b = Vec2_ct(3, 7)`. Hoisting + dedup makes that
// one module-scope `__skull_kc_..._N = Vec2_ct(3.0, 7.0)` referenced twice.
const TWO_CONST_NESTED: &str = r#"
value Vec2 { x: float, y: float }
value Outer { a: Vec2, b: Vec2, c: float }

@nomangle inline(lua) fn __nb_alloc_Outer(n: int) -> Ptr<Outer> = 'return ffi.new("Outer[?]", n)'
fn nb(n: int) -> NativeBuffer<Outer> { return NativeBuffer { ptr: __nb_alloc_Outer(n), len: n } }

fn fill(buf: NativeBuffer<Outer>, n: int) {
    for i in 0..n {
        buf[i] = Outer { a: Vec2 { x: 3.0, y: 7.0 }, b: Vec2 { x: 3.0, y: 7.0 }, c: i as float }
    }
}

fn main() {
    let buf = nb(4)
    fill(buf, 4)
}
"#;

#[test]
fn const_value_struct_ctor_is_hoisted_to_module_scope() {
    let lua = emit_module(TWO_CONST_NESTED);
    // The distinctive ctor is BUILT exactly once, in a module-scope hoist def.
    let builds = count(&lua, "Vec2_ct(3.0, 7.0)");
    assert_eq!(
        builds, 1,
        "the all-constant Vec2 {{3,7}} must be built once (one hoist def), found {builds}:\n{lua}"
    );
    // The def is a `__skull_kc_*` global assignment, not inside the loop.
    assert!(
        lua.contains("__skull_kc_") && lua.contains("= Vec2_ct(3.0, 7.0)"),
        "expected a module-scope `__skull_kc_* = Vec2_ct(3.0, 7.0)` hoist def:\n{lua}"
    );
    // The hot function rebuilds nothing -- it references the global instead.
    let body = function_body(&lua, "fill");
    assert_eq!(
        count(body, "Vec2_ct("),
        0,
        "fill must reference the hoisted global, not rebuild the ctor:\n{body}"
    );
    assert!(
        body.contains("__skull_kc_"),
        "fill must reference the hoisted const global:\n{body}"
    );
}
