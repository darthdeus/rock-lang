//! The tier-0 profile check: which signatures and instructions have a GLSL lowering.

use super::*;

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

/// Walk a fn's MIR, rejecting any non-tier-0 instruction and collecting the
/// INSTANCES it calls directly.
///
/// The callee's instantiation is the one lowering already recorded at the call
/// (`MirFunc::spec_bindings`) -- the same record the Lua and JS backends read,
/// so GLSL cannot disagree with them about which body a call names.
pub(crate) fn profile_body(
    ws: &Workspace,
    f: &Function,
    mf: &MirFunc,
    callees: &mut Vec<ShaderInstance>,
    concrete: bool,
) -> Result<(), CompilerError> {
    let span = f.name_span;
    for (iid, inst) in mf.insts.iter() {
        match inst {
            Inst::Call { func, .. } => {
                callees.push(callee_inst_at(ws, mf, iid, *func, span, concrete)?)
            }
            // A `const` global folds to its literal at the use site; a mutable
            // `global` is runtime state with no GLSL equivalent.
            Inst::Global(name) => {
                if const_lit(ws, *name).is_none() {
                    if ws.const_globals.contains(name) {
                        return Err(err(
                            span,
                            format!(
                                "shader profile: const `{name}` has an initializer that is not a constant expression -- only literals and const aliases fold for GLSL"
                            ),
                        ));
                    }
                    return Err(err(
                        span,
                        format!(
                            "shader profile: `{}` uses a non-tier-0 construct (global read) that has no GLSL lowering -- a mutable `global` has no GLSL equivalent; a `const` folds to its literal",
                            f.name
                        ),
                    ));
                }
            }
            Inst::Const(Lit::Str(_)) => {
                return Err(err(
                    span,
                    format!(
                        "shader profile: `{}` uses a non-tier-0 construct (string literal) that has no GLSL lowering",
                        f.name
                    ),
                ));
            }
            // A bare `Nil` const may be a dead placeholder (`return` in expression
            // position emits an unused nil, `lower.rs`); a genuinely-used null /
            // optional is rejected when it renders (`render_val`). Rejecting every
            // nil here would refuse `if a { return .. } else { .. }`.
            Inst::Const(Lit::Nil) => {}
            // Allowed tier-0 instructions.
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
            | Inst::BitAnd(..)
            | Inst::BitOr(..)
            | Inst::BitXor(..)
            | Inst::Shl(..)
            | Inst::Shr(..)
            | Inst::UintBitAnd(..)
            | Inst::UintBitOr(..)
            | Inst::UintBitXor(..)
            | Inst::UintShl(..)
            | Inst::UintShr(..)
            | Inst::Neg(_)
            | Inst::Not(_)
            | Inst::BitNot(_)
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
            | Inst::Store { .. } => {}
            // Everything else is above tier 0 (strings, containers, closures,
            // coroutines, enums, native buffers, bit ops, globals, ...).
            other => {
                return Err(err(
                    span,
                    format!(
                        "shader profile: `{}` uses a non-tier-0 construct ({}) that has no GLSL lowering",
                        f.name,
                        inst_kind(other)
                    ),
                ));
            }
        }
    }
    Ok(())
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

// --- intrinsic mapping table ------------------------------------------------
