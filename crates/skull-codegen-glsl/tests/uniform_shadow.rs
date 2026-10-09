//! A shader local named like an emitted global must not shadow it: the GLSL
//! emitter hoists every local to function scope, so `let density` beside a
//! `density` uniform lane used to make every `u.density` read the local.
//! The hoisted local is renamed apart; the global read survives.

use skull_codegen_glsl::{GlslProfile, ShaderEmission};
use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn workspace(src: &str) -> (bone_semantic::inference::Workspace, bone_semantic::prelude::ModuleId) {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    (ws, mid)
}

fn emit_entry(src: &str, root: &str) -> ShaderEmission {
    let (ws, mid) = workspace(src);
    skull_codegen_glsl::emit_shader_entry_with_dependencies(&ws, mid, root, GlslProfile::Core330)
        .expect("emit")
}

fn emit_pipeline(src: &str, vert: &str, frag: &str) -> ShaderEmission {
    let (ws, mid) = workspace(src);
    skull_codegen_glsl::emit_shader_pipeline_with_dependencies(&ws, mid, vert, frag, GlslProfile::Core330)
        .expect("emit")
}

/// No hoisted `float` local shadows `lane`: block members indent 4, hoisted
/// locals 2, so the indented pattern distinguishes them.
fn assert_no_hoisted_shadow(glsl: &str, lane: &str) {
    assert!(
        !glsl.contains(&format!("\n  float {lane};")),
        "hoisted local shadows global `{lane}`:\n{glsl}"
    );
}

/// The A1 repro: `let density` beside a `density` uniform lane.
#[test]
fn hoisted_local_renamed_apart_from_uniform_lane() {
    let src = r#"
        value ShadowU { density: float }
        @shader(fragment)
        fn shadow_frag(@uniform u: ShadowU, tc: Vec2) -> Vec4 {
            let density = u.density * 2.0
            return Vec4 { x: density, y: tc.y, z: 0.0, w: 1.0 }
        }
    "#;
    let e = emit_entry(src, "shadow_frag");
    let glsl = &e.source;
    // Oracle: the lane names are the program's own interface plan, not the fixture text.
    let lanes: Vec<String> = e
        .interface
        .uniform_groups
        .iter()
        .flat_map(|g| g.members.iter().map(|(n, _)| n.to_string()))
        .collect();
    assert!(lanes.contains(&"density".to_string()), "fixture must declare the collided lane, got: {lanes:?}\n{glsl}");
    assert!(glsl.contains("(density * 2.0)"), "the multiply must read the uniform lane:\n{glsl}");
    for lane in &lanes {
        assert_no_hoisted_shadow(glsl, lane);
    }
    assert!(
        glsl.contains("density_1 = (density * 2.0)"),
        "the renamed local must hold the doubled density:\n{glsl}"
    );
    assert!(
        glsl.contains("vec4(density_1,"),
        "every use of the local must follow the rename:\n{glsl}"
    );
}

/// Vertex stage: locals shadowing an attribute and a varying output.
#[test]
fn vertex_locals_renamed_apart_from_attribute_and_varying() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4, shade: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            let aPos = a.aPos.x + 1.0
            let shade = a.aPos.y + 2.0
            return Vary {
                position: Vec4 { x: aPos, y: shade, z: 0.0, w: 1.0 },
                shade: Vec4 { x: shade, y: 0.0, z: 0.0, w: 1.0 }
            }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return vary.shade }
    "#;
    let e = emit_pipeline(src, "vt", "fr");
    let glsl = &e.source;
    let attrs: Vec<String> = e
        .interface
        .vertex_inputs
        .iter()
        .flat_map(|i| i.attrs.iter().map(|(n, _)| n.to_string()))
        .collect();
    assert!(attrs.contains(&"aPos".to_string()), "fixture must declare the collided attribute, got: {attrs:?}\n{glsl}");
    let varyings: Vec<String> = e.interface.varyings.iter().map(|v| v.name.to_string()).collect();
    assert!(varyings.contains(&"shade".to_string()), "fixture must declare the collided varying, got: {varyings:?}\n{glsl}");
    assert!(glsl.contains("(aPos.x + 1.0)"), "the attribute read must survive:\n{glsl}");
    assert!(glsl.contains("shade = "), "the varying assignment must survive:\n{glsl}");
    for name in attrs.iter().chain(varyings.iter()) {
        assert_no_hoisted_shadow(glsl, name);
    }
    assert!(glsl.contains("aPos_1 = (aPos.x + 1.0)"), "the renamed local must hold the attribute sum:\n{glsl}");
    assert!(glsl.contains("shade_1 = (aPos.y + 2.0)"), "the renamed local must hold the varying sum:\n{glsl}");
}

/// Fragment stage: a local shadowing a `@textures` group sampler. The group
/// field read renders as a bare global, so the local must move (the m1-arrival
/// `index` instance).
#[test]
fn fragment_local_renamed_apart_from_group_sampler() {
    let src = r#"
        struct T { tint: Sampler2D }
        value QV { aPos: Vec2 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            return Vary {
                position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 },
                uv: a.aPos
            }
        }
        @shader(fragment)
        fn fr(vary: Vary, @textures t: T) -> Vec4 {
            let tint = vary.uv.x + 1.0
            let c = texture(t.tint, vary.uv)
            return Vec4 { x: tint, y: c.y, z: c.z, w: c.w }
        }
    "#;
    let e = emit_pipeline(src, "vt", "fr");
    let glsl = &e.source;
    let samplers: Vec<String> = e
        .interface
        .textures
        .iter()
        .flat_map(|t| t.samplers.iter().map(|(n, _)| n.to_string()))
        .collect();
    assert!(samplers.contains(&"tint".to_string()), "fixture must declare the collided sampler, got: {samplers:?}\n{glsl}");
    assert!(glsl.contains("uniform sampler2D tint;"), "the sampler declaration must survive:\n{glsl}");
    assert!(glsl.contains("texture(tint, uv)"), "the sample must read the sampler global:\n{glsl}");
    for name in &samplers {
        assert_no_hoisted_shadow(glsl, name);
    }
    assert!(glsl.contains("tint_1 = (uv.x + 1.0)"), "the renamed local must hold the sum:\n{glsl}");
}
