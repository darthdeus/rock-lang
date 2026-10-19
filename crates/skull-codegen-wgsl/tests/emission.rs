use bone_semantic::prelude::Span;
use skull_parser::CompilerErrorKind;
use skull_parser::Dialect;
use skull_codegen_wgsl::{
    WgslAbiPlan, WgslPrimitiveTopology, emit_fullscreen_shader, plan_fullscreen_artifact,
};
use skull_mir::mir::{Inst, Term};
use skull_mir::gpu_layout::SamplerKind;
use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            skull_parser::parse_module_clean(
                f.contents,
                Ustr::from(format!("<stdlib:{}>", f.name).as_str()),
            )
            .expect("stdlib parse")
        })
        .collect()
});

// Self-contained shader fixtures also cover renderer integration shapes.
const ROOTS: &[(&str, &str)] = &[
    ("blit_frag", include_str!("fixtures/fullscreen/blit.rock")),
    ("present_frag", include_str!("fixtures/fullscreen/present.rock")),
    ("tonemap_frag", include_str!("fixtures/fullscreen/tonemap.rock")),
];

const HOST_VERTEX: &str = include_str!("fixtures/explicit/host_vertex.rock.frag");
const ENTITY_ID_DEBUG: &str = include_str!("fixtures/fullscreen/entity_id_debug.rock");
const NONFINITE: &str = include_str!("fixtures/fullscreen/nonfinite.rock.frag");

fn outline_target_source() -> String {
    format!("{}\n{}", HOST_VERTEX, include_str!("fixtures/explicit/outline_target.rock"))
}

fn workspace(
    src: &str,
) -> (
    bone_semantic::inference::Workspace,
    bone_semantic::prelude::ModuleId,
) {
    let mut modules = STDLIB.clone();
    // Exercise package-qualified symbols as production shader inputs do.
    let mut module =
        skull_parser::parse_module_clean_with_dialect(src, Ustr::from("<test>"), Dialect::V1)
            .expect("parse");
    module.package = Some(skull_parser::PackageTag {
        id: 0,
        self_name: "shader_fixture".to_string(),
    });
    modules.push(module);
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module");
    (ws, mid)
}

/// The modules-v1 definition key of a root in the test module. A v1 function's
/// registered name is its package-qualified DefKey, so an emitter lookup by the
/// declared name needs the key; a legacy or already-qualified name passes
/// through.
fn root_key(
    ws: &bone_semantic::inference::Workspace,
    mid: bone_semantic::prelude::ModuleId,
    name: &str,
) -> String {
    if ws.functions_by_name.contains_key(&Ustr::from(name)) {
        return name.to_string();
    }
    ws.module(mid)
        .functions
        .iter()
        .map(|(_, f)| f.name.as_str())
        .find(|key| key.rsplit("::").next() == Some(name))
        .unwrap_or_else(|| panic!("root `{name}` is not defined in the test module"))
        .to_string()
}

fn emit_pipeline(
    ws: &bone_semantic::inference::Workspace,
    mid: bone_semantic::prelude::ModuleId,
    vert_root: &str,
    frag_root: &str,
    profile: skull_codegen_glsl::GlslProfile,
) -> Result<skull_codegen_glsl::ShaderEmission, skull_parser::CompilerError> {
    let vert = root_key(ws, mid, vert_root);
    let frag = root_key(ws, mid, frag_root);
    skull_codegen_glsl::emit_shader_pipeline_with_dependencies(ws, mid, &vert, &frag, profile)
}

fn v1_workspace(
    package: &str,
    main_source: &str,
    helpers_source: &str,
) -> (
    bone_semantic::inference::Workspace,
    bone_semantic::prelude::ModuleId,
) {
    let mut modules = STDLIB.clone();
    for (file, source) in [("main.rock", main_source), ("helpers.rock", helpers_source)] {
        let mut module = skull_parser::parse_module_with_dialect(
            source,
            Ustr::from(file),
            Dialect::V1,
        )
        .expect("v1 shader fixture parse");
        module.package = Some(skull_parser::PackageTag {
            id: 1,
            self_name: package.to_string(),
        });
        modules.push(module);
    }
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules)
        .expect("v1 shader fixture semantic pass");
    let mid = *ws.module_ids.get(&Ustr::from("main.rock")).expect("v1 main module");
    (ws, mid)
}

fn assert_naga_valid(source: &str) {
    let module = naga::front::wgsl::parse_str(source).expect("WGSL parses");
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .expect("WGSL validates");
}

fn try_artifact(
    root: &str,
    source: &str,
) -> Result<skull_codegen_wgsl::WgslShaderEmission, skull_parser::CompilerError> {
    let (ws, mid) = workspace(source);
    let key = root_key(&ws, mid, root);
    let fref = skull_codegen_shader::resolve_root(&ws, mid, &key)?;
    let neutral = plan_fullscreen_artifact(&ws, fref)?;
    let abi = WgslAbiPlan::wgsl(&neutral.interface, Span::default())?;
    let emission = emit_fullscreen_shader(&ws, &neutral.ir, &neutral.interface, &abi, Span::default())?;
    assert_eq!(emission.dependencies, neutral.dependencies);
    Ok(emission)
}

fn artifact(root: &str, source: &str) -> skull_codegen_wgsl::WgslShaderEmission {
    try_artifact(root, source).expect("WGSL artifact")
}

#[test]
fn sprite_basic_emitter_fixture_is_admitted_by_wgsl() {
    let source = format!("{}\n{}", HOST_VERTEX, include_str!("fixtures/explicit/sprite_basic.rock"));
    let (ws, mid) = workspace(&source);
    let glsl = emit_pipeline(
        &ws,
        mid,
        "sprite_basic_vert",
        "sprite_basic_frag",
        skull_codegen_glsl::GlslProfile::Core330,
    )
    .expect("the existing paired compiler plan");
    let abi = WgslAbiPlan::explicit(
        &glsl.interface,
        "vertex_main",
        "fragment_main",
        Span::default(),
    )
        .expect("selected sprite_basic must have a WGSL ABI");
    let artifact = skull_codegen_wgsl::emit_explicit_shader(
        &ws,
        &glsl.ir,
        &glsl.interface,
        &abi,
        Span::default(),
    )
    .expect("selected sprite_basic must emit WGSL");
    assert_eq!(glsl.interface.vertex_inputs.len(), 1);
    assert_eq!(glsl.interface.varyings.len(), 2);
    assert_eq!(abi.textures.len(), 1);
    assert!(artifact.source.contains("@vertex"));
    assert!(artifact.source.contains("@fragment"));
    let module = naga::front::wgsl::parse_str(&artifact.source).expect("WGSL parses");
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .expect("WGSL validates");
}

const CONST_GLOBAL_SHADER: &str = r#"
const WATER_SCALE_BASE: float = -0.37
const WATER_SCALE: float = WATER_SCALE_BASE

value ConstAttrs { pos: Vec2 }
value ConstVary { position: Vec4, marker: float }

@shader(vertex)
fn const_global_vert(@attr a: ConstAttrs) -> ConstVary {
    let scaled = a.pos.x * WATER_SCALE
    return ConstVary {
        position: v(scaled, a.pos.y, 0.0, 1.0),
        marker: scaled,
    }
}

@shader(fragment)
fn const_global_frag(vary: ConstVary) -> Vec4 {
    return v(vary.marker, -0.61, 0.0, 1.0)
}
"#;

fn try_explicit_artifact_source(
    source: &str,
    vertex_root: &str,
    fragment_root: &str,
) -> Result<skull_codegen_wgsl::WgslShaderEmission, skull_parser::CompilerError> {
    let (ws, mid) = workspace(source);
    let glsl = emit_pipeline(
        &ws,
        mid,
        vertex_root,
        fragment_root,
        skull_codegen_glsl::GlslProfile::Core330,
    )?;
    let abi = WgslAbiPlan::explicit(
        &glsl.interface,
        "vertex_main",
        "fragment_main",
        Span::default(),
    )?;
    skull_codegen_wgsl::emit_explicit_shader(
        &ws,
        &glsl.ir,
        &glsl.interface,
        &abi,
        Span::default(),
    )
}

#[test]
fn const_globals_emit_planner_proven_literals_and_validate() {
    let artifact = try_explicit_artifact_source(
        CONST_GLOBAL_SHADER,
        "const_global_vert",
        "const_global_frag",
    )
    .expect("planner-proven const globals must emit in WGSL");
    assert!(artifact.source.contains("* -0.37"), "negative const spelling changed");
    assert!(artifact.source.contains("-0.61"), "negative float spelling changed");
    let module = naga::front::wgsl::parse_str(&artifact.source).expect("WGSL parses");
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .expect("const-global WGSL validates");
}

#[test]
fn mutable_and_nonconstant_globals_remain_rejected_controls() {
    let mutable = CONST_GLOBAL_SHADER.replace(
        "const WATER_SCALE: float = WATER_SCALE_BASE",
        "global WATER_SCALE: float = -0.37",
    );
    let mutable_error = match try_explicit_artifact_source(
        &mutable,
        "const_global_vert",
        "const_global_frag",
    ) {
        Ok(_) => panic!("mutable shader globals must remain rejected"),
        Err(error) => error,
    };
    assert!(
        mutable_error.message.contains("global read")
            || mutable_error.message.contains("non-tier-0"),
        "mutable global was not rejected by the planner: {}",
        mutable_error.message
    );

    let nonconstant = CONST_GLOBAL_SHADER.replace(
        "const WATER_SCALE_BASE: float = -0.37",
        "const WATER_SCALE_BASE: float = nonconstant_scale()",
    ) + "\nfn nonconstant_scale() -> float { return 0.25 }\n";
    let nonconstant_error = match try_explicit_artifact_source(
        &nonconstant,
        "const_global_vert",
        "const_global_frag",
    ) {
        Ok(_) => panic!("nonconstant shader initializers must remain rejected"),
        Err(error) => error,
    };
    assert!(
        nonconstant_error.message.contains("constant expression")
            || nonconstant_error.message.contains("global"),
        "nonconstant initializer was not rejected: {}",
        nonconstant_error.message
    );
}

#[test]
fn integer_texture_id_emitter_fixture_emits_sampler_free_load_and_validates() {
    let artifact = artifact("entity_id_debug_frag", ENTITY_ID_DEBUG);
    let module = naga::front::wgsl::parse_str(&artifact.source).expect("WGSL parses");
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .expect("integer texture WGSL validates");
    assert_eq!(artifact.abi.textures.len(), 1);
    assert_eq!(artifact.abi.textures[0].kind, SamplerKind::Int);
    assert_eq!(artifact.abi.textures[0].sampler_binding, None);
    assert!(artifact.source.contains("var tex_id_buffer: texture_2d<i32>"));
    assert!(artifact.source.contains("textureLoad(tex_id_buffer"));
    assert!(!artifact.source.contains("samp_id_buffer"));
}

#[test]
fn integer_texture_id_rejects_a_float_sampler() {
    let source = r#"
struct BadEntityIdTex {
    id_buffer: Sampler2D,
}

@shader(fragment)
fn bad_entity_id_frag(@textures t: BadEntityIdTex, tc: Vec2) -> Vec4 {
    let id = texture_id(t.id_buffer, tc)
    return v(int_to_float(id), 0.0, 0.0, 1.0)
}
"#;
    let result = try_artifact("bad_entity_id_frag", source);
    assert!(result.is_err(), "texture_id must reject a filterable float sampler");
}

#[test]
fn nonfinite_intrinsics_emitter_fixture_emits_typed_ieee_predicates_and_bool_vectors() {
    let artifact = artifact("nonfinite_frag", NONFINITE);
    let module = naga::front::wgsl::parse_str(&artifact.source).expect("WGSL parses");
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .expect("nonfinite WGSL validates");
    assert!(artifact.source.contains("bitcast<u32>"));
    assert!(artifact.source.contains("vec3<bool>"));
    assert!(artifact.source.contains("any("));
    assert!(artifact.source.contains("select(vec3<f32>(0.0), vec3<f32>(1.0)"));
    assert!(artifact.source.contains(" > "));
}

#[test]
fn outline_target_emitter_fixture_emits_an_integer_fragment_output_for_r32i() {
    let source = outline_target_source();
    let (ws, mid) = workspace(&source);
    let glsl = emit_pipeline(
        &ws,
        mid,
        "outline_target_vert",
        "outline_target_frag",
        skull_codegen_glsl::GlslProfile::Core330,
    )
    .expect("the current outline target compiler plan");
    assert!(matches!(
        glsl.interface.fragment_output,
        skull_mir::shader_ir::FragmentOutput::Single(skull_mir::gpu_layout::WireKind::I32)
    ));
    let abi = WgslAbiPlan::explicit(
        &glsl.interface,
        "vertex_main",
        "fragment_main",
        Span::default(),
    )
    .expect("R32I outline target is a supported explicit WGSL output");
    let artifact = skull_codegen_wgsl::emit_explicit_shader(
        &ws,
        &glsl.ir,
        &glsl.interface,
        &abi,
        Span::default(),
    )
    .expect("the outline target WGSL artifact");
    naga_validate(&artifact.source);
    assert_eq!(artifact.abi.fragment_outputs[0].kind, skull_mir::gpu_layout::WireKind::I32);
    assert!(artifact.source.contains("@location(0) i32"), "{}", artifact.source);
}

#[test]
fn explicit_pipeline_without_texture_group_is_admitted_by_wgsl() {
    let source = r#"
value BareVert { aPos: Vec2 }
value BareVary { position: Vec4, TexCoord: Vec2 }
@shader(vertex)
fn bare_vert(@attr a: BareVert) -> BareVary {
    return BareVary { position: v(a.aPos, 0.0, 1.0), TexCoord: a.aPos }
}
@shader(fragment)
fn bare_frag(_vary: BareVary) -> Vec4 {
    return Vec4 { x: 1.0, y: 0.0, z: 1.0, w: 1.0 }
}
"#;
    let (ws, mid) = workspace(source);
    let glsl = emit_pipeline(
        &ws,
        mid,
        "bare_vert",
        "bare_frag",
        skull_codegen_glsl::GlslProfile::Core330,
    )
    .expect("the texture-free paired compiler plan");
    let abi = WgslAbiPlan::explicit(
        &glsl.interface,
        "vertex_main",
        "fragment_main",
        Span::default(),
    )
    .expect("a texture-free explicit interface is admitted");
    assert!(abi.textures.is_empty());
    let artifact = skull_codegen_wgsl::emit_explicit_shader(
        &ws,
        &glsl.ir,
        &glsl.interface,
        &abi,
        Span::default(),
    )
    .expect("a texture-free explicit shader emits WGSL");
    naga_validate(&artifact.source);
}

#[test]
fn explicit_sampler_helpers_forward_texture_and_sampler_pairs() {
    let source = r#"
struct HelperTextures { source: Sampler2D }
value HelperVert { pos: Vec2 }
value HelperVary { position: Vec4, uv: Vec2 }

fn helper_alpha(src: Sampler2D, uv: Vec2) -> float {
    return texture(src, uv).x
}

fn helper_filtered(src: Sampler2D, uv: Vec2) -> float {
    return helper_alpha(src, uv)
}

@shader(vertex)
fn helper_vert(@attr a: HelperVert) -> HelperVary {
    return HelperVary { position: v(a.pos, 0.0, 1.0), uv: a.pos }
}

@shader(fragment)
fn helper_frag(vary: HelperVary, @textures t: HelperTextures) -> Vec4 {
    let alpha = helper_filtered(t.source, vary.uv)
    return v(alpha, alpha, alpha, 1.0)
}
"#;
    let artifact = try_explicit_artifact_source(source, "helper_vert", "helper_frag")
        .expect("explicit sampler helpers must emit WGSL");
    assert!(artifact.source.contains("rock_sampler_tex_"), "{0}", artifact.source);
    assert!(artifact.source.contains("rock_sampler_samp_"), "{0}", artifact.source);
    naga_validate(&artifact.source);
}

#[test]
fn explicit_multiple_vertex_streams_preserve_instance_inputs_and_vertex_index() {
    let source = r#"
value Quad { pos: Vec2 }
value Instance { offset: Vec2 }
value Vary { position: Vec4, uv: Vec2 }
@shader(vertex)
fn multi_vert(@attr a: Quad, @instance i: Instance) -> Vary {
    let vid = vertex_id() % 2
    return Vary { position: v(a.pos.x + i.offset.x + int_to_float(vid) * 0.01, a.pos.y + i.offset.y, 0.0, 1.0), uv: a.pos }
}

@shader(fragment)
fn multi_frag(vary: Vary) -> Vec4 {
    return v(vary.uv, 0.0, 1.0)
}
"#;
    let (ws, mid) = workspace(source);
    let glsl = emit_pipeline(
        &ws,
        mid,
        "multi_vert",
        "multi_frag",
        skull_codegen_glsl::GlslProfile::Core330,
    )
    .expect("the multi-stream compiler plan");
    assert_eq!(glsl.interface.vertex_inputs.len(), 2);
    let abi = WgslAbiPlan::explicit(
        &glsl.interface,
        "vertex_main",
        "fragment_main",
        Span::default(),
    )
    .expect("the measured per-vertex plus per-instance contract");
    let artifact = skull_codegen_wgsl::emit_explicit_shader(
        &ws,
        &glsl.ir,
        &glsl.interface,
        &abi,
        Span::default(),
    )
    .expect("the indexed instanced shader emits WGSL");
    assert!(artifact.source.contains("@builtin(vertex_index)"), "{}", artifact.source);
    assert!(artifact.source.contains("@location(1) input_1"), "{}", artifact.source);
    naga_validate(&artifact.source);
}

#[test]
fn explicit_no_fragment_varyings_are_a_typed_wgsl_omission() {
    let source = r#"
value Quad { pos: Vec2 }
value Vary { position: Vec4 }
@shader(vertex)
fn solid_vert(@attr a: Quad) -> Vary {
    return Vary { position: v(a.pos.x, a.pos.y, 0.0, 1.0) }
}
@shader(fragment)
fn solid_frag(vary: Vary) -> Vec4 {
    return v(1.0, 0.0, 1.0, 1.0)
}
"#;
    let (ws, mid) = workspace(source);
    let glsl = emit_pipeline(
        &ws,
        mid,
        "solid_vert",
        "solid_frag",
        skull_codegen_glsl::GlslProfile::Core330,
    )
    .expect("the no-varying compiler plan");
    assert!(glsl.interface.varyings.is_empty());
    let error = WgslAbiPlan::explicit(
        &glsl.interface,
        "vertex_main",
        "fragment_main",
        Span::default(),
    )
    .expect_err("no fragment varyings are not yet physically representable");
    assert_eq!(
        error.kind,
        CompilerErrorKind::UnsupportedWgslProjection {
            capability: "fragment varyings".into(),
        }
    );
}

// Mirrors rect_batch.rock's `Vertex`; rect_batch itself drags in `DepthMode`,
// which this workspace's semantic pass cannot resolve. The 32-bit twins and the
// `@nomangle` match the real vertex layout and its emitted symbol.
fn sprite_batch_source() -> String {
    format!(
        "{}\n{}",
        HOST_VERTEX,
        include_str!("fixtures/explicit/sprite_batch.rock.frag")
    )
}

fn sprite_batch_artifact() -> (
    skull_codegen_glsl::ShaderEmission,
    skull_codegen_wgsl::WgslShaderEmission,
) {
    let source = sprite_batch_source();
    let (ws, mid) = workspace(&source);
    let glsl = emit_pipeline(
        &ws,
        mid,
        "sprite_batch_vert",
        "sprite_batch_frag",
        skull_codegen_glsl::GlslProfile::Core330,
    )
    .expect("the existing paired compiler plan");
    let abi = WgslAbiPlan::explicit(
        &glsl.interface,
        "vertex_main",
        "fragment_main",
        Span::default(),
    )
    .expect("selected sprite_batch must have a WGSL ABI");
    let emission = skull_codegen_wgsl::emit_explicit_shader(
        &ws,
        &glsl.ir,
        &glsl.interface,
        &abi,
        Span::default(),
    )
    .expect("selected sprite_batch must emit WGSL");
    (glsl, emission)
}

const WGSL_NUMERIC_STAGES: &str = r#"
value MathAttrs { pos: Vec2 }
value VertexMathUniform { v_int_a: int, v_int_b: int, v_float_a: float, v_float_b: float }
value FragmentMathUniform { f_int_a: int, f_int_b: int, f_float_a: float, f_float_b: float }
struct MathTextures { uMathTexture: Sampler2D }
value MathVary {
    position: Vec4,
    idiv: float,
    imod: float,
    fmod: float,
    converted: float,
}

@shader(vertex)
fn numeric_vert(@attr a: MathAttrs, @uniform u: VertexMathUniform) -> MathVary {
    let q = u.v_int_a ./ u.v_int_b
    let r = u.v_int_a % u.v_int_b
    let fr = u.v_float_a % u.v_float_b
    let fi = u.v_float_a as int
    let marker = 1.0
    if q < r || fi < 0 { marker = -1.0 }
    return MathVary {
        position: v(a.pos, 0.0, 1.0),
        idiv: marker,
        imod: marker,
        fmod: fr,
        converted: marker,
    }
}

@shader(fragment)
fn numeric_frag(
    vary: MathVary,
    @uniform u: FragmentMathUniform,
    @textures t: MathTextures,
) -> Vec4 {
    let q = u.f_int_a ./ u.f_int_b
    let r = u.f_int_b % u.f_int_a
    let fr = u.f_float_b % u.f_float_a
    let fi = u.f_float_a as int
    let marker = 1.0
    if q < r || fi < 0 { marker = -1.0 }
    return v(vary.idiv + marker, vary.imod + marker,
        vary.fmod + fr, vary.converted + marker)
}
"#;

fn explicit_artifact_source(
    source: &str,
    vertex_root: &str,
    fragment_root: &str,
) -> skull_codegen_wgsl::WgslShaderEmission {
    let (ws, mid) = workspace(source);
    let glsl = emit_pipeline(
        &ws,
        mid,
        vertex_root,
        fragment_root,
        skull_codegen_glsl::GlslProfile::Core330,
    )
    .expect("the explicit fixture has a neutral graphics plan");
    let abi = WgslAbiPlan::explicit(
        &glsl.interface,
        "vertex_main",
        "fragment_main",
        Span::default(),
    )
    .expect("the explicit fixture has a WGSL ABI");
    skull_codegen_wgsl::emit_explicit_shader(
        &ws,
        &glsl.ir,
        &glsl.interface,
        &abi,
        Span::default(),
    )
    .expect("the explicit fixture emits WGSL")
}

#[test]
fn vertex_and_fragment_preserve_rock_floor_numeric_semantics() {
    let artifact = explicit_artifact_source(WGSL_NUMERIC_STAGES, "numeric_vert", "numeric_frag");
    assert_eq!(artifact.source.matches("rock_floor_idiv(").count(), 4, "{0}", artifact.source);
    assert_eq!(artifact.source.matches("rock_floor_imod(").count(), 3, "{0}", artifact.source);
    assert_eq!(artifact.source.matches("rock_floor_fmod(").count(), 3, "{0}", artifact.source);
    assert_eq!(artifact.source.matches("i32(floor(").count(), 2, "{0}", artifact.source);
    assert!(artifact.source.contains("return select("), "{0}", artifact.source);
    naga_validate(&artifact.source);
}

#[test]
fn explicit_varying_reconstruction_follows_declared_field_names() {
    let source = r#"
value ReorderedAttrs { pos: Vec2 }
value ReorderedVary {
    uv: Vec2,
    position: Vec4,
    tint: Vec4,
}
struct ReorderedTextures { uReorderedTexture: Sampler2D }

@shader(vertex)
fn reordered_vert(@attr a: ReorderedAttrs) -> ReorderedVary {
    return ReorderedVary {
        uv: v(0.25, 0.75),
        position: v(a.pos, 0.0, 1.0),
        tint: v(1.0, 0.5, 0.25, 1.0),
    }
}

@shader(fragment)
fn reordered_frag(vary: ReorderedVary, @textures t: ReorderedTextures) -> Vec4 {
    return vary.tint + v(vary.uv, 0.0, 0.0)
}
"#;
    let artifact = explicit_artifact_source(source, "reordered_vert", "reordered_frag");
    // The package name affects the type suffix, not the reconstruction order.
    let varying_type = artifact.source.lines()
        .find_map(|line| line.strip_prefix("struct ReorderedVary_")
            .and_then(|suffix| suffix.strip_suffix(" {"))
            .filter(|suffix| !suffix.ends_with("Out")))
        .expect("the declared varying type");
    let expected = format!("let vary = ReorderedVary_{varying_type}(input.uv, input.position, input.tint);");
    assert!(artifact.source.contains(&expected), "{0}", artifact.source);
    naga_validate(&artifact.source);
}

#[test]
fn explicit_vec3_varying_is_admitted_with_neighboring_widths() {
    let source = r#"
value Vec3Attrs { pos: Vec2 }
value Vec3Vary {
    position: Vec4,
    scalar: float,
    uv: Vec2,
    @flat emission: Vec3,
    tint: Vec4,
}
@shader(vertex)
fn vec3_vert(@attr a: Vec3Attrs) -> Vec3Vary {
    return Vec3Vary {
        position: v(a.pos, 0.0, 1.0),
        scalar: 0.25,
        uv: v(0.25, 0.75),
        emission: v(1.0, 0.5, 0.25),
        tint: v(0.8, 0.7, 0.6, 1.0),
    }
}
@shader(fragment)
fn vec3_frag(vary: Vec3Vary) -> Vec4 {
    return v(vary.emission, vary.scalar) + vary.tint + v(vary.uv, 0.0, 0.0)
}
"#;
    let artifact = explicit_artifact_source(source, "vec3_vert", "vec3_frag");
    assert_eq!(
        artifact
            .abi
            .varyings
            .iter()
            .map(|varying| (varying.name.as_str(), varying.kind, varying.location))
            .collect::<Vec<_>>(),
        vec![
            ("scalar", skull_mir::gpu_layout::WireKind::F32, 0),
            ("uv", skull_mir::gpu_layout::WireKind::F32x2, 1),
            ("emission", skull_mir::gpu_layout::WireKind::F32x3, 2),
            ("tint", skull_mir::gpu_layout::WireKind::F32x4, 3),
        ]
    );
    assert!(
        artifact.source.contains("@location(2) @interpolate(flat) emission: vec3<f32>,"),
        "Vec3 flat varyings must retain their one-location WGSL type:\n{}",
        artifact.source
    );
    let parsed = skull_wgsl_abi::WgslAbi::parse(&artifact.abi.wire().expect("WGSL ABI wire"))
        .expect("the Vec3 varying must survive the runtime ABI wire");
    assert_eq!(parsed.varyings[2].kind, skull_mir::gpu_layout::WireKind::F32x3.code());
    naga_validate(&artifact.source);
}

fn naga_validate(source: &str) {
    let module = naga::front::wgsl::parse_str(source)
        .unwrap_or_else(|error| panic!("WGSL parses: {error}\n{source}"));
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .unwrap_or_else(|error| panic!("WGSL validates: {error:?}\n{source}"));
}

#[test]
fn sprite_batch_emitter_fixture_is_admitted_by_wgsl() {
    let (glsl, artifact) = sprite_batch_artifact();
    assert_eq!(artifact.abi.textures.len(), 5);
    assert_eq!(
        artifact
            .abi
            .textures
            .iter()
            .map(|texture| texture.name.as_str())
            .collect::<Vec<_>>(),
        [
            "uTexture",
            "uShadowmapTex",
            "uGlowTex",
            "uNormalTex",
            "uSurfaceTex",
        ]
    );
    let engine = artifact.abi.uniform_block(skull_mir::shader_ir::UniformRole::Engine).unwrap();
    let gl_engine = glsl.abi.uniform_blocks.iter()
        .find(|(_, block)| block.name == engine.struct_name).unwrap();
    assert_eq!(engine.size, gl_engine.1.plan.size);
    for member in &engine.members {
        let gl_member = gl_engine.1.plan.members.iter().find(|candidate| candidate.name == member.name).unwrap();
        assert_eq!(member.offset, gl_member.offset, "GL and WGPU share EngineU member offsets");
        assert_eq!(member.kind, gl_member.kind);
    }
    naga_validate(&artifact.source);
}

#[test]
fn explicit_texture_helpers_carry_canvas_orientation_through_nested_calls() {
    let source = format!(
        "{}\n{}",
        HOST_VERTEX,
        r#"
struct OrientationTex { main: Sampler2D }
value OrientationVary { position: Vec4, tex_coord: Vec2 }

@shader(vertex)
fn orientation_vert(@attr a: Vertex, @engine e: EngineU) -> OrientationVary {
    return OrientationVary {
        position: e.uProjection * e.uView * v(a.pos.to_f64(), 0.0, 1.0),
        tex_coord: a.uv.to_f64(),
    }
}

fn orientation_inner(src: Sampler2D, coord: Vec2) -> Vec4 {
    return texture(src, coord)
}

fn orientation_outer(src: Sampler2D, coord: Vec2) -> Vec4 {
    return orientation_inner(src, coord + v(0.0, 0.0))
}

@shader(fragment)
fn orientation_frag(vary: OrientationVary, @textures t: OrientationTex) -> Vec4 {
    return orientation_outer(t.main, vary.tex_coord)
}
"#,
    );
    let artifact = explicit_artifact_source(&source, "orientation_vert", "orientation_frag");
    assert!(
        artifact.source.contains("rock_sampler_flip_")
            && artifact
                .source
                .contains("select(coord.y, 1.0 - coord.y, rock_sampler_flip_"),
        "WGSL sampler helpers must carry the logical texture's orientation into every nested sample:\n{}",
        artifact.source
    );
    naga_validate(&artifact.source);
}

#[test]
fn fixture_float_vertex_inputs_preserve_their_raw_bits() {
    let (_, artifact) = sprite_batch_artifact();
    assert!(
        artifact.source.contains("@location(2) input_2: vec4<u32>,"),
        "float vertex payloads must enter WGSL through an integer bit-preserving format:\n{}",
        artifact.source
    );
    assert!(
        artifact.source.contains(
            "let Vertex_attrs = Vertex(bitcast<vec2<f32>>(input_0), bitcast<vec2<f32>>(input_1), bitcast<vec4<f32>>(input_2), bitcast<f32>(input_3), bitcast<f32>(input_4), bitcast<f32>(input_5));"
        ),
        "the logical Vertex value must be reconstructed before the shader body:\n{}",
        artifact.source
    );
    naga_validate(&artifact.source);
}

#[test]
fn sprite_batch_color_interpolation_follows_its_declaration() {
    for flat in [false, true] {
        let source = if flat {
            sprite_batch_source().replace("    Color: Vec4,", "    @flat Color: Vec4,")
        } else {
            sprite_batch_source()
        };
        let artifact = explicit_artifact_source(&source, "sprite_batch_vert", "sprite_batch_frag");
        let varying = artifact.abi.varyings.iter().find(|varying| varying.name == "Color").unwrap();
        assert_eq!(varying.flat, flat, "the authored color annotation controls interpolation");
        let declaration = artifact.source.lines().find(|line|
            line.contains(&format!("@location({})", varying.location)) && line.contains(" Color:"))
            .expect("the color varying is emitted");
        assert_eq!(declaration.contains("@interpolate(flat)"), flat);
        naga_validate(&artifact.source);
    }
}

#[test]
fn explicit_frag_coord_matches_gl_bottom_left_coordinates() {
    let (_, artifact) = sprite_batch_artifact();
    let engine = artifact.abi.uniform_block(skull_mir::shader_ir::UniformRole::Engine).unwrap();
    let conversion = format!(
        "(vary, vec4<f32>(input.position.x, ((2.0 / abs(u_{}.uProjection[1][1])) - input.position.y), input.position.z, input.position.w))",
        engine.struct_name);
    assert!(artifact.source.contains(&conversion),
        "the fragment wrapper passes Rock's bottom-left coordinates: {}", artifact.source);
    naga_validate(&artifact.source);
}

/// Naga proves well-formedness, not that the right target got the right
/// location. `FragId` is the entity-id attachment: emitting it at location 0
/// would validate and paint ids into the colour buffer.
#[test]
fn the_mrt_outputs_keep_their_declaration_order_and_locations() {
    let (glsl, artifact) = sprite_batch_artifact();
    assert!(
        matches!(
            &glsl.interface.fragment_output,
            skull_mir::shader_ir::FragmentOutput::Mrt(outputs) if outputs.len() == 2
        ),
        "the fixture must present an MRT interface: {:?}",
        glsl.interface.fragment_output
    );
    let outputs = &artifact.abi.fragment_outputs;
    assert_eq!(outputs.len(), 2);
    assert_eq!(
        (outputs[0].name.as_str(), outputs[0].location),
        ("FragColor", 0)
    );
    assert_eq!((outputs[1].name.as_str(), outputs[1].location), ("FragId", 1));
    assert!(
        artifact
            .source
            .contains("  @location(0) FragColor: vec4<f32>,"),
        "{}",
        artifact.source
    );
    assert!(
        artifact.source.contains("  @location(1) FragId: i32,"),
        "{}",
        artifact.source
    );
}

#[test]
fn per_primitive_varyings_carry_the_flat_interpolation_attribute() {
    let (glsl, artifact) = sprite_batch_artifact();
    let expected: Vec<_> = glsl.interface.varyings.iter()
        .map(|varying| (varying.name, varying.kind, varying.flat)).collect();
    let actual: Vec<_> = artifact.abi.varyings.iter()
        .map(|varying| (varying.name, varying.kind, varying.flat)).collect();
    assert!(!expected.is_empty());
    assert_eq!(actual, expected, "WGSL preserves every authored varying and its interpolation");
    for varying in &artifact.abi.varyings {
        let declaration = artifact.source.lines().find(|line|
            line.contains(&format!("@location({})", varying.location))
                && line.contains(&format!(" {}:", varying.name)))
            .expect("every declared varying is emitted");
        assert_eq!(declaration.contains("@interpolate(flat)"), varying.flat, "{}", varying.name);
    }
    assert_eq!(artifact.source.matches("@interpolate(flat)").count(),
        expected.iter().filter(|(_, _, flat)| *flat).count());
}

/// std140 gives every uniform-array element a 16-byte stride, and WGSL's uniform
/// address space demands the same -- so each element is declared as its padded
/// 4-component vector and the read projects back down. A declaration that packed
/// tightly would validate and then read the wrong element of the CPU's buffer.
#[test]
fn uniform_array_elements_are_padded_to_the_std140_stride_and_projected_back() {
    use skull_mir::gpu_layout::WireKind;
    let (glsl, artifact) = sprite_batch_artifact();
    assert!(!artifact.abi.array_blocks.is_empty());
    assert_eq!(artifact.abi.array_blocks.len(), glsl.abi.uniform_array_blocks.len());
    for block in &artifact.abi.array_blocks {
        let (_, expected) = glsl.abi.uniform_array_blocks.iter()
            .find(|(_, expected)| expected.block_name == block.struct_name).unwrap();
        assert_eq!(block.size, expected.size);
        assert_eq!(block.members.len(), expected.members.len());
        for (member, expected) in block.members.iter().zip(&expected.members) {
            assert_eq!((member.name, member.elem, member.capacity, member.offset, member.stride),
                (expected.name, expected.elem, expected.capacity, expected.offset, expected.stride));
            let (scalar, projection) = match expected.elem {
                WireKind::I32 => ("i32", ".x"),
                WireKind::F32 => ("f32", ".x"),
                WireKind::F32x2 => ("f32", ".xy"),
                WireKind::F32x4 => ("f32", ""),
                other => panic!("uncovered production array kind: {other:?}"),
            };
            assert!(artifact.source.contains(&format!("  {}: array<vec4<{scalar}>, {}>,", member.name, expected.capacity)));
            let prefix = format!("u_{}.{}[", block.struct_name, member.name);
            let reads: Vec<_> = artifact.source.split(&prefix).skip(1).collect();
            assert!(!reads.is_empty(), "the production shader reads {}", member.name);
            for read in reads {
                let suffix = &read[read.find(']').expect("array index closes") + 1..];
                let actual: String = suffix.strip_prefix('.').unwrap_or("").chars()
                    .take_while(|ch| ch.is_ascii_alphabetic()).collect();
                assert_eq!(actual, projection.trim_start_matches('.'),
                    "{} projects back to its declared element width", member.name);
            }
        }
    }
}

/// WGSL's uniform address space has no `bool`, so the member crosses as the u32
/// the CPU already writes. A `bool` declaration would not compile; a u32 member
/// read as if it were a bool would be a type error -- but a u32 member compared
/// against the wrong thing would not, so the comparison is asserted here.
#[test]
fn bool_uniforms_cross_as_u32_and_are_compared_at_every_read() {
    let (glsl, artifact) = sprite_batch_artifact();
    let block = artifact
        .abi
        .uniform_block(skull_mir::shader_ir::UniformRole::Stage(
            skull_mir::shader_ir::ShaderStage::Fragment,
        ))
        .expect("the fragment uniform block");
    let bools: Vec<&str> = block
        .members
        .iter()
        .filter(|member| member.kind == skull_mir::gpu_layout::WireKind::Bool32)
        .map(|member| member.name.as_str())
        .collect();
    let expected: Vec<_> = glsl.interface.uniform_group(skull_mir::shader_ir::UniformRole::Stage(
        skull_mir::shader_ir::ShaderStage::Fragment)).unwrap().members.iter()
        .filter(|(_, kind)| *kind == skull_mir::gpu_layout::WireKind::Bool32)
        .map(|(name, _)| name.as_str()).collect();
    assert!(!expected.is_empty());
    assert_eq!(bools, expected, "every authored bool reaches the ABI");
    for name in &bools {
        assert!(
            artifact.source.contains(&format!("  {name}: u32,")),
            "`{name}` is declared u32: {}",
            artifact.source
        );
        assert!(
            artifact
                .source
                .contains(&format!("(u_{}.{name} != 0u)", block.struct_name)),
            "`{name}` is compared at its read: {}",
            artifact.source
        );
    }
}

/// The fragment's own control flow: the light loop is a counted `for`, the
/// alpha cutoff is a `discard`, and `frag_coord()` arrives as the entry's
/// parameter because WGSL has no global fragment position.
#[test]
fn the_fragment_control_flow_and_builtins_reach_wgsl() {
    let (_, artifact) = sprite_batch_artifact();
    let light_loops = artifact.source.lines().filter(|line| {
        let line = line.trim();
        line.starts_with("for (; i < cell_") && line.ends_with(") {")
    }).count();
    assert_eq!(
        light_loops,
        1,
        "the light loop is a counted for: {}",
        artifact.source
    );
    assert_eq!(
        artifact.source.matches("discard;").count(),
        1,
        "the alpha cutoff discards: {}",
        artifact.source
    );
    assert_eq!(
        artifact
            .source
            .matches(&format!("{}: vec4<f32>", skull_codegen_wgsl::FRAG_COORD_PARAM))
            .count(),
        1,
        "the fragment entry takes the builtin position: {}",
        artifact.source
    );
    assert_eq!(
        artifact
            .source
            .matches("vec2<f32>(textureDimensions(tex_uShadowmapTex))")
            .count(),
        1,
        "texture_size reaches textureDimensions: {}",
        artifact.source
    );
}

#[derive(Debug, Default, PartialEq, Eq)]
struct OperationCensus {
    calls: usize,
    constants: usize,
    fields: usize,
    loads: usize,
    stores: usize,
    value_structs: usize,
    returns: usize,
    unsupported: usize,
    non_return_terms: usize,
}

fn operation_census(emission: &skull_codegen_wgsl::WgslShaderEmission) -> OperationCensus {
    let stage = &emission.ir.stages[0];
    let mut census = OperationCensus::default();
    for body in stage.bodies.values().chain(std::iter::once(&stage.body)) {
        for inst in body.insts.values() {
            match inst {
                Inst::Call { .. } => census.calls += 1,
                Inst::Const(_) => census.constants += 1,
                Inst::Field { .. } => census.fields += 1,
                Inst::Load(_) => census.loads += 1,
                Inst::Store { .. } => census.stores += 1,
                Inst::MakeValueStruct { .. } | Inst::MaterializeValueStruct { .. } => {
                    census.value_structs += 1
                }
                _ => census.unsupported += 1,
            }
        }
        for block in body.blocks.values() {
            match block.term {
                Term::Return(_) => census.returns += 1,
                _ => census.non_return_terms += 1,
            }
        }
    }
    census
}

#[test]
fn neutral_ir_operation_census_covers_the_three_fullscreen_fixture_roots() {
    let expected = [
        (
            "blit_frag",
            OperationCensus {
                calls: 1,
                constants: 0,
                fields: 1,
                loads: 2,
                stores: 0,
                value_structs: 0,
                returns: 1,
                unsupported: 0,
                non_return_terms: 0,
            },
        ),
        (
            "present_frag",
            OperationCensus {
                calls: 30,
                constants: 14,
                fields: 18,
                loads: 29,
                stores: 11,
                value_structs: 2,
                returns: 2,
                unsupported: 0,
                non_return_terms: 3,
            },
        ),
        (
            "tonemap_frag",
            OperationCensus {
                calls: 17,
                constants: 7,
                fields: 12,
                loads: 15,
                stores: 3,
                value_structs: 2,
                returns: 2,
                unsupported: 0,
                non_return_terms: 0,
            },
        ),
    ];
    let mut examined = 0;
    for ((root, source), (expected_root, expected_census)) in ROOTS.iter().zip(expected) {
        assert_eq!(*root, expected_root);
        assert_eq!(operation_census(&artifact(root, source)), expected_census);
        examined += 1;
    }
    assert_eq!(examined, 3, "all fullscreen fixture roots were censused");
}

#[test]
fn fullscreen_emitter_fixture_roots_are_examined_and_emitted_3_of_3() {
    let mut examined = 0;
    let mut emitted = 0;
    let mut errors = Vec::new();
    for (root, source) in ROOTS {
        examined += 1;
        match std::panic::catch_unwind(|| artifact(root, source)) {
            Ok(_) => emitted += 1,
            Err(_) => errors.push(*root),
        }
    }
    assert_eq!(examined, 3, "the validator examined every selected root");
    assert_eq!(emitted, 3, "WGSL selected migration roots: {errors:?}");
    assert!(errors.is_empty(), "no selected root was silently skipped");
}

#[test]
fn fixture_source_and_physical_structure_are_deterministic() {
    for (root, source) in ROOTS {
        let emission = artifact(root, source);
        let repeated = artifact(root, source);
        assert_eq!(emission.source, repeated.source, "WGSL source for `{root}` is deterministic");
        assert_eq!(
            emission.source.matches("@vertex").count(),
            1,
            "one compiler-owned vertex entry for `{root}`"
        );
        assert_eq!(
            emission.source.matches("@fragment").count(),
            1,
            "one fragment entry for `{root}`"
        );
        assert!(emission.source.contains("fn fragment_main"));
        assert_eq!(emission.abi.fullscreen_vertex.vertex_count, 4);
        assert_eq!(
            emission.abi.fullscreen_vertex.topology,
            WgslPrimitiveTopology::TriangleStrip
        );
        assert!(emission.source.contains("@builtin(vertex_index)"));
        assert!(!emission.source.contains("FullscreenVertexIn"));
        assert_eq!(emission.abi.fragment_outputs.len(), 1);
        assert_eq!(emission.abi.fragment_outputs[0].location, 0);
        assert!(
            emission
                .abi
                .textures
                .iter()
                .enumerate()
                .all(|(slot, texture)| texture.slot == slot as u32)
        );
        for (slot, texture) in emission.abi.textures.iter().enumerate() {
            assert_eq!(
                texture.texture_binding,
                emission.abi.uniform_blocks.len() as u32 + 1 + slot as u32 * 2
            );
            assert_eq!(texture.sampler_binding, Some(texture.texture_binding + 1));
        }
    }
}

#[test]
fn naga_validates_all_three_fullscreen_emitter_fixtures_3_of_3() {
    let mut examined = 0;
    let mut validated = 0;
    let mut errors = Vec::new();
    for (root, source) in ROOTS {
        examined += 1;
        let artifact = artifact(root, source);
        let validation = match naga::front::wgsl::parse_str(&artifact.source) {
            Ok(module) => naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .map(|_| ())
            .map_err(|error| format!("{error:?}")),
            Err(error) => Err(format!("{error:?}")),
        };
        match validation {
            Ok(()) => validated += 1,
            Err(error) => errors.push(format!("{root}: {error}")),
        }
    }
    assert_eq!(examined, 3, "WGSL validator examined every fullscreen fixture root");
    assert_eq!(validated, 3, "WGSL validator errors: {errors:?}");
    assert!(
        errors.is_empty(),
        "no generated module was silently skipped"
    );
}

#[test]
fn v1_shader_identifiers_and_intrinsics_validate_on_both_wgsl_pipelines() {
    let explicit_main = include_str!("fixtures/shader-v1/explicit_wgpu/main.rock");
    let explicit_helpers = include_str!("fixtures/shader-v1/explicit_wgpu/helpers.rock.frag");
    let (ws, mid) = v1_workspace("shader_probe", explicit_main, explicit_helpers);
    let vertex = skull_codegen_glsl::resolve_root(&ws, mid, "shader_probe::shader_probe_vert")
        .expect("v1 vertex root");
    let fragment = skull_codegen_glsl::resolve_root(&ws, mid, "shader_probe::shader_probe_frag")
        .expect("v1 fragment root");
    let glsl = emit_pipeline(
        &ws,
        mid,
        "shader_probe::shader_probe_vert",
        "shader_probe::shader_probe_frag",
        skull_codegen_glsl::GlslProfile::Core330,
    )
    .expect("v1 GLSL pipeline");
    assert!(!glsl.source.contains("::"), "GLSL emitted a DefKey: {}", glsl.source);
    assert!(glsl.source.contains("dot("), "GLSL intrinsic mapping: {}", glsl.source);
    assert!(glsl.source.contains(" + vec2("), "GLSL vector infix: {}", glsl.source);
    let abi = WgslAbiPlan::explicit(
        &glsl.interface,
        "vertex_main",
        "fragment_main",
        Span::default(),
    )
    .expect("v1 explicit WGSL ABI");
    let explicit = skull_codegen_wgsl::emit_explicit_shader(
        &ws,
        &glsl.ir,
        &glsl.interface,
        &abi,
        Span::default(),
    )
    .expect("v1 explicit WGSL");
    assert!(!explicit.source.contains("::"), "WGSL emitted a DefKey: {}", explicit.source);
    assert!(explicit.source.contains("dot("), "WGSL intrinsic mapping: {}", explicit.source);
    assert!(
        explicit.source.contains(" + vec2<f32>("),
        "WGSL vector infix: {}",
        explicit.source
    );
    assert_naga_valid(&explicit.source);
    assert_eq!(vertex.module, fragment.module, "roots belong to one v1 package");

    let fullscreen_main = include_str!("fixtures/shader-v1/fullscreen/main.rock");
    let fullscreen_helpers = include_str!("fixtures/shader-v1/fullscreen/helpers.rock.frag");
    let (ws, mid) = v1_workspace(
        "shader_probe_fullscreen",
        fullscreen_main,
        fullscreen_helpers,
    );
    let fragment = skull_codegen_glsl::resolve_root(
        &ws,
        mid,
        "shader_probe_fullscreen::shader_probe_frag",
    )
    .expect("v1 fullscreen root");
    let neutral = plan_fullscreen_artifact(&ws, fragment).expect("v1 fullscreen plan");
    let abi = WgslAbiPlan::wgsl(&neutral.interface, Span::default()).expect("v1 fullscreen ABI");
    let fullscreen = emit_fullscreen_shader(
        &ws,
        &neutral.ir,
        &neutral.interface,
        &abi,
        Span::default(),
    )
    .expect("v1 fullscreen WGSL");
    assert!(!fullscreen.source.contains("::"));
    assert!(fullscreen.source.contains("dot("));
    assert!(fullscreen.source.contains(" + vec2<f32>("));
    assert_naga_valid(&fullscreen.source);
}

const FRAG_DEPTH_SHADER: &str = r#"
value DepthAttrs { pos: Vec2 }
value DepthVary { position: Vec4, depth: float }
value DepthOut { FragColor: Vec4, FragDepth: float }

@shader(vertex)
fn depth_vert(@attr a: DepthAttrs) -> DepthVary {
    return DepthVary { position: Vec4 { x: a.pos.x, y: a.pos.y, z: 0.0, w: 1.0 }, depth: a.pos.x }
}

@shader(fragment)
fn depth_frag(vary: DepthVary) -> DepthOut {
    return DepthOut { FragColor: Vec4 { x: 1.0, y: 0.0, z: 0.0, w: 1.0 }, FragDepth: vary.depth }
}
"#;

/// WGSL is a paused lane: a fragment depth output is a spanned rejection, never
/// a silently dropped field.
#[test]
fn frag_depth_output_is_rejected_by_wgsl() {
    let error = match try_explicit_artifact_source(FRAG_DEPTH_SHADER, "depth_vert", "depth_frag") {
        Ok(artifact) => panic!("WGSL must reject FragDepth, emitted:\n{}", artifact.source),
        Err(error) => error,
    };
    assert!(
        error.message.contains("WGSL") && error.message.contains("FragDepth"),
        "unexpected rejection: {}",
        error.message
    );
}

#[test]
fn canvas_orientation_masks_follow_every_declared_texture_slot() {
    for count in [5, skull_codegen_glsl::GLSL_STAGE_SAMPLER_BUDGET] {
        let fields = (0..count).map(|slot| format!("image_{slot}: Sampler2D"))
            .collect::<Vec<_>>().join(", ");
        let samples = (0..count).map(|slot| format!(
            "sample_orientation(t.image_{slot}, vary.tex_coord)"
        )).collect::<Vec<_>>().join(" + ");
        let source = format!(
            "{}\n{}",
            HOST_VERTEX,
            format!(r#"
struct OrientationTex {{ {fields} }}
value OrientationVary {{ position: Vec4, tex_coord: Vec2 }}
@shader(vertex)
fn orientation_vert(@attr a: Vertex, @engine e: EngineU) -> OrientationVary {{
    return OrientationVary {{
        position: e.uProjection * e.uView * v(a.pos.to_f64(), 0.0, 1.0),
        tex_coord: a.uv.to_f64(),
    }}
}}
fn sample_orientation(src: Sampler2D, coord: Vec2) -> Vec4 {{
    return texture(src, coord)
}}
@shader(fragment)
fn orientation_frag(vary: OrientationVary, @textures t: OrientationTex) -> Vec4 {{
    return {samples}
}}
"#),
        );
        let artifact = explicit_artifact_source(&source, "orientation_vert", "orientation_frag");
        assert_eq!(artifact.abi.textures.len(), count as usize);
        for texture in &artifact.abi.textures {
            let suffix = artifact.source.split(&format!("tex_{},", texture.name)).nth(1)
                .unwrap_or_else(|| panic!("slot {} has no sampling call", texture.slot));
            let literal = suffix.split("uTextureVFlip & ").nth(1).unwrap().split('u').next().unwrap();
            let mask: u32 = literal.parse().expect("orientation mask is a WGSL uint literal");
            assert_eq!(mask.count_ones(), 1, "one orientation bit per logical texture");
            assert_eq!(mask.trailing_zeros(), texture.slot, "mask identifies the compiler-declared slot");
        }
        naga_validate(&artifact.source);
    }
}
