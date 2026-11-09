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
                return Err(non_tier0_error(f, "string literal", "GLSL"));
            }
            // A bare `Nil` const may be a dead placeholder (`return` in expression
            // position emits an unused nil, `lower.rs`); a genuinely-used null /
            // optional is rejected when it renders (`render_val`). Rejecting every
            // nil here would refuse `if a { return .. } else { .. }`.
            Inst::Const(Lit::Nil) => {}
            // Signed bit ops have a GLSL lowering beyond the shared tier-0 set.
            Inst::BitAnd(..)
            | Inst::BitOr(..)
            | Inst::BitXor(..)
            | Inst::Shl(..)
            | Inst::Shr(..)
            | Inst::BitNot(_) => {}
            allowed if is_tier0_inst(allowed) => {}
            // Everything else is above tier 0 (strings, containers, closures,
            // coroutines, enums, native buffers, bit ops, globals, ...).
            other => return Err(non_tier0_error(f, inst_kind(other), "GLSL")),
        }
    }
    Ok(())
}

// --- intrinsic mapping table ------------------------------------------------
