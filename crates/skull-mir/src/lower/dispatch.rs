use bone_core::hir::{BinOp, Body, BuiltinTy, Expr, InterpSpec, TyId, TypeCtor, TypeExpr};
use bone_core::prelude::Idx;
use bone_core::workspace::{BodyInference, Workspace};
use ustr::Ustr;

use super::{EnumPlan, RepArena, RepGuarantees, RepUse, RepUseFacts, shape::is_payload_free_enum};
use crate::concrete::{ConcreteBodyPlan, ConcreteTypeTables};
use crate::monoty::MonoSubst;

/// Type context for route decisions. The graph walk uses a target-neutral
/// context with `values_first_class` false; GLSL cannot reach these routes.
#[derive(Clone, Copy)]
pub struct DispatchCtx<'a> {
    pub ws: &'a Workspace,
    pub inf: Option<&'a BodyInference>,
    pub body: &'a Body,
    pub concrete_plan: Option<&'a ConcreteBodyPlan>,
    pub concrete_types: Option<&'a ConcreteTypeTables>,
    pub interner: Option<&'a crate::monoty::MonoInterner>,
    pub rep_arena: &'a RepArena,
    pub target: bone_core::hir::FnBackend,
    /// Concrete lowering sets this to `None`: every type decision must come from
    /// its concrete plan.
    pub logical_subst: Option<&'a MonoSubst>,
    pub values_first_class: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StringifyRoute {
    ToString,
    /// A string hole is already its own text.
    Verbatim,
    /// A string inside a structural rendering, quoted and escaped as a Rock literal.
    EscapedString,
    FormatFloat,
    Newtype(Ustr),
    EnumNames(Ustr),
    Inspect,
    Structural(Box<super::format::FmtNode>),
    Unprintable(String),
    Unsupported,
}

impl StringifyRoute {
    pub fn synthesized_callee(&self) -> Option<&'static str> {
        match self {
            Self::FormatFloat => Some("__rock_format_float"),
            Self::EscapedString => Some("__rock_quote_string"),
            Self::Inspect => Some("inspect"),
            _ => None,
        }
    }

    pub fn obligations(&self) -> Vec<(Vec<u32>, &'static str)> {
        match self {
            Self::Structural(tree) => {
                let mut out = Vec::new();
                tree.obligations(&mut Vec::new(), &mut out);
                out
            }
            route => route
                .synthesized_callee()
                .map(|n| (Vec::new(), n))
                .into_iter()
                .collect(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EqRoute {
    Primitive,
    Structural,
}

impl EqRoute {
    pub fn synthesized_callee(&self, op: BinOp) -> Option<&'static str> {
        match self {
            Self::Structural => Some(if matches!(op, BinOp::Eq) {
                "__op_eq"
            } else {
                "__op_ne"
            }),
            Self::Primitive => None,
        }
    }
}

/// Resolution is target-neutral: backend selection happens when the concrete
/// call plan is built, and JS may supply a body for an `inline(lua)` declaration.
pub fn dispatched_callee_decls(ws: &Workspace, name: &str) -> Vec<crate::mir::FuncRef> {
    let mut resolved = Vec::new();
    for (key, decls) in &ws.functions_by_name {
        if crate::mangle::declared_name(key.as_str()) != name {
            continue;
        }
        resolved.extend(decls.iter().filter_map(|&(module, func)| {
            (crate::mangle::mangle_fn(ws, &ws.module(module).functions[func]) == name)
                .then_some(crate::mir::FuncRef { module, func })
        }));
    }
    resolved.sort_by_key(|r| (r.module.index(), r.func.into_raw()));
    resolved.dedup();
    resolved
}

pub fn dispatched_callee_decl(ws: &Workspace, name: &str) -> Option<crate::mir::FuncRef> {
    let mut reps = dispatched_callee_decls(ws, name)
        .into_iter()
        .map(|r| super::canonical_variant(ws, r));
    let first = reps.next()?;
    reps.all(|r| r == first).then_some(first)
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StructuralRender {
    Formatted,
    Plain(StringifyRoute),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FormatPlan {
    pub inferred: Option<char>,
    pub structural: Option<StructuralRender>,
    pub special: Option<&'static str>,
    pub grouping: Option<(char, i64)>,
    pub needs_pad: bool,
    /// The printf conversion omits the width; the pad helper takes it instead.
    pub skip_width: bool,
    /// A `float` value floors before an integer conversion.
    pub val_is_float: bool,
    pub ty: Option<char>,
}

impl FormatPlan {
    /// Structural rendering always calls pad; no width makes padding a runtime
    /// no-op, not an omitted call.
    pub fn obligations(&self) -> Vec<(Vec<u32>, &'static str)> {
        let mut out: Vec<(Vec<u32>, &'static str)> = Vec::new();
        match &self.structural {
            Some(StructuralRender::Formatted) => {
                out.push((Vec::new(), "inspect_formatted"));
                out.push((Vec::new(), "__rock_fmt_pad"));
                return out;
            }
            Some(StructuralRender::Plain(route)) => {
                out.extend(route.obligations());
                out.push((Vec::new(), "__rock_fmt_pad"));
                return out;
            }
            None => {}
        }
        out.extend(self.special.map(|n| (Vec::new(), n)));
        if self.grouping.is_some() {
            out.push((Vec::new(), "__rock_fmt_grouping"));
        }
        if self.needs_pad {
            out.push((Vec::new(), "__rock_fmt_pad"));
        }
        out
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HolePlan {
    Plain(StringifyRoute),
    Formatted(FormatPlan),
}

impl HolePlan {
    pub fn obligations(&self) -> Vec<(Vec<u32>, &'static str)> {
        match self {
            Self::Plain(route) => route.obligations(),
            Self::Formatted(plan) => plan.obligations(),
        }
    }
}

impl<'a> DispatchCtx<'a> {
    /// The stored type node, without the specialization substitution.
    pub(crate) fn raw_ty_node(&self, ty: TyId) -> Option<TypeExpr> {
        Some(match ty {
            TyId::Static(mid, idx) => self.ws.module(mid).ty_arena[idx],
            TyId::Local(idx) => self.inf?.ty_arena[idx],
        })
    }

    pub(crate) fn concrete_ty(&self, ty: TyId) -> Option<&'a crate::monoty::MonoTy> {
        let id = self.concrete_types?.ty_type(ty)?;
        self.concrete_mono(id)
    }

    pub(crate) fn concrete_expr_ty(&self, expr: Idx<Expr>) -> Option<&'a crate::monoty::MonoTy> {
        let id = self.concrete_types?.expr_type(expr)?;
        self.concrete_mono(id)
    }

    pub(crate) fn concrete_mono(
        &self,
        id: crate::monoty::MonoTyId,
    ) -> Option<&'a crate::monoty::MonoTy> {
        Some(self.interner?.get_ref(id))
    }

    /// Never fabricates a partial HIR node for a concrete generic argument; use
    /// `concrete_ty` or `concrete_expr_ty` for resolved facts.
    pub(crate) fn ty_node(&self, ty: TyId) -> Option<TypeExpr> {
        self.raw_ty_node(ty)
    }

    /// Chase `Infer` and peel `Nullable`.
    pub(crate) fn resolve_ty(&self, ty: TyId) -> Option<TypeExpr> {
        match self.ty_node(ty)? {
            TypeExpr::Infer(var, _) => self.resolve_ty(self.inf?.substitutions.get(var)?),
            TypeExpr::Nullable(inner) => self.resolve_ty(ty.with_idx(inner)),
            other => Some(other),
        }
    }

    /// Chase `Infer` without peeling `Nullable`, keeping the `TyId`.
    pub(crate) fn resolve_tyid(&self, ty: TyId) -> Option<TyId> {
        match self.ty_node(ty)? {
            TypeExpr::Infer(var, _) => self.resolve_tyid(self.inf?.substitutions.get(var)?),
            _ => Some(ty),
        }
    }

    pub(crate) fn expr_ty(&self, e: Idx<Expr>) -> Option<TypeExpr> {
        self.resolve_ty(*self.inf?.expr_types.get(e)?)
    }

    pub(crate) fn is_nullable_ty(&self, ty: TyId) -> bool {
        if let Some(ty) = self.concrete_ty(ty) {
            return ty.peel_nullable().1;
        }
        if self.concrete_plan.is_some() {
            return false;
        }
        // A generic bound to `E?` is nullable, but `ty_node` substitutes the payload;
        // read the raw node before that erases the `?`.
        if let Some(TypeExpr::Generic(var, _)) = self.raw_ty_node(ty) {
            if let Some(b) = self.logical_subst.and_then(|subst| subst.get(&var)) {
                return b.peel_nullable().1;
            }
        }
        match self.ty_node(ty) {
            Some(TypeExpr::Nullable(_)) => true,
            Some(TypeExpr::Infer(var, _)) => self
                .inf
                .and_then(|i| i.substitutions.get(var))
                .map(|s| self.is_nullable_ty(s))
                .unwrap_or(false),
            _ => false,
        }
    }

    pub(crate) fn expr_is_nullable(&self, e: Idx<Expr>) -> bool {
        if let Some(ty) = self.concrete_expr_ty(e) {
            return ty.peel_nullable().1;
        }
        if self.concrete_plan.is_some() {
            return false;
        }
        self.inf
            .and_then(|i| i.expr_types.get(e).copied())
            .map(|ty| self.is_nullable_ty(ty))
            .unwrap_or(false)
    }

    pub(crate) fn is_null_lit(&self, e: Idx<Expr>) -> bool {
        matches!(self.body.expr_arena[e], Expr::Null)
    }

    pub(crate) fn is_nobox_struct(&self, name: Ustr) -> bool {
        if self.values_first_class {
            return false;
        }
        let ty = crate::monoty::MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            params: Vec::new(),
        };
        self.rep_arena
            .plan(
                self.ws,
                self.target,
                &ty,
                RepUse::Value,
                &RepUseFacts::default(),
                RepGuarantees {
                    nobox: true,
                    allow_opaque: true,
                },
            )
            .map(|id| self.rep_arena.get(id).nobox())
            .unwrap_or(false)
    }

    pub(crate) fn nobox_value_name(&self, e: Idx<Expr>) -> Option<Ustr> {
        if let Some(ty) = self.concrete_expr_ty(e) {
            return match ty.peel_nullable().0 {
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name),
                    ..
                } => self.is_nobox_struct(*name).then_some(*name),
                _ => None,
            };
        }
        if self.concrete_plan.is_some() {
            return None;
        }
        let TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } = self.expr_ty(e)?
        else {
            return None;
        };
        self.is_nobox_struct(name).then_some(name)
    }

    /// Lua/C equality reaches a value struct's `__eq` directly; JS uses the
    /// structural equality helper.
    pub fn is_value_struct(&self, name: Ustr) -> bool {
        crate::lower::shape::is_value_struct(self.ws, name)
    }

    /// Unlike [`Self::nobox_value_name`] this does not require `nobox`: every
    /// `value` struct carries a native-`==`-reachable `__eq`.
    pub(crate) fn value_struct_name(&self, e: Idx<Expr>) -> Option<Ustr> {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let (payload, _) = ty.peel_nullable();
            return match payload {
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name),
                    ..
                } => self.is_value_struct(*name).then_some(*name),
                _ => None,
            };
        }
        if self.concrete_plan.is_some() {
            return None;
        }
        let TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } = self.expr_ty(e)?
        else {
            return None;
        };
        self.is_value_struct(name).then_some(name)
    }

    /// All-unit enum variants lower to interned singleton globals, reference-equal
    /// across uses, so `==`/`!=` is native Lua `==`/`~=`.
    pub(crate) fn is_payloadless_enum(&self, e: Idx<Expr>) -> bool {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let (payload, _) = ty.peel_nullable();
            let crate::monoty::MonoTy::App {
                ctor: crate::monoty::MonoCtor::Nominal(name),
                ..
            } = payload
            else {
                return false;
            };
            return is_payload_free_enum(self.ws, *name);
        }
        if self.concrete_plan.is_some() {
            return false;
        }
        let Some(TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        }) = self.expr_ty(e)
        else {
            return false;
        };
        is_payload_free_enum(self.ws, name)
    }

    pub(crate) fn eq_is_scalar(&self, e: Idx<Expr>) -> bool {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let (payload, _) = ty.peel_nullable();
            return matches!(
                payload,
                crate::monoty::MonoTy::Int
                    | crate::monoty::MonoTy::Uint
                    | crate::monoty::MonoTy::Float
                    | crate::monoty::MonoTy::Bool
                    | crate::monoty::MonoTy::String
            ) || self.is_payloadless_enum(e);
        }
        if self.concrete_plan.is_some() {
            return false;
        }
        matches!(
            self.expr_ty(e),
            Some(TypeExpr::Builtin(
                BuiltinTy::Int
                    | BuiltinTy::Uint
                    | BuiltinTy::Float
                    | BuiltinTy::Bool
                    | BuiltinTy::String
            ))
        ) || self.is_payloadless_enum(e)
    }

    pub(crate) fn expr_is_flat_enum(&self, e: Idx<Expr>) -> bool {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let (payload, _) = ty.peel_nullable();
            return matches!(
                payload,
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(n), ..
                } if EnumPlan::for_name(self.ws, *n).is_some_and(|plan| plan.flat().is_some())
            );
        }
        if self.concrete_plan.is_some() {
            return false;
        }
        matches!(
            self.expr_ty(e),
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. })
                if EnumPlan::for_name(self.ws, n).is_some_and(|plan| plan.flat().is_some())
        )
    }

    /// For a value already known present: the nullable guard wraps this route, so
    /// the arm is picked from the non-null shape on both sides.
    pub(crate) fn stringify_route(&self, expr: Idx<Expr>) -> StringifyRoute {
        if self.is_null_lit(expr) {
            return StringifyRoute::ToString;
        }
        if let Some(route) = self.structural_route(expr) {
            return route;
        }
        if let Some(ty) = self.concrete_expr_ty(expr) {
            let (payload, _) = ty.peel_nullable();
            return match payload {
                crate::monoty::MonoTy::Int
                | crate::monoty::MonoTy::Uint
                | crate::monoty::MonoTy::Bool => StringifyRoute::ToString,
                crate::monoty::MonoTy::String => StringifyRoute::Verbatim,
                crate::monoty::MonoTy::Float => StringifyRoute::FormatFloat,
                crate::monoty::MonoTy::Tuple(_)
                | crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Array | crate::monoty::MonoCtor::Map,
                    ..
                } => StringifyRoute::Inspect,
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name),
                    ..
                } if self.ws.newtypes_by_name.contains_key(name) => StringifyRoute::Newtype(*name),
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name),
                    ..
                } if is_payload_free_enum(self.ws, *name) => StringifyRoute::EnumNames(*name),
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name),
                    ..
                } if self.ws.struct_by_name.contains_key(name)
                    || self.ws.enums_by_name.contains_key(name) =>
                {
                    StringifyRoute::Inspect
                }
                _ => StringifyRoute::Unsupported,
            };
        }
        if self.concrete_plan.is_some() {
            return StringifyRoute::Unsupported;
        }
        match self.expr_ty(expr) {
            Some(TypeExpr::Builtin(BuiltinTy::Int | BuiltinTy::Uint | BuiltinTy::Bool)) => {
                StringifyRoute::ToString
            }
            Some(TypeExpr::Builtin(BuiltinTy::String)) => StringifyRoute::Verbatim,
            Some(TypeExpr::Builtin(BuiltinTy::Float)) => StringifyRoute::FormatFloat,
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            }) if self.ws.newtypes_by_name.contains_key(&name) => StringifyRoute::Newtype(name),
            Some(TypeExpr::Tuple { .. })
            | Some(TypeExpr::App {
                ctor: TypeCtor::Array | TypeCtor::Map,
                ..
            }) => StringifyRoute::Inspect,
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            }) if is_payload_free_enum(self.ws, name) => StringifyRoute::EnumNames(name),
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            }) if self.ws.struct_by_name.contains_key(&name)
                || self.ws.enums_by_name.contains_key(&name) =>
            {
                StringifyRoute::Inspect
            }
            _ => StringifyRoute::Unsupported,
        }
    }

    pub(crate) fn structural_route(&self, expr: Idx<Expr>) -> Option<StringifyRoute> {
        let ty = self.hole_mono(expr)?;
        let (payload, _) = ty.peel_nullable();
        let name = payload.nominal()?;
        if !super::format::formats_structurally(self.ws, name) {
            return None;
        }
        Some(match super::format::format_tree(self.ws, payload) {
            Ok(tree) => StringifyRoute::Structural(Box::new(tree)),
            Err(reason) => StringifyRoute::Unprintable(reason),
        })
    }

    pub fn print_routes(&self, call: Idx<Expr>) -> Vec<(Idx<Expr>, StringifyRoute)> {
        let Expr::Call {
            callee: bone_core::hir::Callee::Unresolved { name, .. },
            args,
            ..
        } = &self.body.expr_arena[call]
        else {
            return Vec::new();
        };
        if name.as_str() != "print" {
            return Vec::new();
        }
        let resolved = self
            .inf
            .and_then(|i| i.resolved_overload_calls.get(&call))
            .copied()
            .or_else(|| super::unique_fn(self.ws, *name));
        let Some((module, func)) = resolved else {
            return Vec::new();
        };
        let def = &self.ws.module(module).functions[func];
        if !def.is_extern
            || !bone_core::modules::is_stdlib_module_file(self.ws.module(module).file.as_str())
        {
            return Vec::new();
        }
        args.into_iter()
            .filter_map(|arg| {
                if self.body.spread_args.contains(&arg) {
                    return None;
                }
                let ty = self.hole_mono(arg)?;
                let name = ty.peel_nullable().0.nominal()?;
                let leaf_table = super::shape::planned_value(self.ws, name)
                    .is_some_and(|plan| plan.storage == super::ValueStorage::LeafTable);
                if !self.ws.enums_by_name.contains_key(&name) && !leaf_table {
                    return None;
                }
                Some((arg, self.structural_route(arg)?))
            })
            .collect()
    }

    /// Only the stdlib's extern `tostring` and the `to_string(any)` fallback
    /// qualify; a user function or a typed overload of either name is ordinary.
    pub fn tostring_route(&self, call: Idx<Expr>) -> Option<(Idx<Expr>, StringifyRoute)> {
        let Expr::Call {
            callee: bone_core::hir::Callee::Unresolved { name, .. },
            args,
            ..
        } = &self.body.expr_arena[call]
        else {
            return None;
        };
        if !matches!(name.as_str(), "tostring" | "to_string") {
            return None;
        }
        let mut args = *args;
        let (Some(arg), None) = (args.next(), args.next()) else {
            return None;
        };
        let (module, func) = match self.inf.and_then(|i| i.resolved_overload_calls.get(&call)) {
            Some(&resolved) => resolved,
            None => super::unique_fn(self.ws, *name)?,
        };
        let def = &self.ws.module(module).functions[func];
        let takes_any = match def.param_decls.as_slice() {
            [param] => matches!(
                self.raw_ty_node(param.ty),
                Some(TypeExpr::Builtin(BuiltinTy::Any))
            ),
            _ => false,
        };
        if !takes_any || (name.as_str() == "tostring" && !def.is_extern) {
            return None;
        }
        Some((arg, self.structural_route(arg)?))
    }

    fn hole_mono(&self, expr: Idx<Expr>) -> Option<crate::monoty::MonoTy> {
        if let Some(ty) = self.concrete_expr_ty(expr) {
            return Some((*ty).clone());
        }
        if self.concrete_plan.is_some() {
            return None;
        }
        let inf = self.inf?;
        let ty = *inf.expr_types.get(expr)?;
        let empty = MonoSubst::default();
        crate::monoty::convert(self.ws, inf, ty, self.logical_subst.unwrap_or(&empty)).ok()
    }

    pub(crate) fn is_composite_expr(&self, e: Idx<Expr>) -> bool {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let (payload, _) = ty.peel_nullable();
            return match payload {
                crate::monoty::MonoTy::Tuple(_)
                | crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Array | crate::monoty::MonoCtor::Map,
                    ..
                } => true,
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name),
                    ..
                } => {
                    self.ws.struct_by_name.contains_key(name)
                        || self.ws.enums_by_name.contains_key(name)
                }
                _ => false,
            };
        }
        if self.concrete_plan.is_some() {
            return false;
        }
        match self.expr_ty(e) {
            Some(TypeExpr::Tuple { .. }) => true,
            Some(TypeExpr::App {
                ctor: TypeCtor::Array | TypeCtor::Map,
                ..
            }) => true,
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            }) => {
                self.ws.struct_by_name.contains_key(&name)
                    || self.ws.enums_by_name.contains_key(&name)
            }
            _ => false,
        }
    }

    pub fn hole_plan(&self, hole: Idx<Expr>, spec: Option<&InterpSpec>) -> HolePlan {
        let Some(spec) = spec else {
            return HolePlan::Plain(self.stringify_route(hole));
        };
        HolePlan::Formatted(self.format_plan(hole, spec))
    }

    /// Every branch that decides a stdlib call is here; the printf conversion
    /// string is not, because it emits no call.
    pub(crate) fn format_plan(&self, hole: Idx<Expr>, spec: &InterpSpec) -> FormatPlan {
        let inferred = if self.concrete_plan.is_some() {
            match self.concrete_expr_ty(hole) {
                Some(crate::monoty::MonoTy::Int) => Some('d'),
                Some(crate::monoty::MonoTy::Uint) => Some('d'),
                Some(crate::monoty::MonoTy::Float) => Some('f'),
                Some(crate::monoty::MonoTy::String | crate::monoty::MonoTy::Bool) => Some('s'),
                Some(crate::monoty::MonoTy::Nullable(inner)) => match inner.as_ref() {
                    crate::monoty::MonoTy::Int => Some('d'),
                    crate::monoty::MonoTy::Uint => Some('d'),
                    crate::monoty::MonoTy::Float => Some('f'),
                    crate::monoty::MonoTy::String | crate::monoty::MonoTy::Bool => Some('s'),
                    _ => None,
                },
                Some(_) | None => None,
            }
        } else {
            match self.expr_ty(hole) {
                Some(TypeExpr::Builtin(BuiltinTy::Int | BuiltinTy::Uint)) => Some('d'),
                Some(TypeExpr::Builtin(BuiltinTy::Float)) => Some('f'),
                Some(TypeExpr::Builtin(BuiltinTy::String | BuiltinTy::Bool)) => Some('s'),
                _ => None,
            }
        };
        let has_precision = spec.precision.is_some() || spec.precision_expr.is_some();
        // A spec carrying a numeric-presentation flag falls through to the printf path
        // and gaps there rather than being silently ignored on a structural render.
        let structural = (spec.format_type.is_none()
            && inferred.is_none()
            && spec.grouping.is_none()
            && spec.sign.is_none()
            && !spec.alternate
            && !spec.zero_pad
            && self.is_composite_expr(hole))
        .then(|| {
            if has_precision {
                StructuralRender::Formatted
            } else {
                StructuralRender::Plain(self.stringify_route(hole))
            }
        });
        let grouping_sep = spec.grouping;
        let has_width = spec.width.is_some() || spec.width_expr.is_some();
        let ty = spec.format_type.or(inferred);
        let special_conv = matches!(spec.format_type, Some('b') | Some('%'));
        let val_is_float = if self.concrete_plan.is_some() {
            matches!(
                self.concrete_expr_ty(hole),
                Some(crate::monoty::MonoTy::Float)
            )
        } else {
            matches!(
                self.expr_ty(hole),
                Some(TypeExpr::Builtin(BuiltinTy::Float))
            )
        };
        let needs_pad = spec.fill.is_some()
            || matches!(spec.alignment, Some('^'))
            || ((grouping_sep.is_some() || special_conv) && has_width);
        let skip_width = grouping_sep.is_some() || needs_pad || special_conv;
        let special = if spec.format_type == Some('%') {
            Some("__rock_fmt_percent")
        } else if ty == Some('b') {
            Some("__rock_fmt_binary")
        } else {
            None
        };
        let grouping = grouping_sep.map(|sep| {
            // `_` with a radix conversion groups by nibble (4), else by 3.
            let group = match (sep, ty) {
                ('_', Some('x' | 'X' | 'o' | 'b')) => 4i64,
                _ => 3i64,
            };
            (sep, group)
        });
        FormatPlan {
            inferred,
            structural,
            special,
            grouping,
            needs_pad,
            skip_width,
            val_is_float,
            ty,
        }
    }

    /// Null tests and same-type `nobox` comparisons are primitive operations.
    pub fn eq_route(&self, lhs: Idx<Expr>, rhs: Idx<Expr>) -> EqRoute {
        if self.is_null_lit(lhs) || self.is_null_lit(rhs) {
            return EqRoute::Primitive;
        }
        if let (Some(ln), Some(rn)) = (self.nobox_value_name(lhs), self.nobox_value_name(rhs)) {
            if ln == rn {
                return EqRoute::Primitive;
            }
        }
        // Lua/C value structs have native equality; JS reaches its per-type registry
        // through the structural helper. Different names keep the type-mismatch check.
        if let (Some(ln), Some(rn)) = (self.value_struct_name(lhs), self.value_struct_name(rhs)) {
            if ln == rn {
                return if matches!(
                    self.target,
                    bone_core::hir::FnBackend::Any | bone_core::hir::FnBackend::Js
                ) {
                    EqRoute::Structural
                } else {
                    EqRoute::Primitive
                };
            }
        }
        if self.eq_is_scalar(lhs) || self.eq_is_scalar(rhs) {
            EqRoute::Primitive
        } else {
            EqRoute::Structural
        }
    }
}
