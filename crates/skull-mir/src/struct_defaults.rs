use bone_core::prelude::StructId;
use bone_core::workspace::{BodyInference, Workspace};
use ustr::Ustr;

use crate::monoty::{self, MonoCtor, MonoSubst, MonoTy, MonoTyReason};

#[derive(Debug, Clone, PartialEq)]
pub enum Args {
    At(Vec<MonoTy>),
    Shared,
}

impl Args {
    pub fn to_vec(&self) -> Vec<MonoTy> {
        match self {
            Self::At(a) => a.clone(),
            Self::Shared => Vec::new(),
        }
    }
}

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
        Err(MonoTyReason::UnboundGeneric(_) | MonoTyReason::UnresolvedInfer(_)) => {
            return Ok(Args::Shared);
        }
        Err(r) => return Err(ArgsError::Unconvertible(r)),
    };
    match mono {
        MonoTy::App {
            ctor: MonoCtor::Nominal(n),
            params,
        } if n == struct_name && params.len() == generics => Ok(Args::At(params)),
        other => Err(ArgsError::NotThisStruct(other.render())),
    }
}

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
