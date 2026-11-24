//! Lowering and the instance walk must produce the same key for a struct-default
//! injection, or the join reports a hole that is really a disagreement.
use bone_core::prelude::StructId;
use bone_core::workspace::{BodyInference, Workspace};
use ustr::Ustr;

use crate::monoty::{self, MonoCtor, MonoSubst, MonoTy, MonoTyReason};

#[derive(Debug, Clone, PartialEq)]
pub enum Args {
    /// One per declared generic, in declaration order; empty for a non-generic struct.
    At(Vec<MonoTy>),
    /// Not determinable inside a shared generic body. Both consumers then use the
    /// declaration-keyed injection, sound because a shared body is lowered once.
    Shared,
}

impl Args {
    /// Empty for [`Args::Shared`].
    pub fn to_vec(&self) -> Vec<MonoTy> {
        match self {
            Self::At(a) => a.clone(),
            Self::Shared => Vec::new(),
        }
    }
}

/// A defect, unlike the expected shared-body case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgsError {
    Untyped,
    Unconvertible(MonoTyReason),
    NotThisStruct(String),
}

pub fn declared_generics(ws: &Workspace, sid: StructId) -> usize {
    ws.modules[sid.module.index()].structs[sid.def]
        .generics
        .len()
}

/// For a generic struct the type must be an application of this struct with one
/// argument per declared generic; anything else is rejected.
pub fn args_at(
    ws: &Workspace,
    inf: &BodyInference,
    subst: &MonoSubst,
    e: comfy_la_arena::Idx<bone_core::hir::Expr>,
    struct_name: Ustr,
    generics: usize,
) -> Result<Args, ArgsError> {
    if generics == 0 {
        return Ok(Args::At(Vec::new()));
    }
    let ty = *inf.expr_types.get(e).ok_or(ArgsError::Untyped)?;
    let mono = match monoty::convert(ws, inf, ty, subst) {
        Ok(m) => m,
        // Only these two reasons come from a shared generic body; any other is a defect.
        Err(MonoTyReason::UnboundGeneric(_) | MonoTyReason::UnresolvedInfer(_)) => {
            return Ok(Args::Shared);
        }
        Err(r) => return Err(ArgsError::Unconvertible(r)),
    };
    match mono {
        MonoTy::App {
            ctor: MonoCtor::Nominal(n),
            params,
        } if crate::layout_instance::decl_name(n) == struct_name && params.len() == generics => {
            Ok(Args::At(params))
        }
        other => Err(ArgsError::NotThisStruct(other.render())),
    }
}

/// Positional. Empty when bone published no slots, so the initializer fails to
/// resolve rather than binding by guess.
pub fn slot_subst(defaults_inf: Option<&BodyInference>, args: &[MonoTy]) -> MonoSubst {
    let mut out = MonoSubst::default();
    let Some(inf) = defaults_inf else { return out };
    for (slot, arg) in inf.struct_default_generic_slots.iter().zip(args) {
        if let Some(var) = monoty::infer_var_of(inf, *slot) {
            out.insert(var, arg.clone());
        }
    }
    out
}
