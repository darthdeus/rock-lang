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

cases! { reject:
    oversized_array("@size(1025) big: UniformArray<Vec4>", "return at(big, 0)", "16384");
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
