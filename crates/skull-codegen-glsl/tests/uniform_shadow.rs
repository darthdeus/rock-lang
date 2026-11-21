//! Hoisted locals must not shadow shader interface names.

use crate::support::entry_emission;

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
    let e = entry_emission(src, "shadow_frag").expect("emit");
    let glsl = &e.source;
    // Oracle: the lane names are the program's own interface plan, not the fixture text.
    let lanes: Vec<String> = e
        .interface
        .uniform_groups
        .iter()
        .flat_map(|g| g.members.iter().map(|(n, _)| n.to_string()))
        .collect();
    assert!(
        lanes.contains(&"density".to_string()),
        "fixture must declare the collided lane, got: {lanes:?}\n{glsl}"
    );
    assert!(
        glsl.contains("(density * 2.0)"),
        "the multiply must read the uniform lane:\n{glsl}"
    );
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
