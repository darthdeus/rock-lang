use skull_mir::mir::MirFunc;

pub(crate) fn lower(src: &str, fn_name: &str) -> MirFunc {
    let ast = skull_parser::parse_module(src, ustr::Ustr::from("<test>")).expect("parse");
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut [ast]).expect("semantic_pass");
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == fn_name)
        .expect("function");
    let root = skull_mir::mir::FuncRef {
        module: mid,
        func: fid,
    };
    let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
    let graph = skull_graph::instance_graph::build_in(
        &ws,
        vec![root],
        false,
        std::sync::Arc::clone(&interner),
    );
    let concrete = skull_graph::concrete_body::build(&ws, &graph);
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(interner);
    let plan = concrete
        .plans
        .get(&skull_mir::instance::Instance::base(root))
        .expect("test fixture concrete plan");
    skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &concrete.plans).expect("lower")
}
