use std::sync::Arc;

use bone_core::workspace::Workspace;
use skull_mir::{mir::MirFunc, monoty::MonoInterner};
use ustr::Ustr;

pub(crate) fn lower(src: &str, fn_name: &str) -> MirFunc {
    lower_fixture(src, "<test>", fn_name).2
}

pub(crate) fn lower_fixture(
    src: &str,
    file: &str,
    fn_name: &str,
) -> (Workspace, Arc<MonoInterner>, MirFunc) {
    let file = Ustr::from(file);
    let ast = skull_parser::parse_module_clean(src, file).expect("parse");
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut [ast]).expect("semantic_pass");
    let mid = ws.module_ids[&file];
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| skull_mir::mangle::declared_name(f.name.as_str()) == fn_name)
        .expect("function");
    let root = skull_mir::mir::FuncRef {
        module: mid,
        func: fid,
    };
    let interner = Arc::new(MonoInterner::new());
    let graph =
        skull_graph::instance_graph::build_in(&ws, vec![root], false, Arc::clone(&interner));
    let concrete = skull_graph::concrete_body::build(&ws, &graph);
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(Arc::clone(&interner));
    let plan = concrete
        .plans
        .get(&skull_mir::instance::Instance::base(root))
        .expect("test fixture concrete plan");
    let body = skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &concrete.plans)
        .expect("lower");
    (ws, interner, body)
}
