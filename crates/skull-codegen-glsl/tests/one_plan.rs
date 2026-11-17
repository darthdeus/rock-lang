//! Program planning is exercised through GLSL output and rejection diagnostics.

use crate::support::{emit_program, workspace};
use skull_codegen_glsl::{GlslProfile, ShaderEmission};

fn emit_entry(src: &str, root: &str) -> Result<ShaderEmission, String> {
    let (ws, mid) = workspace(src);
    emit_program(&ws, mid, None, root, GlslProfile::Core330).map_err(|e| e.message)
}

const STANDALONE_VERTEX: &str = r#"
value QuadVert { aPos: Vec2 }
value PipeVary  { position: Vec4, TexCoord: Vec2 }

@shader(vertex)
fn lone_vert(@attr a: QuadVert) -> PipeVary {
    return PipeVary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos }
}
"#;

#[test]
fn a_standalone_vertex_entry_is_rejected() {
    let err = emit_entry(STANDALONE_VERTEX, "lone_vert")
        .err()
        .expect("a vertex stage cannot be emitted as a standalone entry");
    assert!(
        err.contains("only @shader(fragment)"),
        "unexpected error: {err}"
    );
}

#[test]
fn an_unpackable_uniform_group_is_rejected() {
    let src = r#"
struct GcU { scale: float }

@shader(fragment)
fn bad_uniform_frag(uTexture: Sampler2D, @uniform u: GcU, tc: Vec2) -> Vec4 {
    let px = texture(uTexture, tc)
    return v(px.r * u.scale, 0.0, 0.0, 1.0)
}
"#;
    let err = emit_entry(src, "bad_uniform_frag")
        .err()
        .expect("a non-value uniform group rejects");
    assert!(
        err.contains("uniform block") && err.contains("`value` struct"),
        "unexpected error: {err}"
    );
}

#[test]
fn an_exhaustive_bool_match_entry_keeps_both_arms() {
    let src = r#"
@shader(fragment)
fn bool_match_frag(tc: Vec2) -> Vec4 {
    let b = tc.x > 0.5
    let x = match b { true => 10, false => 20 }
    return v(float(x), 0.0, 0.0, 1.0)
}
"#;
    let emission = emit_entry(src, "bool_match_frag")
        .expect("an exhaustive bool match with a real join must structure");
    for arm in ["= 10;", "= 20;"] {
        assert!(
            emission.source.contains(arm),
            "arm `{arm}` must survive; got:\n{}",
            emission.source
        );
    }
}

#[test]
fn the_portable_block_budget_is_enforced_during_emission() {
    let src = |n: usize| {
        let fields: Vec<String> = (0..n).map(|i| format!("m{i}: Mat4")).collect();
        format!(
            "value BigU {{ {} }}\n\n\
             @shader(fragment)\n\
             fn big_frag(@uniform u: BigU, tc: Vec2) -> Vec4 {{\n\
             \x20   let _ = tc\n\
             \x20   return u.m0 * Vec4 {{ x: 1.0, y: 0.0, z: 0.0, w: 1.0 }}\n\
             }}\n",
            fields.join(", ")
        )
    };

    let at_limit = emit_entry(&src(256), "big_frag").expect("16384 bytes is the budget");
    assert!(at_limit.source.contains("layout(std140) uniform BigU {"));

    let err = emit_entry(&src(257), "big_frag")
        .err()
        .expect("16448 bytes must be refused");
    assert!(
        err.contains("16448") && err.contains("16384"),
        "the rejection must name the size and budget, got: {err}"
    );
}

#[test]
fn a_generic_helper_is_emitted_once_per_instantiation() {
    let src = r#"
@pure fn passthrough<T>(x: T) -> T { return x }
@pure fn concrete_helper(x: float) -> float { return x * 2.0 }
@shader(fragment)
fn generic_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    let d = concrete_helper(passthrough(tc.x))
    let n = passthrough(3)
    return v(d, d, d, float(n))
}
fn main() { let a = rock_shader(generic_frag) }
"#;
    let out = emit_entry(src, "generic_frag")
        .expect("a concretely instantiated generic helper must emit")
        .source;

    let defs: Vec<&str> = out
        .lines()
        .filter(|l| l.starts_with("float passthrough") || l.starts_with("int passthrough"))
        .collect();
    assert_eq!(
        defs.len(),
        2,
        "the two instantiations did not become two definitions: {defs:?}\n{out}"
    );
    assert!(
        defs.iter().any(|d| d.starts_with("float passthrough")),
        "{defs:?}"
    );
    assert!(
        defs.iter().any(|d| d.starts_with("int passthrough")),
        "{defs:?}"
    );

    let entry_body = {
        let start = out
            .find("vec4 generic_frag(")
            .expect("the entry definition");
        let rest = &out[start..];
        let end = rest.find("\n}\n").expect("the entry's closing brace");
        &rest[..end]
    };
    for definition in &defs {
        let name = definition
            .split_whitespace()
            .nth(1)
            .unwrap()
            .split('(')
            .next()
            .unwrap();
        assert!(
            entry_body.contains(&format!("{name}(")),
            "the entry body calls no `{name}`:\n{entry_body}"
        );
    }
    assert!(
        !out.contains(" passthrough(") && !out.contains("=passthrough("),
        "a call reached the unspecialized `passthrough`:\n{out}"
    );
    assert!(out.contains("float concrete_helper_float(float "), "{out}");
    assert!(!out.contains("concrete_helper_float_spec"), "{out}");
}

#[test]
fn a_generic_helper_bound_outside_tier_zero_is_rejected_by_its_binding() {
    let src = r#"
@pure fn passthrough<T>(x: T) -> T { return x }
@shader(fragment)
fn bad_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    let s = passthrough("nope")
    return v(tc.x, 0.0, 0.0, 1.0)
}
fn main() { let a = rock_shader(bad_frag) }
"#;
    let err = emit_entry(src, "bad_frag")
        .err()
        .expect("a string-bound generic helper must reject");
    assert!(
        err.contains("string"),
        "the rejection does not name the binding that made it non-tier-0: {err}"
    );
}
