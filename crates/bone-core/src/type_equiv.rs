//! Semantic type equivalence over the HIR type arenas.
//!
//! [`Workspace::render`] is presentation: distinct types can render the same
//! (two declarations' generic `T`, or a generic `T` and a nominal `T`). Code
//! that asks "are these the same type?" goes through
//! [`Workspace::types_equivalent`] instead.

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
            body: self.body,
            ty: self.ty.with_idx(idx),
        }
    }
}

/// The arena a `TyId` indexes. Generic parameter ids are allocated per
/// arena, so they compare only within one.
#[derive(PartialEq, Eq)]
enum Arena {
    Module(ModuleId),
    Body(*const BodyInference),
}

impl Workspace {
    /// Whether `a` and `b` denote the same type.
    ///
    /// The comparison is structural over every arena, so the same type built
    /// in two modules' arenas (or in a module arena and a body arena) is
    /// equivalent; the module or file a `TyId` lives in is not part of the
    /// type's identity.
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
    fn resolve_infer<'a>(&self, mut t: TyRef<'a>) -> (TyRef<'a>, TypeExpr) {
        loop {
            let e = match t.ty {
                TyId::Static(mid, idx) => self.module(mid).ty_arena[idx],
                TyId::Local(idx) => {
                    t.body
                        .expect("a Local type needs its BodyInference")
                        .ty_arena[idx]
                }
            };
            match e {
                TypeExpr::Infer(v, _) => match t.body.and_then(|b| b.substitutions.get(v)) {
                    Some(sub) => t.ty = sub,
                    None => return (t, e),
                },
                _ => return (t, e),
            }
        }
    }
}

fn same_arena(a: TyRef<'_>, b: TyRef<'_>) -> bool {
    arena_of(a) == arena_of(b)
}

fn same_body(a: TyRef<'_>, b: TyRef<'_>) -> bool {
    match (a.body, b.body) {
        (Some(x), Some(y)) => std::ptr::eq(x, y),
        _ => false,
    }
}

fn arena_of(t: TyRef<'_>) -> Option<Arena> {
    match t.ty {
        TyId::Static(mid, _) => Some(Arena::Module(mid)),
        TyId::Local(_) => t.body.map(|b| Arena::Body(b as *const _)),
    }
}
