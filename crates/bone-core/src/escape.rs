//! Proven-nonescaping closures: a closure may capture an identity borrow only
//! when it cannot outlive the call that receives it. Anything this analysis
//! cannot prove is treated as escaping.
//!
//! A lambda is nonescaping when it is called in place, or passed as an
//! argument to a parameter that its callee only calls or forwards to another
//! nonescaping parameter. Callee bodies are read from HIR alone (their calls
//! resolve by a unique name), so the answer does not depend on which bodies
//! were inferred first.

use crate::hir::{Callee, Expr, Function};
use crate::prelude::*;
use crate::workspace::{BodyInference, Workspace};

/// Whether `lambda`, an `Expr::Lambda` in `body`, provably cannot outlive the
/// call it is passed to. `bi` is `body`'s inference, used for its resolved
/// overloads.
pub fn lambda_is_nonescaping(
    ws: &Workspace,
    body: &Body,
    bi: &BodyInference,
    lambda: Idx<Expr>,
) -> bool {
    let mut visiting = FxHashSet::default();
    for (call, expr) in body.expr_arena.iter() {
        let Expr::Call { callee, args, .. } = expr else {
            continue;
        };
        if matches!(callee, Callee::Value(value) if *value == lambda) {
            return true;
        }
        let Some(index) = args.into_iter().position(|arg| arg == lambda) else {
            continue;
        };
        let target = bi
            .resolved_overload_calls
            .get(&call)
            .copied()
            .or_else(|| unique_target(ws, callee));
        return target.is_some_and(|target| param_is_nonescaping(ws, target, index, &mut visiting));
    }
    false
}

fn unique_target(ws: &Workspace, callee: &Callee) -> Option<(ModuleId, Idx<Function>)> {
    let Callee::Unresolved { name, .. } = callee else {
        return None;
    };
    match ws
        .functions_by_name
        .get(name)
        .map(|candidates| candidates.as_slice())
    {
        Some([only]) => Some(*only),
        _ => None,
    }
}

fn contains(outer: Span, inner: Span) -> bool {
    outer.file == inner.file
        && outer.offset_range.0 <= inner.offset_range.0
        && inner.offset_range.1 <= outer.offset_range.1
}

fn param_is_nonescaping(
    ws: &Workspace,
    (mid, fid): (ModuleId, Idx<Function>),
    index: usize,
    visiting: &mut FxHashSet<(ModuleId, Idx<Function>, usize)>,
) -> bool {
    // A cycle is not a proof.
    if !visiting.insert((mid, fid, index)) {
        return false;
    }
    let nonescaping = param_body_is_nonescaping(ws, (mid, fid), index, visiting);
    visiting.remove(&(mid, fid, index));
    nonescaping
}

fn param_body_is_nonescaping(
    ws: &Workspace,
    (mid, fid): (ModuleId, Idx<Function>),
    index: usize,
    visiting: &mut FxHashSet<(ModuleId, Idx<Function>, usize)>,
) -> bool {
    let module = ws.module(mid);
    let function = &module.functions[fid];
    // A co fn keeps its parameters in the coroutine frame; extern, inline and
    // variadic bodies are opaque.
    if function.is_coroutine
        || function.is_extern
        || function.inline_body.is_some()
        || function.is_variadic
    {
        return false;
    }
    let Some(body) = module.bodies.get(&BodyOwner::Function(fid)) else {
        return false;
    };
    let Some(param) = body.param_bindings.into_iter().nth(index) else {
        return false;
    };
    let lambdas: Vec<Span> = body
        .expr_arena
        .iter()
        .filter(|(_, expr)| matches!(expr, Expr::Lambda { .. }))
        .map(|(id, _)| body.expr_spans[id])
        .collect();
    let in_lambda = |id: Idx<Expr>| {
        lambdas
            .iter()
            .any(|outer| contains(*outer, body.expr_spans[id]))
    };
    let mut forwarded = FxHashSet::default();
    for (call, expr) in body.expr_arena.iter() {
        let Expr::Call { callee, args, .. } = expr else {
            continue;
        };
        if matches!(callee, Callee::Local(binding) if *binding == param) && in_lambda(call) {
            return false;
        }
        for (arg_index, arg) in args.into_iter().enumerate() {
            if !matches!(body.expr_arena[arg], Expr::Var(binding) if binding == param) {
                continue;
            }
            if in_lambda(call) {
                return false;
            }
            let Some(target) = unique_target(ws, callee) else {
                return false;
            };
            if !param_is_nonescaping(ws, target, arg_index, visiting) {
                return false;
            }
            forwarded.insert(arg);
        }
    }
    body.expr_arena.iter().all(|(id, expr)| {
        !matches!(expr, Expr::Var(binding) if *binding == param) || forwarded.contains(&id)
    })
}
