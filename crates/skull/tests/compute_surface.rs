use bone_semantic::hir::BodyOwner;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use super::common;
use skull_codegen_shader::{ConcreteStageInput, ShaderPlannerPolicy};
use ustr::Ustr;

fn emit_errors(src: &str) -> Vec<String> {
    let ws = common::ws_with_stdlib(src);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
    match skull::mir_emit::emit_full(&ws, mid) {
        Ok(_) => Vec::new(),
        Err(error) => vec![error.message],
    }
}

fn test_mid(ws: &Workspace) -> ModuleId {
    *ws.module_ids.get(&Ustr::from("<test>")).expect("test module")
}

fn body_id(ws: &Workspace, name: &str) -> (ModuleId, BodyOwner) {
    let mid = test_mid(ws);
    for (fid, function) in ws.module(mid).functions.iter() {
        if function.name.as_str() == name {
            return (mid, BodyOwner::Function(fid));
        }
    }
    panic!("no function named {name}");
}

const SAXPY: &str = r#"
value SaxpyParams { a: int, n: int }

@shader(compute)
@workgroup(64)
fn saxpy(
    @storage(read) x: StorageBuffer<int>,
    @storage(read) y: StorageBuffer<int>,
    @storage(read_write) out: StorageBuffer<int>,
    @uniform params: SaxpyParams,
) {
    let i = global_invocation_id().x
    if i >= params.n { return }
    storage_store(out, i,
        params.a * storage_load(x, i) + storage_load(y, i))
}

global SAXPY: string = rock_compute(saxpy)
"#;

#[test]
fn valid_saxpy_compute_declaration_is_admitted() {
    let errors = common::bone_rejections(SAXPY);
    assert!(errors.is_empty(), "valid compute declaration was rejected: {errors:#?}");
    let ws = common::ws_with_stdlib(SAXPY);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
    let (_emit, cache) = skull::mir_emit::emit_full(&ws, mid).expect("full compute emit");
    let view = cache.shader_view();
    assert_eq!(view.compute_views.len(), 1, "one compute root is retained");
    let compute = &view.compute_views[0];
    assert_eq!(compute.stage.stage, skull_mir::shader_ir::ShaderStage::Compute);
    assert_eq!(
        ws.module(compute.stage.entry.func.module).functions[compute.stage.entry.func.func].name.as_str(),
        "saxpy"
    );
    assert_eq!(compute.stage.retained.len(), 1, "entry plus no helper declarations");
    assert_eq!(compute.stage.builtins.len(), 3, "three compute intrinsics are retained as builtins");
    assert!(compute.concrete_plans.contains_key(&compute.stage.entry));

    struct NeutralComputePolicy;
    impl ShaderPlannerPolicy for NeutralComputePolicy {
        fn inline_call(&self, function: &bone_semantic::hir::Function) -> bool {
            skull_mir::intrinsics::is_intrinsic(function)
                || (function.name.as_str().starts_with("__op_") && function.param_decls.len() == 2)
        }
        fn backend_name(&self) -> &'static str { "compute-test" }
    }
    let scx = skull_mir::lower::ScalarCtx::new_values_first_class(&ws)
        .sharing(cache.graph().interner().clone());
    let input = ConcreteStageInput {
        stage: compute.stage.stage,
        entry: compute.stage.entry.clone(),
        retained: compute.stage.retained.iter().cloned().collect(),
        helpers: compute.stage.helpers.iter().cloned().collect(),
        builtins: compute.stage.builtins.iter().cloned().collect(),
        order: compute.stage.order.clone(),
        dependencies: compute.stage.dependencies.clone(),
        concrete_plans: compute.concrete_plans.clone(),
    };
    let artifact = skull_codegen_shader::plan_compute_from_concrete(
        &ws,
        &scx,
        &input,
        &NeutralComputePolicy,
    )
    .expect("retained compute plan");
    let compute_iface = artifact.interface.compute.as_ref().expect("compute interface");
    assert_eq!(
        compute_iface.resources.iter().map(|resource| resource.name.as_str()).collect::<Vec<_>>(),
        ["x", "y", "out"]
    );
    assert_eq!(compute_iface.resources[0].shape, skull_mir::shader_ir::ComputeResourceShape::Array(skull_mir::shader_ir::ComputeValueKind::I32));
    assert_eq!(compute_iface.resources[0].access, skull_mir::shader_ir::ComputeAccess::Read);
    assert_eq!(compute_iface.resources[2].access, skull_mir::shader_ir::ComputeAccess::ReadWrite);
    assert_eq!(compute_iface.builtins, [skull_mir::shader_ir::ComputeBuiltin::GlobalInvocationId]);
    assert_eq!(compute_iface.workgroup_size, skull_mir::shader_ir::ComputeWorkgroupSize { x: 64, y: 1, z: 1 });
    assert_eq!(compute_iface.dispatch, skull_mir::shader_ir::ComputeDispatchContract::Runtime { dimensions: 1 });
    assert_eq!(artifact.interface.uniform_groups.len(), 1);
    assert_eq!(artifact.interface.uniform_groups[0].role, skull_mir::shader_ir::UniformRole::Stage(skull_mir::shader_ir::ShaderStage::Compute));

    let mut missing = (*compute.concrete_plans).clone();
    assert!(missing.remove(&compute.stage.entry).is_some());
    let rejected = match skull_codegen_shader::plan_compute_from_concrete(
        &ws,
        &scx,
        &ConcreteStageInput {
            concrete_plans: std::sync::Arc::new(missing),
            ..input
        },
        &skull_codegen_wgsl::WgslShaderPolicy,
    ) {
        Ok(_) => panic!("a compute stage without its mandatory concrete plan must reject"),
        Err(error) => error,
    };
    assert!(rejected.message.contains("no concrete plan"), "{rejected}");
}

#[test]
fn dropping_a_retained_compute_helper_rejects_graph_view_disagreement() {
    let source = SAXPY
        .replace(
            "value SaxpyParams { a: int, n: int }",
            "value SaxpyParams { a: int, n: int }\n\nfn saxpy_double(v: int) -> int { return v * 2 }",
        )
        .replace(
            "params.a * storage_load(x, i) + storage_load(y, i)",
            "params.a * saxpy_double(storage_load(x, i)) + storage_load(y, i)",
        );
    let ws = common::ws_with_stdlib(&source);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
    let (_emit, cache) = skull::mir_emit::emit_full(&ws, mid).expect("helper compute emit");
    let compute = &cache.shader_view().compute_views[0];
    assert!(!compute.stage.helpers.is_empty(), "fixture did not retain a helper");
    let mut input = ConcreteStageInput {
        stage: compute.stage.stage,
        entry: compute.stage.entry.clone(),
        retained: compute.stage.retained.iter().cloned().collect(),
        helpers: compute.stage.helpers.iter().cloned().collect(),
        builtins: compute.stage.builtins.iter().cloned().collect(),
        order: compute.stage.order.clone(),
        dependencies: compute.stage.dependencies.clone(),
        concrete_plans: compute.concrete_plans.clone(),
    };
    input.helpers.pop();
    let rejected = match skull_codegen_shader::plan_compute_from_concrete(
        &ws,
        &skull_mir::lower::ScalarCtx::new_values_first_class(&ws)
            .sharing(cache.graph().interner().clone()),
        &input,
        &skull_codegen_wgsl::WgslShaderPolicy,
    ) {
        Ok(_) => panic!("a helper population disagreement must reject"),
        Err(error) => error,
    };
    assert!(rejected.message.contains("helper order") || rejected.message.contains("entry plus helpers"), "{rejected}");
}

#[test]
fn saxpy_compute_emits_deterministic_wgsl_and_v3_abi() {
    let ws = common::ws_with_stdlib(SAXPY);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
    let (_emit, cache) = skull::mir_emit::emit_full(&ws, mid).expect("compute emit admission");
    let compute = &cache.shader_view().compute_views[0];
    let input = ConcreteStageInput {
        stage: compute.stage.stage,
        entry: compute.stage.entry.clone(),
        retained: compute.stage.retained.iter().cloned().collect(),
        helpers: compute.stage.helpers.iter().cloned().collect(),
        builtins: compute.stage.builtins.iter().cloned().collect(),
        order: compute.stage.order.clone(),
        dependencies: compute.stage.dependencies.clone(),
        concrete_plans: compute.concrete_plans.clone(),
    };
    let scx = skull_mir::lower::ScalarCtx::new_values_first_class(&ws)
        .sharing(cache.graph().interner().clone());
    let neutral = skull_codegen_shader::plan_compute_from_concrete(
        &ws,
        &scx,
        &input,
        &skull_codegen_wgsl::WgslShaderPolicy,
    )
    .expect("compute neutral plan");
    let logical_entry = neutral.entry.inst.symbol(&ws, &scx.interner());
    let abi = skull_codegen_wgsl::WgslAbiPlan::compute(
        &neutral.interface,
        &format!("{logical_entry}_compute"),
        bone_semantic::prelude::Span::default(),
    )
    .expect("compute ABI");
    assert_eq!(abi.compute.as_ref().unwrap().resources.iter().map(|resource| resource.name.as_str()).collect::<Vec<_>>(), ["x", "y", "out"]);
    assert_eq!(abi.compute.as_ref().unwrap().resources[0].binding, 0);
    assert_eq!(abi.compute.as_ref().unwrap().resources[2].binding, 2);
    let emission = skull_codegen_wgsl::emit_compute_shader(
        &ws,
        &neutral.ir,
        &neutral.interface,
        &abi,
        bone_semantic::prelude::Span::default(),
    )
    .expect("compute WGSL emission");
    assert!(emission.source.contains("@compute @workgroup_size(64, 1, 1)"));
    assert!(emission.source.contains("@builtin(global_invocation_id) global_id: vec3<u32>"));
    assert!(emission.source.contains("var<storage, read> s_x: array<i32>"));
    assert!(emission.source.contains("var<storage, read_write> s_out: array<i32>"));
    assert!(emission.source.contains("s_out[i] = ((u_SaxpyParams.a * s_x[i]) + s_y[i])"));
    let wire = abi.wire();
    assert!(wire.starts_with("WGSL_ABI_V3\n"));

    let mut wrong_symbol = abi.clone();
    wrong_symbol.compute.as_mut().unwrap().entry = "wrong_instance".into();
    let rejected = match skull_codegen_wgsl::emit_compute_shader(
        &ws,
        &neutral.ir,
        &neutral.interface,
        &wrong_symbol,
        bone_semantic::prelude::Span::default(),
    ) {
        Ok(_) => panic!("a physical entry symbol detached from the retained instance must reject"),
        Err(error) => error,
    };
    assert!(rejected.message.contains("retained instance"), "{rejected}");
}

#[test]
fn wgpu_renderer_registers_the_compute_artifact_from_the_view() {
    let ws = common::ws_with_stdlib(SAXPY);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
    let (emit, cache) = skull::mir_emit::emit_full_in_for_renderer(
        &ws,
        mid,
        None,
        skull::mir_emit::RendererTarget::Wgpu,
    )
    .expect("compute WGPU registration");
    assert_eq!(cache.shader_view().compute_views.len(), 1);
    assert_eq!(emit.lua.matches("__rock_wgpu_compute_register(").count(), 1);
    assert!(emit.lua.contains("__rock_wgpu_compute_register(\"saxpy\""));
}

#[test]
fn compute_body_edit_reloads_the_compiler_artifact_through_the_overlay() {
    let ws1 = common::ws_with_stdlib(SAXPY);
    let mid1 = test_mid(&ws1);
    let (_full, mut cache) = skull::mir_emit::emit_full_in_for_renderer(
        &ws1,
        mid1,
        None,
        skull::mir_emit::RendererTarget::Wgpu,
    )
    .expect("initial compute emit");
    let source2 = SAXPY.replace(
        "params.a * storage_load(x, i) + storage_load(y, i)",
        "(params.a + 1) * storage_load(x, i) + storage_load(y, i)",
    );
    let ws2 = common::ws_with_stdlib(&source2);
    let overlay = skull::mir_emit::emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "saxpy")])
        .expect("compute body overlay");
    assert_eq!(overlay.lua.matches("__rock_wgpu_compute_register(\"saxpy\"").count(), 1);
    assert!(overlay.lua.contains("(u_SaxpyParams.a + 1)"), "edited compute source missing:\n{}", overlay.lua);
}

#[test]
fn compute_root_addition_and_removal_request_a_full_reload() {
    let ws = common::ws_with_stdlib(SAXPY);
    let mid = test_mid(&ws);
    let (_full, cache) = skull::mir_emit::emit_full_in_for_renderer(
        &ws,
        mid,
        None,
        skull::mir_emit::RendererTarget::Wgpu,
    )
    .expect("initial compute emit");

    let removed = common::ws_with_stdlib(&SAXPY.replace("global SAXPY: string = rock_compute(saxpy)", ""));
    assert!(cache.shader_registration_keys_changed(&removed).expect("removed key set"));

    let added_source = format!(
        "{}\n\n@shader(compute)\n@workgroup(64)\nfn saxpy_extra(\n    @storage(read) x: StorageBuffer<int>,\n    @storage(read) y: StorageBuffer<int>,\n    @storage(read_write) out: StorageBuffer<int>,\n    @uniform params: SaxpyParams,\n) {{\n    let i = global_invocation_id().x\n    if i >= params.n {{ return }}\n    storage_store(out, i, params.a * storage_load(x, i) + storage_load(y, i))\n}}\n\nglobal SAXPY_EXTRA: string = rock_compute(saxpy_extra)\n",
        SAXPY
    );
    let added = common::ws_with_stdlib(&added_source);
    assert!(cache.shader_registration_keys_changed(&added).expect("added key set"));
}

#[test]
fn missing_compute_root_is_reported_by_the_compute_population() {
    let source = SAXPY.replace("global SAXPY: string = rock_compute(saxpy)", "");
    let ws = common::ws_with_stdlib(&source);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
    let (_emit, cache) = skull::mir_emit::emit_full(&ws, mid).expect("emit without compute root");
    assert!(cache.shader_view().compute_views.is_empty(), "omitted root was retained");
}

#[test]
fn an_indirect_compute_root_is_rejected() {
    let source = SAXPY.replace(
        "global SAXPY: string = rock_compute(saxpy)",
        "global ROOT_FN: any = saxpy\nglobal ROOT: string = rock_compute(ROOT_FN)",
    );
    let errors = emit_errors(&source);
    assert!(
        errors.iter().any(|error| error.contains("direct reference")),
        "indirect compute root must reject at its argument: {errors:#?}"
    );
}

#[test]
fn zero_workgroup_size_is_rejected() {
    let source = SAXPY.replace("@workgroup(64)", "@workgroup(0)");
    let errors = emit_errors(&source);
    assert!(
        errors.iter().any(|error| error.contains("workgroup size")),
        "zero workgroup size must be rejected: {errors:#?}"
    );
}

#[test]
fn storage_store_requires_read_write_access() {
    let source = SAXPY.replace("@storage(read_write) out", "@storage(read) out");
    let errors = emit_errors(&source);
    assert!(
        errors.iter().any(|error| error.contains("storage_store") && error.contains("Read")),
        "store through a read resource must reject: {errors:#?}"
    );
}

#[test]
fn storage_load_rejects_write_only_access() {
    let source = SAXPY.replace("@storage(read) x", "@storage(write) x");
    let errors = emit_errors(&source);
    assert!(
        errors.iter().any(|error| error.contains("storage_load") && error.contains("Write")),
        "load through a write-only resource must reject: {errors:#?}"
    );
}

#[test]
fn graphics_entries_cannot_declare_compute_storage() {
    let source = SAXPY
        .replace("@shader(compute)", "@shader(fragment)")
        .replace("rock_compute(saxpy)", "rock_shader(saxpy)");
    let errors = emit_errors(&source);
    assert!(
        errors.iter().any(|error| error.contains("compute") && error.contains("storage")),
        "graphics storage must reject: {errors:#?}"
    );
}

#[test]
fn global_invocation_id_is_gpu_only() {
    let source = r#"
fn cpu() { let id = global_invocation_id() }
"#;
    let errors = emit_errors(source);
    assert!(
        errors.iter().any(|error| error.contains("GPU-only")),
        "CPU invocation of global_invocation_id must reject: {errors:#?}"
    );
}
