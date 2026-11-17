//! The tier-0 profile check: which signatures and instructions have a GLSL lowering.

use super::*;
use bone_core::hir::{BodyOwner, Expr};

pub(crate) fn check_tier0_signature_concrete(
    interner: &MonoInterner,
    signature: &ConcreteSignature,
    span: Span,
) -> Result<(), CompilerError> {
    for id in &signature.params {
        concrete_glsl_ty(interner, *id, span, "parameter")?;
    }
    if let Some(id) = signature.result {
        if concrete_glsl_ty(interner, id, span, "return")?.is_sampler() {
            return Err(err(
                span,
                "shader profile: a sampler cannot be returned (GLSL forbids sampler locals/returns)",
            ));
        }
    }
    Ok(())
}

pub(crate) fn lower_profiled_body(
    ws: &Workspace,
    scx: &ScalarCtx,
    instance: &Instance,
    plans: &ConcretePlans,
    is_entry: bool,
    mut stage_globals: Option<&mut HashMap<Ustr, Lit>>,
) -> Result<(MirFunc, Vec<ShaderInstance>), CompilerError> {
    let f = &ws.module(instance.func.module).functions[instance.func.func];
    let span = f.name_span;
    let plan = plans.get(instance).ok_or_else(|| {
        err(
            span,
            format!("shader profile: no concrete plan for `{}`", f.name),
        )
    })?;
    // Entry signatures are checked against their stage interface.
    if !is_entry {
        check_tier0_signature_concrete(scx.interner(), &plan.signature, span)?;
    }
    let body = skull_mir::lower::lower_instance_with_plans(ws, scx, plan, plans).map_err(|e| {
        err(
            span,
            format!(
                "shader profile: `{}` cannot lower for GLSL: {}",
                f.name, e.message
            ),
        )
    })?;
    let mut callees = Vec::new();
    for (iid, inst) in body.insts.iter() {
        let construct = match inst {
            Inst::Call { func, .. } => {
                let interner = stage_globals.is_some().then(|| scx.interner());
                callees.push(callee_inst_at(ws, &body, iid, *func, span, interner)?);
                continue;
            }
            Inst::Global(name) => {
                if let Some(lit) = const_lit(ws, *name) {
                    if let Some(globals) = &mut stage_globals {
                        globals.insert(*name, lit);
                    }
                    continue;
                }
                if ws.const_globals.contains(name) {
                    let detail = if stage_globals.is_some() {
                        ""
                    } else {
                        " -- only literals and const aliases fold for GLSL"
                    };
                    return Err(err(
                        span,
                        format!(
                            "shader profile: const `{name}` has an initializer that is not a constant expression{detail}"
                        ),
                    ));
                }
                if stage_globals.is_none() {
                    return Err(err(
                        span,
                        format!(
                            "shader profile: `{}` uses a non-tier-0 construct (global read) that has no GLSL lowering -- a mutable `global` has no GLSL equivalent; a `const` folds to its literal",
                            f.name
                        ),
                    ));
                }
                "global read"
            }
            Inst::Const(Lit::Str(_)) => "string literal",
            Inst::BitAnd(..)
            | Inst::BitOr(..)
            | Inst::BitXor(..)
            | Inst::Shl(..)
            | Inst::Shr(..)
            | Inst::BitNot(_)
                if stage_globals.is_none() =>
            {
                continue;
            }
            // An unused nil placeholder is legal; rendering rejects live nulls.
            allowed if is_tier0_inst(allowed) => continue,
            other => inst_kind(other),
        };
        return Err(err(
            span,
            format!(
                "shader profile: `{}` uses a non-tier-0 construct ({construct}) that has no GLSL lowering",
                f.name
            ),
        ));
    }
    Ok((body, callees))
}

pub(crate) fn checked_body_calls(
    ws: &Workspace,
    caller: &ShaderInstance,
    callees: Vec<ShaderInstance>,
    allowed: &BTreeSet<Instance>,
    interner: &MonoInterner,
    builtins: &mut BTreeSet<Instance>,
    view: &str,
) -> Result<Vec<ShaderInstance>, CompilerError> {
    let caller_f = &ws.module(caller.fref().module).functions[caller.fref().func];
    let mut retained = Vec::new();
    for callee in callees {
        let f = &ws.module(callee.fref().module).functions[callee.fref().func];
        if renders_inline_instance(ws, f, &callee.inst, interner) {
            builtins.insert(callee.inst);
            continue;
        }
        if is_lowered_native_operator(ws, f, &callee.inst, interner) {
            continue;
        }
        if f.is_extern || f.inline_body.is_some() {
            return Err(err(
                f.name_span,
                format!(
                    "shader profile: `{}` has no emittable body and no GLSL intrinsic mapping",
                    f.name
                ),
            ));
        }
        if !allowed.contains(&callee.inst) {
            return Err(err(
                caller_f.name_span,
                format!(
                    "{view} omitted the concrete callee `{}` reached by `{}`",
                    f.name, caller_f.name
                ),
            ));
        }
        retained.push(callee);
    }
    Ok(retained)
}

/// Depth cap on `const A = B` alias chains. A cycle is impossible in a
/// well-formed program (bone rejects one), so this only bounds a pathological
/// chain rather than implementing cycle detection.
const CONST_CHAIN_DEPTH: usize = 16;

/// Fold a `const` global to its literal value, or `None` if it is not a `const`
/// or its initializer is not a constant expression.
///
/// GLSL has no module-level mutable state, so a `global` read can never lower.
/// A `const` is different in kind: bone enforces it read-only
/// (`ws.const_globals`, rejected at any rebind), so baking its value into the
/// shader cannot drift from the CPU-side value. It folds at the use site rather
/// than emitting a GLSL `const` declaration -- a declaration would have to join
/// the one cross-stage namespace shared by uniforms, varyings, attributes and
/// samplers, and collide-check against it, for no gain the folded literal
/// doesn't already give.
///
/// Only literals and alias chains fold today. Arithmetic (`const TAU = PI * 2`)
/// lowers to an operator CALL in HIR, not a `BinOp`, so folding it means
/// evaluating calls -- deliberately out of scope; such a const rejects with a
/// message naming the real cause.
pub(crate) fn const_lit(ws: &Workspace, name: Ustr) -> Option<Lit> {
    const_lit_depth(ws, name, 0)
}

fn const_lit_depth(ws: &Workspace, name: Ustr, depth: usize) -> Option<Lit> {
    if depth > CONST_CHAIN_DEPTH || !ws.const_globals.contains(&name) {
        return None;
    }
    let (module, global_id) = ws.modules.iter().find_map(|module| {
        module
            .globals
            .iter()
            .find(|(_, global)| global.name == name && global.is_const)
            .map(|(global_id, _)| (module, global_id))
    })?;
    let body = module.bodies.get(&BodyOwner::Global(global_id))?;
    fold_const_expr(ws, body, body.root?, depth)
}

fn fold_const_expr(
    ws: &Workspace,
    body: &bone_core::hir::Body,
    expr: Idx<Expr>,
    depth: usize,
) -> Option<Lit> {
    match body.expr_arena[expr] {
        Expr::Int(value) => Some(Lit::Int(value)),
        Expr::Float(value) => Some(Lit::Float(value.into_inner())),
        Expr::Bool(value) => Some(Lit::Bool(value)),
        Expr::UnaryMinus(inner, _) => match fold_const_expr(ws, body, inner, depth)? {
            Lit::Int(value) => Some(Lit::Int(-value)),
            Lit::Float(value) => Some(Lit::Float(-value)),
            _ => None,
        },
        Expr::Global(name) => const_lit_depth(ws, name, depth + 1),
        _ => None,
    }
}

/// The instructions every shader backend lowers. A backend may accept more.
fn is_tier0_inst(inst: &Inst) -> bool {
    matches!(
        inst,
        Inst::Const(_)
            | Inst::Load(_)
            | Inst::IntAdd(..)
            | Inst::IntSub(..)
            | Inst::IntMul(..)
            | Inst::IntDiv(..)
            | Inst::IntMod(..)
            | Inst::UintAdd(..)
            | Inst::UintSub(..)
            | Inst::UintMul(..)
            | Inst::UintDiv(..)
            | Inst::UintMod(..)
            | Inst::FloatAdd(..)
            | Inst::FloatSub(..)
            | Inst::FloatMul(..)
            | Inst::FloatDiv(..)
            | Inst::FloatMod(..)
            | Inst::IntLt(..)
            | Inst::IntLeq(..)
            | Inst::IntGt(..)
            | Inst::IntGeq(..)
            | Inst::UintLt(..)
            | Inst::UintLeq(..)
            | Inst::UintGt(..)
            | Inst::UintGeq(..)
            | Inst::FloatLt(..)
            | Inst::FloatLeq(..)
            | Inst::FloatGt(..)
            | Inst::FloatGeq(..)
            | Inst::Eq(..)
            | Inst::NotEq(..)
            | Inst::UintBitAnd(..)
            | Inst::UintBitOr(..)
            | Inst::UintBitXor(..)
            | Inst::UintShl(..)
            | Inst::UintShr(..)
            | Inst::Neg(_)
            | Inst::Not(_)
            | Inst::UintBitNot(_)
            | Inst::FloatToInt(_)
            | Inst::IntToFloat(_)
            | Inst::UintToFloat(_)
            | Inst::FloatToF32(_)
            | Inst::IntToUint(_)
            | Inst::UintToInt(_)
            | Inst::FloatToUint(_)
            | Inst::UintBitcastInt(_)
            | Inst::IntBitcastUint(_)
            | Inst::MakeValueStruct { .. }
            | Inst::MaterializeValueStruct { .. }
            | Inst::Field { .. }
            | Inst::Store { .. }
    )
}

/// A short human name for a rejected instruction kind (for the error message).
pub(crate) fn inst_kind(inst: &Inst) -> &'static str {
    match inst {
        Inst::Global(_) => "global read",
        Inst::StringConcat(..)
        | Inst::ToString(_)
        | Inst::Format { .. }
        | Inst::FormatDyn { .. } => "string",
        Inst::BitAnd(..)
        | Inst::BitOr(..)
        | Inst::BitXor(..)
        | Inst::Shl(..)
        | Inst::Shr(..)
        | Inst::BitNot(_)
        | Inst::UintBitAnd(..)
        | Inst::UintBitOr(..)
        | Inst::UintBitXor(..)
        | Inst::UintShl(..)
        | Inst::UintShr(..)
        | Inst::UintBitNot(_) => "bitwise op",
        Inst::CallMulti { .. }
        | Inst::CallValue { .. }
        | Inst::CallValueMulti { .. }
        | Inst::CallBoundMulti { .. } => "indirect/multi-value call",
        Inst::Lambda { .. } => "closure",
        Inst::MakeTuple(_) => "tuple",
        Inst::MakeVec(_) => "Vec",
        Inst::MakeStruct { .. } => "heap struct",
        Inst::MakeEnum { .. } | Inst::MakeEnumDyn { .. } => "enum",
        Inst::Index { .. }
        | Inst::Len { .. }
        | Inst::ArrayPush { .. }
        | Inst::StoreIndex { .. } => "container op",
        Inst::StoreField { .. } => "field mutation",
        Inst::StoreGlobal { .. } => "global write",
        Inst::CoroSetInner(_) | Inst::CoroHook { .. } | Inst::MapNext { .. } => {
            "coroutine/iteration"
        }
        _ => "unsupported instruction",
    }
}

pub(crate) fn reject_stage_violations<'a>(
    ws: &Workspace,
    stage: ShaderStage,
    bodies: impl Iterator<Item = &'a MirFunc>,
    span: Span,
) -> Result<(), CompilerError> {
    for body in bodies {
        for (_, instruction) in body.insts.iter() {
            let Inst::Call { func, .. } = instruction else {
                continue;
            };
            let name = ws.module(func.module).functions[func.func].name;
            match (stage, name.as_str()) {
                (ShaderStage::Vertex, "fwidth" | "dfdx" | "dfdy" | "frag_coord" | "discard") => {
                    return Err(err(
                        span,
                        format!(
                            "shader vertex: `{name}()` is a fragment-stage builtin, not available in the vertex stage"
                        ),
                    ));
                }
                (ShaderStage::Fragment, "vertex_id") => {
                    return Err(err(
                        span,
                        format!(
                            "shader fragment: `{name}()` is a vertex-stage builtin, not available in the fragment stage"
                        ),
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(())
}
