use skull_mir::mir::{Inst, MirFunc};
use skull_transform::scope::ScopeTree;

/// Lower `fn_name` from `src` to raw MIR (no stdlib -- avoid arithmetic, which
/// routes through a stdlib op fn). Captures are about cell access shape, not
/// operations, so `() => x` / `n = 5` suffice.
fn lower(src: &str, fn_name: &str) -> MirFunc {
    let ast = skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse");
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut [ast]).expect("semantic_pass");
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("module id");
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| skull_mir::mangle::declared_name(f.name.as_str()) == fn_name)
        .expect("function by name");
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
        .expect("concrete test plan");
    skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &concrete.plans)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message))
}

fn rewrite_last_store_to_dest_write(
    func: &mut MirFunc,
    cell: comfy_la_arena::Idx<skull_mir::mir::Cell>,
) {
    let last_store = func
        .insts
        .iter()
        .filter(|(_, i)| matches!(i, Inst::Store { cell: c, .. } if *c == cell))
        .map(|(id, _)| id)
        .next_back()
        .expect("a Store of the cell");
    for (_, b) in func.blocks.iter_mut() {
        b.insts.retain(|&i| i != last_store);
    }
    let call = func
        .insts
        .iter()
        .filter(|(_, i)| matches!(i, Inst::CallMulti { .. }))
        .map(|(id, _)| id)
        .next_back()
        .expect("a CallMulti");
    let Inst::CallMulti { dests, .. } = &mut func.insts[call] else {
        unreachable!()
    };
    dests[0] = cell;
}

/// A captured cell whose reassignment is a `CallMulti` DEST write (no second
/// `Inst::Store`): write counting must see dests or the cell wrongly
/// classifies single-assignment by-value.
#[test]
fn dest_written_captures_box_from_either_scope() {
    for (name, source) in [
        (
            "after capture",
            "value V { a: int, b: int }\n\
             fn g() -> V { return V { a: 1, b: 2 } }\n\
             fn f() -> fn() -> int { let v = g() let h = () => v.a v = g() return h }",
        ),
        (
            "inside lambda",
            "value V { a: int, b: int }\n\
             fn g() -> V { return V { a: 1, b: 2 } }\n\
             fn f() -> fn() -> int { let v = g() let h = () => { v = g() return v.a } return h }",
        ),
    ] {
        let mut func = lower(source, "f");
        let tree = ScopeTree::build(&func);
        let own = tree.analyze(&func);
        let lam = tree
            .scopes
            .iter()
            .position(|scope| scope.lambda.is_some())
            .unwrap();
        assert_eq!(own.captures[lam].len(), 1, "{name}: one scalar capture");
        let va = *own.captures[lam]
            .iter()
            .next()
            .expect("captured scalar cell");
        rewrite_last_store_to_dest_write(&mut func, va);

        let tree = ScopeTree::build(&func);
        let own = tree.analyze(&func);
        let boxed = tree.boxed_cells(&own);
        assert!(own.captures[lam].contains(&va), "{name}: capture remains");
        assert!(
            boxed.contains(&va),
            "{name}: dest write must box; {boxed:?}"
        );
    }
}
