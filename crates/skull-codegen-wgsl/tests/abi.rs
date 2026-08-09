use bone_semantic::prelude::Span;
use skull_codegen_glsl::ShaderAbiPlan;
use skull_codegen_wgsl::WgslAbiPlan;
use skull_mir::gpu_layout::{SamplerKind, WireKind};
use skull_mir::shader_ir::{
    FragmentOutput, LogicalTextureGroup, LogicalUniformGroup, ShaderInterfacePlan, ShaderStage,
    UniformRole,
};
use ustr::Ustr;

fn fixture() -> ShaderInterfacePlan {
    ShaderInterfacePlan {
        stages: vec![ShaderStage::Fragment],
        uniform_groups: vec![LogicalUniformGroup {
            role: UniformRole::Stage(ShaderStage::Fragment),
            struct_name: Ustr::from("FragU"),
            members: vec![
                (Ustr::from("cutoff"), WireKind::F32),
                (Ustr::from("resolution"), WireKind::F32x2),
            ],
        }],
        textures: Some(LogicalTextureGroup {
            struct_name: Some(Ustr::from("Tex")),
            samplers: vec![
                (Ustr::from("scene"), SamplerKind::Float),
                (Ustr::from("bloom"), SamplerKind::Float),
            ],
            stages: vec![ShaderStage::Fragment],
        }),
        fragment_output: FragmentOutput::Single(WireKind::F32x4),
        ..Default::default()
    }
}

#[test]
fn wgsl_and_glsl_project_the_same_interface_independently() {
    let iface = fixture();
    let wgsl = WgslAbiPlan::wgsl(&iface, Span::default()).expect("WGSL projection");
    let glsl = ShaderAbiPlan::glsl(&iface, Span::default()).expect("GLSL projection");

    assert_eq!(iface.uniform_groups.len(), 1, "one uniform block examined");
    assert_eq!(
        iface.sampler_names().len(),
        2,
        "two ordered texture slots examined"
    );
    assert_eq!(wgsl.uniform_blocks.len(), 1, "one WGSL block admitted");
    assert_eq!(
        wgsl.textures.len(),
        2,
        "two WGSL texture/sampler pairs admitted"
    );

    let glsl_block = &glsl.uniform_blocks[0];
    let wgsl_block = &wgsl.uniform_blocks[0];
    assert_eq!(glsl_block.0, 2, "GLSL fragment binding");
    assert_eq!(wgsl_block.binding, 0, "WGSL owns its bind-group numbering");
    assert_eq!(wgsl_block.members[0].offset, 0);
    assert_eq!(wgsl_block.members[1].offset, 8);
    assert_eq!(
        glsl_block.1.members[1].offset, wgsl_block.members[1].offset,
        "the logical members agree while physical bindings differ"
    );

    assert_eq!(
        glsl.texture_units,
        vec![Ustr::from("scene"), Ustr::from("bloom")]
    );
    assert_eq!(wgsl.textures[0].texture_binding, 1);
    assert_eq!(wgsl.textures[0].sampler_binding, 2);
    assert_eq!(wgsl.textures[1].texture_binding, 3);
    assert_eq!(wgsl.textures[1].sampler_binding, 4);
    assert_ne!(
        glsl_block.0, wgsl.textures[0].texture_binding,
        "copying the GLSL binding would bind the first WGSL texture over the uniform block"
    );
    assert_eq!(
        wgsl.textures.iter().map(|t| t.name).collect::<Vec<_>>(),
        iface.sampler_names()
    );
}

#[test]
fn abi_validation_rejects_offset_mutation() {
    let iface = fixture();
    let mut plan = WgslAbiPlan::wgsl(&iface, Span::default()).expect("WGSL projection");

    plan.uniform_blocks[0].members[1].offset = 4;
    assert!(
        plan.validate(&iface, Span::default()).is_err(),
        "mutated WGSL uniform offset must be rejected"
    );
}

#[test]
fn abi_validation_rejects_swapped_texture_bindings() {
    let iface = fixture();
    let mut plan = WgslAbiPlan::wgsl(&iface, Span::default()).expect("WGSL projection");
    plan.textures.swap(0, 1);
    assert!(
        plan.validate(&iface, Span::default()).is_err(),
        "swapped logical texture slots must be rejected"
    );
}

#[test]
fn abi_validation_rejects_stage_visibility_drift() {
    let iface = fixture();
    let mut plan = WgslAbiPlan::wgsl(&iface, Span::default()).expect("WGSL projection");
    plan.uniform_blocks[0].visibility.fragment = false;
    assert!(
        plan.validate(&iface, Span::default()).is_err(),
        "stage visibility drift must be rejected"
    );
}

#[test]
fn unsupported_fullscreen_shapes_are_spanned_rejections() {
    let mut iface = fixture();
    iface.stages = vec![ShaderStage::Vertex, ShaderStage::Fragment];
    let error =
        WgslAbiPlan::wgsl(&iface, Span::default()).expect_err("explicit vertex must be rejected");
    assert!(error.message.contains("explicit vertex"));

    let mut iface = fixture();
    iface.uniform_groups[0].members[0].1 = WireKind::Bool32;
    let error =
        WgslAbiPlan::wgsl(&iface, Span::default()).expect_err("bool uniform must be rejected");
    assert!(error.message.contains("unsupported wire kind"));
}
