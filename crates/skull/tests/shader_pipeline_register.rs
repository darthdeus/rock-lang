use crate::common::{
    body_id, edit, emit_full_of, emit_mir, emit_module, function_body, ws_with_stdlib,
};

fn overlay_lua(before: &str, after: &str, edited: &str) -> String {
    let (_, mut cache) = emit_full_of(&ws_with_stdlib(before), "base emit");
    let after = ws_with_stdlib(after);
    skull::mir_emit::emit_overlay(&after, &mut cache, &[body_id(&after, edited)])
        .expect("emit_overlay")
        .lua
}

const SRC: &str = r#"
value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
value UiVertU  { uProjection: Mat4, uView: Mat4, uTransform: Mat4 }
value UiVary   { position: Vec4, TexCoord: Vec2 }

@shader(vertex)
fn ui_vert(@attr a: QuadVert, @uniform u: UiVertU) -> UiVary {
    let clip = u.uProjection * u.uView * u.uTransform * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
    return UiVary { position: clip, TexCoord: a.aTexCoord }
}

@shader(fragment)
fn ui_sim_solid_frag(vary: UiVary) -> Vec4 {
    return Vec4 { x: 1.0, y: 0.0, z: 1.0, w: 1.0 }
}

fn make_a() -> string { return rock_shader(ui_vert, ui_sim_solid_frag) }
fn make_b() -> string { return rock_shader(ui_vert, ui_sim_solid_frag) }
"#;

const KEY: &str = "ui_vert+ui_sim_solid_frag";

#[test]
fn a_pipeline_call_collapses_to_its_key_and_registers_once() {
    let lua = emit_module(SRC);
    let body = function_body(&lua, "make_a");
    assert!(body.contains(&format!("\"{KEY}\"")));
    assert!(!body.contains("rock_shader("));
    assert!(!body.contains("compile-time shader-pipeline intrinsic"));
    let needle = format!("__rock_shader_register(\"{KEY}\"");
    assert_eq!(lua.matches(&needle).count(), 1);
    let reg = &lua[lua.find(&needle).unwrap()..];
    for glsl in [
        "#version 330 core",
        "#ifdef VERTEX",
        "#ifdef PIXEL",
        "layout (location = 0) in vec2 aPos;",
    ] {
        assert!(reg.contains(glsl), "{glsl}");
    }
    assert!(reg.contains("false, [[") && reg.contains("fullscreen 0"));
}

#[test]
fn emission_lists_each_registered_shader_source() {
    let emit = emit_mir(SRC);
    let shaders: Vec<_> = emit
        .shaders
        .iter()
        .map(|s| (s.key.as_str(), s.language))
        .collect();
    assert_eq!(shaders, [(KEY, skull::mir_emit::ShaderLanguage::Glsl)]);
    let source = &emit.shaders[0].source;
    assert!(source.contains("void main"));
    assert!(emit.lua.contains(source.as_str()));
}

fn src_with_frag(x: &str) -> String {
    format!(
        r#"
value QuadVert {{ aPos: Vec2, aTexCoord: Vec2 }}
value UiVertU  {{ uProjection: Mat4, uView: Mat4, uTransform: Mat4 }}
value UiVary   {{ position: Vec4, TexCoord: Vec2 }}

@shader(vertex)
fn ui_vert(@attr a: QuadVert, @uniform u: UiVertU) -> UiVary {{
    let clip = u.uProjection * u.uView * u.uTransform * Vec4 {{ x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }}
    return UiVary {{ position: clip, TexCoord: a.aTexCoord }}
}}

@shader(fragment)
fn ui_sim_solid_frag(vary: UiVary) -> Vec4 {{ return Vec4 {{ x: {x}, y: 0.0, z: 1.0, w: 1.0 }} }}

fn use_it() -> string {{ return rock_shader(ui_vert, ui_sim_solid_frag) }}
"#
    )
}

#[test]
fn changed_fragment_body_changes_registration() {
    let a = emit_module(&src_with_frag("1.0"));
    let b = emit_module(&src_with_frag("0.5"));
    assert!(a.contains("vec4(1.0, 0.0, 1.0, 1.0)"));
    assert!(b.contains("vec4(0.5, 0.0, 1.0, 1.0)"));
    assert_ne!(a, b);
}

#[test]
fn a_fragment_edit_rebuilds_its_registration_incrementally_and_in_an_overlay() {
    let before = ws_with_stdlib(&src_with_frag("1.0"));
    let after = ws_with_stdlib(&src_with_frag("0.5"));
    let edit = edit(&before, &after, &[body_id(&after, "ui_sim_solid_frag")]);
    let overlay = edit.overlay.expect("emit_overlay").lua;
    assert!(overlay.contains(&format!("__rock_shader_register(\"{KEY}\"")));
    assert!(overlay.contains("vec4(0.5, 0.0, 1.0, 1.0)"));
}

#[test]
fn an_unrelated_body_overlay_omits_shader_registrations() {
    let src = format!(
        "{}\nfn unrelated_shader_user() -> int {{ return 1 }}\n",
        src_with_frag("1.0")
    );
    assert!(!overlay_lua(&src, &src, "unrelated_shader_user").contains("__rock_shader_register("));
}

fn src_with_shader_helper(x: &str) -> String {
    src_with_frag(&format!("shader_color_component({x})")).replace(
        "@shader(fragment)",
        "fn shader_color_component(x: float) -> float { return x }\n\n@shader(fragment)",
    )
}

fn src_with_optional_call(include_call: bool) -> String {
    let body = if include_call {
        "return rock_shader(ui_vert, ui_sim_solid_frag)"
    } else {
        "return \"none\""
    };
    let base = src_with_frag("1.0").replace(
        "fn use_it() -> string { return rock_shader(ui_vert, ui_sim_solid_frag) }\n",
        "",
    );
    format!("{base}\nfn shader_choice() -> string {{ {body} }}\n")
}

#[test]
fn overlays_that_reach_a_pipeline_carry_its_registration() {
    let helper = src_with_shader_helper("1.0");
    let helper_overlay = overlay_lua(
        &helper,
        &helper.replace("return x }", "return x * 0.5 }"),
        "shader_color_component",
    );
    assert!(helper_overlay.contains(&format!("__rock_shader_register(\"{KEY}\"")));
    assert!(helper_overlay.contains("0.5"));
    let callsite_overlay = overlay_lua(
        &src_with_optional_call(false),
        &src_with_optional_call(true),
        "shader_choice",
    );
    assert!(callsite_overlay.contains(&format!("__rock_shader_register(\"{KEY}\"")));
    let single = overlay_lua(MIXED_SRC, MIXED_SRC, "fs_frag");
    assert!(single.contains("__rock_shader_register(\"fs_frag\""));
}

const FS_SRC: &str = r#"
@shader(fragment)
fn fs_frag(tc: Vec2) -> Vec4 {
    return Vec4 { x: tc.x, y: 0.0, z: 1.0, w: 1.0 }
}

fn make_fs() -> string { return rock_shader(fs_frag) }
"#;

const MIXED_SRC: &str = r#"
value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
value UiVertU  { uProjection: Mat4, uView: Mat4, uTransform: Mat4 }
value UiVary   { position: Vec4, TexCoord: Vec2 }

@shader(vertex)
fn ui_vert(@attr a: QuadVert, @uniform u: UiVertU) -> UiVary {
    let clip = u.uProjection * u.uView * u.uTransform * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
    return UiVary { position: clip, TexCoord: a.aTexCoord }
}

@shader(fragment)
fn ui_sim_solid_frag(vary: UiVary) -> Vec4 { return Vec4 { x: 1.0, y: 0.0, z: 1.0, w: 1.0 } }

@shader(fragment)
fn fs_frag(tc: Vec2) -> Vec4 { return Vec4 { x: tc.x, y: 0.0, z: 1.0, w: 1.0 } }

fn make_pair() -> string { return rock_shader(ui_vert, ui_sim_solid_frag) }
fn make_fs() -> string { return rock_shader(fs_frag) }
"#;

#[test]
fn a_single_fragment_call_collapses_to_its_key_and_registers_fullscreen() {
    let lua = emit_module(FS_SRC);
    let body = function_body(&lua, "make_fs");
    assert!(body.contains("\"fs_frag\""));
    assert!(!body.contains("rock_shader("));
    assert!(!body.contains("compile-time shader-pipeline intrinsic"));
    let needle = "__rock_shader_register(\"fs_frag\"";
    assert_eq!(lua.matches(needle).count(), 1);
    let reg = &lua[lua.find(needle).unwrap()..];
    for glsl in ["#version 330 core", "#ifdef PIXEL", "#ifdef VERTEX"] {
        assert!(reg.contains(glsl), "{glsl}");
    }
    assert!(reg.contains("true, [[") && reg.contains("fullscreen 1"));
}

#[test]
fn mixed_single_and_pair_register_both_sorted() {
    let lua = emit_module(MIXED_SRC);
    let fs = lua
        .find("__rock_shader_register(\"fs_frag\"")
        .expect("single-arg registration");
    let pair = lua
        .find("__rock_shader_register(\"ui_vert+ui_sim_solid_frag\"")
        .expect("pair registration");
    assert!(fs < pair);
}

#[test]
fn mixed_reemission_byte_identical() {
    assert_eq!(emit_module(MIXED_SRC), emit_module(MIXED_SRC));
}
