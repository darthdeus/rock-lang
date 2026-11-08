//! Profile-pass rejection tests: one per error class. The GLSL profile is strict
//! (reject, never skip-and-continue), so each of these must return an Err whose
//! message names the offending construct.

use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn emit(src: &str, roots: &[&str]) -> Result<String, String> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let roots: Vec<String> = roots.iter().map(|s| s.to_string()).collect();
    let root_refs = roots
        .iter()
        .map(|root| skull_codegen_glsl::resolve_root(&ws, mid, root))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.message)?;
    let session =
        skull_codegen_glsl::shader_library_session(&ws, root_refs).map_err(|e| e.message)?;
    skull_codegen_glsl::emit_shader_library_with_view(&ws, &session.scx, &session.view)
        .map_err(|e| e.message)
}

fn reject(src: &str, roots: &[&str], needle: &str) {
    match emit(src, roots) {
        Ok(glsl) => panic!("expected rejection ({needle}) but emitted:\n{glsl}"),
        Err(msg) => assert!(
            msg.contains(needle),
            "rejection message should mention `{needle}`, got: {msg}"
        ),
    }
}

#[test]
fn reject_recursion() {
    let src = r#"
        @pure fn fact(n: float) -> float {
            return fact(n - 1.0)
        }
    "#;
    reject(src, &["fact"], "recursion");
}

#[test]
fn reject_string_param() {
    let src = r#"
        @pure fn takes_string(s: string) -> float { return 1.0 }
    "#;
    reject(src, &["takes_string"], "not tier 0");
}

#[test]
fn reject_vec_return() {
    let src = r#"
        fn makes_vec(n: int) -> [int] { return [n] }
    "#;
    reject(src, &["makes_vec"], "not tier 0");
}

#[test]
fn reject_string_body() {
    // A tier-0 signature but a string literal in the body.
    let src = r#"
        fn has_string(x: float) -> float {
            let s = "hello"
            return x
        }
    "#;
    reject(src, &["has_string"], "string literal");
}

#[test]
fn reject_unmapped_inline_lua_call() {
    let src = r#"
        @pure @inline inline(lua) fn myprim(x: float) -> float = "math.log(x)"
        @pure fn uses_prim(x: float) -> float { return myprim(x) }
    "#;
    reject(src, &["uses_prim"], "no GLSL intrinsic mapping");
}

#[test]
fn stdlib_log2_is_available_in_shader_forms() {
    let src = r#"
        @pure fn shader_log2(x: float) -> float { return log2(x) }
    "#;
    let glsl = emit(src, &["shader_log2"])
        .unwrap_or_else(|msg| panic!("stdlib log2 should have a GLSL form: {msg}"));
    assert!(
        glsl.contains("log2(x)"),
        "GLSL should use the log2 intrinsic: {glsl}"
    );
}

#[test]
fn omitted_default_argument_is_available_in_shader_forms() {
    let src = r#"
        @pure fn with_default(x: float, offset: float = 0.25) -> float {
            return x + offset
        }
        @pure fn shader_default(x: float) -> float { return with_default(x) }
    "#;
    let glsl = emit(src, &["shader_default"])
        .unwrap_or_else(|msg| panic!("omitted source default should lower for GLSL: {msg}"));
    let caller = glsl
        .split("float shader_default_float(float x) {")
        .nth(1)
        .expect("the shader root must be emitted")
        .split('}')
        .next()
        .expect("the shader root body must be closed");
    assert!(
        caller.contains("with_default_float_float(x,") && !caller.contains("null"),
        "omitted source argument should produce a concrete GLSL argument: {caller}"
    );
}

fn shader_body<'a>(glsl: &'a str, root: &str) -> &'a str {
    glsl.split(&format!("vec3 {root}_float(float x) {{"))
        .nth(1)
        .unwrap_or_else(|| panic!("missing shader root {root}: {glsl}"))
        .split('}')
        .next()
        .expect("shader body")
}

fn vector_argument(glsl: &str, root: &str) -> Vec<f64> {
    let caller = shader_body(glsl, root);
    let call = "vector_default_float_Vec3_float(";
    assert_eq!(caller.matches(call).count(), 1, "one direct call: {caller}");
    let args = caller.split(call).nth(1).expect("call arguments");
    assert!(
        args.starts_with("x, vec3("),
        "vector must be the tint argument: {caller}"
    );
    args.split("vec3(")
        .nth(1)
        .expect("vector argument")
        .split(')')
        .next()
        .expect("vector components")
        .split(',')
        .map(|v| v.trim().parse().expect("constant component"))
        .collect()
}

#[test]
fn omitted_vector_default_argument_is_available_in_shader_forms() {
    let declared = [0.125, 0.375, 0.625];
    let override_value = [0.25, 0.5, 0.75];
    let [x, y, z] = declared;
    for default in [
        format!("v({x}, {y}, {z})"),
        format!("v(z: {z}, x: {x}, y: {y})"),
        format!("Vec3 {{ z: {z}, x: {x}, y: {y} }}"),
    ] {
        let [ox, oy, oz] = override_value;
        let src = format!(
            r#"
            @pure fn vector_default(x: float, tint: Vec3 = {default}, gain: float = 0.875) -> Vec3 {{
                return tint * (x + gain)
            }}
            @pure fn shader_vector_default(x: float) -> Vec3 {{
                let tint = v(9.0, 8.0, 7.0)
                return vector_default(x)
            }}
            @pure fn shader_vector_named(x: float) -> Vec3 {{
                return vector_default(gain: 0.5, x: x)
            }}
            @pure fn shader_vector_explicit(x: float) -> Vec3 {{
                return vector_default(x, v({x}, {y}, {z}))
            }}
            @pure fn shader_vector_override(x: float) -> Vec3 {{
                return vector_default(tint: v({ox}, {oy}, {oz}), x: x)
            }}
            @pure fn supplied_vector(x: float) -> Vec3 {{ return v(x, x + 1.0, x + 2.0) }}
            @pure fn shader_vector_evaluated(x: float) -> Vec3 {{
                return vector_default(x, supplied_vector(x))
            }}
        "#
        );
        let glsl = emit(
            &src,
            &[
                "shader_vector_default",
                "shader_vector_named",
                "shader_vector_explicit",
                "shader_vector_override",
                "shader_vector_evaluated",
            ],
        )
        .unwrap_or_else(|msg| {
            panic!("omitted vector default {default} should lower for GLSL: {msg}")
        });
        let explicit = vector_argument(&glsl, "shader_vector_explicit");
        assert_eq!(
            explicit, declared,
            "explicit control must use the declared components"
        );
        assert_eq!(vector_argument(&glsl, "shader_vector_default"), explicit);
        assert_eq!(vector_argument(&glsl, "shader_vector_named"), explicit);
        assert!(
            shader_body(&glsl, "shader_vector_named").contains(", 0.5)"),
            "named gain must retain its slot: {glsl}"
        );
        assert_eq!(
            vector_argument(&glsl, "shader_vector_override"),
            override_value
        );
        let evaluated = shader_body(&glsl, "shader_vector_evaluated");
        assert_eq!(
            evaluated.matches("supplied_vector_float(x)").count(),
            1,
            "an explicit expression is evaluated exactly once: {evaluated}"
        );
        assert!(
            !evaluated.contains("vec3("),
            "explicit expressions must not acquire default components: {evaluated}"
        );
    }
}

#[test]
fn reject_omitted_nonconstant_vector_default_argument() {
    for (default, diagnostic) in [
        ("v(x, 0.375, 0.625)", "no concrete plan"),
        ("v(0.125 + 0.25, 0.375, 0.625)", "null literal"),
    ] {
        let src = format!(
            r#"
            @pure fn vector_default(x: float, tint: Vec3 = {default}) -> Vec3 {{ return tint * x }}
            @pure fn shader_vector_default(x: float) -> Vec3 {{ return vector_default(x) }}
        "#
        );
        reject(&src, &["shader_vector_default"], diagnostic);
    }
}

#[test]
fn reject_omitted_effectful_vector_constructor_default() {
    let src = r#"
        global calls: int = 0
        fn make_tint(x: float, y: float, z: float) -> Vec3 {
            calls = calls + 1
            return Vec3 { x: x, y: y, z: z }
        }
        fn vector_default(x: float, tint: Vec3 = make_tint(0.125, 0.375, 0.625)) -> Vec3 { return tint * x }
        fn shader_vector_default(x: float) -> Vec3 { return vector_default(x) }
    "#;
    reject(src, &["shader_vector_default"], "global read");
}

#[test]
fn reject_omitted_expression_default_argument() {
    let src = r#"
        @pure fn calculated_default(x: float, offset: float = 0.25 + 0.5) -> float {
            return x + offset
        }
        @pure fn shader_expression_default(x: float) -> float {
            return calculated_default(x)
        }
    "#;
    reject(src, &["shader_expression_default"], "null literal");
}

// `match` lowers to `Term::Branch` chains (NOT `Term::Switch`, which lowering
// never produces), so a tier-0 int `match` is accepted and structured -- see
// `golden::match_int_ladder`. A match on a non-tier-0 scrutinee still rejects at
// the tier-0 gate; the structurizer never silently swallows it.
#[test]
fn reject_match_non_tier0() {
    let src = r#"
        enum Dir { North, South }
        @pure fn f(d: Dir) -> int { match d { Dir::North => { return 1 } Dir::South => { return 2 } } }
    "#;
    reject(src, &["f"], "not tier 0");
}

#[test]
fn reject_unknown_root() {
    let src = r#"
        @pure fn present(x: float) -> float { return x }
    "#;
    reject(src, &["absent"], "not found");
}

// --- A1 entry-contract rejects ------------------------

fn reject_entry(src: &str, root: &str, needle: &str) {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("module id");
    let err = skull_codegen_glsl::emit_shader_entry(
        &ws,
        mid,
        root,
        skull_codegen_glsl::GlslProfile::Core330,
    )
    .expect_err("must reject");
    assert!(
        err.message.contains(needle),
        "error `{}` should contain `{needle}`",
        err.message
    );
}

#[test]
fn reject_entry_two_uniform_structs() {
    let src = r#"
        value Ua { a: float }
        value Ub { b: float }
        @shader(fragment)
        fn f(@uniform u1: Ua, @uniform u2: Ub, tc: Vec2) -> Vec4 { return Vec4 { x: u1.a, y: u2.b, z: 0.0, w: 1.0 } }
    "#;
    reject_entry(src, "f", "one uniform struct");
}

/// A1 requires @uniform on the fragment uniform struct (one rule everywhere with
/// the A2 pipeline). An unmarked value-struct param rejects, naming the fix.
#[test]
fn reject_entry_unmarked_uniform_struct() {
    let src = r#"
        value U { strength: float }
        @shader(fragment)
        fn f(u: U, tc: Vec2) -> Vec4 { return Vec4 { x: u.strength, y: tc.y, z: 0.0, w: 1.0 } }
    "#;
    reject_entry(src, "f", "@uniform");
}

#[test]
fn reject_entry_missing_tc() {
    let src = r#"
        @shader(fragment)
        fn f(scene: Sampler2D) -> Vec4 { return texture(scene, v(0.5, 0.5)) }
    "#;
    reject_entry(src, "f", "Vec2");
}

#[test]
fn reject_entry_two_tc() {
    let src = r#"
        @shader(fragment)
        fn f(a: Vec2, b: Vec2) -> Vec4 { return Vec4 { x: a.x, y: b.y, z: 0.0, w: 1.0 } }
    "#;
    reject_entry(src, "f", "exactly one");
}

#[test]
fn reject_entry_scalar_param() {
    let src = r#"
        @shader(fragment)
        fn f(strength: float, tc: Vec2) -> Vec4 { return Vec4 { x: strength, y: tc.y, z: 0.0, w: 1.0 } }
    "#;
    reject_entry(src, "f", "uniform struct");
}

#[test]
fn reject_entry_wrong_return() {
    let src = r#"
        @shader(fragment)
        fn f(tc: Vec2) -> float { return tc.x }
    "#;
    reject_entry(src, "f", "Vec4");
}

#[test]
fn reject_entry_uniform_name_collision() {
    let src = r#"
        value U { scene: float }
        @shader(fragment)
        fn f(@uniform u: U, scene: Sampler2D, tc: Vec2) -> Vec4 { return texture(scene, tc) }
    "#;
    reject_entry(src, "f", "collide");
}

#[test]
fn reject_entry_not_fragment() {
    let src = r#"
        @shader(vertex)
        fn f(tc: Vec2) -> Vec4 { return Vec4 { x: tc.x, y: tc.y, z: 0.0, w: 1.0 } }
    "#;
    reject_entry(src, "f", "fragment");
}

// `pass_uniforms` returns `[(string, <enum>)]` --
// a CPU-side value, not a GPU-expressible type. The GLSL profile rejects it as
// non-tier-0 before any expansion, so the intrinsic can never leak into shader
// source (its `@struct_expand` annotation is inert here).
#[test]
fn reject_struct_expand_pairs_in_shader() {
    let src = r#"
        value SU { a: float }
        enum SE { A(float) }
        @struct_expand inline(lua) fn su_pairs<T>(u: T) -> [(string, SE)] = """
            error("expanded")
        """
        @pure fn shader_uses_pairs(u: SU) -> [(string, SE)] {
            return su_pairs(u)
        }
    "#;
    reject(src, &["shader_uses_pairs"], "not tier 0");
}
