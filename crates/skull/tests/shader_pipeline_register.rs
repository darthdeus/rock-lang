use crate::common::{body_id, emit_full_of, ws_with_stdlib};

fn overlay_lua(before: &str, after: &str, edited: &str) -> String {
    let (_, mut cache) = emit_full_of(&ws_with_stdlib(before), "base emit");
    let after = ws_with_stdlib(after);
    skull::mir_emit::emit_overlay(&after, &mut cache, &[body_id(&after, edited)])
        .expect("emit_overlay")
        .lua
}

const KEY: &str = "ui_vert+ui_sim_solid_frag";

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
