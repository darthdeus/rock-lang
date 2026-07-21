//! Profile-pass rejection tests: one per error class. The GLSL profile is strict
//! (reject, never skip-and-continue), so each of these must return an Err whose
//! message names the offending construct.

use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn emit(src: &str, roots: &[&str]) -> Result<String, String> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let roots: Vec<String> = roots.iter().map(|s| s.to_string()).collect();
    skull_codegen_glsl::emit_shader_library(&ws, mid, &roots).map_err(|e| e.message)
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

// --- A1 entry-contract rejects (docs/shaders.md) ------------------------

fn reject_entry(src: &str, root: &str, needle: &str) {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, ustr::Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("module id");
    let err = skull_codegen_glsl::emit_shader_entry(&ws, mid, root, skull_codegen_glsl::GlslProfile::Core330)
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

// `pass_uniforms` (docs/shaders.md) returns `[(string, <enum>)]` --
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
