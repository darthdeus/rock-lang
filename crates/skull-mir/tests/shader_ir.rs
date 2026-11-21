use comfy_la_arena::Idx;
use skull_mir::gpu_layout::{SamplerKind, WireKind};
use skull_mir::mir::FuncRef;
use skull_mir::shader_ir::{
    LogicalTextureGroup, LogicalUniformGroup, NominalType, ShaderIR, ShaderInstance,
    ShaderInterfacePlan, ShaderStage, ShaderStageIr, UniformRole,
};
use ustr::Ustr;

fn fref(module: u32, func: u32) -> FuncRef {
    FuncRef {
        module: bone_core::prelude::ModuleId::new(module),
        func: Idx::from_raw(func.into()),
    }
}

fn inst(module: u32, func: u32, tag: Option<skull_mir::monoty::MonoTy>) -> ShaderInstance {
    static ARENA: std::sync::LazyLock<skull_mir::monoty::MonoInterner> =
        std::sync::LazyLock::new(skull_mir::monoty::MonoInterner::new);
    ShaderInstance::new(skull_mir::instance::Instance::intern(
        &ARENA,
        fref(module, func),
        tag.into_iter().collect(),
    ))
}

fn stage_ir(entry: ShaderInstance, order: Vec<ShaderInstance>) -> ShaderStageIr {
    ShaderStageIr {
        stage: ShaderStage::Fragment,
        entry,
        body: skull_mir::mir::MirFunc::new(Ustr::from("entry")),
        order,
        bodies: Default::default(),
        const_globals: Default::default(),
    }
}

#[test]
fn a_program_reload_set_dedups_across_stages() {
    use skull_mir::monoty::MonoTy;
    let shared = fref(0, 7);
    let vert = stage_ir(
        ShaderInstance::plain(fref(0, 1)),
        vec![inst(0, 7, Some(MonoTy::Float))],
    );
    let mut frag = stage_ir(
        ShaderInstance::plain(fref(0, 2)),
        vec![inst(0, 7, Some(MonoTy::Int))],
    );
    frag.stage = ShaderStage::Fragment;
    let ir = ShaderIR {
        interner: std::sync::Arc::new(skull_mir::monoty::MonoInterner::new()),
        stages: vec![vert, frag],
    };

    let decls = ir.declarations();
    assert_eq!(
        decls.iter().filter(|f| **f == shared).count(),
        1,
        "a helper both stages reach is listed twice: {decls:?}"
    );
    assert_eq!(decls.len(), 3, "two entries + one shared helper: {decls:?}");
}

#[test]
fn perturbing_an_interface_member_changes_the_interface() {
    let base = ShaderInterfacePlan {
        uniform_groups: vec![LogicalUniformGroup {
            role: UniformRole::Stage(ShaderStage::Fragment),
            struct_name: nominal("U"),
            members: vec![
                (Ustr::from("a"), WireKind::F32),
                (Ustr::from("b"), WireKind::F32x4),
            ],
        }],
        ..Default::default()
    };
    let mut widened = base.clone();
    widened.uniform_groups[0].members[0].1 = WireKind::F32x4;
    assert_ne!(
        base, widened,
        "the interface plan does not distinguish its own members"
    );
}

fn nominal(name: &str) -> NominalType {
    NominalType {
        key: Ustr::from(name),
        symbol: Ustr::from(name),
    }
}

/// The runtime joins a group to its physical block by symbol; the CPU packer
/// fingerprints the same type by key.
#[test]
fn semantic_wire_spells_symbols_and_fingerprints_keys() {
    let exempt = NominalType {
        key: Ustr::from("pkg::gfx::Params"),
        symbol: Ustr::from("Params"),
    };
    let members = vec![(Ustr::from("a"), WireKind::F32)];
    let plan = ShaderInterfacePlan {
        uniform_groups: vec![LogicalUniformGroup {
            role: UniformRole::Stage(ShaderStage::Fragment),
            struct_name: exempt,
            members: members.clone(),
        }],
        ..Default::default()
    };
    let wire = plan.semantic_wire(true);
    let group = &plan.uniform_groups[0];
    assert!(
        wire.contains(&format!(
            "group fragment Params {} 1\n",
            group.semantic_fingerprint()
        )),
        "{wire}"
    );
    assert!(!wire.contains("pkg::"), "{wire}");
    assert_eq!(
        group.semantic_fingerprint(),
        skull_mir::shader_ir::semantic_uniform_group_fingerprint(exempt.key, &members)
    );
}

#[test]
fn semantic_wire_is_neutral_and_fingerprint_tracks_logical_shape() {
    let base = ShaderInterfacePlan {
        uniform_groups: vec![LogicalUniformGroup {
            role: UniformRole::Stage(ShaderStage::Fragment),
            struct_name: nominal("U"),
            members: vec![(Ustr::from("a"), WireKind::F32)],
        }],
        textures: Some(LogicalTextureGroup {
            struct_name: Some(nominal("Textures")),
            samplers: vec![
                (Ustr::from("scene"), SamplerKind::Float),
                (Ustr::from("bloom"), SamplerKind::Float),
            ],
            stages: vec![ShaderStage::Fragment],
        }),
        ..Default::default()
    };
    let wire = base.semantic_wire(true);
    for forbidden in ["offset", "binding", "std140", "WGSL", "GLSL"] {
        assert!(
            !wire.contains(forbidden),
            "semantic wire leaked {forbidden}: {wire}"
        );
    }
    assert!(
        !wire.contains("compute"),
        "graphics semantic wire changed without a compute contract: {wire}"
    );
    let mut renamed = base.clone();
    renamed.uniform_groups[0].members[0].0 = Ustr::from("b");
    assert_ne!(
        base.semantic_fingerprint(true),
        renamed.semantic_fingerprint(true)
    );

    let mut swapped = base.clone();
    swapped.textures.as_mut().unwrap().samplers.swap(0, 1);
    assert_ne!(
        base.semantic_fingerprint(true),
        swapped.semantic_fingerprint(true)
    );
    assert_ne!(
        base.semantic_fingerprint(true),
        base.semantic_fingerprint(false)
    );
}
