//! Profile-pass rejection tests: one per error class. The GLSL profile is strict
//! (reject, never skip-and-continue), so each of these must return an Err whose
//! message names the offending construct.

use crate::support::{emit_entry, emit_library_unchecked as emit};

fn reject(src: &str, roots: &[&str], needle: &str) {
    match emit(src, roots) {
        Ok(glsl) => panic!("expected rejection ({needle}) but emitted:\n{glsl}"),
        Err(msg) => assert!(
            msg.contains(needle),
            "rejection message should mention `{needle}`, got: {msg}"
        ),
    }
}

cases! { reject:
    reject_unconstrained_generic_call(
        "fn make<T>() -> [T] { return [] } fn root() -> int { let xs = make(); return xs.len() }",
        &["root"], "unconstrained-type-var");

    reject_recursion(r#"
        @pure fn fact(n: float) -> float {
            return fact(n - 1.0)
        }
    "#, &["fact"], "recursion");

    reject_string_param(r#"
        @pure fn takes_string(s: string) -> float { return 1.0 }
    "#, &["takes_string"], "not tier 0");

    // A tier-0 signature but a string literal in the body.
    reject_unmapped_inline_lua_call(r#"
        @pure @inline inline(lua) fn myprim(x: float) -> float = "math.log(x)"
        @pure fn uses_prim(x: float) -> float { return myprim(x) }
    "#, &["uses_prim"], "no GLSL intrinsic mapping");
}

#[test]
fn vector_defaults_emit_as_glsl_arguments() {
    let src = r#"
        @pure fn vector_default(x: float, tint: Vec3 = v(0.125, 0.375, 0.625), gain: float = 0.875) -> Vec3 {
            return tint * (x + gain)
        }
        @pure fn supplied(x: float) -> Vec3 { return v(x, x + 1.0, x + 2.0) }
        @pure fn omitted(x: float) -> Vec3 { return vector_default(x) }
        @pure fn named(x: float) -> Vec3 { return vector_default(gain: 0.5, x: x) }
        @pure fn explicit(x: float) -> Vec3 { return vector_default(x, supplied(x)) }
    "#;
    let glsl = emit(src, &["omitted", "named", "explicit"]).expect("emit");
    for expected in [
        "return vector_default_float_Vec3_float(x, vec3(0.125, 0.375, 0.625), 0.875);",
        "return vector_default_float_Vec3_float(x, vec3(0.125, 0.375, 0.625), 0.5);",
        "return vector_default_float_Vec3_float(x, supplied_float(x), 0.875);",
    ] {
        assert!(glsl.contains(expected), "missing `{expected}`:\n{glsl}");
    }
    assert_eq!(glsl.matches("supplied_float(x)").count(), 1, "{glsl}");
    assert!(!glsl.contains("null"), "{glsl}");
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

cases! { reject:
}

// The tier-0 integer match in golden::match_int_ladder lowers to branches.
// The enum scrutinee here fails the tier-0 check before structuring.
cases! { reject:
    reject_unknown_root(r#"
        @pure fn present(x: float) -> float { return x }
    "#, &["absent"], "not found");
}

fn reject_entry(src: &str, root: &str, needle: &str) {
    let err = emit_entry(src, root).expect_err("must reject");
    assert!(
        err.contains(needle),
        "error `{err}` should contain `{needle}`"
    );
}

cases! { reject_entry:
    reject_entry_two_uniform_structs(r#"
        value Ua { a: float }
        value Ub { b: float }
        @shader(fragment)
        fn f(@uniform u1: Ua, @uniform u2: Ub, tc: Vec2) -> Vec4 { return Vec4 { x: u1.a, y: u2.b, z: 0.0, w: 1.0 } }
    "#, "f", "one uniform struct");

    /// A1 requires @uniform on the fragment uniform struct (one rule everywhere with
    /// the A2 pipeline). An unmarked value-struct param rejects, naming the fix.
    reject_entry_unmarked_uniform_struct(r#"
        value U { strength: float }
        @shader(fragment)
        fn f(u: U, tc: Vec2) -> Vec4 { return Vec4 { x: u.strength, y: tc.y, z: 0.0, w: 1.0 } }
    "#, "f", "@uniform");

    reject_entry_missing_tc(r#"
        @shader(fragment)
        fn f(scene: Sampler2D) -> Vec4 { return texture(scene, v(0.5, 0.5)) }
    "#, "f", "Vec2");

    reject_entry_two_tc(r#"
        @shader(fragment)
        fn f(a: Vec2, b: Vec2) -> Vec4 { return Vec4 { x: a.x, y: b.y, z: 0.0, w: 1.0 } }
    "#, "f", "exactly one");

    reject_entry_scalar_param(r#"
        @shader(fragment)
        fn f(strength: float, tc: Vec2) -> Vec4 { return Vec4 { x: strength, y: tc.y, z: 0.0, w: 1.0 } }
    "#, "f", "uniform struct");

    reject_entry_wrong_return(r#"
        @shader(fragment)
        fn f(tc: Vec2) -> float { return tc.x }
    "#, "f", "Vec4");

    reject_entry_uniform_name_collision(r#"
        value U { scene: float }
        @shader(fragment)
        fn f(@uniform u: U, scene: Sampler2D, tc: Vec2) -> Vec4 { return texture(scene, tc) }
    "#, "f", "collide");

}

#[test]
fn unconstrained_entry_call_reports_profile_lowering_context() {
    let source = "fn make<T>() -> [T] { return [] } @shader(fragment) fn root(tc: Vec2) -> Vec4 { let xs = make(); let n = xs.len(); return v(n as float, 0.0, 0.0, 1.0) }";
    let error = emit_entry(source, "root").expect_err("unconstrained call cannot lower");
    assert!(error.contains("cannot lower for GLSL"), "{error}");
}
