use crate::support::{assert_has, emit_pipeline, pipeline_emission};

fn fragment_arrays(params: &str, body: &str) -> String {
    format!(
        r#"
        value QV {{ aPos: Vec2 }}
        value Vary {{ position: Vec4 }}
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {{ return Vary {{ position: v(a.aPos, 0.0, 1.0) }} }}
        @shader(fragment)
        fn fr(vary: Vary, {params}) -> Vec4 {{ {body} }}
    "#
    )
}

fn emit(src: &str) -> String {
    pipeline_emission(src, "vt", "fr").expect("emit").source
}

#[test]
fn admitted_array_kinds_emit_one_std140_block() {
    let src = fragment_arrays(
        "@size(3) fs: UniformArray<float>, @size(5) is: UniformArray<int>, @size(2) v2s: UniformArray<Vec2>, @size(4) v4s: UniformArray<Vec4>",
        "let f = at(fs, 0) let i = at(is, 0) let p = at(v2s, 0) let c = at(v4s, 0) return Vec4 { x: f + (i as float) + p.x, y: c.y, z: 0.0, w: 1.0 }",
    );
    let glsl = emit(&src);
    assert_has(&glsl, "layout(std140) uniform fr_Arrays {");
    for member in ["float fs[3];", "int is[5];", "vec2 v2s[2];", "vec4 v4s[4];"] {
        assert_has(&glsl, member);
    }
    assert_has(&glsl, "v2s[");
    assert!(!glsl.contains(" at("), "{glsl}");
}

cases! { reject:
    uint_array("@size(4) masks: UniformArray<uint>", "let m = at(masks, 0) return v((m as int) as float, 0.0, 0.0, 1.0)", "`uint`");
    oversized_array("@size(1025) big: UniformArray<Vec4>", "return at(big, 0)", "16384");
    zero_capacity("@size(0) empty: UniformArray<Vec4>", "return at(empty, 0)", "capacity 0");
}

#[test]
fn array_block_name_cannot_collide_with_a_uniform_group() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        value fr_Arrays { scale: float }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: v(a.aPos, 0.0, 1.0) } }
        @shader(fragment)
        fn fr(vary: Vary, @uniform u: fr_Arrays, @size(8) points: UniformArray<Vec2>) -> Vec4 {
            let p = at(points, 0)
            return v(p.x * u.scale, p.y, 0.0, 1.0)
        }
    "#;
    let err = emit_pipeline(src, "vt", "fr").expect_err("must reject");
    assert!(err.contains("fr_Arrays"), "{err}");
}

fn reject(params: &str, body: &str, needle: &str) {
    let src = fragment_arrays(params, body);
    let err = emit_pipeline(&src, "vt", "fr").expect_err("must reject");
    assert!(err.contains(needle), "{err}");
}
