//! The identity of a struct-default INJECTION: which struct, instantiated with
//! what, and how the defaults body's open type variables bind.
//!
//! One oracle, because two consumers must agree exactly. Lowering stamps every
//! call it injects with [`crate::site::SiteDomain::StructDefaults`] and resolves
//! the callees under the binding; the instance walk records the obligations for
//! the same injection and must produce the same key, or the two describe
//! different things and the join reports a hole that is really a disagreement.

use bone_semantic::inference::{BodyInference, Workspace};
use bone_semantic::prelude::StructId;
use ustr::Ustr;

use crate::monoty::{self, MonoCtor, MonoSubst, MonoTy, MonoTyReason};

/// A struct's generic arguments at one injection.
#[derive(Debug, Clone, PartialEq)]
pub enum Args {
    /// Determined -- one per declared generic, in declaration order. Empty for
    /// a non-generic struct.
    At(Vec<MonoTy>),
    /// NOT determinable at this injection, because the enclosing body is a
    /// shared generic snapshot: the literal's own type still carries the
    /// enclosing function's unbound generics, so `Box<T>` names no `T`.
    ///
    /// Both consumers fall back to the declaration-keyed injection here, which
    /// is what every injection did before arguments existed. That is sound
    /// while shared bodies exist because a shared body is lowered ONCE, so the
    /// empty key names exactly one injection and cannot collide with a
    /// concrete `Box<int>`'s.
    ///
    /// Cut C+D removes shared generic bodies, and with them this arm.
    Shared,
}

impl Args {
    /// The arguments to key and bind with; empty for [`Args::Shared`].
    pub fn to_vec(&self) -> Vec<MonoTy> {
        match self {
            Self::At(a) => a.clone(),
            Self::Shared => Vec::new(),
        }
    }
}

/// Why an injection's arguments could not be determined, as a DEFECT rather
/// than as the expected shared-body case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgsError {
    /// The literal carries no inferred type.
    Untyped,
    /// Its type did not convert to a total logical type.
    Unconvertible(MonoTyReason),
    /// It converted to something that is not an application of this struct with
    /// one argument per declared generic.
    NotThisStruct(String),
}

impl ArgsError {
    pub fn render(&self, struct_name: Ustr) -> String {
        match self {
            Self::Untyped => format!("generic struct `{struct_name}` literal is untyped"),
            Self::Unconvertible(r) => {
                format!("generic struct `{struct_name}` literal: {}", r.bucket())
            }
            Self::NotThisStruct(got) => format!(
                "generic struct `{struct_name}` default injected while `{got}` is current, \
                 so the arguments would not be its own"
            ),
        }
    }
}

/// How many generics the struct declares.
pub fn declared_generics(ws: &Workspace, sid: StructId) -> usize {
    ws.modules[sid.module.index()].structs[sid.def].generics.len()
}

/// The struct's generic ARGUMENTS at an injection, from the literal expression
/// `e` typed in `inf` and read under `subst`.
///
/// A non-generic struct has none and `e` is not consulted at all -- which is
/// every struct in today's corpus, so this decides nothing there.
///
/// For a generic one the answer VERIFIES rather than assumes: the type must be
/// an application of this very struct with one argument per declared generic.
/// A caller that offers some other expression gets a rejection, not a
/// plausible-looking binding drawn from the wrong literal.
pub fn args_at(
    ws: &Workspace,
    inf: &BodyInference,
    subst: &MonoSubst,
    e: comfy_la_arena::Idx<bone_semantic::hir::Expr>,
    struct_name: Ustr,
    generics: usize,
) -> Result<Args, ArgsError> {
    if generics == 0 {
        return Ok(Args::At(Vec::new()));
    }
    let ty = *inf.expr_types.get(e).ok_or(ArgsError::Untyped)?;
    let mono = match monoty::convert(ws, inf, ty, subst) {
        Ok(m) => m,
        // The two reasons a SHARED generic body produces, and only those.
        // `UnboundGeneric` is the enclosing fn's own generic; `UnresolvedInfer`
        // is an open inference class the caller could not name. Every other
        // reason -- recovery poison, a missing node, a cyclic chain -- is a
        // defect and still rejects.
        Err(MonoTyReason::UnboundGeneric(_) | MonoTyReason::UnresolvedInfer(_)) => {
            return Ok(Args::Shared);
        }
        Err(r) => return Err(ArgsError::Unconvertible(r)),
    };
    match mono {
        MonoTy::App { ctor: MonoCtor::Nominal(n), params }
            if n == struct_name && params.len() == generics =>
        {
            Ok(Args::At(params))
        }
        other => Err(ArgsError::NotThisStruct(other.render())),
    }
}

/// Bind the defaults body's published generic slots to `args`, positionally.
///
/// Empty when bone published no slots. That is the non-generic case, and it is
/// also the honest answer if the recording is ever absent: the initializer then
/// fails to resolve, rather than binding by guess.
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
