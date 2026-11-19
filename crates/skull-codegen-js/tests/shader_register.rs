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
fn rock_shader_pair_roots_a_js_servable_register_under_its_bare_name() {
    let err = crate::common::emit_program_js(&ws_with(SHADER_SRC))
        .expect_err("rock_shader without a js register sibling must reject");
    assert!(
        err.contains("__rock_shader_register"),
        "reject must name the missing register sibling:\n{err}"
    );
    for (register, markers) in [
        (
            r#"
global WEB_SHADER_SRCS: Map<string, string> = map_new()

@nomangle fn __rock_shader_register(key: string, src: string) {
    WEB_SHADER_SRCS.insert(key, src)
}
"#,
            &["function __rock_shader_register("][..],
        ),
        (
            r#"
@nomangle inline(js) fn __rock_shader_register(key: string, src: string) = """
  (globalThis.__ROCK_SHADERS = globalThis.__ROCK_SHADERS || {})[key] = src;
"""
"#,
            &[
                "function __rock_shader_register(key, src)",
                "globalThis.__ROCK_SHADERS",
            ],
        ),
    ] {
        let prog = crate::common::emit_program_js(&ws_with(&format!("{SHADER_SRC}{register}")))
            .expect("emit js program with a register");
        for marker in markers {
            assert!(prog.contains(marker), "missing `{marker}`:\n{prog}");
        }
    }
}

fn qualified_register_workspace(registrations: &[&str]) -> bone_core::workspace::Workspace {
    let mut users = vec![
        skull_parser::parse_module_clean(SHADER_SRC, ustr::Ustr::from("<test>"))
            .expect("shader source"),
    ];
    for (index, registration) in registrations.iter().enumerate() {
        users.push(crate::common::package_module(
            "shaders",
            &format!("<registry{index}>"),
            &format!("mod registry{index};\n{registration}"),
        ));
    }
    crate::common::ws_of_modules(users)
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
        let program =
            crate::common::emit_program_js(&ws).expect("qualified registration must emit");
        assert!(
            program.contains("function __rock_shader_register("),
            "{program}"
        );
    }
}

#[test]
fn unservable_qualified_registers_are_named_rejections() {
    let rock_register =
        "pub @nomangle fn __rock_shader_register(key: string, src: string) { print(key) }";
    for (registrations, needle) in [
        (
            &[
                "pub @nomangle inline(lua) fn __rock_shader_register(key: string, src: string) = \"print(key)\"",
            ][..],
            "no JS-servable `__rock_shader_register`",
        ),
        (&[rock_register, rock_register], "ambiguous"),
    ] {
        let ws = qualified_register_workspace(registrations);
        let error = crate::common::emit_program_js(&ws).expect_err("register cannot serve JS");
        assert!(
            error.contains(needle) && error.contains("__rock_shader_register"),
            "{error}"
        );
    }
}
