//! Type shape questions, answered through `Workspace::resolve_ty` so a
//! solved inference variable reads as its solution.

use bone_semantic::hir::{BuiltinTy, TypeCtor, TypeExpr};
use bone_semantic::prelude::{Arena, BodyInference, Idx, TyId, Workspace};
use ustr::Ustr;

/// The node behind `ty`; `Error` for a `Local` type without its body.
pub(crate) fn ty_of(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> TypeExpr {
    ws.resolve_ty(bi, ty)
        .map_or(TypeExpr::Builtin(BuiltinTy::Error), |(_, te)| te)
}

pub(crate) fn peel_nullable(
    ws: &Workspace,
    bi: Option<&BodyInference>,
    ty: TyId,
) -> Option<(TyId, TypeExpr)> {
    let (mut id, mut te) = ws.resolve_ty(bi, ty)?;
    while let TypeExpr::Nullable(inner) = te {
        (id, te) = ws.resolve_ty(bi, id.with_idx(inner))?;
    }
    Some((id, te))
}

pub(crate) fn is_nullable(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> bool {
    matches!(ws.resolve_ty(bi, ty), Some((_, TypeExpr::Nullable(_))))
}

pub(crate) fn is_unit(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> bool {
    matches!(
        ws.resolve_ty(bi, ty),
        Some((_, TypeExpr::Builtin(BuiltinTy::Unit)))
    )
}

fn app(
    ws: &Workspace,
    bi: Option<&BodyInference>,
    ty: TyId,
) -> Option<(TypeCtor, impl Iterator<Item = TyId>)> {
    match peel_nullable(ws, bi, ty)? {
        (id, TypeExpr::App { ctor, params }) => {
            Some((ctor, params.into_iter().map(move |p| id.with_idx(p))))
        }
        _ => None,
    }
}

pub(crate) fn nominal_args(
    ws: &Workspace,
    bi: Option<&BodyInference>,
    ty: TyId,
) -> Option<(Ustr, impl Iterator<Item = TyId>)> {
    match app(ws, bi, ty)? {
        (TypeCtor::Nominal(name), args) => Some((name, args)),
        _ => None,
    }
}

pub(crate) fn nominal_name(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> Option<Ustr> {
    nominal_args(ws, bi, ty).map(|(name, _)| name)
}

/// `[T]` reads as the stdlib `Vec` struct it is type-checked as.
pub(crate) fn member_owner_name(
    ws: &Workspace,
    bi: Option<&BodyInference>,
    ty: TyId,
) -> Option<Ustr> {
    match app(ws, bi, ty)? {
        (TypeCtor::Nominal(name), _) => Some(name),
        (TypeCtor::Array, _) => Some(Ustr::from("Vec")),
        _ => None,
    }
}

pub(crate) fn array_elem(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> Option<TyId> {
    match app(ws, bi, ty)? {
        (TypeCtor::Array, mut args) => args.next(),
        _ => None,
    }
}

/// `Entity<Glyph>` -> `Glyph`.
pub(crate) fn sole_arg_name(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> Option<Ustr> {
    let (_, mut args) = nominal_args(ws, bi, ty)?;
    match (args.next(), args.next()) {
        (Some(arg), None) => nominal_name(ws, bi, arg),
        _ => None,
    }
}

pub(crate) fn annotation_nominal(arena: &Arena<TypeExpr>, mut idx: Idx<TypeExpr>) -> Option<Ustr> {
    loop {
        match arena[idx] {
            TypeExpr::Nullable(inner) => idx = inner,
            TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            } => return Some(name),
            _ => return None,
        }
    }
}
