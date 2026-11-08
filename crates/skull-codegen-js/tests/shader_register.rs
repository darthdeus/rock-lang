//! `rock_shader` under JS emission.
//!
//! `rock_shader(vert, frag)` collapses to a pipeline key const; the driver emits
//! one top-level `__rock_shader_register(key, glsl)` per distinct pair. Those
//! calls are raw driver text (not MIR), so the register fn is never discovered
//! transitively. When a program has a rock_shader pair, emit must force a
//! JS sibling or a portable Rock registration body
//! into the program under its bare `@nomangle` name (so the register lines
//! resolve), or reject loudly if none is reachable.
use crate::common::ws_with;

fn emit_program_js(ws: &bone_core::workspace::Workspace) -> Result<String, String> {
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    skull_codegen_js::emit::emit_program(ws, mid)
        .map(|p| p.program)
        .map_err(|e| e.message)
}

const SHADER_SRC: &str = r#"
value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
value UiVertU  { uProjection: Mat4, uView: Mat4, uTransform: Mat4 }
value UiVary   { position: Vec4, TexCoord: Vec2 }

@shader(vertex)
fn ui_vert(@attr a: QuadVert, @uniform u: UiVertU) -> UiVary {
    let clip = u.uProjection * u.uView * u.uTransform * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
    return UiVary { position: clip, TexCoord: a.aTexCoord }
}

@shader(fragment)
fn ui_solid_frag(vary: UiVary) -> Vec4 {
    return Vec4 { x: 1.0, y: 0.0, z: 1.0, w: 1.0 }
}

global SHADER_KEY: string = rock_shader(ui_vert, ui_solid_frag)

fn main() {
    print("{SHADER_KEY}")
}
"#;

#[test]
fn rock_shader_pair_without_js_register_sibling_rejects() {
    // No `__rock_shader_register` sibling reachable -> the shader would never be
    // registered on web. Emit must reject loudly rather than silently drop it.
    let ws = ws_with(SHADER_SRC);
    let err =
        emit_program_js(&ws).expect_err("rock_shader without a js register sibling must reject");
    assert!(
        err.contains("__rock_shader_register"),
        "reject must name the missing register sibling:\n{err}"
    );
}

#[test]
fn rock_shader_pair_with_plain_rock_register_emits_it_bare() {
    // A host's real `__rock_shader_register` is typically a plain-Rock
    // @nomangle fn -- portable on JS once its two inline(lua) helpers have js
    // siblings. A reachable plain-Rock register fn must be rooted under its
    // bare name (the driver register lines call it verbatim), not rejected.
    let mut src = String::from(SHADER_SRC);
    src.push_str(
        r#"
global WEB_SHADER_SRCS: Map<string, string> = map_new()

@nomangle fn __rock_shader_register(key: string, src: string) {
    WEB_SHADER_SRCS.insert(key, src)
}
"#,
    );
    let ws = ws_with(&src);
    let prog = emit_program_js(&ws).expect("emit js program with plain-rock register");
    assert!(
        prog.contains("function __rock_shader_register("),
        "plain-Rock register fn must emit under its bare name:\n{prog}"
    );
}

#[test]
fn rock_shader_pair_with_inline_js_register_sibling_emits() {
    // The web layer supplies the sibling; the register fn is forced into the
    // program under its bare @nomangle name so driver register lines resolve.
    let mut src = String::from(SHADER_SRC);
    src.push_str(
        r#"
@nomangle inline(js) fn __rock_shader_register(key: string, src: string) = """
  (globalThis.__ROCK_SHADERS = globalThis.__ROCK_SHADERS || {})[key] = src;
"""
"#,
    );
    let ws = ws_with(&src);
    let prog = emit_program_js(&ws).expect("emit js program with register sibling");
    assert!(
        prog.contains("function __rock_shader_register(key, src)"),
        "register sibling must emit under its bare @nomangle name:\n{prog}"
    );
    assert!(
        prog.contains("globalThis.__ROCK_SHADERS"),
        "register sibling body must emit verbatim:\n{prog}"
    );
}

fn qualified_register_workspace(registrations: &[&str]) -> bone_core::workspace::Workspace {
    let mut modules = skull::pipeline::with_stdlib([skull_parser::parse_module_clean(
        SHADER_SRC,
        ustr::Ustr::from("<test>"),
    )
    .expect("shader source")])
    .expect("stdlib");
    for (index, registration) in registrations.iter().enumerate() {
        let mut module = skull_parser::parse_module_clean_with_dialect(
            &format!("mod registry{index};\n{registration}"),
            ustr::Ustr::from(&format!("<registry{index}>")),
            skull_parser::Dialect::V1,
        )
        .expect("v1 registration source");
        module.package = Some(skull_parser::PackageTag {
            id: 0,
            self_name: "shaders".into(),
            identity_storage: Default::default(),
        });
        modules.push(module);
    }
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("workspace");
    let errors: Vec<_> = ws
        .inferences
        .iter()
        .flatten()
        .flat_map(|inference| inference.errors.values().flatten())
        .chain(
            ws.modules
                .iter()
                .flat_map(|module| module.lowering_errors.values().flatten()),
        )
        .chain(
            ws.modules
                .iter()
                .flat_map(|module| module.interface_errors.iter()),
        )
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    ws
}

#[test]
fn qualified_register_is_rooted_under_the_driver_symbol() {
    for registration in [
        "global SOURCES: Map<string, string> = map_new()\n\
         pub @nomangle fn __rock_shader_register(key: string, src: string) { SOURCES.insert(key, src) }",
        "pub @nomangle inline(js) fn __rock_shader_register(key: string, src: string) = \"console.log(key);\"",
    ] {
        let ws = qualified_register_workspace(&[registration]);
        assert!(
            !ws.functions_by_name
                .contains_key(&ustr::Ustr::from("__rock_shader_register"))
        );
        let program = emit_program_js(&ws).expect("qualified registration must emit");
        assert!(
            program.contains("function __rock_shader_register("),
            "{program}"
        );
    }
}

#[test]
fn qualified_lua_only_register_is_a_named_rejection() {
    let ws = qualified_register_workspace(&[
        "pub @nomangle inline(lua) fn __rock_shader_register(key: string, src: string) = \"print(key)\"",
    ]);
    let error = emit_program_js(&ws).expect_err("Lua-only registration cannot serve JS");
    assert!(
        error.contains("no JS-servable `__rock_shader_register`"),
        "{error}"
    );
}

#[test]
fn ambiguous_qualified_registers_are_a_named_rejection() {
    let registration =
        "pub @nomangle fn __rock_shader_register(key: string, src: string) { print(key) }";
    let ws = qualified_register_workspace(&[registration, registration]);
    let error = emit_program_js(&ws).expect_err("two registration bodies cannot own one symbol");
    assert!(
        error.contains("ambiguous") && error.contains("__rock_shader_register"),
        "{error}"
    );
}
