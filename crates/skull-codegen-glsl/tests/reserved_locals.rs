//! Lexical keywords are distinct from uniform shadowing: even an unused
//! keyword binding is invalid GLSL. Drive entry and helper allocation.

use crate::support::checked_workspace;
use skull_codegen_glsl::GlslProfile;

const SOURCE: &str = r#"
fn keyword_helper(sample: float, packed: float, resource: float) -> float {
    return sample * packed + resource
}

@shader(fragment)
fn local_frag(tc: Vec2) -> Vec4 {
    let sample_1 = tc.y + 2.0
    let sample = tc.x + 1.0
    return Vec4 { x: sample, y: sample_1, z: 0.0, w: 1.0 }
}

@shader(fragment)
fn param_frag(tc: Vec2) -> Vec4 {
    return Vec4 { x: keyword_helper(tc.x, tc.y, 2.0), y: tc.y, z: 0.0, w: 1.0 }
}

fn register() {
    let a = rock_shader(local_frag)
    let b = rock_shader(param_frag)
}
"#;

fn emit(root: &str, profile: GlslProfile) -> String {
    let (ws, mid) = checked_workspace(SOURCE);
    let emission = crate::support::emit_program(&ws, mid, None, root, profile).expect("emit");
    emission.source
}

fn identifiers(source: &str) -> impl Iterator<Item = &str> {
    source
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
}
