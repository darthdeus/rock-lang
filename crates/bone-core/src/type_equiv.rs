//! Semantic type equivalence over the HIR type arenas.
//!
//! [`Workspace::render`] is presentation: distinct types can render the same,
//! so equality questions go through [`Workspace::types_equivalent`].

use crate::hir::*;
use crate::prelude::*;
use crate::workspace::{BodyInference, Workspace};

/// A type together with the arena that resolves it. `body` is required for a
/// `TyId::Local` and supplies the inference substitutions that resolve a
/// solved `Infer` variable; a `TyId::Static` needs none.
#[derive(Copy, Clone, Debug)]
pub struct TyRef<'a> {
    pub body: Option<&'a BodyInference>,
    pub ty: TyId,
}

impl<'a> TyRef<'a> {
    /// A `TyId::Static`, which needs no body.
    pub fn of_static(ty: TyId) -> Self {
        TyRef { body: None, ty }
    }

    pub fn new(body: Option<&'a BodyInference>, ty: TyId) -> Self {
        TyRef { body, ty }
    }

    fn with_idx(self, idx: Idx<TypeExpr>) -> Self {
        TyRef {
            ty: self.ty.with_idx(idx),
            ..self
        }
    }
}

impl Workspace {
    /// Whether `a` and `b` denote the same type.
    ///
    /// - Builtins compare by kind.
    /// - Nominal types compare by name and then argument-wise. The name is the
    ///   declaration's identity: type names are unique workspace-wide, and a
    ///   duplicate declaration in another module is rejected.
    /// - `[T]`, `Map<K, V>`, `ref T`, nullables, tuples and function types
    ///   compare structurally.
    /// - A solved `Infer` variable is replaced by its solution from its own
    ///   arena's body before comparing.
    /// - A declared generic parameter (`Generic`) is equivalent only to the
    ///   same parameter id in the same arena: the same module arena, or the
    ///   same body arena. Ids are allocated per lowering context, so across
    ///   arenas the parameters are distinct even when their names match.
    /// - An unsolved `Infer` variable is equivalent only to the same variable
    ///   of the same body's inference.
    /// - A generic is never equivalent to a nominal type of the same name.
    pub fn types_equivalent(&self, a: TyRef<'_>, b: TyRef<'_>) -> bool {
        let (a, ea) = self.resolve_infer(a);
        let (b, eb) = self.resolve_infer(b);
        match (ea, eb) {
            (TypeExpr::Builtin(x), TypeExpr::Builtin(y)) => x == y,
            (TypeExpr::Generic(x, _), TypeExpr::Generic(y, _)) => x == y && same_arena(a, b),
            (TypeExpr::Infer(x, _), TypeExpr::Infer(y, _)) => x == y && same_body(a, b),
            (TypeExpr::Nullable(x), TypeExpr::Nullable(y)) => {
                self.types_equivalent(a.with_idx(x), b.with_idx(y))
            }
            (
                TypeExpr::App {
                    ctor: ca,
                    params: pa,
                },
                TypeExpr::App {
                    ctor: cb,
                    params: pb,
                },
            ) => ca == cb && self.all_equivalent(a, pa, b, pb),
            (TypeExpr::Tuple { elements: xa }, TypeExpr::Tuple { elements: xb }) => {
                self.all_equivalent(a, xa, b, xb)
            }
            (
                TypeExpr::Function {
                    params: pa,
                    ret: ra,
                },
                TypeExpr::Function {
                    params: pb,
                    ret: rb,
                },
            ) => {
                self.all_equivalent(a, pa, b, pb)
                    && self.types_equivalent(a.with_idx(ra), b.with_idx(rb))
            }
            _ => false,
        }
    }

    fn all_equivalent(
        &self,
        a: TyRef<'_>,
        xs: IdxRange<TypeExpr>,
        b: TyRef<'_>,
        ys: IdxRange<TypeExpr>,
    ) -> bool {
        xs.len() == ys.len()
            && xs
                .zip(ys)
                .all(|(x, y)| self.types_equivalent(a.with_idx(x), b.with_idx(y)))
    }

    /// The node behind `t`, following solved inference variables (a solution
    /// may live in another arena).
    fn resolve_infer<'a>(&self, t: TyRef<'a>) -> (TyRef<'a>, TypeExpr) {
        let (ty, expr) = self
            .resolve_ty(t.body, t.ty)
            .expect("a Local type needs its BodyInference");
        (TyRef { ty, ..t }, expr)
    }
}

/// Generic parameter ids are allocated per arena, so they compare only within one.
fn same_arena(a: TyRef<'_>, b: TyRef<'_>) -> bool {
    match (a.ty, b.ty) {
        (TyId::Static(x, _), TyId::Static(y, _)) => x == y,
        (TyId::Local(_), TyId::Local(_)) => same_body(a, b),
        _ => false,
    }
}

fn same_body(a: TyRef<'_>, b: TyRef<'_>) -> bool {
    match (a.body, b.body) {
        (Some(x), Some(y)) => std::ptr::eq(x, y),
        _ => false,
    }
}
