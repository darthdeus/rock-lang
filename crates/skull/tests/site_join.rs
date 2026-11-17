use crate::common::{
    fn_name, graph_of as graph_of_ws, key, lowered_body, prepare_body, scalar_ctx, ws_and_graph,
    ws_with_stdlib,
};
use bone_semantic::inference::Workspace;
use skull::concrete_body::ConcreteBodies;
use skull::instance_graph::{Instance, InstanceGraph};
use skull_mir::lower::ScalarCtx;
use skull_mir::mir::{Inst, MirFunc};
use skull_mir::monoty::{MonoCtor, MonoTy};

#[test]
fn spliced_iterator_headers_preserve_numeric_loop_emission() {
    let lua = crate::common::emit_module(include_str!("../../../tests/functions/site_calls.rock"));
    let caller = "iterator_headers_survive_splicing";
    let body = crate::common::function_body(&lua, caller);
    assert!(body.contains("for "), "{body}");
    assert!(crate::common::callees_named_in(&lua, caller, "sum_inner").is_empty());
}

fn concrete(ws: &Workspace) -> (InstanceGraph, ConcreteBodies, ScalarCtx) {
    let g = graph_of_ws(ws);
    let bodies = skull::concrete_body::build(ws, &g);
    let scx = scalar_ctx(ws, &g);
    (g, bodies, scx)
}

fn calls_to(ws: &Workspace, mf: &MirFunc, name: &str) -> usize {
    mf.blocks
        .iter()
        .flat_map(|(_, block)| block.insts.iter())
        .filter(|id| {
            matches!(&mf.insts[**id], Inst::Call { func, .. } | Inst::CallMulti { func, .. }
                if fn_name(ws, *func) == name)
        })
        .count()
}

const WRAP_VEC2: &str = r#"
@inline
fn wrap<T>(x: T) -> T { return id2(x) }
fn id2<T>(x: T) -> T { return x }
fn top() -> float {
    let b = wrap(Vec2 { x: 1, y: 2 })
    return b.x
}
fn main() { print("{top()}") }
"#;

/// Candidate identity comes from `callee_instances`, not from the optional
/// delivery entry: with delivery removed the concrete call still splices,
/// and the spliced sites keep the callee as owner.
#[test]
fn concrete_inline_uses_owner_when_delivery_is_absent() {
    let ws = ws_with_stdlib(WRAP_VEC2);
    let (_, bodies, scx) = concrete(&ws);
    let mut mf = lowered_body(&ws, &bodies, &scx, "top");
    let call = mf
        .insts
        .iter()
        .find_map(|(id, inst)| match inst {
            Inst::Call { func, .. } | Inst::CallMulti { func, .. }
                if fn_name(&ws, *func) == "wrap" =>
            {
                Some(id)
            }
            _ => None,
        })
        .expect("concrete wrap call");
    assert!(
        !mf.callee_instances[&call].args.is_empty(),
        "the fixture must have a concrete wrap owner"
    );
    assert!(
        mf.spec_bindings.remove(&call).is_some(),
        "the fixture must have delivery to remove"
    );
    prepare_body(&ws, &scx, &bodies, "top", &mut mf);
    assert_eq!(
        calls_to(&ws, &mf, "wrap"),
        0,
        "the concrete owner did not admit the splice"
    );
    assert!(
        mf.call_sites
            .values()
            .any(|cs| fn_name(&ws, mf.site_owners[cs.owner.0 as usize].func) == "wrap"),
        "the spliced body lost its concrete callee owner"
    );
}

/// Same-width value instances are still different ABIs: repointing the call's
/// owner from `Vec2` to a two-int `Pair` must decline the splice.
#[test]
fn concrete_inline_declines_same_width_incompatible_abi() {
    let ws = ws_with_stdlib(
        r#"
value Pair { x: int, y: int }
@inline
fn wrap<T>(x: T) -> T { return x }
fn top() -> float {
    let b = wrap(Vec2 { x: 1, y: 2 })
    return b.x
}
fn main() { print("{top()}") }
"#,
    );
    let (_, bodies, scx) = concrete(&ws);
    let mut mf = lowered_body(&ws, &bodies, &scx, "top");
    let call = mf
        .insts
        .iter()
        .find_map(|(id, inst)| match inst {
            Inst::CallMulti { func, .. } if fn_name(&ws, *func) == "wrap" => Some(id),
            _ => None,
        })
        .expect("value call must use the multi-lane ABI");
    let pair = skull_mir::instance::Instance::intern(
        scx.interner(),
        mf.callee_instances[&call].func,
        vec![MonoTy::App {
            ctor: MonoCtor::Nominal(key(&ws, "Pair")),
            params: Vec::new(),
        }],
    );
    mf.callee_instances.insert(call, pair);
    assert!(
        mf.spec_bindings.remove(&call).is_some(),
        "the negative arm must remove delivery"
    );
    prepare_body(&ws, &scx, &bodies, "top", &mut mf);
    assert_eq!(
        calls_to(&ws, &mf, "wrap"),
        1,
        "an incompatible same-width owner was spliced"
    );
}

#[test]
fn standalone_bounded_resolution_records_a_nonempty_resolved_tree() {
    let (ws, g) = ws_and_graph(
        r#"
struct Item { id: int, p: float }

fn main() {
    let xs = [Item { id: 1, p: 3.0 }, Item { id: 2, p: 2.0 }]
    let ids = xs.map(_.id).collect()
    print("{ids[0]}")
}
"#,
    );
    let bodies = skull::concrete_body::build(&ws, &g);
    let scx = scalar_ctx(&ws, &g);
    let lower = |inst: &Instance| {
        let plan = bodies.plans.get(inst)?;
        skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &bodies.plans).ok()
    };
    let (mut mf, tree) = g
        .bound_trees()
        .filter(|(candidate, tree)| {
            tree.next.values().any(|w| !w.sub.next.is_empty())
                && ws.module(candidate.func.module).functions[candidate.func.func]
                    .generic_bounds
                    .iter()
                    .any(Option::is_some)
        })
        .find_map(|(candidate, tree)| {
            let mf = lower(candidate)?;
            (mf.insts
                .iter()
                .filter(|(_, inst)| matches!(inst, Inst::CallBoundMulti { .. }))
                .count()
                > 0)
            .then(|| (mf, tree.to_resolved()))
        })
        .expect("the fixture has no prepared bounded body with a tree");
    assert!(
        !tree.is_empty(),
        "the prepared bounded fixture has no incoming tree"
    );
    let placeholder_ids: Vec<_> = mf
        .insts
        .iter()
        .filter_map(|(id, inst)| matches!(inst, Inst::CallBoundMulti { .. }).then_some(id))
        .collect();
    assert!(
        !placeholder_ids.is_empty(),
        "the standalone fixture has no placeholder"
    );
    skull_transform::bound::resolve_standalone_bound_calls(
        &ws,
        &scx,
        scx.interner(),
        &mut mf,
        &tree,
        &bodies.plans,
    )
    .expect("resolve bounded instance");
    assert!(
        !mf.resolved_bound_trees.is_empty(),
        "standalone substitution did not retain the witness's resolved tree"
    );
    for id in placeholder_ids {
        assert!(
            mf.callee_instances.contains_key(&id),
            "the concrete bounded callee lost its owner identity at {id:?}"
        );
    }
}
