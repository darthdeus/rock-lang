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
