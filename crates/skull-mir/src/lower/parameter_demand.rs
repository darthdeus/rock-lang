//! What a body reads of each parameter, before parameter expansion: `None` when
//! the parameter is used as a whole value, otherwise the set of its fields the
//! body names (empty when the body never mentions the parameter). Any mention
//! that is not a direct field read outside a lambda -- passing it on, writing
//! into it, capturing it, calling it -- counts as a whole use, so the answer is
//! conservative. This is
//! not an effect or alias analysis: argument evaluation stays at the call site.
use super::{BodyOwner, Workspace, raw};
use crate::mir::FuncRef;
use bone_core::hir::{Body, Callee, Expr};
use bone_core::hir_walk::HirChild;
use std::ops::Range;
use ustr::Ustr;

/// Field lists hold each name once, in first-mention order.
pub(super) type Demand = Option<Vec<Option<Vec<Ustr>>>>;

pub(super) fn of_function(ws: &Workspace, func: FuncRef) -> Demand {
    let body = ws
        .module(func.module)
        .bodies
        .get(&BodyOwner::Function(func.func))?;
    fields(body)
}

/// Identifies a demand across edits; equal keys give equal transports.
pub(super) fn key(demand: &Demand) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for param in demand.iter().flatten() {
        match param {
            None => u8::MAX.hash(&mut hasher),
            Some(fields) => {
                let mut fields: Vec<&str> = fields.iter().map(|f| f.as_str()).collect();
                fields.sort_unstable();
                fields.hash(&mut hasher);
            }
        }
    }
    demand.is_some().hash(&mut hasher);
    hasher.finish()
}

fn fields(body: &Body) -> Demand {
    let params = body.param_bindings.make_range();
    let mut demand = vec![Some(Vec::new()); params.len()];
    // Once every parameter has a whole use, no later mention can change the answer.
    fn whole(slot: &mut Option<Vec<Ustr>>, open: &mut usize) {
        if slot.take().is_some() {
            *open -= 1;
        }
    }
    fn visit(
        body: &Body,
        node: HirChild,
        params: &Range<u32>,
        demand: &mut [Option<Vec<Ustr>>],
        open: &mut usize,
        captured: bool,
    ) {
        if *open == 0 {
            return;
        }
        let param = |binding| {
            let id = raw(binding);
            params.contains(&id).then(|| (id - params.start) as usize)
        };
        if let HirChild::Expr(e) = node {
            match body.expr_arena[e] {
                Expr::FieldAccess { base, field } => {
                    if let Expr::Var(binding) = body.expr_arena[base]
                        && let Some(i) = param(binding)
                    {
                        if captured {
                            whole(&mut demand[i], open);
                        } else if let Some(fields) = &mut demand[i]
                            && !fields.contains(&field)
                        {
                            fields.push(field);
                        }
                        return;
                    }
                }
                Expr::Var(binding) => {
                    if let Some(i) = param(binding) {
                        whole(&mut demand[i], open);
                    }
                }
                Expr::Call {
                    callee: Callee::Local(binding),
                    ..
                } => {
                    if let Some(i) = param(binding) {
                        whole(&mut demand[i], open);
                    }
                }
                // Field transport does not cover writes into the parameter.
                Expr::Assign { target, .. } | Expr::CompoundAssign { target, .. } => {
                    let mut root = target;
                    while let Expr::FieldAccess { base, .. }
                    | Expr::SafeNav { base, .. }
                    | Expr::Index { base, .. } = body.expr_arena[root]
                    {
                        root = base;
                    }
                    if let Expr::Var(binding) = body.expr_arena[root]
                        && let Some(i) = param(binding)
                    {
                        whole(&mut demand[i], open);
                    }
                }
                // A capture holds the whole value.
                Expr::Lambda { body: inner, .. } => {
                    visit(body, HirChild::Expr(inner), params, demand, open, true);
                    return;
                }
                _ => {}
            }
        }
        body.for_each_child(node, |child| {
            visit(body, child, params, demand, open, captured)
        });
    }
    let mut open = params.len();
    visit(
        body,
        HirChild::Expr(body.root?),
        &params,
        &mut demand,
        &mut open,
        false,
    );
    Some(demand)
}
