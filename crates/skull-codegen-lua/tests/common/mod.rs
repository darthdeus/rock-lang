use std::sync::Arc;

use skull_graph::instance_graph;
use skull_mir::instance::Instance;
use skull_mir::lower::{lower_instance_with_plans, ScalarCtx};
use skull_mir::mir::{FuncRef, MirFunc};
use skull_mir::monoty::MonoInterner;

pub fn lower_test_ref(
    ws: &bone_semantic::inference::Workspace,
    mid: bone_semantic::prelude::ModuleId,
    fid: comfy_la_arena::Idx<bone_semantic::hir::Function>,
) -> Result<(MirFunc, Arc<MonoInterner>), skull_parser::CompilerError> {
    let root = FuncRef { module: mid, func: fid };
    let interner = Arc::new(MonoInterner::new());
    let graph = instance_graph::build_in(ws, vec![root], false, Arc::clone(&interner));
    let scx = ScalarCtx::new(ws).sharing(Arc::clone(&interner));
    let concrete = skull_graph::concrete_body::build(ws, &graph);
    let plan = concrete
        .plans
        .get(&Instance::base(FuncRef { module: mid, func: fid }))
        .ok_or_else(|| skull_parser::CompilerError::new_no_location("no concrete plan for test root"))?;
    let func = lower_instance_with_plans(ws, &scx, plan, &concrete.plans)?;
    Ok((func, interner))
}
