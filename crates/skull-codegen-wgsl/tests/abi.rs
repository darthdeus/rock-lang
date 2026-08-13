use bone_semantic::prelude::Span;
use skull_codegen_glsl::ShaderAbiPlan;
use skull_codegen_wgsl::WgslAbiPlan;
use skull_mir::gpu_layout::{ArrayMemberDecl, AttrKind, SamplerKind, WireKind};
use skull_mir::shader_ir::{
    ComputeAccess, ComputeAddressSpace, ComputeDispatchContract, ComputeInterfacePlan,
    ComputeResourceShape, ComputeValueKind, ComputeBuiltin, ComputeWorkgroupSize, FragmentOutput,
    LogicalArrayGroup, LogicalComputeResource, LogicalTextureGroup, LogicalUniformGroup,
    LogicalVarying, LogicalVertexInput, ShaderInterfacePlan, ShaderStage, UniformRole, VertexRate,
};
use skull_wgsl_abi::WgslAbi;
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
    assert_eq!(wgsl_block.binding, 1, "WGSL reserves binding 0 for geometry");
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
    assert_eq!(wgsl.textures[0].texture_binding, 2);
    assert_eq!(wgsl.textures[0].sampler_binding, Some(3));
    assert_eq!(wgsl.textures[1].texture_binding, 4);
    assert_eq!(wgsl.textures[1].sampler_binding, Some(5));
    assert_eq!(
        glsl_block.0, wgsl.textures[0].texture_binding,
        "the independent target projections use different binding spaces"
    );
    assert_ne!(
        wgsl_block.binding, wgsl.textures[0].texture_binding,
        "WGSL keeps geometry and logical bindings distinct"
    );
    assert_eq!(
        wgsl.textures.iter().map(|t| t.name).collect::<Vec<_>>(),
        iface.sampler_names()
    );
    assert_eq!(
        iface.semantic_fingerprint(true),
        wgsl.semantic_fingerprint,
        "the semantic identity is independent of the two physical projections"
    );
}

#[test]
fn fullscreen_uniform_arrays_keep_abi_order() {
    let mut iface = fixture();
    iface.array_groups = vec![LogicalArrayGroup {
        stage: ShaderStage::Fragment,
        owner: Ustr::from("frag"),
        members: vec![decl("values", WireKind::F32, 1)],
    }];

    let plan = WgslAbiPlan::wgsl(&iface, Span::default()).expect("fullscreen array projection");
    assert_eq!(plan.uniform_blocks[0].binding, 1, "ordinary block binding");
    assert_eq!(plan.array_blocks.len(), 1, "array block population");
    assert_eq!(plan.array_blocks[0].binding, 2, "array follows ordinary blocks");
    assert_eq!(plan.textures[0].texture_binding, 3, "textures follow both block classes");
    plan.validate(&iface, Span::default()).expect("projection validates");
    let parsed = WgslAbi::parse(&plan.wire()).expect("fullscreen array ABI parses");
    assert_eq!(parsed.array_blocks[0].binding, 2, "array binding reaches the runtime wire");
}

#[test]
fn semantic_wire_has_no_physical_layout_and_mutations_are_load_bearing() {
    let iface = fixture();
    let wire = iface.semantic_wire(true);
    for forbidden in ["offset", "binding", "std140", "WGSL", "GLSL"] {
        assert!(!wire.contains(forbidden), "semantic wire leaked {forbidden}: {wire}");
    }

    let mut swapped = iface.clone();
    swapped.textures.as_mut().unwrap().samplers.swap(0, 1);
    assert_ne!(iface.semantic_fingerprint(true), swapped.semantic_fingerprint(true));

    let explicit = iface.clone();
    assert_ne!(iface.semantic_fingerprint(true), explicit.semantic_fingerprint(false));

    let mut changed_member = iface.clone();
    changed_member.uniform_groups[0].members[0].0 = Ustr::from("renamed");
    assert_ne!(iface.semantic_fingerprint(true), changed_member.semantic_fingerprint(true));
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
fn integer_texture_slots_are_sampler_free_and_keep_logical_order() {
    let mut iface = fixture();
    iface.textures.as_mut().unwrap().samplers = vec![
        (Ustr::from("ids"), SamplerKind::Int),
        (Ustr::from("scene"), SamplerKind::Float),
    ];
    let plan = WgslAbiPlan::wgsl(&iface, Span::default()).expect("integer texture projection");
    assert_eq!(plan.textures[0].slot, 0);
    assert_eq!(plan.textures[0].texture_binding, 2);
    assert_eq!(plan.textures[0].sampler_binding, None);
    assert_eq!(plan.textures[1].slot, 1);
    assert_eq!(plan.textures[1].texture_binding, 3);
    assert_eq!(plan.textures[1].sampler_binding, Some(4));
    let wire = plan.wire();
    assert!(wire.contains("texture 0 ids int 2 none f"));
    assert!(wire.contains("texture 1 scene float 3 4 f"));
    let parsed = WgslAbi::parse(&wire).expect("sampler-free integer wire parses");
    assert_eq!(parsed.textures[0].sampler_binding, None);
    assert_eq!(parsed.textures[1].sampler_binding, Some(4));
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
    iface.uniform_groups[0].members[0].1 = WireKind::Mat4;
    iface.uniform_groups[0].members[0].0 = ustr::Ustr::from("mat");
    let plan = WgslAbiPlan::wgsl(&iface, Span::default()).expect("mat4 uniform is admitted");
    assert_eq!(plan.uniform_blocks[0].members[0].size, 64);
}

/// A bool uniform crosses as the 4-byte integer the CPU writes: WGSL's uniform
/// address space has no `bool` at all, so the member is `u32` and the boolean is
/// recovered at the read.
#[test]
fn a_bool_uniform_member_is_laid_out_and_declared_as_u32() {
    let mut iface = fixture();
    iface.uniform_groups[0].members[0].1 = WireKind::Bool32;
    let plan = WgslAbiPlan::wgsl(&iface, Span::default()).expect("bool uniform is admitted");
    let member = &plan.uniform_blocks[0].members[0];
    assert_eq!((member.offset, member.align, member.size), (0, 4, 4));
    assert_eq!(
        skull_codegen_wgsl::uniform_member_type(WireKind::Bool32),
        Some("u32")
    );
    assert_eq!(skull_codegen_wgsl::wgsl_type(WireKind::Bool32), None);
}

#[test]
fn abi_wire_is_deterministic_and_carries_physical_order() {
    let plan = WgslAbiPlan::wgsl(&fixture(), Span::default()).expect("WGSL projection");
    let wire = plan.wire();
    assert!(wire.starts_with("WGSL_ABI_V4\nchecksum "));
    assert!(wire.contains("geometry fullscreen"));
    assert!(wire.contains("entries fullscreen_vertex fragment_main"));
    assert!(wire.contains("fullscreen_vertex triangle_strip 4 0"));
    assert!(wire.contains("fullscreen_fragment 0"));
    assert!(wire.contains("fullscreen_uniform fullscreen FullscreenU 0 16 v 2"));
    assert!(wire.contains("output 0 color 3"));
    assert!(wire.contains("array_blocks 0"));
    assert!(wire.contains("block fragment FragU 1 16 f 2"));
    assert!(wire.contains("member cutoff 0 0 4 4"));
    assert!(wire.contains("member resolution 1 8 8 8"));
    assert!(wire.contains("texture 0 scene float 2 3 f"));
    assert!(wire.contains("texture 1 bloom float 4 5 f"));
    assert!(wire.ends_with("end\n"));
    let parsed = WgslAbi::parse(&wire).expect("compiler output uses the production parser");
    assert_eq!(parsed.fragment_outputs.len(), 1);
    assert_eq!(parsed.array_blocks.len(), 0);
    assert_eq!(wire, plan.wire(), "the compiler wire must be deterministic");
}

fn compute_fixture() -> ShaderInterfacePlan {
    ShaderInterfacePlan {
        stages: vec![ShaderStage::Compute],
        uniform_groups: vec![LogicalUniformGroup {
            role: UniformRole::Stage(ShaderStage::Compute),
            struct_name: Ustr::from("SaxpyParams"),
            members: vec![(Ustr::from("a"), WireKind::I32), (Ustr::from("n"), WireKind::I32)],
        }],
        compute: Some(ComputeInterfacePlan {
            resources: vec![
                LogicalComputeResource {
                    name: Ustr::from("x"),
                    shape: ComputeResourceShape::Array(ComputeValueKind::I32),
                    address_space: ComputeAddressSpace::Storage,
                    access: ComputeAccess::Read,
                },
                LogicalComputeResource {
                    name: Ustr::from("out"),
                    shape: ComputeResourceShape::Array(ComputeValueKind::I32),
                    address_space: ComputeAddressSpace::Storage,
                    access: ComputeAccess::ReadWrite,
                },
            ],
            workgroup: Vec::new(),
            builtins: vec![ComputeBuiltin::GlobalInvocationId],
            workgroup_size: ComputeWorkgroupSize { x: 64, y: 1, z: 1 },
            dispatch: ComputeDispatchContract::Runtime { dimensions: 1 },
        }),
        fragment_output: FragmentOutput::None,
        ..Default::default()
    }
}

fn rect_loss_interface_fixture() -> ShaderInterfacePlan {
    ShaderInterfacePlan {
        stages: vec![ShaderStage::Compute],
        uniform_groups: vec![LogicalUniformGroup {
            role: UniformRole::Stage(ShaderStage::Compute),
            struct_name: Ustr::from("RectLossParams"),
            members: vec![
                (Ustr::from("width"), WireKind::I32),
                (Ustr::from("region_x"), WireKind::I32),
                (Ustr::from("region_y"), WireKind::I32),
                (Ustr::from("region_w"), WireKind::I32),
                (Ustr::from("region_h"), WireKind::I32),
            ],
        }],
        compute: Some(ComputeInterfacePlan {
            resources: vec![
                LogicalComputeResource {
                    name: Ustr::from("backdrop"),
                    shape: ComputeResourceShape::Array(ComputeValueKind::U32),
                    address_space: ComputeAddressSpace::Storage,
                    access: ComputeAccess::Read,
                },
                LogicalComputeResource {
                    name: Ustr::from("reference"),
                    shape: ComputeResourceShape::Array(ComputeValueKind::U32),
                    address_space: ComputeAddressSpace::Storage,
                    access: ComputeAccess::Read,
                },
                LogicalComputeResource {
                    name: Ustr::from("batch"),
                    shape: ComputeResourceShape::Array(ComputeValueKind::U32),
                    address_space: ComputeAddressSpace::Storage,
                    access: ComputeAccess::Read,
                },
                LogicalComputeResource {
                    name: Ustr::from("loss"),
                    shape: ComputeResourceShape::Array(ComputeValueKind::AtomicU32),
                    address_space: ComputeAddressSpace::Storage,
                    access: ComputeAccess::ReadWrite,
                },
            ],
            workgroup: Vec::new(),
            builtins: vec![
                ComputeBuiltin::GlobalInvocationId,
                ComputeBuiltin::LocalInvocationId,
                ComputeBuiltin::WorkgroupId,
            ],
            workgroup_size: ComputeWorkgroupSize { x: 64, y: 1, z: 1 },
            dispatch: ComputeDispatchContract::Runtime { dimensions: 2 },
        }),
        fragment_output: FragmentOutput::None,
        ..Default::default()
    }
}

#[test]
fn rect_loss_neutral_plan_has_ordered_resources_builtins_and_2d_dispatch() {
    let abi = WgslAbiPlan::compute(&rect_loss_interface_fixture(), "rect_loss_compute", Span::default())
        .expect("rect-loss neutral interface is representable without source syntax");
    let compute = abi.compute.as_ref().unwrap();
    assert_eq!(compute.dispatch_dimensions, 2);
    assert_eq!(compute.workgroup_size, [64, 1, 1]);
    assert_eq!(compute.builtins.len(), 3);
    assert_eq!(compute.resources.len(), 4, "the complete storage population is examined");
    assert_eq!(compute.resources.iter().map(|resource| resource.binding).collect::<Vec<_>>(), [0, 1, 2, 3]);
    assert_eq!(compute.resources[3].element, skull_wgsl_abi::WgslStorageElementKind::AtomicU32);
    assert_eq!(abi.uniform_blocks[0].binding, 4);
}

#[test]
fn compute_abi_preserves_unsigned_storage_without_reinterpreting_it() {
    let mut iface = compute_fixture();
    iface.compute.as_mut().unwrap().resources[0].shape =
        ComputeResourceShape::Array(ComputeValueKind::U32);

    WgslAbiPlan::compute(&iface, "rect_loss_compute", Span::default())
        .expect("neutral U32 storage must have a physical ABI representation");
}

#[test]
fn compute_abi_carries_multidimensional_dispatch_and_builtin_delivery() {
    let mut iface = compute_fixture();
    let compute = iface.compute.as_mut().unwrap();
    compute.builtins = vec![
        ComputeBuiltin::GlobalInvocationId,
        ComputeBuiltin::LocalInvocationId,
        ComputeBuiltin::WorkgroupId,
    ];
    compute.dispatch = ComputeDispatchContract::Runtime { dimensions: 2 };

    WgslAbiPlan::compute(&iface, "rect_loss_compute", Span::default())
        .expect("neutral 2D dispatch and invocation builtins must have an ABI representation");
}

#[test]
fn compute_abi_preserves_atomic_output_element_identity() {
    let mut iface = compute_fixture();
    let output = &mut iface.compute.as_mut().unwrap().resources[1];
    output.shape = ComputeResourceShape::Array(ComputeValueKind::AtomicU32);

    let abi = WgslAbiPlan::compute(&iface, "rect_loss_compute", Span::default())
        .expect("atomic output must retain its element identity in the ABI");
    assert_eq!(
        abi.compute.as_ref().unwrap().resources[1].element,
        skull_wgsl_abi::WgslStorageElementKind::AtomicU32
    );
}

#[test]
fn compute_abi_retains_four_ordered_storage_resources() {
    let mut iface = compute_fixture();
    iface.compute.as_mut().unwrap().resources.push(LogicalComputeResource {
        name: Ustr::from("scratch_input"),
        shape: ComputeResourceShape::Array(ComputeValueKind::I32),
        address_space: ComputeAddressSpace::Storage,
        access: ComputeAccess::Read,
    });
    iface.compute.as_mut().unwrap().resources.push(LogicalComputeResource {
        name: Ustr::from("scratch_output"),
        shape: ComputeResourceShape::Array(ComputeValueKind::I32),
        address_space: ComputeAddressSpace::Storage,
        access: ComputeAccess::ReadWrite,
    });

    let abi = WgslAbiPlan::compute(&iface, "rect_loss_compute", Span::default())
        .expect("four ordinary storage resources are already representable");
    assert_eq!(
        abi.compute
            .as_ref()
            .unwrap()
            .resources
            .iter()
            .map(|resource| resource.name.as_str())
            .collect::<Vec<_>>(),
        ["x", "out", "scratch_input", "scratch_output"],
        "the fixture must expose the actual declaration order"
    );
}

#[test]
fn compute_abi_round_trips_resources_workgroup_and_builtin() {
    let plan = WgslAbiPlan::compute(&compute_fixture(), "saxpy_compute", Span::default())
        .expect("compute ABI projection");
    assert_eq!(plan.compute.as_ref().unwrap().resources[0].binding, 0);
    assert_eq!(plan.compute.as_ref().unwrap().resources[1].binding, 1);
    assert_eq!(plan.uniform_blocks[0].binding, 2);
    let wire = plan.wire();
    assert!(wire.contains("artifact compute"));
    assert!(wire.contains("compute_entry saxpy_compute"));
    assert!(wire.contains("workgroup 64 1 1"));
    assert!(wire.contains("builtin global_invocation_id"));
    assert!(wire.contains("storage 0 x read i32"));
    assert!(wire.contains("storage 1 out read_write i32"));
    let parsed = WgslAbi::parse(&wire).expect("compute ABI parser");
    assert_eq!(parsed.artifact, skull_wgsl_abi::WgslArtifactKind::Compute);
    assert_eq!(parsed.compute.unwrap().resources.len(), 2);
    assert_eq!(wire, plan.wire(), "compute wire order is deterministic");
}

#[test]
fn compute_projection_mutations_are_rejected_against_the_neutral_plan() {
    let iface = compute_fixture();
    let mut workgroup = WgslAbiPlan::compute(&iface, "saxpy_compute", Span::default()).unwrap();
    workgroup.compute.as_mut().unwrap().workgroup_size[0] = 32;
    assert!(workgroup.validate(&iface, Span::default()).is_err());

    let mut access = WgslAbiPlan::compute(&iface, "saxpy_compute", Span::default()).unwrap();
    access.compute.as_mut().unwrap().resources[0].access = skull_wgsl_abi::WgslStorageAccess::ReadWrite;
    assert!(access.validate(&iface, Span::default()).is_err());

    let mut binding = WgslAbiPlan::compute(&iface, "saxpy_compute", Span::default()).unwrap();
    binding.compute.as_mut().unwrap().resources[1].binding = 0;
    assert!(binding.validate(&iface, Span::default()).is_err());
}

fn explicit_fixture(arrays: Vec<ArrayMemberDecl>) -> ShaderInterfacePlan {
    ShaderInterfacePlan {
        stages: vec![ShaderStage::Vertex, ShaderStage::Fragment],
        vertex_inputs: vec![LogicalVertexInput {
            struct_name: Ustr::from("Vtx"),
            rate: VertexRate::PerVertex,
            attrs: vec![(Ustr::from("pos"), AttrKind::F32x2)],
        }],
        varyings: vec![LogicalVarying {
            name: Ustr::from("uv"),
            kind: WireKind::F32x2,
            flat: false,
        }],
        array_groups: vec![LogicalArrayGroup {
            stage: ShaderStage::Fragment,
            owner: Ustr::from("frag"),
            members: arrays,
        }],
        textures: Some(LogicalTextureGroup {
            struct_name: Some(Ustr::from("Tex")),
            samplers: vec![(Ustr::from("scene"), SamplerKind::Float)],
            stages: vec![ShaderStage::Fragment],
        }),
        fragment_output: FragmentOutput::Single(WireKind::F32x4),
        ..Default::default()
    }
}

fn decl(name: &str, elem: WireKind, capacity: u32) -> ArrayMemberDecl {
    ArrayMemberDecl {
        name: Ustr::from(name),
        elem,
        capacity,
        span: Span::default(),
    }
}

/// `@size(N)` is program text, so `N * stride` is source-controlled and must be
/// computed wide and refused before it narrows -- a wrap would land on a small
/// size that passes the very budget it evades. The WGSL lane does not recompute
/// it: it delegates to the one layout authority, which is what this asserts.
#[test]
fn an_oversized_uniform_array_is_rejected_rather_than_narrowed() {
    let plan = WgslAbiPlan::explicit(
        &explicit_fixture(vec![decl("lights", WireKind::F32x4, 16)]),
        "vertex_main",
        "fragment_main",
        Span::default(),
    )
    .expect("an in-budget array is admitted");
    assert_eq!(plan.array_blocks[0].size, 256);
    assert_eq!(plan.array_blocks[0].members[0].stride, 16);
    // The budget is a size the block may REACH: 1024 * 16 is exactly it.
    let edge = WgslAbiPlan::explicit(
        &explicit_fixture(vec![decl("lights", WireKind::F32x4, 1024)]),
        "vertex_main",
        "fragment_main",
        Span::default(),
    )
    .expect("the block may reach the budget");
    assert_eq!(edge.array_blocks[0].size, 16384);

    // 2^28 elements at stride 16 is 2^32 bytes: exactly the product that wraps
    // to a passing size in 32 bits.
    for capacity in [1u32 << 28, 1025, u32::MAX] {
        let error = WgslAbiPlan::explicit(
            &explicit_fixture(vec![decl("lights", WireKind::F32x4, capacity)]),
            "vertex_main",
            "fragment_main",
            Span::default(),
        )
        .expect_err("an over-budget `@size` must be rejected");
        assert!(
            error.message.contains("uniform array block"),
            "capacity {capacity}: {}",
            error.message
        );
    }

    // A `Vec3` array has no consistent stride and is refused by both lanes.
    let error = WgslAbiPlan::explicit(
        &explicit_fixture(vec![decl("bad", WireKind::F32x3, 4)]),
        "vertex_main",
        "fragment_main",
        Span::default(),
    )
    .expect_err("a Vec3 array element must be rejected");
    assert!(error.message.contains("Vec3"), "{}", error.message);
}
