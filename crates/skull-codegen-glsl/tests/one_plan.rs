//! **GLSL checking and emission consume ONE validated plan**.
//!
//! `check_entry_profile` / `check_pipeline_profile` exist so the mono shader
//! artifact view cannot admit what emission refuses. That guarantee is only
//! worth something if the two run the SAME sequence: a check that is a
//! re-implementation of emission's preparation is free to be weaker, and was --
//! it accepted a standalone `@shader(vertex)` entry the emitter rejects, and it
//! never reached the uniform-block, engine-group or texture-group planning that
//! produces real rejections.
//!
//! Every case here asserts the two answers are EQUAL, message included. A test
//! that only asserted "the check rejects" would pass against two different
//! diagnostics.

use crate::support::workspace;
use skull_codegen_glsl::GlslProfile;
use skull_mir::mir::FuncRef;

fn fref(
    ws: &bone_core::workspace::Workspace,
    mid: bone_core::prelude::ModuleId,
    name: &str,
) -> FuncRef {
    skull_codegen_glsl::resolve_root(ws, mid, name).expect("shader root")
}

/// The two answers for a single-entry program, as `Result<(), String>` so a
/// message difference is a failure and not a coincidence of both being `Err`.
fn entry_answers(src: &str, root: &str) -> (Result<(), String>, Result<(), String>) {
    let (ws, mid) = workspace(src);
    let checked =
        skull_codegen_glsl::check_entry_profile(&ws, fref(&ws, mid, root)).map_err(|e| e.message);
    let emitted = skull_codegen_glsl::emit_shader_entry(&ws, mid, root, GlslProfile::Core330)
        .map(|_| ())
        .map_err(|e| e.message);
    (checked, emitted)
}

fn pipeline_answers(src: &str, vert: &str, frag: &str) -> (Result<(), String>, Result<(), String>) {
    let (ws, mid) = workspace(src);
    let checked =
        skull_codegen_glsl::check_pipeline_profile(&ws, fref(&ws, mid, vert), fref(&ws, mid, frag))
            .map_err(|e| e.message);
    let emitted =
        skull_codegen_glsl::emit_shader_pipeline(&ws, mid, vert, frag, GlslProfile::Core330)
            .map(|_| ())
            .map_err(|e| e.message);
    (checked, emitted)
}

fn assert_agree(what: &str, (checked, emitted): (Result<(), String>, Result<(), String>)) {
    assert_eq!(
        checked, emitted,
        "{what}: the check and the emitter disagree (check={checked:?}, emit={emitted:?})"
    );
}

// --- the divergences that motivated one plan --------------------------------

const STANDALONE_VERTEX: &str = r#"
value QuadVert { aPos: Vec2 }
value PipeVary  { position: Vec4, TexCoord: Vec2 }

@shader(vertex)
fn lone_vert(@attr a: QuadVert) -> PipeVary {
    return PipeVary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos }
}
"#;

/// A `@shader(vertex)` entry compiled ALONE. `emit_shader_entry` refuses it
/// (the single-entry form is the fragment one; a vertex stage has meaning only
/// inside a pair), and the check accepted it -- so the artifact view could
/// admit a program emission cannot produce.
#[test]
fn a_standalone_vertex_entry_gets_one_answer() {
    let answers = entry_answers(STANDALONE_VERTEX, "lone_vert");
    assert!(
        answers.1.is_err(),
        "the emitter accepted a standalone vertex entry"
    );
    assert_agree("standalone vertex", answers);
}

/// The `@uniform` group's std140 plan is a REJECTION-PRODUCING step of
/// emission. A non-`value` group struct classifies fine (its fields are all
/// GLSL-typed) and fails at `uniform_block`, so a check that stopped before the
/// layout planning answered `Ok` for a program that cannot be emitted.
#[test]
fn an_unpackable_uniform_group_gets_one_answer() {
    let src = r#"
struct GcU { scale: float }

@shader(fragment)
fn bad_uniform_frag(uTexture: Sampler2D, @uniform u: GcU, tc: Vec2) -> Vec4 {
    let px = texture(uTexture, tc)
    return v(px.r * u.scale, 0.0, 0.0, 1.0)
}
"#;
    let answers = entry_answers(src, "bad_uniform_frag");
    assert!(
        answers.1.is_err(),
        "the emitter packed a non-value uniform group"
    );
    assert_agree("unpackable uniform group", answers);
}

/// A `@textures` group is PROGRAM-scoped: two stages naming different groups is
/// a rejection `program_texture_slots` produces, after both stages have already
/// lowered clean. The check never ran it.
#[test]
fn a_pipeline_texture_group_mismatch_gets_one_answer() {
    let src = r#"
value QuadVert { aPos: Vec2 }
value PipeVary  { position: Vec4, TexCoord: Vec2 }

struct VertTex { uHeight: Sampler2D }
struct FragTex { uAlbedo: Sampler2D }

@shader(vertex)
fn tex_vert(@attr a: QuadVert, @textures t: VertTex) -> PipeVary {
    let h = texture(t.uHeight, a.aPos)
    return PipeVary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: h.r, w: 1.0 }, TexCoord: a.aPos }
}

@shader(fragment)
fn tex_frag(vary: PipeVary, @textures t: FragTex) -> Vec4 {
    return texture(t.uAlbedo, vary.TexCoord)
}
"#;
    let answers = pipeline_answers(src, "tex_vert", "tex_frag");
    assert!(
        answers.1.is_err(),
        "the emitter accepted two different @textures groups"
    );
    assert_agree("pipeline texture-group mismatch", answers);
}

/// The `@engine` block is one per PROGRAM. Both paths already rejected this;
/// it stays here because the unified plan must not lose the check.
#[test]
fn a_pipeline_engine_group_mismatch_gets_one_answer() {
    let src = r#"
value QuadVert { aPos: Vec2 }
value PipeVary  { position: Vec4, TexCoord: Vec2 }
value VertEng   { uProjection: Mat4 }
value FragEng   { uTime: float }

@shader(vertex)
fn eng_vert(@attr a: QuadVert, @engine e: VertEng) -> PipeVary {
    let clip = e.uProjection * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
    return PipeVary { position: clip, TexCoord: a.aPos }
}

@shader(fragment)
fn eng_frag(vary: PipeVary, @engine e: FragEng) -> Vec4 {
    return v(e.uTime, vary.TexCoord.x, 0.0, 1.0)
}
"#;
    let answers = pipeline_answers(src, "eng_vert", "eng_frag");
    assert!(
        answers.1.is_err(),
        "the emitter accepted two different @engine groups"
    );
    assert_agree("pipeline engine-group mismatch", answers);
}

// --- the rejections the check already owned, kept covered --------------------

/// A helper whose SIGNATURE is tier 0 and whose BODY holds a string.
#[test]
fn a_string_body_helper_gets_one_answer() {
    let src = r#"
@pure fn stringy_body(x: float) -> float {
    let s = "not tier 0"
    return x
}

@shader(fragment)
fn body_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    let d = stringy_body(tc.x)
    return v(d, d, d, 1.0)
}
"#;
    let answers = entry_answers(src, "body_frag");
    assert!(
        answers.1.is_err(),
        "the emitter accepted a string-bodied helper"
    );
    assert_agree("string body helper", answers);
}

/// A helper with a foreign body and no GLSL mapping.
#[test]
fn a_foreign_helper_gets_one_answer() {
    let src = r#"
inline(lua) fn lua_only_helper(x: float) -> float = "return x * 2"

@shader(fragment)
fn foreign_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    let d = lua_only_helper(tc.x)
    return v(d, d, d, 1.0)
}
"#;
    let answers = entry_answers(src, "foreign_frag");
    assert!(
        answers.1.is_err(),
        "the emitter accepted a foreign-bodied helper"
    );
    assert_agree("foreign helper", answers);
}

/// Recursion: GLSL has no call stack.
#[test]
fn recursion_gets_one_answer() {
    let src = r#"
@pure fn recur(x: float) -> float {
    if x <= 0.0 { return 0.0 }
    return recur(x - 1.0)
}

@shader(fragment)
fn recur_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    let d = recur(tc.x)
    return v(d, d, d, 1.0)
}
"#;
    let answers = entry_answers(src, "recur_frag");
    assert!(answers.1.is_err(), "the emitter accepted shader recursion");
    assert_agree("recursion", answers);
}

/// A fragment-stage builtin used from the VERTEX stage of a pair.
#[test]
fn a_wrong_stage_builtin_gets_one_answer() {
    let src = r#"
value QuadVert { aPos: Vec2 }
value PipeVary  { position: Vec4, TexCoord: Vec2 }

@shader(vertex)
fn stage_vert(@attr a: QuadVert) -> PipeVary {
    let w = fwidth(a.aPos.x)
    return PipeVary { position: Vec4 { x: w, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos }
}

@shader(fragment)
fn stage_frag(vary: PipeVary) -> Vec4 {
    return v(vary.TexCoord.x, 0.0, 0.0, 1.0)
}
"#;
    let answers = pipeline_answers(src, "stage_vert", "stage_frag");
    assert!(
        answers.1.is_err(),
        "the emitter accepted a fragment builtin in the vertex stage"
    );
    assert_agree("wrong-stage builtin", answers);
}

/// The POSITIVE control: a real A1 entry and a real A2 pair are accepted by
/// both. Without it every assertion above could be satisfied by a check that
/// rejects everything.
#[test]
fn a_valid_entry_and_a_valid_pipeline_are_accepted_by_both() {
    let entry = r#"
@pure fn shader_helper(x: float) -> float { return x * 2.0 }

@shader(fragment)
fn good_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    let px = texture(uTexture, tc)
    let d = shader_helper(px.r)
    return v(d, d, d, 1.0)
}
"#;
    let answers = entry_answers(entry, "good_frag");
    assert_eq!(answers.0, Ok(()), "a valid entry was refused by the check");
    assert_agree("valid entry", answers);

    let pipeline = r#"
value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
value PipeU    { uProjection: Mat4 }
value PipeVary { position: Vec4, TexCoord: Vec2 }

struct PipeTex { uTexture: Sampler2D }

@shader(vertex)
fn good_vert(@attr a: QuadVert, @uniform u: PipeU) -> PipeVary {
    let clip = u.uProjection * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
    return PipeVary { position: clip, TexCoord: a.aTexCoord }
}

@shader(fragment)
fn good_pipe_frag(vary: PipeVary, @textures t: PipeTex) -> Vec4 {
    return texture(t.uTexture, vary.TexCoord)
}
"#;
    let answers = pipeline_answers(pipeline, "good_vert", "good_pipe_frag");
    assert_eq!(
        answers.0,
        Ok(()),
        "a valid A2 pipeline was refused by the check"
    );
    assert_agree("valid pipeline", answers);
}

/// The POSITIVE control for the relooper `Unreachable` rule: an exhaustive
/// boolean `match` expression with a real join inside a fragment entry. Both
/// the check and the emitter must accept it (GLSL used to reject the shape as
/// `unstructured control flow`), under BOTH shader profiles, with both arm
/// literals surviving in the emitted source.
#[test]
fn an_exhaustive_bool_match_entry_is_accepted_by_both_under_both_profiles() {
    let src = r#"
@shader(fragment)
fn bool_match_frag(tc: Vec2) -> Vec4 {
    let b = tc.x > 0.5
    let x = match b { true => 10, false => 20 }
    return v(float(x), 0.0, 0.0, 1.0)
}
"#;
    let answers = entry_answers(src, "bool_match_frag");
    assert_eq!(
        answers.0,
        Ok(()),
        "a valid bool-match entry was refused by the check"
    );
    assert_agree("exhaustive bool-match entry", answers);

    let profile = GlslProfile::Core330;
    let (ws, mid) = workspace(src);
    let out = skull_codegen_glsl::emit_shader_entry(&ws, mid, "bool_match_frag", profile)
        .expect("a valid bool-match entry must emit under every profile");
    for arm in ["= 10;", "= 20;"] {
        assert!(
            out.contains(arm),
            "arm `{arm}` must survive under {profile:?}; got:\n{out}"
        );
    }
}

/// The ES 3.0 block budget is enforced on the REAL entry path, and the check and
/// the emitter answer identically -- the whole point of one plan.
///
/// Population matters here: this repo's live blocks are all a few hundred bytes,
/// so a budget check could sit dead forever and look fine. `@uniform` groups get
/// far bigger the moment uniform ARRAYS move inside a block (backlog 4b), which
/// is what this guards. Both sides of the boundary are driven, because the
/// rejecting side alone cannot show the check admits anything.
#[test]
fn the_portable_block_budget_is_enforced_where_shaders_are_planned() {
    // Mat4 is 64 std140 bytes, the densest member a block can hold.
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

    let at_limit = entry_answers(&src(256), "big_frag");
    assert_eq!(at_limit.0, Ok(()), "16384 bytes is the budget, not over it");
    assert_agree("block exactly at the ES 3.0 budget", at_limit);

    let over = entry_answers(&src(257), "big_frag");
    let msg = over.0.clone().expect_err("16448 bytes must be refused");
    assert!(
        msg.contains("16448") && msg.contains("16384"),
        "the rejection must name the size and the budget, got: {msg}"
    );
    assert_agree("block over the ES 3.0 budget", over);
}

/// **A generic helper is checked and emitted at each INSTANTIATION.**
///
/// GLSL has no boxed representation, so a declared `T` is not a shape any
/// program can carry: the declaration-based pass rejected `passthrough` as
/// "not tier 0" and was right about the declaration and silent about the two
/// bodies the program actually needs. Under the instance substitutions both
/// are tier 0, and they are two GLSL functions -- `float` and `int` are not one
/// definition.
///
/// The DENOMINATOR is asserted: two distinct specialized definitions must
/// appear. A test that only asserted "it emits" would pass on one.
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
    assert_agree(
        "a generic helper at two instantiations",
        entry_answers(src, "generic_frag"),
    );

    let (ws, mid) = workspace(src);
    let out = skull_codegen_glsl::emit_shader_entry(&ws, mid, "generic_frag", GlslProfile::Core330)
        .expect("a concretely instantiated generic helper must emit");

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
    // Each definition's own name is what its CALL SITE spells -- a specialized
    // callee and its definition cannot disagree, because both come off the key
    // lowering recorded at the site.
    //
    // Asserted against the ENTRY's body, not against the whole file: a
    // definition line contains its own name, so `out.contains(name)` is true
    // for any name that was emitted at all and proves nothing about a call.
    let entry_body = {
        let start = out
            .find("vec4 generic_frag(")
            .expect("the entry definition");
        let rest = &out[start..];
        let end = rest.find("\n}\n").expect("the entry's closing brace");
        &rest[..end]
    };
    for d in &defs {
        let name = d
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
    // ...and the SHARED spelling appears nowhere: a bare `passthrough(` call
    // would mean a call site fell back to the declaration while the
    // definitions specialized.
    assert!(
        !out.contains(" passthrough(") && !out.contains("=passthrough("),
        "a call reached the unspecialized `passthrough`:\n{out}"
    );
    // CONTROL: the NON-generic helper keeps its ordinary mangled name with no
    // instantiation suffix, which is what makes a program with no generics
    // byte-identical to the pre-instance emission.
    assert!(out.contains("float concrete_helper_float(float "), "{out}");
    assert!(!out.contains("concrete_helper_float_spec"), "{out}");
}

/// **A generic helper bound to a non-tier-0 type is still rejected**, and the
/// rejection names the binding rather than the declaration. The nearest control
/// to the test above: it is the same helper, the same call shape, and only the
/// instantiation differs -- so "generics are accepted now" cannot pass here.
#[test]
fn a_generic_helper_bound_outside_tier_0_is_rejected_by_its_binding() {
    let src = r#"
@pure fn passthrough<T>(x: T) -> T { return x }
@shader(fragment)
fn bad_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    let s = passthrough("nope")
    return v(tc.x, 0.0, 0.0, 1.0)
}
fn main() { let a = rock_shader(bad_frag) }
"#;
    let (checked, emitted) = entry_answers(src, "bad_frag");
    assert_eq!(checked, emitted, "the check and the emitter disagreed");
    let msg = checked.expect_err("a string-bound generic helper must reject");
    assert!(
        msg.contains("string"),
        "the rejection does not name the BINDING that made it non-tier-0: {msg}"
    );
}
