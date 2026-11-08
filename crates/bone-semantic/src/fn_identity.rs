//! FN IDENTITY THROUGH GENERICS: `==`/`!=` on fn values is rejected at the
//! comparison when the operands are statically fn-typed (bone-inference's Eq
//! arm). A generic body erases that type: `fn same<T>(a: T, b: T) { a == b }`
//! type-checks, and `same(f, g)` compares fn identity, which is not meaningful
//! (a capture-free lambda is one shared fn, a capturing lambda a fresh record
//! per creation).
//!
//! Every generic function gets a summary: the indices of its generics whose
//! values it may compare. A generic is compared when an `==`/`!=` operand's
//! inferred type is that generic (or its nullable/ref), however the operand
//! got there, or when the function instantiates a callee's compared generic
//! with it. The summaries are a whole-program fixpoint, and a call that
//! instantiates a compared generic with a fn type is rejected.

use bone_core::workspace::BodyInference;

use crate::hir::{BinOp, Body, Expr, Function, TypeExpr, VarId};
use crate::prelude::*;
use crate::relocation::strip_ref;

/// Every rejection starts with this, so a session can replace the previous
/// run's rejections wholesale.
const REJECTION_PREFIX: &str = "Cannot compare fn values: `";

type FnKey = (ModuleId, Idx<Function>);

/// A generic function's own comparisons and generic-to-generic forwards.
struct Facts {
    compared: FxHashSet<usize>,
    /// `(callee, callee generic index, own generic index)`: a call that
    /// instantiates the callee's generic with this function's own generic.
    forwards: Vec<(FnKey, usize, usize)>,
}

/// Which of the enclosing function's generics `ty` is, under `?` and `ref`.
fn own_generic(
    ws: &Workspace,
    bi: &BodyInference,
    generics: &[Option<VarId>],
    ty: TyId,
) -> Option<usize> {
    match strip_ref(ws, Some(bi), ty)? {
        (_, TypeExpr::Generic(var, _)) => generics.iter().position(|&g| g == Some(var)),
        _ => None,
    }
}

/// Positions aligned with `function.generics`.
fn generic_vars(ws: &Workspace, function: &Function, body: &Body) -> Vec<Option<VarId>> {
    function
        .generics
        .iter()
        .map(|&g| {
            let declared = match g {
                TyId::Static(m, i) => ws.module(m).ty_arena[i],
                TyId::Local(i) => body.lowered_tys[i],
            };
            match declared {
                TypeExpr::Generic(var, _) => Some(var),
                _ => None,
            }
        })
        .collect()
}

fn facts(ws: &Workspace, function: &Function, body: &Body, bi: &BodyInference) -> Facts {
    let generics = generic_vars(ws, function, body);
    let mut compared = FxHashSet::default();
    let mut forwards = Vec::new();
    for (id, expr) in body.expr_arena.iter() {
        if let Expr::BinOp {
            op: BinOp::Eq | BinOp::NotEq,
            lhs,
            rhs,
        } = *expr
        {
            // A null check is refinement, not identity.
            if matches!(body.expr_arena[lhs], Expr::Null)
                || matches!(body.expr_arena[rhs], Expr::Null)
            {
                continue;
            }
            for operand in [lhs, rhs] {
                if let Some(&ty) = bi.expr_types.get(operand)
                    && let Some(i) = own_generic(ws, bi, &generics, ty)
                {
                    compared.insert(i);
                }
            }
        }
        if let (Some(&callee), Some(bindings)) = (
            bi.resolved_overload_calls.get(&id),
            bi.call_generic_bindings.get(&id),
        ) {
            for (j, &ty) in bindings.iter().enumerate() {
                if let Some(i) = own_generic(ws, bi, &generics, ty) {
                    forwards.push((callee, j, i));
                }
            }
        }
    }
    Facts { compared, forwards }
}

fn inferred_bodies(
    ws: &Workspace,
) -> impl Iterator<Item = (ModuleId, BodyOwner, &Body, &BodyInference)> {
    ws.modules.iter().flat_map(move |module| {
        let inference = ws.inference(module.id);
        module.bodies.iter().filter_map(move |(&owner, body)| {
            let bi = inference?.per_fn.get(&owner)?;
            Some((module.id, owner, body, bi))
        })
    })
}

/// Compared generic indices per generic function, at the fixpoint.
fn summaries(ws: &Workspace) -> FxHashMap<FnKey, FxHashSet<usize>> {
    let mut all: FxHashMap<FnKey, Facts> = FxHashMap::default();
    for (mid, owner, body, bi) in inferred_bodies(ws) {
        let BodyOwner::Function(fid) = owner else {
            continue;
        };
        let function = &ws.module(mid).functions[fid];
        if function.generics.is_empty() {
            continue;
        }
        all.insert((mid, fid), facts(ws, function, body, bi));
    }
    let mut summary: FxHashMap<FnKey, FxHashSet<usize>> = all
        .iter()
        .map(|(&key, facts)| (key, facts.compared.clone()))
        .collect();
    loop {
        let mut changed = false;
        for (&key, facts) in &all {
            for &(callee, j, i) in &facts.forwards {
                if summary.get(&callee).is_some_and(|s| s.contains(&j))
                    && summary.entry(key).or_default().insert(i)
                {
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    summary.retain(|_, s| !s.is_empty());
    summary
}

fn is_fn_type(ws: &Workspace, bi: &BodyInference, ty: TyId) -> bool {
    matches!(
        strip_ref(ws, Some(bi), ty),
        Some((_, TypeExpr::Function { .. }))
    )
}

/// Replace the previous run's rejections with this one's, on the calling
/// body. Whole-program: a callee's summary can change a caller's verdict.
pub fn check_generic_fn_eq(ws: &mut Workspace) {
    for inference in ws.inferences.iter_mut().flatten() {
        inference.errors.retain(|_, errors| {
            errors.retain(|e| !e.message.starts_with(REJECTION_PREFIX));
            !errors.is_empty()
        });
    }
    let found: Vec<(ModuleId, BodyOwner, CompilerError)> = {
        let summary = summaries(ws);
        let mut found = Vec::new();
        for (mid, owner, body, bi) in inferred_bodies(ws) {
            for (id, _) in body.expr_arena.iter() {
                let (Some(&callee), Some(bindings)) = (
                    bi.resolved_overload_calls.get(&id),
                    bi.call_generic_bindings.get(&id),
                ) else {
                    continue;
                };
                let Some(compared) = summary.get(&callee) else {
                    continue;
                };
                if compared
                    .iter()
                    .any(|&j| bindings.get(j).is_some_and(|&ty| is_fn_type(ws, bi, ty)))
                {
                    let name = ws.module(callee.0).functions[callee.1].name;
                    found.push((
                        mid,
                        owner,
                        CompilerError::new_with_label(
                            body.expr_spans[id],
                            format!(
                                "{REJECTION_PREFIX}{name}` compares a generic parameter with `==`/`!=`, but it is instantiated with a fn type here -- fn identity is not meaningful (a capture-free lambda is one shared fn; a capturing lambda is a fresh record per creation). Restructure around explicit keys"
                            ),
                        ),
                    ));
                }
            }
        }
        found
    };
    for (mid, owner, error) in found {
        if let Some(Some(inference)) = ws.inferences.get_mut(mid.index()) {
            inference.errors.entry(owner).or_default().push(error);
        }
    }
}
