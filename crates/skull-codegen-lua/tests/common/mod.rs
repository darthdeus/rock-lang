use std::sync::Arc;

use skull_graph::instance_graph;
use skull_mir::instance::Instance;
use skull_mir::lower::{ScalarCtx, lower_instance_with_plans};
use skull_mir::mir::{FuncRef, MirFunc};
use skull_mir::monoty::MonoInterner;

/// Panic unless `luajit` runs. Execution-oracle tests call this up front
/// instead of skipping when the binary is absent: a host without luajit must
/// report red for tests that never ran, never green.
// The `emit` target's execution oracles call this; `coverage` shares this file but never executes.
pub fn require_luajit() {
    if std::process::Command::new("luajit")
        .arg("-v")
        .output()
        .is_err()
    {
        panic!("luajit is required for this execution test but was not found on PATH");
    }
}

pub fn lower_test_ref(
    ws: &bone_semantic::inference::Workspace,
    mid: bone_semantic::prelude::ModuleId,
    fid: comfy_la_arena::Idx<bone_semantic::hir::Function>,
) -> Result<(MirFunc, Arc<MonoInterner>), skull_parser::CompilerError> {
    lower_test_ref_with_graph(ws, mid, fid).map(|(func, interner, _)| (func, interner))
}

/// [`lower_test_ref`] plus the instance graph it lowered through, for a test
/// that needs the call edges as well as the body (the tail-call shape reads
/// them through `skull_codegen_lua::callees_on_cycles`).
pub fn lower_test_ref_with_graph(
    ws: &bone_semantic::inference::Workspace,
    mid: bone_semantic::prelude::ModuleId,
    fid: comfy_la_arena::Idx<bone_semantic::hir::Function>,
) -> Result<(MirFunc, Arc<MonoInterner>, instance_graph::InstanceGraph), skull_parser::CompilerError>
{
    let root = FuncRef {
        module: mid,
        func: fid,
    };
    let interner = Arc::new(MonoInterner::new());
    let graph = instance_graph::build_in(ws, vec![root], false, Arc::clone(&interner));
    let scx = ScalarCtx::new(ws).sharing(Arc::clone(&interner));
    let concrete = skull_graph::concrete_body::build(ws, &graph);
    let plan = concrete
        .plans
        .get(&Instance::base(FuncRef {
            module: mid,
            func: fid,
        }))
        .ok_or_else(|| {
            skull_parser::CompilerError::new_no_location("no concrete plan for test root")
        })?;
    let func = lower_instance_with_plans(ws, &scx, plan, &concrete.plans)?;
    Ok((func, interner, graph))
}

/// The functions on a call cycle in `graph`, spelled the way production spells
/// it (`skull::mir_emit::recursive_callees`).
pub fn cycle_callees(
    graph: &instance_graph::InstanceGraph,
) -> bone_semantic::prelude::FxHashSet<FuncRef> {
    skull_codegen_lua::callees_on_cycles(
        graph
            .contributions
            .values()
            .flat_map(|contribution| contribution.edges.iter())
            .map(|edge| (edge.from.func, edge.to.func)),
    )
}

/// Every function's call-graph component number in `graph`, spelled the way
/// production spells it (`skull::mir_emit::call_sccs`).
pub fn call_sccs(
    graph: &instance_graph::InstanceGraph,
) -> bone_semantic::prelude::FxHashMap<FuncRef, u32> {
    skull_codegen_lua::call_sccs(
        graph
            .contributions
            .values()
            .flat_map(|contribution| contribution.edges.iter())
            .map(|edge| (edge.from.func, edge.to.func)),
    )
}
