use bone_semantic::prelude::Span;
use skull_codegen_wgsl::{
    WgslAbiPlan, WgslPrimitiveTopology, emit_fullscreen_shader, plan_fullscreen_artifact,
};
use skull_mir::mir::{Inst, Term};
use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
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

// This is the deliberate migration slice, not a census of every shader root.
const ROOTS: &[(&str, &str, &str)] = &[
    (
        "blit_frag",
        include_str!("../../../stone/engine/shaders/post_processing/blit_shader.rock"),
        include_str!("goldens/blit.wgsl"),
    ),
    (
        "present_frag",
        include_str!("../../../stone/engine/shaders/post_processing/present_shader.rock"),
        include_str!("goldens/present.wgsl"),
    ),
    (
        "tonemap_frag",
        include_str!("../../../stone/engine/shaders/post_processing/tonemapping_shader.rock"),
        include_str!("goldens/tonemap.wgsl"),
    ),
];

fn workspace(
    src: &str,
) -> (
    bone_semantic::inference::Workspace,
    bone_semantic::prelude::ModuleId,
) {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module");
    (ws, mid)
}

fn artifact(root: &str, source: &str) -> skull_codegen_wgsl::WgslShaderEmission {
    let source = format!(
        "{}\n{}",
        include_str!("../../../stone/engine/shaders/common/pass_textures.rock"),
        source
    );
    let (ws, mid) = workspace(&source);
    let fref = skull_codegen_shader::resolve_root(&ws, mid, root).expect("root");
    let neutral = plan_fullscreen_artifact(&ws, fref).expect("neutral ShaderIR/interface");
    let abi = WgslAbiPlan::wgsl(&neutral.interface, Span::default()).expect("WGSL ABI");
    let emission =
        emit_fullscreen_shader(&ws, &neutral.ir, &neutral.interface, &abi, Span::default())
            .expect("WGSL emission");
    assert_eq!(emission.dependencies, neutral.dependencies);
    emission
}

#[test]
fn selected_sprite_basic_pipeline_is_admitted_by_wgsl() {
    let source = format!(
        "{}\n{}\n{}",
        include_str!("../../../stone/engine/rendering/engine_uniforms.rock"),
        include_str!("../../../stone/engine/shaders/common/pass_textures.rock"),
        include_str!("../../../stone/engine/shaders/rendering/sprite_basic_shader.rock"),
    );
    let (ws, mid) = workspace(&source);
    let glsl = skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
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

// Mirrors rect_batch.rock's `Vertex`; rect_batch itself drags in `DepthMode`,
// which this workspace's semantic pass cannot resolve.
const SPRITE_BATCH_VERTEX: &str = "value Vertex { pos: Vec2, uv: Vec2, color: Vec4, pivot_y: float, entity_id: float }";

fn sprite_batch_source() -> String {
    format!(
        "{}\n{}\n{}\n{}",
        include_str!("../../../stone/engine/rendering/engine_uniforms.rock"),
        SPRITE_BATCH_VERTEX,
        include_str!("../../../stone/engine/shaders/common/pass_textures.rock"),
        include_str!("../../../stone/engine/shaders/rendering/sprite_batch_shader.rock"),
    )
}

fn sprite_batch_artifact() -> (
    skull_codegen_glsl::ShaderEmission,
    skull_codegen_wgsl::WgslShaderEmission,
) {
    let source = sprite_batch_source();
    let (ws, mid) = workspace(&source);
    let glsl = skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
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
fn selected_sprite_batch_pipeline_is_admitted_by_wgsl() {
    let (_, artifact) = sprite_batch_artifact();
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

/// `EntityId` is an integer id carried as a float: interpolating it across the
/// triangle produces ids no entity has, and every other varying must NOT be
/// flat or the sprite loses its gradients.
#[test]
fn only_the_flat_varying_carries_the_flat_interpolation_attribute() {
    let (_, artifact) = sprite_batch_artifact();
    let flat: Vec<&str> = artifact
        .abi
        .varyings
        .iter()
        .filter(|varying| varying.flat)
        .map(|varying| varying.name.as_str())
        .collect();
    assert_eq!(flat, ["EntityId"], "exactly one flat varying is declared");
    assert_eq!(artifact.abi.varyings.len(), 4, "and three that are not");
    assert!(
        artifact
            .source
            .contains("@location(3) @interpolate(flat) EntityId: f32,"),
        "{}",
        artifact.source
    );
    assert_eq!(
        artifact.source.matches("@interpolate(flat)").count(),
        1,
        "no other varying may be flat: {}",
        artifact.source
    );
}

/// std140 gives every uniform-array element a 16-byte stride, and WGSL's uniform
/// address space demands the same -- so each element is declared as its padded
/// 4-component vector and the read projects back down. A declaration that packed
/// tightly would validate and then read the wrong element of the CPU's buffer.
#[test]
fn uniform_array_elements_are_padded_to_the_std140_stride_and_projected_back() {
    let (_, artifact) = sprite_batch_artifact();
    assert_eq!(artifact.abi.array_blocks.len(), 1);
    let block = &artifact.abi.array_blocks[0];
    assert_eq!(block.members.len(), 7, "seven light arrays");
    for (index, member) in block.members.iter().enumerate() {
        assert_eq!(member.capacity, 16, "`{}` capacity", member.name);
        assert_eq!(member.stride, 16, "`{}` stride", member.name);
        assert_eq!(
            member.offset,
            index as u32 * 16 * 16,
            "`{}` offset",
            member.name
        );
    }
    assert_eq!(block.size, 7 * 16 * 16);
    // The element type is the PADDED vector, per element kind.
    assert!(
        artifact
            .source
            .contains("  uLightTypes: array<vec4<i32>, 16>,"),
        "{}",
        artifact.source
    );
    assert!(
        artifact
            .source
            .contains("  uLightRadii: array<vec4<f32>, 16>,"),
        "{}",
        artifact.source
    );
    // ... and the read projects it back to the DECLARED element width.
    for (read, kind) in [
        ("u_sprite_batch_frag_Arrays.uLightTypes[i].x", "int"),
        ("u_sprite_batch_frag_Arrays.uLightRadii[i].x", "float"),
        ("u_sprite_batch_frag_Arrays.uLightPositions[i].xy", "Vec2"),
        ("u_sprite_batch_frag_Arrays.uLightColors[i]", "Vec4"),
    ] {
        assert!(
            artifact.source.contains(read),
            "a {kind} array element reads as `{read}`: {}",
            artifact.source
        );
    }
    // A Vec4 element takes the whole slot, so it must NOT be projected.
    assert!(
        !artifact
            .source
            .contains("u_sprite_batch_frag_Arrays.uLightColors[i].x"),
        "a Vec4 element must not be narrowed: {}",
        artifact.source
    );
}

/// WGSL's uniform address space has no `bool`, so the member crosses as the u32
/// the CPU already writes. A `bool` declaration would not compile; a u32 member
/// read as if it were a bool would be a type error -- but a u32 member compared
/// against the wrong thing would not, so the comparison is asserted here.
#[test]
fn bool_uniforms_cross_as_u32_and_are_compared_at_every_read() {
    let (_, artifact) = sprite_batch_artifact();
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
    assert_eq!(bools, ["uLightsEnabled", "uUnlit", "uGlowEnabled", "uHasNormalTex"]);
    for name in &bools {
        assert!(
            artifact.source.contains(&format!("  {name}: u32,")),
            "`{name}` is declared u32: {}",
            artifact.source
        );
        assert!(
            artifact
                .source
                .contains(&format!("(u_SpriteFragU.{name} != 0u)")),
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
    assert_eq!(
        artifact.source.matches("  for (; i < cell_22; i = i + 1) {").count(),
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
    assert!(
        artifact.source.contains("(vary, input.position)"),
        "and the wrapper passes it: {}",
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
                calls: 28,
                constants: 14,
                fields: 10,
                loads: 23,
                stores: 8,
                value_structs: 0,
                returns: 1,
                unsupported: 0,
                non_return_terms: 0,
            },
        ),
        (
            "tonemap_frag",
            OperationCensus {
                calls: 18,
                constants: 7,
                fields: 14,
                loads: 24,
                stores: 10,
                value_structs: 2,
                returns: 2,
                unsupported: 0,
                non_return_terms: 0,
            },
        ),
    ];
    let mut examined = 0;
    for ((root, source, _), (expected_root, expected_census)) in ROOTS.iter().zip(expected) {
        assert_eq!(*root, expected_root);
        assert_eq!(operation_census(&artifact(root, source)), expected_census);
        examined += 1;
    }
    assert_eq!(examined, 3, "all fullscreen fixture roots were censused");
}

#[test]
fn the_selected_fullscreen_migration_roots_are_examined_and_emitted_3_of_3() {
    let mut examined = 0;
    let mut emitted = 0;
    let mut errors = Vec::new();
    for (root, source, _) in ROOTS {
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
fn source_goldens_and_physical_structure_are_deterministic() {
    for (root, source, golden) in ROOTS {
        let artifact = artifact(root, source);
        assert_eq!(artifact.source, *golden, "WGSL source golden for `{root}`");
        assert_eq!(
            artifact.source.matches("@vertex").count(),
            1,
            "one compiler-owned vertex entry for `{root}`"
        );
        assert_eq!(
            artifact.source.matches("@fragment").count(),
            1,
            "one fragment entry for `{root}`"
        );
        assert!(artifact.source.contains("fn fragment_main"));
        assert_eq!(artifact.abi.fullscreen_vertex.vertex_count, 4);
        assert_eq!(
            artifact.abi.fullscreen_vertex.topology,
            WgslPrimitiveTopology::TriangleStrip
        );
        assert!(artifact.source.contains("@builtin(vertex_index)"));
        assert!(!artifact.source.contains("FullscreenVertexIn"));
        assert_eq!(artifact.abi.fragment_outputs.len(), 1);
        assert_eq!(artifact.abi.fragment_outputs[0].location, 0);
        assert!(
            artifact
                .abi
                .textures
                .iter()
                .enumerate()
                .all(|(slot, texture)| texture.slot == slot as u32)
        );
        for (slot, texture) in artifact.abi.textures.iter().enumerate() {
            assert_eq!(
                texture.texture_binding,
                artifact.abi.uniform_blocks.len() as u32 + 1 + slot as u32 * 2
            );
            assert_eq!(texture.sampler_binding, texture.texture_binding + 1);
        }
    }
}

#[test]
fn naga_validates_all_three_fullscreen_fixture_modules_3_of_3() {
    let mut examined = 0;
    let mut validated = 0;
    let mut errors = Vec::new();
    for (root, source, _) in ROOTS {
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
