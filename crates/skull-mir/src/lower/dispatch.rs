use bone_core::hir::{BinOp, Body, BuiltinTy, Expr, InterpSpec, TyId, TypeExpr};
use bone_core::prelude::Idx;
use bone_core::workspace::{BodyInference, Workspace};
use ustr::Ustr;

use super::{EnumPlan, RepArena, RepGuarantees, RepUse, RepUseFacts, shape::is_payload_free_enum};
use crate::concrete::{ConcreteBodyPlan, ConcreteTypeTables};
use crate::monoty::{MonoCtor, MonoSubst, MonoTy};
use std::borrow::Cow;

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
    /// Never fabricates a partial HIR node for a concrete generic argument; use
    /// `concrete_ty` or `concrete_expr_ty` for resolved facts.
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

    /// Chase `Infer` and peel `Nullable`.
    pub(crate) fn resolve_ty(&self, ty: TyId) -> Option<TypeExpr> {
        match self.raw_ty_node(ty)? {
            TypeExpr::Infer(var, _) => self.resolve_ty(self.inf?.substitutions.get(var)?),
            TypeExpr::Nullable(inner) => self.resolve_ty(ty.with_idx(inner)),
            other => Some(other),
        }
    }

    /// Chase `Infer` without peeling `Nullable`, keeping the `TyId`.
    pub(crate) fn resolve_tyid(&self, ty: TyId) -> Option<TyId> {
        match self.raw_ty_node(ty)? {
            TypeExpr::Infer(var, _) => self.resolve_tyid(self.inf?.substitutions.get(var)?),
            _ => Some(ty),
        }
    }

    pub(crate) fn expr_ty(&self, e: Idx<Expr>) -> Option<TypeExpr> {
        self.resolve_ty(*self.inf?.expr_types.get(e)?)
    }

    /// `ty` under the concrete plan, else the logical type under `logical_subst`.
    fn ty_mono(&self, ty: TyId) -> Option<Cow<'a, MonoTy>> {
        if self.concrete_plan.is_some() {
            return self.concrete_ty(ty).map(Cow::Borrowed);
        }
        self.logical_mono(ty)
    }

    fn expr_mono(&self, e: Idx<Expr>) -> Option<Cow<'a, MonoTy>> {
        if self.concrete_plan.is_some() {
            return self.concrete_expr_ty(e).map(Cow::Borrowed);
        }
        self.logical_mono(*self.inf?.expr_types.get(e)?)
    }

    fn logical_mono(&self, ty: TyId) -> Option<Cow<'a, MonoTy>> {
        let empty = MonoSubst::default();
        let subst = self.logical_subst.unwrap_or(&empty);
        crate::monoty::convert(self.ws, self.inf?, ty, subst)
            .ok()
            .map(Cow::Owned)
    }

    pub(crate) fn is_nullable_ty(&self, ty: TyId) -> bool {
        self.ty_mono(ty).is_some_and(|ty| ty.peel_nullable().1)
    }

    pub(crate) fn expr_is_nullable(&self, e: Idx<Expr>) -> bool {
        self.expr_mono(e).is_some_and(|ty| ty.peel_nullable().1)
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

    /// The nominal name of `e`'s type, nullable or not; without a concrete
    /// plan, of its HIR type.
    fn expr_nominal(&self, e: Idx<Expr>) -> Option<Ustr> {
        self.expr_mono(e)?.peel_nullable().0.nominal()
    }

    pub(crate) fn nobox_value_name(&self, e: Idx<Expr>) -> Option<Ustr> {
        self.expr_nominal(e)
            .filter(|name| self.is_nobox_struct(*name))
    }

    /// Lua/C equality reaches a value struct's `__eq` directly; JS uses the
    /// structural equality helper.
    pub fn is_value_struct(&self, name: Ustr) -> bool {
        crate::lower::shape::is_value_struct(self.ws, name)
    }

    /// Unlike [`Self::nobox_value_name`] this does not require `nobox`: every
    /// `value` struct carries a native-`==`-reachable `__eq`.
    pub(crate) fn value_struct_name(&self, e: Idx<Expr>) -> Option<Ustr> {
        self.expr_nominal(e)
            .filter(|name| self.is_value_struct(*name))
    }

    /// All-unit enum variants lower to interned singleton globals, reference-equal
    /// across uses, so `==`/`!=` is native Lua `==`/`~=`.
    pub(crate) fn is_payloadless_enum(&self, e: Idx<Expr>) -> bool {
        self.expr_nominal(e)
            .is_some_and(|name| is_payload_free_enum(self.ws, name))
    }

    pub(crate) fn eq_is_scalar(&self, e: Idx<Expr>) -> bool {
        self.expr_mono(e).is_some_and(|ty| {
            matches!(
                ty.peel_nullable().0,
                MonoTy::Int | MonoTy::Uint | MonoTy::Float | MonoTy::Bool | MonoTy::String
            )
        }) || self.is_payloadless_enum(e)
    }

    pub(crate) fn expr_is_flat_enum(&self, e: Idx<Expr>) -> bool {
        self.expr_nominal(e).is_some_and(|name| {
            EnumPlan::for_name(self.ws, name).is_some_and(|plan| plan.flat().is_some())
        })
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
        let Some(ty) = self.expr_mono(expr) else {
            return StringifyRoute::Unsupported;
        };
        match ty.peel_nullable().0 {
            MonoTy::Int | MonoTy::Uint | MonoTy::Bool => StringifyRoute::ToString,
            MonoTy::String => StringifyRoute::Verbatim,
            MonoTy::Float => StringifyRoute::FormatFloat,
            MonoTy::Tuple(_)
            | MonoTy::App {
                ctor: MonoCtor::Array | MonoCtor::Map,
                ..
            } => StringifyRoute::Inspect,
            MonoTy::App {
                ctor: MonoCtor::Nominal(name),
                ..
            } if self.ws.nominals.newtype_id(*name).is_some() => StringifyRoute::Newtype(*name),
            MonoTy::App {
                ctor: MonoCtor::Nominal(name),
                ..
            } if is_payload_free_enum(self.ws, *name) => StringifyRoute::EnumNames(*name),
            MonoTy::App {
                ctor: MonoCtor::Nominal(name),
                ..
            } if self.ws.nominals.struct_id(*name).is_some()
                || self.ws.nominals.enum_id(*name).is_some() =>
            {
                StringifyRoute::Inspect
            }
            _ => StringifyRoute::Unsupported,
        }
    }

    pub(crate) fn structural_route(&self, expr: Idx<Expr>) -> Option<StringifyRoute> {
        let ty = self.expr_mono(expr)?;
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
                let ty = self.expr_mono(arg)?;
                let name = ty.peel_nullable().0.nominal()?;
                let leaf_table = super::shape::planned_value(self.ws, name)
                    .is_some_and(|plan| plan.storage == super::ValueStorage::LeafTable);
                if !self.ws.nominals.enum_id(name).is_some() && !leaf_table {
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

    pub(crate) fn is_composite_expr(&self, e: Idx<Expr>) -> bool {
        self.expr_mono(e)
            .is_some_and(|ty| match ty.peel_nullable().0 {
                MonoTy::Tuple(_)
                | MonoTy::App {
                    ctor: MonoCtor::Array | MonoCtor::Map,
                    ..
                } => true,
                MonoTy::App {
                    ctor: MonoCtor::Nominal(name),
                    ..
                } => {
                    self.ws.nominals.struct_id(*name).is_some()
                        || self.ws.nominals.enum_id(*name).is_some()
                }
                _ => false,
            })
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
        let inferred = self
            .expr_mono(hole)
            .and_then(|ty| match ty.peel_nullable().0 {
                MonoTy::Int | MonoTy::Uint => Some('d'),
                MonoTy::Float => Some('f'),
                MonoTy::String | MonoTy::Bool => Some('s'),
                _ => None,
            });
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
        let val_is_float = self
            .expr_mono(hole)
            .is_some_and(|ty| matches!(*ty, MonoTy::Float));
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
