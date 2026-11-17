use crate::common::{
    InlineOffGuard, body_calls, bound_callee_name, definitions_matching, fn_name, function_body,
    key, scalar_ctx, ws_with_stdlib,
};
use skull_mir::mir::{Inst, MirFunc};
use skull_mir::monoty::{MonoCtor, MonoTy};

const BOUNDED: &str = r#"
fn total<I: Iterator<int>>(it: I) -> int {
    let s = 0
    for v in it { s = s + v }
    return s
}

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{total(xs.iter())}")
}
"#;

const CONCRETE: &str = r#"
fn total(xs: [int]) -> int {
    let s = 0
    for v in xs { s = s + v }
    return s
}

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{total(xs)}")
}
"#;

const NESTED: &str = r#"
struct Item { id: int, p: float }

fn main() {
    let xs = [
        Item { id: 1, p: 3.0 },
        Item { id: 2, p: 2.0 },
    ]
    let ids = xs.map(_.id).collect()
    print("{ids[0]}")
}
"#;

fn emit(src: &str) -> Result<String, String> {
    let ws = ws_with_stdlib(src);
    let mid = skull::user_module_id(&ws).expect("user module");
    skull::mir_emit::emit_workspace_mir(&ws, mid)
        .map(|e| e.lua)
        .map_err(|e| e.message)
}

fn emit_err(src: &str) -> Option<String> {
    emit(src).err()
}

fn body_of<'a>(lua: &'a str, symbol: &str) -> Vec<&'a str> {
    function_body(lua, symbol)
        .lines()
        .skip(1)
        .filter(|l| *l != "end" && *l != "end end")
        .collect()
}

fn specialization<'a>(lua: &'a str, prefix: &str) -> &'a str {
    let found: Vec<&str> = definitions_matching(lua, "_spec_")
        .into_iter()
        .filter(|name| name.starts_with(prefix))
        .collect();
    assert_eq!(found.len(), 1);
    found[0]
}

#[test]
fn a_bounded_body_emits_standalone_with_inlining_disabled() {
    let _inline = InlineOffGuard::on();
    let lua = emit(BOUNDED).expect("a bounded body must emit standalone under its demand");
    let symbol = specialization(&lua, "total_");
    let body = body_of(&lua, symbol);
    let resolved_next = definitions_matching(&lua, "next_")
        .into_iter()
        .any(|target| {
            let callee = bound_callee_name(&lua, symbol, target);
            body.iter().any(|line| body_calls(line, &callee))
        });
    assert!(resolved_next);
    let main = function_body(&lua, "main");
    let caller_name = bound_callee_name(&lua, "main", symbol);
    assert!(body_calls(main, &caller_name));
}

#[test]
fn a_witness_that_is_itself_bounded_resolves_from_the_leafs_sub_tree() {
    let _inline = InlineOffGuard::on();
    let lua = emit(NESTED).expect("a bounded witness of a bounded body must emit standalone");
    let symbol = specialization(&lua, "next_MapIter_");
    let body = body_of(&lua, symbol);
    let resolved_next = definitions_matching(&lua, "next_VecIter")
        .into_iter()
        .any(|target| {
            let callee = bound_callee_name(&lua, symbol, target);
            body.iter().any(|line| body_calls(line, &callee))
        });
    assert!(resolved_next);
}

#[test]
fn an_unbounded_body_emits_with_inlining_disabled() {
    let _inline = InlineOffGuard::on();
    let err = emit_err(CONCRETE);
    assert!(err.is_none());
}

fn first_call_multi_arity(mf: &MirFunc) -> usize {
    mf.insts
        .values()
        .find_map(|inst| match inst {
            Inst::CallMulti { dests, .. } => Some(dests.len()),
            _ => None,
        })
        .expect("resolved bounded body has no concrete next call")
}

#[test]
fn standalone_bound_rewrite_consumes_the_selected_result_plan() {
    let ws = ws_with_stdlib(
        r#"
nobox value Pair { x: int, y: int }

fn sum_all<I: Iterator<int>>(it: I) -> int {
    let total = 0
    for v in it { total = total + v }
    return total
}

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{sum_all(xs.iter())}")
}
"#,
    );
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let graph = skull::instance_graph::build(&ws, roots, false);
    let bodies = skull::concrete_body::build(&ws, &graph);
    let (_inst, plan) = bodies
        .plans
        .iter()
        .find(|(inst, plan)| fn_name(&ws, inst.func) == "sum_all" && !plan.bound_tree.is_empty())
        .expect("no standalone bounded instance with a resolved tree");
    let witness = plan
        .bound_tree
        .next
        .values()
        .find_map(|next| match next {
            skull_mir::bound::ResolvedBoundNext::Concrete { instance, .. } => {
                Some(instance.clone())
            }
            skull_mir::bound::ResolvedBoundNext::CallerGeneric(_) => None,
        })
        .expect("bounded plan has no concrete witness");

    let scx = scalar_ctx(&ws, &graph);
    let mut intact = skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &bodies.plans)
        .expect("intact bounded body did not lower");
    skull_transform::bound::resolve_standalone_bound_calls(
        &ws,
        &scx,
        graph.interner(),
        &mut intact,
        &plan.bound_tree,
        &bodies.plans,
    )
    .expect("intact bounded body did not resolve");
    assert_eq!(first_call_multi_arity(&intact), 2);

    let mut poisoned_plans = (*bodies.plans).clone();
    let pair = MonoTy::App {
        ctor: MonoCtor::Nominal(key(&ws, "Pair")),
        params: Vec::new(),
    };
    let wrong_result = graph
        .interner()
        .intern(MonoTy::Tuple(vec![MonoTy::Bool, pair]));
    let witness_plan = std::sync::Arc::make_mut(
        poisoned_plans
            .get_mut(&witness)
            .expect("witness plan disappeared"),
    );
    std::sync::Arc::make_mut(&mut witness_plan.signature).result = Some(wrong_result);
    let mut poisoned_owner = (**plan).clone();
    for callee in poisoned_owner.callees.values_mut() {
        if callee.instance == witness {
            std::sync::Arc::make_mut(&mut callee.signature).result = Some(wrong_result);
        }
    }
    let mut poisoned =
        skull_mir::lower::lower_instance_with_plans(&ws, &scx, &poisoned_owner, &poisoned_plans)
            .expect("plan mutation made the bounded body fail before rewrite");
    skull_transform::bound::resolve_standalone_bound_calls(
        &ws,
        &scx,
        graph.interner(),
        &mut poisoned,
        &poisoned_owner.bound_tree,
        &poisoned_plans,
    )
    .expect("poisoned bounded body did not resolve");
    assert_eq!(first_call_multi_arity(&poisoned), 3);
}
