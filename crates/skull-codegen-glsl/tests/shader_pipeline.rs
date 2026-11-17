use crate::support::{assert_has, assert_lacks, emit_entry, emit_pipeline, workspace};
use skull_codegen_glsl::GlslProfile;

#[test]
fn vector_scalar_bounds_are_splatted() {
    for width in [2, 3, 4] {
        let components = &['x', 'y', 'z', 'w'][..width];
        let ctor = std::iter::repeat_n("tc.x", width)
            .collect::<Vec<_>>()
            .join(", ");
        let min = components
            .iter()
            .map(|c| format!("min(a.{c}, b)"))
            .collect::<Vec<_>>()
            .join(", ");
        let max = components
            .iter()
            .map(|c| format!("max(a.{c}, b)"))
            .collect::<Vec<_>>()
            .join(", ");
        let src = format!(
            r#"
            fn Vec{width}.min(a, b: float) -> Vec{width} {{ return v({min}) }}
            fn Vec{width}.max(a, b: float) -> Vec{width} {{ return v({max}) }}
            @shader(fragment)
            fn f(tc: Vec2) -> Vec4 {{
                let a = v({ctor})
                let b = a.clamp(0.0, 6.0)
                let c = a.min(6.0)
                let d = a.max(0.0)
                let e = a.clamp(a, a) + a.min(a) + a.max(a)
                return v(b.x + c.x + d.x + e.x, min(tc.x, 6.0), max(tc.x, 0.0), clamp(tc.x, 0.0, 6.0))
            }}
        "#
        );
        let (ws, mid) = workspace(&src);
        let profile = GlslProfile::Core330;
        let glsl = crate::support::emit_program(&ws, mid, None, "f", profile)
            .map(|e| e.source)
            .expect("emit");
        for expected in [
            format!("clamp(a, vec{width}(0.0), vec{width}(6.0))"),
            format!("vec{width} min_Vec{width}_float(vec{width} a, float b) {{"),
            format!("min_Vec{width}_float(a, 6.0)"),
            format!("max_Vec{width}_float(a, 0.0)"),
            "clamp(a, a, a)".to_string(),
            "min(a, a)".to_string(),
            "max(a, a)".to_string(),
            "min(tc.x, 6.0)".to_string(),
            "max(tc.x, 0.0)".to_string(),
            "clamp(tc.x, 0.0, 6.0)".to_string(),
        ] {
            assert!(glsl.contains(&expected), "missing {expected}:\n{glsl}");
        }
    }
}

fn derivative_maps(rock: &str, glsl_call: &str) {
    let src = format!(
        r#"
        @shader(fragment)
        fn f(tc: Vec2) -> Vec4 {{
            let w = {rock}(tc.x)
            return Vec4 {{ x: w, y: 0.0, z: 0.0, w: 1.0 }}
        }}
    "#
    );
    let glsl = emit_entry(&src, "f").expect("emit");
    assert_has(&glsl, &format!("{glsl_call}(tc.x)"));
}

cases! { derivative_maps:
    builtin_fwidth_maps("fwidth", "fwidth");
    builtin_dfdx_maps("dfdx", "dFdx");
    builtin_dfdy_maps("dfdy", "dFdy");
}

fn reject_pipeline(src: &str, vert: &str, frag: &str, needle: &str) {
    match emit_pipeline(src, vert, frag) {
        Ok(glsl) => panic!("expected rejection ({needle}) but emitted:\n{glsl}"),
        Err(msg) => assert!(
            msg.contains(needle),
            "rejection should mention `{needle}`, got: {msg}"
        ),
    }
}

fn reject_fragment_builtin_in_vertex(name: &str) {
    let src = format!(
        r#"
        value QV {{ aPos: Vec2 }}
        value Vary {{ position: Vec4 }}
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {{
            let w = {name}(a.aPos.x)
            return Vary {{ position: Vec4 {{ x: w, y: 0.0, z: 0.0, w: 1.0 }} }}
        }}
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 {{ return vary.position }}
    "#
    );
    reject_pipeline(&src, "vt", "fr", "fragment");
}

cases! { reject_pipeline:
    reject_varyings_missing_position(r#"
        value QV { aPos: Vec2 }
        value Vary { TexCoord: Vec2 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { TexCoord: a.aPos } }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return Vec4 { x: vary.TexCoord.x, y: 0.0, z: 0.0, w: 1.0 } }
    "#, "vt", "fr", "position");

    reject_fragment_reads_position(r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4, TexCoord: Vec2 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos } }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return vary.position }
    "#, "vt", "fr", "position");

    reject_varyings_type_mismatch(r#"
        value QV { aPos: Vec2 }
        value VaryA { position: Vec4, TexCoord: Vec2 }
        value VaryB { position: Vec4, Other: Vec2 }
        @shader(vertex)
        fn vt(@attr a: QV) -> VaryA { return VaryA { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos } }
        @shader(fragment)
        fn fr(vary: VaryB) -> Vec4 { return Vec4 { x: vary.Other.x, y: 0.0, z: 0.0, w: 1.0 } }
    "#, "vt", "fr", "varyings");

    reject_unmarked_vertex_struct_param(r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } } }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return vary.position }
    "#, "vt", "fr", "@attr");

    reject_int_varying_without_flat(r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4, kind: int }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, kind: 3 } }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 } }
    "#, "vt", "fr", "flat");

    reject_cross_stage_name_collision(r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        value VU { uShared: float }
        value FU { uShared: float }
        @shader(vertex)
        fn vt(@attr a: QV, @uniform u: VU) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: u.uShared, z: 0.0, w: 1.0 } } }
        @shader(fragment)
        fn fr(vary: Vary, @uniform u: FU) -> Vec4 { return Vec4 { x: u.uShared, y: 0.0, z: 0.0, w: 1.0 } }
    "#, "vt", "fr", "collide");

    reject_global_struct_param_alias(r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4, TexCoord: Vec2 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos } }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 {
            let v = vary
            return Vec4 { x: v.TexCoord.x, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#, "vt", "fr", "field-by-field");

    reject_bool_varying(r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4, ok: bool }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, ok: true } }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 } }
    "#, "vt", "fr", "bool");

    reject_bool_attribute(r#"
        value QV { aPos: Vec2, flag: bool }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } } }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 } }
    "#, "vt", "fr", "bool");
}

#[test]
fn bool_uniform_is_allowed() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        value FU { uEnabled: bool }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } } }
        @shader(fragment)
        fn fr(vary: Vary, @uniform u: FU) -> Vec4 {
            if u.uEnabled { return Vec4 { x: 1.0, y: 1.0, z: 1.0, w: 1.0 } }
            return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    let glsl = emit_pipeline(src, "vt", "fr").expect("emit");
    assert_has(&glsl, "layout(std140) uniform FU {\n    bool uEnabled;\n};");
}

const MRT_SRC: &str = r#"
    value QV   { aPos: Vec2, aEntityId: float }
    value Vary { position: Vec4, TexCoord: Vec2, @flat EntityId: float }
    value SpriteOut { FragColor: Vec4, FragId: Vec4 }
    @shader(vertex)
    fn vt(@attr a: QV) -> Vary {
        return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos, EntityId: a.aEntityId }
    }
    @shader(fragment)
    fn fr(vary: Vary, uTexture: Sampler2D) -> SpriteOut {
        let c = texture(uTexture, vary.TexCoord)
        return SpriteOut { FragColor: c, FragId: Vec4 { x: vary.EntityId, y: 0.0, z: 0.0, w: 1.0 } }
    }
"#;

cases! { reject_pipeline:
    reject_mrt_non_vec4_field(r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        value BadOut { FragColor: Vec4, depth: float }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } } }
        @shader(fragment)
        fn fr(vary: Vary) -> BadOut { return BadOut { FragColor: Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }, depth: 0.5 } }
    "#, "vt", "fr", "Vec4");

    reject_mrt_output_name_collision(r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        value FU { FragId: float }
        value Out2 { FragColor: Vec4, FragId: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } } }
        @shader(fragment)
        fn fr(vary: Vary, @uniform u: FU) -> Out2 {
            return Out2 { FragColor: Vec4 { x: u.FragId, y: 0.0, z: 0.0, w: 1.0 }, FragId: Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 } }
        }
    "#, "vt", "fr", "collide");
}

const FRAG_DEPTH_SRC: &str = r#"
    value QV   { aPos: Vec2, aDepth: float }
    value Vary { position: Vec4, TexCoord: Vec2, Depth: float }
    value DepthOut { FragColor: Vec4, FragDepth: float, FragId: Vec4 }
    @shader(vertex)
    fn vt(@attr a: QV) -> Vary {
        return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos, Depth: a.aDepth }
    }
    @shader(fragment)
    fn fr(vary: Vary, uTexture: Sampler2D) -> DepthOut {
        let c = texture(uTexture, vary.TexCoord)
        return DepthOut { FragColor: c, FragDepth: vary.Depth * 0.5, FragId: Vec4 { x: 1.0, y: 0.0, z: 0.0, w: 1.0 } }
    }
"#;

#[test]
fn frag_depth_field_writes_gl_frag_depth_without_an_out_location() {
    let glsl = emit_pipeline(FRAG_DEPTH_SRC, "vt", "fr").expect("emit");
    assert_has(&glsl, "layout(location = 0) out vec4 FragColor;");
    assert_has(&glsl, "layout(location = 1) out vec4 FragId;");
    assert!(
        !glsl.contains("layout(location = 2)"),
        "FragDepth must not take a location:\n{glsl}"
    );
    assert!(
        !glsl.contains("out float FragDepth"),
        "FragDepth is not an out:\n{glsl}"
    );
    assert_has(&glsl, "gl_FragDepth = (Depth * 0.5);");
    assert_eq!(
        glsl.matches("FragDepth = ").count(),
        1,
        "only gl_FragDepth is assigned:\n{glsl}"
    );
}

#[test]
fn reject_frag_depth_that_is_not_float() {
    let src = FRAG_DEPTH_SRC
        .replace("FragDepth: float", "FragDepth: Vec4")
        .replace(
            "FragDepth: vary.Depth * 0.5",
            "FragDepth: Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }",
        );
    reject_pipeline(&src, "vt", "fr", "`FragDepth` must be float");
}

#[test]
fn reject_frag_depth_without_a_colour_output() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        value DepthOnly { FragDepth: float }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } } }
        @shader(fragment)
        fn fr(vary: Vary) -> DepthOnly { return DepthOnly { FragDepth: 0.5 } }
    "#;
    reject_pipeline(src, "vt", "fr", "colour");
}

#[test]
fn instanced_locations_continue_across_structs() {
    let src = r#"
        value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
        value GrassInst { InstancePosition: Vec2, InstanceRotation: float, InstanceScale: float, InstanceHeight: float }
        value Vary { position: Vec4, TexCoord: Vec2, @flat y_pos: float }
        @shader(vertex)
        fn vt(@attr a: QuadVert, @instance inst: GrassInst) -> Vary {
            let bend = 0.0
            let vid = vertex_id() % 4
            if vid == 0 || vid == 1 {
                bend = 0.1
            }
            let px = inst.InstancePosition.x + a.aPos.x * inst.InstanceScale + bend
            let py = inst.InstancePosition.y + a.aPos.y * inst.InstanceHeight
            return Vary {
                position: Vec4 { x: px, y: py, z: 0.0, w: 1.0 },
                TexCoord: a.aTexCoord,
                y_pos: inst.InstancePosition.y,
            }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 {
            return Vec4 { x: vary.TexCoord.x, y: vary.y_pos, z: 0.0, w: 1.0 }
        }
    "#;
    let glsl = emit_pipeline(src, "vt", "fr").expect("emit");
    assert_has(&glsl, "layout (location = 0) in vec2 aPos;");
    assert_has(&glsl, "layout (location = 1) in vec2 aTexCoord;");
    assert_has(&glsl, "layout (location = 2) in vec2 InstancePosition;");
    assert_has(&glsl, "layout (location = 3) in float InstanceRotation;");
    assert_has(&glsl, "layout (location = 4) in float InstanceScale;");
    assert_has(&glsl, "layout (location = 5) in float InstanceHeight;");
    assert_has(&glsl, "flat out float y_pos;");
    assert_has(&glsl, "flat in float y_pos;");
    assert_has(&glsl, "gl_VertexID");
}

#[test]
fn uniform_array_vertex_decl_and_at() {
    let src = r#"
        value QV { aPos: Vec2 }
        value VU { entity_count: int }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV, @uniform u: VU, @size(32) entity_positions: UniformArray<Vec2>) -> Vary {
            let off = v(0.0, 0.0)
            for i in 0..min(u.entity_count, 32) {
                off = off + at(entity_positions, i)
            }
            return Vary { position: Vec4 { x: a.aPos.x + off.x, y: off.y, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 } }
    "#;
    let glsl = emit_pipeline(src, "vt", "fr").expect("emit");
    assert_has(&glsl, "entity_positions[");
    assert_lacks(&glsl, "at(");
    let line = glsl
        .lines()
        .find(|l| l.contains("= min(entity_count, 32);"))
        .expect("min bound assignment");
    let name = line.trim().split(" = ").next().unwrap();
    assert!(
        glsl.contains(&format!("int {name};")),
        "bound temp `{name}` must be int:\n{glsl}"
    );
}

cases! { reject_pipeline:
    reject_uniform_array_without_size(r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV, entity_positions: UniformArray<Vec2>) -> Vary {
            return Vary { position: Vec4 { x: at(entity_positions, 0).x, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 } }
    "#, "vt", "fr", "@size");

    reject_uniform_array_bad_elem(r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV, @size(4) mats: UniformArray<Mat4>) -> Vary {
            return Vary { position: Vec4 { x: a.aPos.x, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 } }
    "#, "vt", "fr", "element");
}

#[test]
fn reject_vertex_id_in_fragment_stage() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            return Vary { position: Vec4 { x: a.aPos.x, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 {
            let vid = vertex_id()
            return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    reject_pipeline(src, "vt", "fr", "vertex-stage builtin");
}

#[test]
fn reject_vertex_id_in_a1_entry() {
    let src = r#"
        @shader(fragment)
        fn f(tc: Vec2) -> Vec4 {
            let vid = vertex_id()
            return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    match emit_entry(src, "f") {
        Ok(glsl) => panic!("expected rejection but emitted:\n{glsl}"),
        Err(msg) => assert!(
            msg.contains("vertex-stage builtin"),
            "rejection should mention `vertex-stage builtin`, got: {msg}"
        ),
    }
}

#[test]
fn bvec_nonfinite_entry_golden() {
    let src = r#"
        @shader(fragment)
        fn nonfinite_frag(src_tex: Sampler2D, tc: Vec2) -> Vec4 {
            let c = texture(src_tex, tc)
            let bad = bvec3(
                isinf(c.r) || isnan(c.r),
                isinf(c.g) || isnan(c.g),
                isinf(c.b) || isnan(c.b),
            )
            if any(bad) {
                return v(to_vec3(bad), 1.0)
            } else if any(greater_than(c.rgb.abs(), v(60000.0, 60000.0, 60000.0))) {
                return v(1.0, 1.0, 0.0, 1.0)
            } else {
                let luma = c.rgb.dot(v(0.2126, 0.7152, 0.0722))
                let d = luma * 0.05
                return v(d, d, d, 1.0)
            }
        }
    "#;
    let glsl = emit_entry(src, "nonfinite_frag").expect("emit");
    assert!(glsl.contains("bvec3("), "bvec3 ctor inline:\n{glsl}");
    assert!(
        glsl.contains("isinf(") && glsl.contains("isnan("),
        "isinf/isnan:\n{glsl}"
    );
    assert!(glsl.contains("any("), "any reduction:\n{glsl}");
    assert!(glsl.contains("greaterThan("), "greaterThan:\n{glsl}");
    assert!(
        glsl.contains("vec3(bad)"),
        "to_vec3 bool->float cast:\n{glsl}"
    );
    assert!(
        !glsl.contains("bvec3__"),
        "no mangled bvec3 helper:\n{glsl}"
    );
}

cases! { reject_pipeline:
    reject_bvec_varying(r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4, flags: Bvec3 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            return Vary {
                position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 },
                flags: bvec3(true, false, false),
            }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return vary.position }
    "#, "vt", "fr", "bvec3");

    reject_bvec_attribute(r#"
        value QV { aPos: Vec2, flags: Bvec3 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return vary.position }
    "#, "vt", "fr", "bvec3");
}

const INT_MRT_SRC: &str = r#"
    value QV   { aPos: Vec2, aEntityId: float }
    value Vary { position: Vec4, TexCoord: Vec2, @flat EntityId: float }
    value IdOut { FragColor: Vec4, FragId: int }
    @shader(vertex)
    fn vt(@attr a: QV) -> Vary {
        return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos, EntityId: a.aEntityId }
    }
    @shader(fragment)
    fn fr(vary: Vary, uTexture: Sampler2D) -> IdOut {
        let c = texture(uTexture, vary.TexCoord)
        return IdOut { FragColor: c, FragId: vary.EntityId as int }
    }
"#;

#[test]
fn mrt_int_output_field() {
    let glsl = emit_pipeline(INT_MRT_SRC, "vt", "fr").expect("emit");
    assert_has(&glsl, "layout(location = 0) out vec4 FragColor;");
    assert_has(&glsl, "layout(location = 1) out int FragId;");
    assert_has(&glsl, "FragId = ");
}

#[test]
fn single_int_fragment_output() {
    let src = r#"
        value QV   { aPos: Vec2, aEntityId: float }
        value Vary { position: Vec4, @flat entity_id: float }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, entity_id: a.aEntityId }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> int {
            if vary.entity_id <= 0.0 { discard() }
            return vary.entity_id as int
        }
    "#;
    let glsl = emit_pipeline(src, "vt", "fr").expect("emit");
    assert_has(&glsl, "out int FragColor;");
    assert_has(&glsl, "FragColor = fr(");
}

const ISAMPLER_SRC: &str = r#"
    value QV   { aPos: Vec2 }
    value Vary { position: Vec4, TexCoord: Vec2 }
    @shader(vertex)
    fn vt(@attr a: QV) -> Vary {
        return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos }
    }
    @shader(fragment)
    fn fr(vary: Vary, ids: ISampler2D) -> Vec4 {
        let id = texture_id(ids, vary.TexCoord)
        let ts = texture_size(ids)
        if id <= 0 { discard() }
        return Vec4 { x: 1.0, y: ts.x, z: 0.0, w: 1.0 }
    }
"#;

#[test]
fn isampler_texture_id() {
    let glsl = emit_pipeline(ISAMPLER_SRC, "vt", "fr").expect("emit");
    assert_has(&glsl, "uniform highp isampler2D ids;");
    assert_has(&glsl, "texture(ids, TexCoord).r");
    assert_has(&glsl, "vec2(textureSize(ids, 0))");
}

#[test]
fn reject_isampler_return() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } } }
        @pure fn pick(s: ISampler2D) -> ISampler2D { return s }
        @shader(fragment)
        fn fr(vary: Vary, ids: ISampler2D) -> Vec4 {
            let s = pick(ids)
            return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    reject_pipeline(src, "vt", "fr", "sampler");
}

const OPAQUE_PRELUDE: &str = r#"
    struct Tex { a: Sampler2D, b: Sampler2D }
    value Quad { aPos: Vec2, aUv: Vec2 }
    value Vary { position: Vec4, uv: Vec2 }
    @shader(vertex)
    fn vs(@attr q: Quad) -> Vary {
        return Vary { position: v(q.aPos, 0.0, 1.0), uv: q.aUv }
    }
"#;

#[test]
fn reject_sampler_local_copy() {
    let src = format!(
        "{OPAQUE_PRELUDE}{}",
        r#"
        @shader(fragment)
        fn fs(vary: Vary, tex: Sampler2D) -> Vec4 {
            let s = tex
            return texture(s, vary.uv)
        }
    "#
    );
    reject_pipeline(&src, "vs", "fs", "local `s` holds a `sampler2D`");
}

#[test]
fn reject_sampler_param_select() {
    let src = format!(
        "{OPAQUE_PRELUDE}{}",
        r#"
        @shader(fragment)
        fn fs(vary: Vary, tex: Sampler2D, tex2: Sampler2D) -> Vec4 {
            return texture(if vary.uv.x > 0.5 { tex } else { tex2 }, vary.uv)
        }
    "#
    );
    reject_pipeline(&src, "vs", "fs", "samplers are opaque");
}

#[test]
fn reject_texture_field_select() {
    let src = format!(
        "{OPAQUE_PRELUDE}{}",
        r#"
        @shader(fragment)
        fn fs(vary: Vary, @textures t: Tex) -> Vec4 {
            return texture(if vary.uv.x > 0.5 { t.a } else { t.b }, vary.uv)
        }
    "#
    );
    reject_pipeline(
        &src,
        "vs",
        "fs",
        "`a` is not a value field of the shader interface",
    );
}

#[test]
fn every_attr_kind_emits_its_glsl_type() {
    let src = r#"
        value All { f: float, i: int, v2: Vec2, v3: Vec3, v4: Vec4 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: All) -> Vary {
            return Vary { position: a.v4 + v(a.v3, 0.0) * float(a.i) * a.f, uv: a.v2 }
        }
        @shader(fragment)
        fn fs(vary: Vary) -> Vec4 { return v(vary.uv, 0.0, 1.0) }
    "#;
    let glsl = emit_pipeline(src, "vs", "fs").expect("must emit");
    for (loc, ty, name) in [
        (0, "float", "f"),
        (1, "int", "i"),
        (2, "vec2", "v2"),
        (3, "vec3", "v3"),
        (4, "vec4", "v4"),
    ] {
        assert!(
            glsl.contains(&format!("layout (location = {loc}) in {ty} {name};")),
            "want `layout (location = {loc}) in {ty} {name};`:\n{glsl}"
        );
    }
}

#[test]
fn mat4_attribute_rejects_rather_than_misnumbering_what_follows() {
    let src = r#"
        value Bad { m: Mat4, tail: Vec2 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: Bad) -> Vary {
            return Vary { position: a.m * v(a.tail, 0.0, 1.0), uv: a.tail }
        }
        @shader(fragment)
        fn fs(vary: Vary) -> Vec4 { return v(vary.uv, 0.0, 1.0) }
    "#;
    match emit_pipeline(src, "vs", "fs") {
        Ok(glsl) => panic!("a Mat4 attribute must reject, but emitted:\n{glsl}"),
        Err(msg) => assert!(
            msg.contains("m") && msg.to_lowercase().contains("attribute"),
            "the error must name the offending attribute: {msg}"
        ),
    }
}

#[test]
fn textures_group_keeps_int_sampler_kind() {
    let src = r#"
        struct PickTex { color: Sampler2D, ids: ISampler2D }
        value Quad { aPos: Vec2, aUv: Vec2 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: Quad) -> Vary {
            return Vary { position: v(a.aPos, 0.0, 1.0), uv: a.aUv }
        }
        @shader(fragment)
        fn fs(vary: Vary, @textures t: PickTex) -> Vec4 {
            let id = texture_id(t.ids, vary.uv)
            let c = texture(t.color, vary.uv)
            return Vec4 { x: c.x, y: c.y, z: float(id), w: 1.0 }
        }
    "#;
    let glsl = emit_pipeline(src, "vs", "fs").expect("must emit");
    assert_has(&glsl, "uniform sampler2D color;");
    assert_has(&glsl, "isampler2D ids;");
}

cases! { reject_pipeline:
    textures_conflicting_role_rejects(r#"
        struct T { tex: Sampler2D }
        value Quad { aPos: Vec2, aUv: Vec2 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: Quad) -> Vary {
            return Vary { position: v(a.aPos, 0.0, 1.0), uv: a.aUv }
        }
        @shader(fragment)
        fn fs(vary: Vary, @uniform @textures t: T) -> Vec4 { return texture(t.tex, vary.uv) }
    "#, "vs", "fs", "conflicting role markers");

    textures_group_with_a_non_sampler_field_rejects(r#"
        struct T { tex: Sampler2D, scale: float }
        value Quad { aPos: Vec2, aUv: Vec2 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: Quad) -> Vary {
            return Vary { position: v(a.aPos, 0.0, 1.0), uv: a.aUv }
        }
        @shader(fragment)
        fn fs(vary: Vary, @textures t: T) -> Vec4 { return texture(t.tex, vary.uv) }
    "#, "vs", "fs", "scale");

    textures_group_plus_bare_sampler_rejects(r#"
        struct T { tex: Sampler2D }
        value Quad { aPos: Vec2, aUv: Vec2 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: Quad) -> Vary { return Vary { position: v(a.aPos, 0.0, 1.0), uv: a.aUv } }
        @shader(fragment)
        fn fs(vary: Vary, @textures t: T, loose: Sampler2D) -> Vec4 {
            let a = texture(t.tex, vary.uv)
            let b = texture(loose, vary.uv)
            return Vec4 { x: a.x, y: b.y, z: 0.0, w: 1.0 }
        }
    "#, "vs", "fs", "every sampler must join it");

    textures_groups_differing_across_stages_reject(r#"
        struct VT { vtex: Sampler2D }
        struct FT { ftex: Sampler2D }
        value Quad { aPos: Vec2, aUv: Vec2 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: Quad, @textures t: VT) -> Vary {
            let h = texture(t.vtex, a.aUv)
            return Vary { position: v(a.aPos, h.x, 1.0), uv: a.aUv }
        }
        @shader(fragment)
        fn fs(vary: Vary, @textures t: FT) -> Vec4 { return texture(t.ftex, vary.uv) }
    "#, "vs", "fs", "one program has one group");
}

const ENGINE_SRC: &str = r#"
    value QuadVert { aPos: Vec2, aTexCoord: Vec2, aColor: Vec4 }
    value EngineU  { uProjection: Mat4, uView: Mat4, uYBounds: Vec2, uTime: float }
    value GrassVertU { entity_count: int }
    value UiVary   { position: Vec4, TexCoord: Vec2, Color: Vec4 }
    value UiFragU  { uSize: Vec2, uRatio: float }

    @shader(vertex)
    fn e_vert(@attr a: QuadVert, @engine e: EngineU, @uniform u: GrassVertU) -> UiVary {
        let clip = e.uProjection * e.uView * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
        let k = clamp((a.aPos.y - e.uYBounds.x) / max(e.uYBounds.y - e.uYBounds.x, 0.0001), 0.0, 1.0)
        return UiVary {
            position: Vec4 { x: clip.x, y: clip.y, z: k + float(u.entity_count), w: clip.w },
            TexCoord: a.aTexCoord,
            Color: a.aColor,
        }
    }

    @shader(fragment)
    fn e_frag(vary: UiVary, @engine e: EngineU, @uniform u: UiFragU) -> Vec4 {
        return Vec4 { x: vary.Color.x, y: e.uTime, z: u.uRatio, w: vary.Color.w }
    }
"#;

#[test]
fn engine_block_coexists_across_stages() {
    let glsl = emit_pipeline(ENGINE_SRC, "e_vert", "e_frag").expect("emit");
    let glsl = glsl.as_str();
    let decl = "layout(std140) uniform EngineU {\n    mat4 uProjection;\n    mat4 uView;\n    vec2 uYBounds;\n    float uTime;\n};\n";
    assert_eq!(glsl.matches(decl).count(), 2, "one per stage:\n{glsl}");
    assert_has(glsl, "uProjection * uView");
    assert_lacks(glsl, "e.uProjection");
    assert_has(
        glsl,
        "layout(std140) uniform UiFragU {\n    vec2 uSize;\n    float uRatio;\n};\n",
    );
    assert_has(glsl, "layout(std140) uniform EngineU {");
    assert_has(
        glsl,
        "layout(std140) uniform GrassVertU {\n    int entity_count;\n};\n",
    );
    assert_lacks(glsl, "uniform mat4 uProjection;");
}

#[test]
fn engine_groups_must_agree_across_stages() {
    let src = ENGINE_SRC.replace(
        "fn e_frag(vary: UiVary, @engine e: EngineU",
        "fn e_frag(vary: UiVary, @engine e: UiFragU",
    );
    let e = emit_pipeline(&src, "e_vert", "e_frag").expect_err("must reject");
    assert!(e.contains("different @engine groups"), "got: {e}");
}

#[test]
fn textures_and_engine_conflict() {
    let src = r#"
        value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
        value EngineU  { uProjection: Mat4, uView: Mat4 }
        struct Tex     { atlas: Sampler2D }
        value Vary     { position: Vec4, TexCoord: Vec2 }

        @shader(vertex)
        fn t_vert(@attr a: QuadVert, @textures @engine t: Tex) -> Vary {
            return Vary {
                position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 },
                TexCoord: a.aTexCoord,
            }
        }

        @shader(fragment)
        fn t_frag(vary: Vary) -> Vec4 {
            return Vec4 { x: vary.TexCoord.x, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    let e = emit_pipeline(src, "t_vert", "t_frag").expect_err("must reject");
    assert!(e.contains("@textures"), "got: {e}");
}

fn engine_member_named(name: &str) -> String {
    format!(
        r#"
    value QuadVert  {{ aPos: Vec2, TexCoordAttr: Vec2 }}
    value EngineU   {{ uProjection: Mat4, {name}: float }}
    value Vary      {{ position: Vec4, TexCoord: Vec2 }}
    value FragU     {{ uTime: float }}
    value SpriteOut {{ FragColor: Vec4, FragId: Vec4 }}

    @shader(vertex)
    fn c_vert(@attr a: QuadVert, @engine e: EngineU) -> Vary {{
        return Vary {{
            position: e.uProjection * Vec4 {{ x: a.aPos.x, y: a.aPos.y, z: e.{name}, w: 1.0 }},
            TexCoord: a.TexCoordAttr,
        }}
    }}

    @shader(fragment)
    fn c_frag(vary: Vary, @uniform u: FragU, tex: Sampler2D, @size(4) tints: UniformArray<Vec4>) -> SpriteOut {{
        let color = texture(tex, vary.TexCoord) + at(tints, 0)
        return SpriteOut {{ FragColor: color, FragId: color }}
    }}
"#
    )
}

fn engine_member_collides(name: &str) {
    let e = emit_pipeline(&engine_member_named(name), "c_vert", "c_frag").unwrap_err();
    assert!(e.contains(&format!("`{name}` would collide")), "got: {e}");
}

cases! { engine_member_collides:
    engine_member_colliding_with_an_attribute_rejects("aPos");
    engine_member_colliding_with_a_varying_rejects("TexCoord");
    engine_member_colliding_with_a_sampler_rejects("tex");
    engine_member_colliding_with_a_uniform_array_rejects("tints");
    engine_member_colliding_with_an_mrt_output_rejects("FragId");
    engine_member_colliding_with_a_stage_block_member_rejects("uTime");
}

#[test]
fn engine_member_with_a_free_name_emits() {
    let glsl =
        emit_pipeline(&engine_member_named("uEngineTime"), "c_vert", "c_frag").expect("emit");
    assert_has(&glsl, "layout(std140) uniform EngineU {");
    assert_has(&glsl, "float uEngineTime;");
}

cases! { reject_fragment_builtin_in_vertex:
    reject_dfdx_in_vertex("dfdx");
    reject_dfdy_in_vertex("dfdy");
    reject_fwidth_in_vertex("fwidth");
}

#[test]
fn mrt_early_return_terminates_the_entry() {
    let source = MRT_SRC.replace(
        "let c = texture(uTexture, vary.TexCoord)",
        "if vary.EntityId > 0.0 { return SpriteOut { FragColor: Vec4 { x: 1.0, y: 0.0, z: 0.0, w: 1.0 }, FragId: Vec4 { x: 1.0, y: 0.0, z: 0.0, w: 1.0 } } }\nlet c = texture(uTexture, vary.TexCoord)",
    );
    let glsl = emit_pipeline(&source, "vt", "fr").expect("emit");
    let branch = glsl.split("if (").last().expect("conditional entry");
    let end = branch.find('}').expect("branch end");
    assert!(
        branch[..end].contains("return;"),
        "MRT branch must not fall through:\n{glsl}"
    );
}

fn sampler_group_src(n: usize) -> String {
    let fields: Vec<String> = (0..n).map(|i| format!("s{i}: Sampler2D")).collect();
    let reads: Vec<String> = (0..n)
        .map(|i| format!("acc += texture(t.s{i}, uv).x"))
        .collect();
    format!(
        r#"
        struct ManyTex {{ {} }}
        value Quad {{ aPos: Vec2, aUv: Vec2 }}
        value Vary {{ position: Vec4, uv: Vec2 }}
        @shader(vertex)
        fn many_vs(@attr a: Quad) -> Vary {{ return Vary {{ position: v(a.aPos, 0.0, 1.0), uv: a.aUv }} }}
        @shader(fragment)
        fn many_fs(vary: Vary, @textures t: ManyTex) -> Vec4 {{
            let uv = vary.uv
            let acc = 0.0
            {}
            return Vec4 {{ x: acc, y: 0.0, z: 0.0, w: 1.0 }}
        }}
        @shader(fragment)
        fn many_entry(@textures t: ManyTex, uv: Vec2) -> Vec4 {{
            let acc = 0.0
            {}
            return Vec4 {{ x: acc, y: 0.0, z: 0.0, w: 1.0 }}
        }}
    "#,
        fields.join(", "),
        reads.join("\n            "),
        reads.join("\n            ")
    )
}

#[test]
fn fragment_stage_at_sixteen_samplers_emits() {
    let src = sampler_group_src(16);
    emit_pipeline(&src, "many_vs", "many_fs").expect("16 samplers fit every GLSL target");
    emit_entry(&src, "many_entry").expect("16 samplers fit every GLSL target");
}

#[test]
fn fragment_stage_over_sixteen_samplers_rejects_naming_the_shader() {
    let src = sampler_group_src(17);
    let e = emit_pipeline(&src, "many_vs", "many_fs").expect_err("17 samplers must reject");
    assert!(
        e.contains("many_fs") && e.contains("17 samplers") && e.contains("16"),
        "got: {e}"
    );
    let e = emit_entry(&src, "many_entry").expect_err("17 samplers must reject");
    assert!(
        e.contains("many_entry") && e.contains("17 samplers") && e.contains("16"),
        "got: {e}"
    );
}
