//! What a body reads of each parameter, before parameter expansion: `None` when
//! the parameter is used as a whole value, otherwise the set of its fields the
//! body names (empty when the body never mentions the parameter). Any mention
//! that is not a direct field read outside a lambda -- passing it on, writing
//! into it, capturing it, calling it -- counts as a whole use, so the answer is
//! conservative. This is
//! not an effect or alias analysis: argument evaluation stays at the call site.
use super::{BodyOwner, Workspace};
use crate::mir::FuncRef;
use bone_core::hir::{BindingInfo, Body, Callee, Expr};
use bone_core::hir_walk::HirChild;
use comfy_la_arena::Idx;
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
    let params: Vec<Idx<BindingInfo>> = body.param_bindings.into_iter().collect();
    let mut demand = vec![Some(Vec::new()); params.len()];
    /// Marks a whole use. `open` counts the parameters still read by field
    /// only; at zero no later mention can change the answer and the walk stops.
    fn whole(slot: &mut Option<Vec<Ustr>>, open: &mut usize) {
        if slot.take().is_some() {
            *open -= 1;
        }
    }
    fn visit(
        body: &Body,
        node: HirChild,
        params: &[Idx<BindingInfo>],
        demand: &mut [Option<Vec<Ustr>>],
        open: &mut usize,
    ) {
        if *open == 0 {
            return;
        }
        let param = |binding| params.iter().position(|p| *p == binding);
        if let HirChild::Expr(e) = node {
            match body.expr_arena[e] {
                Expr::FieldAccess { base, field } => {
                    if let Expr::Var(binding) = body.expr_arena[base]
                        && let Some(i) = param(binding)
                    {
                        if let Some(fields) = &mut demand[i]
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
                    let mut captured = vec![Some(Vec::new()); params.len()];
                    let mut inner_open = params.len();
                    visit(
                        body,
                        HirChild::Expr(inner),
                        params,
                        &mut captured,
                        &mut inner_open,
                    );
                    for (slot, seen) in demand.iter_mut().zip(captured) {
                        if seen.is_none_or(|fields| !fields.is_empty()) {
                            whole(slot, open);
                        }
                    }
                    return;
                }
                _ => {}
            }
        }
        body.for_each_child(node, |child| visit(body, child, params, demand, open));
    }
    let mut open = params.len();
    visit(
        body,
        HirChild::Expr(body.root?),
        &params,
        &mut demand,
        &mut open,
    );
    Some(demand)
}
