use crate::common::{graph_for, ws_with_stdlib};
use skull::shader_artifact::{ShaderArtifactRequest, ShaderProgram, realize_shader};
use skull::target_projection::LUAJIT_DESKTOP;

#[test]
fn shader_validation_preserves_profile_diagnostics() {
    for (helper, message) in [
        (
            "@pure fn helper(x: float) -> float { if x <= 0.0 { return 0.0 }; return helper(x - 1.0) }",
            "recursion",
        ),
        (
            "@pure fn helper(x: float) -> float { let s = \"not tier 0\"; return x }",
            "string",
        ),
        (
            "inline(lua) fn helper(x: float) -> float = \"return x * 2\"",
            "no emittable body",
        ),
        ("@pure fn helper(x: float) -> float { return x * 2.0 }", ""),
    ] {
        let source = format!(
            r#"{helper}
@shader(fragment)
fn frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {{
    let d = helper(tc.x)
    return v(d, d, d, 1.0)
}}
fn main() {{ let key = rock_shader(frag) }}
"#
        );
        let ws = ws_with_stdlib(&source);
        let (graph, _) = graph_for(&ws);
        let programs: Vec<_> = skull_mir::lower::collect_shader_collection(&ws)
            .unwrap()
            .pairs
            .into_iter()
            .map(|p| ShaderProgram {
                vert: p.vert,
                frag: p.frag,
            })
            .collect();
        let view = realize_shader(
            &ws,
            &graph,
            &ShaderArtifactRequest {
                spec: LUAJIT_DESKTOP,
                programs: &programs,
            },
        )
        .unwrap();
        if message.is_empty() {
            assert!(view.rejections.is_empty());
        } else {
            let errors = view.render_rejections(&ws);
            assert_eq!(errors.len(), 1);
            assert!(errors[0].contains(message));
            assert!(!skull::instance_graph::is_spanless(
                &view.rejections[0].span
            ));
        }
    }
}
