use bone_semantic::prelude::Span;
use skull_codegen_glsl::GlslProfile;
use skull_codegen_wgsl::{WgslAbiPlan, emit_fullscreen_shader};
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
    let neutral = skull_codegen_glsl::emit_shader_entry_with_dependencies(
        &ws,
        mid,
        root,
        GlslProfile::Core330,
    )
    .expect("neutral ShaderIR/interface");
    let abi = WgslAbiPlan::wgsl(&neutral.interface, Span::default()).expect("WGSL ABI");
    emit_fullscreen_shader(&ws, &neutral.ir, &neutral.interface, &abi, Span::default())
        .expect("WGSL emission")
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
fn neutral_ir_operation_census_covers_the_three_admitted_roots() {
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
    assert_eq!(examined, 3, "all admitted roots were censused");
}

#[test]
fn the_admitted_fullscreen_population_is_examined_and_admitted_3_of_3() {
    let mut examined = 0;
    let mut admitted = 0;
    let mut errors = Vec::new();
    for (root, source, _) in ROOTS {
        examined += 1;
        match std::panic::catch_unwind(|| artifact(root, source)) {
            Ok(_) => admitted += 1,
            Err(_) => errors.push(*root),
        }
    }
    assert_eq!(examined, 3, "the validator examined every admitted root");
    assert_eq!(admitted, 3, "WGSL admitted roots: {errors:?}");
    assert!(errors.is_empty(), "no admitted root was silently skipped");
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
        assert_eq!(artifact.abi.fullscreen_vertex.stride, 16);
        assert_eq!(artifact.abi.fragment.output_location, 0);
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
                artifact.abi.uniform_blocks.len() as u32 + slot as u32 * 2
            );
            assert_eq!(texture.sampler_binding, texture.texture_binding + 1);
        }
    }
}

#[test]
fn naga_validates_all_three_generated_modules_3_of_3() {
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
    assert_eq!(examined, 3, "WGSL validator examined every admitted root");
    assert_eq!(validated, 3, "WGSL validator errors: {errors:?}");
    assert!(
        errors.is_empty(),
        "no generated module was silently skipped"
    );
}
