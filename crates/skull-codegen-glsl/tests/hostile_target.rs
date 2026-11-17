use crate::support::pipeline_emission;

#[test]
fn grouped_sampler_is_declared_in_each_stage_that_uses_it() {
    let src = r#"
        value QuadVert { pos: Vec2, texcoord: Vec2 }
        struct Tex { height: Sampler2D }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn tv(@attr a: QuadVert, @textures t: Tex) -> Vary {
            let h = texture(t.height, a.texcoord)
            return Vary { position: v(a.pos.x, a.pos.y + h.r, 0.0, 1.0), uv: a.texcoord }
        }
        @shader(fragment)
        fn tf(vary: Vary, @textures t: Tex) -> Vec4 { return texture(t.height, vary.uv) }
    "#;
    let glsl = pipeline_emission(src, "tv", "tf").expect("emit").source;
    let vertex = glsl
        .split("#ifdef VERTEX")
        .nth(1)
        .expect("vertex stage")
        .split("#endif")
        .next()
        .expect("end of vertex stage");
    let pixel = glsl.split("#ifdef PIXEL").nth(1).expect("fragment stage");
    assert!(vertex.contains("uniform sampler2D height;"), "{vertex}");
    assert!(pixel.contains("uniform sampler2D height;"), "{pixel}");
}
