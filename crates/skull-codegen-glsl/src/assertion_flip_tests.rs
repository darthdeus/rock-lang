use super::*;
use crate::fixture_workspace::workspace;

fn lower_test_instance(
    ws: &Workspace,
    fref: FuncRef,
) -> (ScalarCtx, MirFunc, Arc<ConcreteBodyPlan>) {
    let interner = Arc::new(MonoInterner::new());
    let graph = skull_graph::instance_graph::build_in(ws, vec![fref], false, Arc::clone(&interner));
    let concrete = skull_graph::concrete_body::build(ws, &graph);
    let scx = ScalarCtx::new_values_first_class(ws).sharing(Arc::clone(&interner));
    let plan = Arc::clone(
        concrete
            .plans
            .get(&Instance::base(fref))
            .expect("shader test concrete plan"),
    );
    let mf = skull_mir::lower::lower_instance_with_plans(ws, &scx, &plan, &concrete.plans)
        .expect("shader test lower");
    (scx, mf, plan)
}

fn emit_plain(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
    mf: &MirFunc,
    plan: &ConcreteBodyPlan,
) -> Result<String, CompilerError> {
    emit_fn_def_with(
        ws,
        scx.interner(),
        &ShaderInstance::plain(fref),
        mf,
        None,
        plan,
    )
}

#[test]
fn missed_int_to_float_sink_is_a_hard_emit_error() {
    let (ws, mid) = workspace("@pure fn f(i: int) -> float { return i }");
    let fref = resolve_root(&ws, mid, "f").expect("root");
    let (scx, mut mf, plan) = lower_test_instance(&ws, fref);
    let bids: Vec<_> = mf.blocks.iter().map(|(b, _)| b).collect();
    for bid in bids {
        if let Term::Return(vs) = mf.blocks[bid].term.clone() {
            let stripped = vs
                .iter()
                .map(|&v| match mf.insts[v] {
                    Inst::IntToFloat(x) => x,
                    _ => v,
                })
                .collect();
            mf.set_term(bid, Term::Return(stripped));
        }
    }

    let err = emit_plain(&ws, &scx, fref, &mf, &plan)
        .expect_err("a missed int-to-float sink must fail emission");
    assert!(err.message.contains("missed lowering sink"), "{err}");
}
