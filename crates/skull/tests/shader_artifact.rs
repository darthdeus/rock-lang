//! The GLSL ARTIFACT VIEW.
//!
//! The shader half of an artifact: what the GPU side of one target packages,
//! taken over the SAME target-neutral instance graph the CPU view realizes.
//! Three properties, each with its own control:
//!
//! - the profile is the TYPED value carried on `TargetSpec`'s shader axis, not
//!   a constant the emitter picks;
//! - retention is the shader entries and the helpers they actually reach, not
//!   every declaration that could compile;
//! - GLSL's rejections stay GLSL's -- recursion has no call stack there and is
//!   rejected with a span, while the CPU artifact over the same program keeps
//!   compiling.

use std::sync::LazyLock;

use bone_semantic::inference::Workspace;
use skull::artifact::{self, ArtifactRequest};
use skull::instance_graph::{self, InstanceGraph};
use skull::shader_artifact::{self, ShaderArtifactRequest, ShaderProgram, ShaderRejection};
use skull::target_projection::{JAVASCRIPT_WEB, LUAJIT_DESKTOP};
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn check(src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

fn graph_for(ws: &Workspace) -> (InstanceGraph, Vec<skull_mir::mir::FuncRef>) {
    let roots = skull::mir_emit::emission_roots(ws, None);
    (instance_graph::build(ws, roots.clone(), false), roots)
}

/// The shader PROGRAMS production hands over: the `rock_shader` pairs, from
/// `skull_mir`'s own collector. An input, exactly like the CPU artifact's
/// emission roots -- the view never re-derives them.
fn shader_roots(ws: &Workspace) -> Vec<ShaderProgram> {
    skull_mir::lower::collect_shader_collection(ws)
        .expect("shader pairs")
        .pairs
        .into_iter()
        .map(|p| ShaderProgram {
            vert: p.vert,
            frag: p.frag,
        })
        .collect()
}

fn names(
    ws: &Workspace,
    set: impl IntoIterator<Item = skull::instance_graph::Instance>,
) -> Vec<String> {
    set.into_iter()
        .map(|i| {
            ws.module(i.func.module).functions[i.func.func]
                .name
                .to_string()
        })
        .collect()
}

const REACHED: &str = r#"
@pure fn shader_helper(x: float) -> float { return x * 2.0 }
@pure fn cpu_only_helper(x: float) -> float { return x + 1.0 }

@shader(fragment)
fn probe_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    let px = texture(uTexture, tc)
    let d = shader_helper(px.r)
    return v(d, d, d, 1.0)
}

fn main() {
    let key = rock_shader(probe_frag)
    let n = cpu_only_helper(1.0)
}
"#;

/// The profile is the axis's TYPED value, and a target with no shader axis
/// packages no shader artifact at all.
#[test]
fn the_shader_view_carries_the_typed_profile_from_the_axis() {
    let ws = check(REACHED);
    let (g, _) = graph_for(&ws);
    let roots = shader_roots(&ws);

    let view = shader_artifact::realize_shader(
        &ws,
        &g,
        &ShaderArtifactRequest {
            spec: LUAJIT_DESKTOP,
            programs: &roots,
        },
    )
    .expect("a target with a GLSL axis packages a shader artifact");
    assert_eq!(view.profile, skull_codegen_glsl::GlslProfile::Core330);

    // CONTROL: no shader axis, no shader artifact. Native C now packages the
    // same Core330 side artifact as the other desktop GL targets.
    assert!(
        shader_artifact::realize_shader(
            &ws,
            &g,
            &ShaderArtifactRequest {
                spec: skull::target_projection::WASM32_WASI,
                programs: &roots,
            }
        )
        .is_none()
    );
}

/// Retention is REACHABILITY from the shader entries over the shared graph's
/// own call sites: the helper a shader body calls is packaged, the helper only
/// CPU code calls is not.
#[test]
fn the_shader_view_retains_the_helpers_its_entries_reach() {
    let ws = check(REACHED);
    let (g, roots) = graph_for(&ws);
    let view = shader_artifact::realize_shader(
        &ws,
        &g,
        &ShaderArtifactRequest {
            spec: LUAJIT_DESKTOP,
            programs: &shader_roots(&ws),
        },
    )
    .expect("shader artifact");

    let root_names = names(&ws, view.roots.iter().cloned());
    assert_eq!(
        root_names,
        vec!["probe_frag".to_string()],
        "the entry set is not the pairs"
    );
    let helper_names = names(&ws, view.helpers.iter().cloned());
    assert!(
        helper_names.iter().any(|n| n == "shader_helper"),
        "a helper the entry calls was not packaged: {helper_names:?}"
    );
    assert!(
        !helper_names.iter().any(|n| n == "cpu_only_helper"),
        "a CPU-only helper was packaged into the shader artifact: {helper_names:?}"
    );
    assert!(
        !helper_names.iter().any(|n| n == "main"),
        "retention walked the whole program: {helper_names:?}"
    );

    // A GLSL-mapped callee (`texture`, and the `Vec4` ctor's operators) is a
    // LEAF: reached, never packaged as a definition. Without this the walk
    // would descend into an `inline(lua)` body GLSL never emits.
    let builtin_names = names(&ws, view.builtins.iter().cloned());
    assert!(
        builtin_names.iter().any(|n| n == "texture"),
        "the GLSL intrinsic leaf rule did not fire: {builtin_names:?}"
    );
    assert!(
        !helper_names.iter().any(|n| n == "texture"),
        "an inline-rendered builtin was packaged as a definition: {helper_names:?}"
    );
    assert!(
        view.sites_walked > 0,
        "no site was walked, so the exclusions above say nothing"
    );

    // ...and the CPU artifact keeps both, so the exclusion above is the SHADER
    // view's decision rather than the program's.
    let cpu = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT_DESKTOP, &roots));
    let cpu_names = names(&ws, cpu.realization.keys().cloned());
    assert!(cpu_names.iter().any(|n| n == "cpu_only_helper"));
    assert!(cpu_names.iter().any(|n| n == "shader_helper"));
}

/// GLSL has no call stack, so recursion among retained shader instances is a
/// SPANNED rejection -- and the same program's CPU artifact is untouched by it.
#[test]
fn shader_recursion_is_a_spanned_rejection_the_cpu_artifact_does_not_share() {
    let ws = check(
        r#"
@pure fn rec_helper(x: float) -> float {
    if x <= 0.0 { return 0.0 }
    return rec_helper(x - 1.0)
}

@shader(fragment)
fn rec_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    let d = rec_helper(tc.x)
    return v(d, d, d, 1.0)
}

fn main() { let key = rock_shader(rec_frag) }
"#,
    );
    let (g, roots) = graph_for(&ws);
    let view = shader_artifact::realize_shader(
        &ws,
        &g,
        &ShaderArtifactRequest {
            spec: LUAJIT_DESKTOP,
            programs: &shader_roots(&ws),
        },
    )
    .expect("shader artifact");

    let recursions: Vec<&ShaderRejection> = view
        .rejections
        .iter()
        .filter(|r| matches!(r.kind, shader_artifact::ShaderRejectionKind::Recursion))
        .collect();
    assert_eq!(
        recursions.len(),
        1,
        "expected exactly the `rec_helper` cycle: {:?}",
        view.render_rejections(&ws)
    );
    // The rejection is filed against the ENTRY -- the GLSL pass runs per entry,
    // exactly as emission does -- and NAMES the recursive helper in the
    // emitter's own message and span.
    assert_eq!(
        ws.module(recursions[0].instance.func.module).functions[recursions[0].instance.func.func]
            .name
            .as_str(),
        "rec_frag"
    );
    assert!(
        recursions[0].message.contains("rec_helper") && recursions[0].message.contains("recursion"),
        "the rejection lost the emitter's diagnostic: {}",
        recursions[0].message
    );
    assert!(view.disagreements.is_empty(), "{:?}", view.disagreements);
    assert!(
        view.entries_checked > 0,
        "no entry was checked, so the kind says nothing"
    );
    assert!(
        !instance_graph::is_spanless(&recursions[0].span),
        "the recursion rejection carries no source span"
    );

    // The CPU artifact over the SAME graph compiles it: recursion is a GLSL
    // restriction, and a shader decision may not reach CPU admission.
    let cpu = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT_DESKTOP, &roots));
    assert!(
        cpu.realization.keys().any(|i| {
            ws.module(i.func.module).functions[i.func.func]
                .name
                .as_str()
                == "rec_helper"
        }),
        "the CPU artifact dropped a recursive fn because GLSL cannot take one"
    );
    assert_eq!(
        cpu.severed.len(),
        0,
        "the CPU artifact severed a call over a shader rejection"
    );

    // CONTROL: the non-recursive fixture reports no recursion at all, so the
    // detector is not answering yes to every shader program.
    let ws2 = check(REACHED);
    let (g2, _) = graph_for(&ws2);
    let ok = shader_artifact::realize_shader(
        &ws2,
        &g2,
        &ShaderArtifactRequest {
            spec: LUAJIT_DESKTOP,
            programs: &shader_roots(&ws2),
        },
    )
    .expect("shader artifact");
    assert!(
        ok.rejections.is_empty(),
        "a non-recursive shader was rejected: {:?}",
        ok.render_rejections(&ws2)
    );
}

/// The STRICT PROFILE check is GLSL's own (`skull_codegen_glsl`'s tier-0
/// signature pass), asked of what this artifact retains -- not a second
/// re-implementation that could disagree with the emitter.
#[test]
fn a_retained_helper_outside_tier_zero_is_a_spanned_profile_rejection() {
    let ws = check(
        r#"
@pure fn stringy_helper(s: string) -> float { return 1.0 }

@shader(fragment)
fn tier_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    let d = stringy_helper("x")
    return v(d, d, d, 1.0)
}

fn main() { let key = rock_shader(tier_frag) }
"#,
    );
    let (g, _) = graph_for(&ws);
    let view = shader_artifact::realize_shader(
        &ws,
        &g,
        &ShaderArtifactRequest {
            spec: LUAJIT_DESKTOP,
            programs: &shader_roots(&ws),
        },
    )
    .expect("shader artifact");

    let profile: Vec<&ShaderRejection> = view
        .rejections
        .iter()
        .filter(|r| matches!(r.kind, shader_artifact::ShaderRejectionKind::Profile))
        .collect();
    assert_eq!(
        profile.len(),
        1,
        "expected the tier-0 violation and nothing else: {:?}",
        view.render_rejections(&ws)
    );
    assert!(
        !instance_graph::is_spanless(&profile[0].span),
        "the profile rejection carries no source span"
    );

    // CONTROL, and the one this check was measured wrong by first: an ENTRY's
    // signature is classified by STAGE, where an `@uniform` struct param is
    // legal. Asking the library's tier-0 leaf check there rejected every real
    // shader in this repo.
    let ws3 = check(
        r#"
value ProbeU { scale: float }

@shader(fragment)
fn uniform_frag(uTexture: Sampler2D, @uniform u: ProbeU, tc: Vec2) -> Vec4 {
    let px = texture(uTexture, tc)
    return v(px.r * u.scale, 0.0, 0.0, 1.0)
}

fn main() { let key = rock_shader(uniform_frag) }
"#,
    );
    let (g3, _) = graph_for(&ws3);
    let entry = shader_artifact::realize_shader(
        &ws3,
        &g3,
        &ShaderArtifactRequest {
            spec: LUAJIT_DESKTOP,
            programs: &shader_roots(&ws3),
        },
    )
    .expect("shader artifact");
    assert!(
        !entry.roots.is_empty(),
        "the uniform fixture registered no entry"
    );
    assert!(
        entry.rejections.is_empty(),
        "an entry's uniform-block param was refused by the library tier-0 check: {:?}",
        entry.render_rejections(&ws3)
    );

    // CONTROL: the tier-0 fixture reports no profile rejection.
    let ws2 = check(REACHED);
    let (g2, _) = graph_for(&ws2);
    let ok = shader_artifact::realize_shader(
        &ws2,
        &g2,
        &ShaderArtifactRequest {
            spec: LUAJIT_DESKTOP,
            programs: &shader_roots(&ws2),
        },
    )
    .expect("shader artifact");
    assert!(ok.rejections.is_empty(), "{:?}", ok.render_rejections(&ws2));
}

/// CPU target selection preserves the shared shader program.
#[test]
fn cpu_target_does_not_change_the_shader_artifact() {
    let ws = check(REACHED);
    let (g, _) = graph_for(&ws);
    let roots = shader_roots(&ws);
    let of = |spec| {
        shader_artifact::realize_shader(
            &ws,
            &g,
            &ShaderArtifactRequest {
                spec,
                programs: &roots,
            },
        )
        .expect("shader artifact")
    };
    let desktop = of(LUAJIT_DESKTOP);
    let web = of(JAVASCRIPT_WEB);
    assert!(
        !desktop.roots.is_empty(),
        "the fixture packages no shader entry"
    );
    assert_eq!(desktop.helpers, web.helpers);
}

/// **The view runs emission's pass, not a summary of it.** A helper whose
/// SIGNATURE is tier 0 and whose BODY is not (a string literal), and a helper
/// with a foreign body and no GLSL mapping, both compile as far as any
/// signature check can see -- and both are refused by `emit_shader_entry`.
/// A view that only checked signatures reported them clean.
#[test]
fn a_body_the_emitter_refuses_is_refused_by_the_view() {
    let cases: [(&str, &str); 2] = [
        (
            r#"
@pure fn stringy_body(x: float) -> float {
    let s = "not tier 0"
    return x
}

@shader(fragment)
fn body_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    let d = stringy_body(tc.x)
    return v(d, d, d, 1.0)
}

fn main() { let key = rock_shader(body_frag) }
"#,
            "string",
        ),
        (
            r#"
inline(lua) fn lua_only_helper(x: float) -> float = "return x * 2"

@shader(fragment)
fn foreign_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    let d = lua_only_helper(tc.x)
    return v(d, d, d, 1.0)
}

fn main() { let key = rock_shader(foreign_frag) }
"#,
            "no emittable body",
        ),
    ];
    for (src, needle) in cases {
        let ws = check(src);
        let (g, _) = graph_for(&ws);
        let view = shader_artifact::realize_shader(
            &ws,
            &g,
            &ShaderArtifactRequest {
                spec: LUAJIT_DESKTOP,
                programs: &shader_roots(&ws),
            },
        )
        .expect("shader artifact");
        assert!(view.entries_checked > 0, "no entry was checked at all");
        let msgs = view.render_rejections(&ws);
        assert_eq!(
            msgs.len(),
            1,
            "expected exactly one rejection, got {msgs:?}"
        );
        assert!(
            msgs[0].contains(needle),
            "the rejection is not the emitter's own ({needle}): {msgs:?}"
        );
    }
}

/// **A generic helper is CHECKED and EMITTED under its instantiation, and its
/// program joins the validated denominator.**
///
/// GLSL lowers each instance under the substitution the call site recorded, so
/// `passthrough<float>` and `passthrough<int>` are two tier-0 bodies rather
/// than one un-answerable declaration. The generic population is asserted
/// first: this fixture calls the helper at TWO types under one program, so a
/// green here cannot be a zero denominator.
#[test]
fn a_program_reaching_a_generic_helper_is_validated_at_its_instantiations() {
    let ws = check(
        r#"
@pure fn passthrough<T>(x: T) -> T { return x }
@pure fn concrete_helper(x: float) -> float { return x * 2.0 }

@shader(fragment)
fn generic_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    let d = concrete_helper(passthrough(tc.x))
    let n = passthrough(3)
    return v(d, d, d, float(n))
}

@shader(fragment)
fn concrete_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    let d = concrete_helper(tc.x)
    return v(d, d, d, 1.0)
}

fn main() {
    let a = rock_shader(generic_frag)
    let b = rock_shader(concrete_frag)
}
"#,
    );
    let (g, _) = graph_for(&ws);
    let programs = shader_roots(&ws);
    assert_eq!(
        programs.len(),
        2,
        "the fixture registered {} programs",
        programs.len()
    );
    let view = shader_artifact::realize_shader(
        &ws,
        &g,
        &ShaderArtifactRequest {
            spec: LUAJIT_DESKTOP,
            programs: &programs,
        },
    )
    .expect("shader artifact");

    // THE DENOMINATOR. Two packaged instances of one generic declaration, both
    // with a non-empty substitution -- so the checked count below is measured
    // over a real generic population and not over an empty one.
    let generic_insts: Vec<&skull::instance_graph::Instance> = view
        .packaged()
        .filter(|i| {
            ws.module(i.func.module).functions[i.func.func]
                .name
                .as_str()
                == "passthrough"
        })
        .collect();
    assert_eq!(
        generic_insts.len(),
        2,
        "the fixture lost its generic population: {:?}",
        names(&ws, view.packaged().cloned())
    );
    for i in &generic_insts {
        assert!(
            !g.substitution(i)
                .expect("a recorded substitution")
                .is_empty(),
            "a `passthrough` instance carries an EMPTY substitution, so it is not generic"
        );
    }

    // Both programs are validated: nothing is excluded, and the partition still
    // accounts for every registered program.
    assert_eq!(view.programs, 2);
    assert_eq!(
        view.entries_checked, 2,
        "the generic-helper program was excluded rather than checked at its instantiations"
    );
    assert_eq!(view.programs_unchecked, 0);
    assert_eq!(
        view.entries_checked + view.programs_unchecked,
        view.programs
    );
    assert!(
        view.rejections.is_empty(),
        "a concretely instantiated generic helper was rejected: {:?}",
        view.render_rejections(&ws)
    );
    assert!(view.disagreements.is_empty(), "{:?}", view.disagreements);
    assert!(view.missing_substitutions.is_empty());

    let root_names = names(&ws, view.roots.iter().cloned());
    assert!(
        root_names.iter().any(|n| n == "generic_frag"),
        "{root_names:?}"
    );
    assert!(
        root_names.iter().any(|n| n == "concrete_frag"),
        "{root_names:?}"
    );
}

/// **A missing substitution record fails CLOSED.** `reach` records a
/// substitution for every instance it reaches, roots included; an absent record
/// is an invariant break, and the old `unwrap_or(true)` read it as "empty", i.e.
/// concrete. Removing one record must move the program out of the validated
/// population and into an explicit finding -- never into `entries_checked`.
#[test]
fn a_missing_substitution_record_is_a_finding_not_a_concrete_shape() {
    let ws = check(REACHED);
    let (mut g, _) = graph_for(&ws);
    let programs = shader_roots(&ws);

    // The control: with the record present, the program IS validated.
    let before = shader_artifact::realize_shader(
        &ws,
        &g,
        &ShaderArtifactRequest {
            spec: LUAJIT_DESKTOP,
            programs: &programs,
        },
    )
    .expect("shader artifact");
    assert_eq!(
        before.entries_checked, 1,
        "the fixture's program was not checked to begin with"
    );
    assert!(before.missing_substitutions.is_empty());

    // Remove exactly ONE record -- the entry's own.
    let entry = before
        .roots
        .iter()
        .find(|i| {
            ws.module(i.func.module).functions[i.func.func]
                .name
                .as_str()
                == "probe_frag"
        })
        .expect("the entry instance")
        .clone();
    let removed = g.index.subst.remove(&entry).is_some() | g.root_instances.remove(&entry);
    assert!(removed, "the entry had no substitution to remove");

    let after = shader_artifact::realize_shader(
        &ws,
        &g,
        &ShaderArtifactRequest {
            spec: LUAJIT_DESKTOP,
            programs: &programs,
        },
    )
    .expect("shader artifact");
    assert_eq!(
        after.entries_checked, 0,
        "a program with a missing substitution record was declaration-validated"
    );
    assert_eq!(after.programs_unchecked, 1);
    assert!(
        after.rejections.is_empty(),
        "{:?}",
        after.render_rejections(&ws)
    );
    // The finding is EXPLICIT and names the instance -- not folded into the
    // generic bucket, which would read as "we know it is generic".
    assert!(
        after.missing_substitutions.contains(&entry),
        "the missing record produced no finding: {:?}",
        names(&ws, after.missing_substitutions.iter().cloned())
    );
}

/// **A PIPELINE fragment is not a standalone entry.** Its signature carries the
/// vertex stage's varyings value, which `classify_entry` refuses outright --
/// so a view given a flat root list rather than the registered PAIRS asks the
/// wrong question and rejects every A2 shader this repo ships (543 corpus
/// rejections, measured, before the request carried programs).
#[test]
fn an_a2_pipeline_is_checked_as_a_pair_not_as_two_entries() {
    let ws = check(
        r#"
value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
value PipeU    { uProjection: Mat4 }
value PipeVary { position: Vec4, TexCoord: Vec2 }

struct PipeTex { uTexture: Sampler2D }

@shader(vertex)
fn pipe_vert(@attr a: QuadVert, @uniform u: PipeU) -> PipeVary {
    let clip = u.uProjection * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
    return PipeVary { position: clip, TexCoord: a.aTexCoord }
}

@shader(fragment)
fn pipe_frag(vary: PipeVary, @textures t: PipeTex) -> Vec4 {
    return texture(t.uTexture, vary.TexCoord)
}

global PIPE_KEY: string = rock_shader(pipe_vert, pipe_frag)

fn main() { print("{PIPE_KEY}") }
"#,
    );
    let (g, _) = graph_for(&ws);
    let programs = shader_roots(&ws);
    assert_eq!(
        programs.len(),
        1,
        "the fixture registered {} programs",
        programs.len()
    );
    assert!(
        programs[0].vert.is_some(),
        "the fixture is not a PAIR, so it tests nothing"
    );

    let view = shader_artifact::realize_shader(
        &ws,
        &g,
        &ShaderArtifactRequest {
            spec: LUAJIT_DESKTOP,
            programs: &programs,
        },
    )
    .expect("shader artifact");
    assert_eq!(view.entries_checked, 1, "the pair was not checked");
    assert!(
        view.rejections.is_empty(),
        "a real A2 pipeline was rejected: {:?}",
        view.render_rejections(&ws)
    );
    // Both stages are packaged, and the vertex one is a root rather than a
    // helper: a pair is two entries of one program.
    let root_names = names(&ws, view.roots.iter().cloned());
    assert!(
        root_names.iter().any(|n| n == "pipe_vert"),
        "{root_names:?}"
    );
    assert!(
        root_names.iter().any(|n| n == "pipe_frag"),
        "{root_names:?}"
    );
}
