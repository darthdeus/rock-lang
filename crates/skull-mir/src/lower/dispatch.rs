//! **The semantic dispatch a conditionally-synthesized call comes from.**
//!
//! Two source constructs lower to a call on SOME of their arms and to no call
//! at all on the others: an interpolation hole (`"{x}"`) and structural
//! equality (`a == b`). Which arm is taken is a type decision, and the callee
//! -- `__rock_format_float`, `inspect`, `__op_eq`, `__op_ne` -- exists only on
//! that arm.
//!
//! A name-keyed trigger table cannot express either one: the source expression
//! is an `Expr::Interp` or an `Expr::Binary`, and it names no callee. So the
//! instance walk has to ask the same question lowering asks -- and asking it by
//! restating lowering's `match` walk-side is the restatement-of-a-pass trap
//! this repo has already paid for twice. [`DispatchCtx`] is the shared answer:
//! ONE resolution context and ONE route per construct, built here, CONSUMED by
//! `Builder` to pick its arm and by `skull::instance_graph` to record the
//! obligation.
//!
//! The context holds exactly what a dispatch decision reads -- the workspace,
//! one body's inference and expression arena, and the substitution the instance
//! is lowered under. `Builder` delegates every predicate below to it rather
//! than keeping a second copy, so there is no second answer to diverge.

use bone_semantic::hir::{BinOp, Body, BuiltinTy, Expr, InterpSpec, TyId, TypeCtor, TypeExpr};
use bone_semantic::inference::{BodyInference, Workspace};
use bone_semantic::prelude::Idx;
use std::rc::Rc;
use ustr::Ustr;

use super::{
    EnumPlan, RepArena, RepGuarantees, RepUse, RepUseFacts, shape::payload_free_enum_variants,
};
use crate::concrete::{ConcreteBodyPlan, ConcreteTypeTables};
use crate::monoty::MonoSubst;

/// The resolution context a lowering DISPATCH decision reads.
///
/// `values_first_class` is the GLSL representation flag (`ScalarCtx`): under it
/// nothing is nobox, because a `value` struct IS a first-class `vecN`. The
/// target-neutral instance walk passes `false`, which is not a target choice it
/// is making -- GLSL lowers neither interpolation nor structural equality, so
/// no route below is reachable with the flag set.
#[derive(Clone, Copy)]
pub struct DispatchCtx<'a> {
    pub ws: &'a Workspace,
    pub inf: Option<&'a BodyInference>,
    pub body: &'a Body,
    pub concrete_plan: Option<&'a ConcreteBodyPlan>,
    pub concrete_types: Option<&'a ConcreteTypeTables>,
    pub concrete_type_cache: Option<&'a super::ConcreteTypeCache>,
    pub lower_attribution: Option<&'a std::cell::RefCell<super::LowerAttribution>>,
    pub lower_timing_enabled: bool,
    pub interner: Option<&'a crate::monoty::MonoInterner>,
    pub rep_arena: &'a RepArena,
    pub target: bone_semantic::hir::FnBackend,
    /// Logical substitution for the target-neutral graph walk. Concrete
    /// lowering sets this to `None` because every type decision must come from
    /// its concrete plan.
    pub logical_subst: Option<&'a MonoSubst>,
    pub values_first_class: bool,
}

/// What a `{x}` interpolation hole's default (spec-free) render lowers to.
///
/// [`Self::obligations`] is the whole point: it is the ONE answer to "does
/// this hole synthesize calls, and to what".
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StringifyRoute {
    /// `tostring(v)` -- int, bool, string, and a literal `null` hole.
    ToString,
    /// `__rock_quote_string(v)` -- a string inside a structural rendering,
    /// quoted and escaped as a Rock literal.
    EscapedString,
    /// `__rock_format_float(v)`.
    FormatFloat,
    /// `Name(<tostring inner>)` for a transparent newtype.
    Newtype(Ustr),
    /// `__rock_enum_<E>_names[v]` for a payload-free enum.
    EnumNames(Ustr),
    /// `inspect(v)`.
    Inspect,
    /// The structural enum / value-record / handle rendering.
    Structural(Box<super::format::FmtNode>),
    /// The structural formatter cannot print this type; the reason names the
    /// part it rejects.
    Unprintable(String),
    /// No render exists -- lowering gaps.
    Unsupported,
}

impl StringifyRoute {
    /// The stdlib entry this route CALLS, if any. `None` is a route that emits
    /// only primitive instructions.
    pub fn synthesized_callee(&self) -> Option<&'static str> {
        match self {
            Self::FormatFloat => Some("__rock_format_float"),
            Self::EscapedString => Some("__rock_quote_string"),
            Self::Inspect => Some("inspect"),
            _ => None,
        }
    }

    /// Every call this route synthesizes, with its path within the hole. Only
    /// a structural rendering makes more than one.
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

/// What `a == b` / `a != b` lowers to.
///
/// Deliberately NOT a model of every arm lowering takes. The arms above the
/// structural decision -- a null-literal side, a nobox pair, a flat enum tag --
/// differ in REPRESENTATION, not in which callee exists, and each returns
/// before the decision this enum names. Enumerating them here would put arms in
/// lowering's `match` that no input can reach, which is exactly the
/// no-population gate `CLAUDE.md` warns about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EqRoute {
    /// Primitive `==` / `~=` on already-lowered operands.
    Primitive,
    /// The structural stdlib walk -- `__op_eq` / `__op_ne`.
    Structural,
}

impl EqRoute {
    /// The stdlib entry this route CALLS, if any.
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

/// The declarations a conditionally-synthesized call to `name` can resolve to.
///
/// These calls name a BARE SYMBOL: lowering writes `__op_eq`, not a mangled
/// spelling, so the declaration it reaches is the one whose EMITTED SYMBOL is
/// that name. Nothing else distinguishes them -- `__op_eq` has six declarations
/// in `stdlib/unified.rock`: the `@nomangle` structural helper at :743 and five
/// `@pure @inline operator(lua) ==` overloads, which MIR turns into typed
/// primitive ops and never emits as calls.
///
/// Selecting by parse order (what `resolve_op_helper` did) reached the right
/// one only because the structural helper is declared first, and its comment's
/// claim that the six share one body was false -- three of them take `int`,
/// `float` and `string`. Selecting by the mangler is the property that makes a
/// declaration callable by this name at all.
///
/// Resolution is TARGET-NEUTRAL, and both consumers use
/// [`dispatched_callee_decl`] over this set. A helper declared only
/// `inline(lua)` is still the callee a JS body names -- the JS emitter supplies
/// its own rendering through `js_supply` -- so filtering by the serving backend
/// here rejects a program the backend can emit.
pub fn dispatched_callee_decls(ws: &Workspace, name: &str) -> Vec<crate::mir::FuncRef> {
    let Some(decls) = ws.functions_by_name.get(&Ustr::from(name)) else {
        return Vec::new();
    };
    decls
        .iter()
        .map(|&(module, func)| crate::mir::FuncRef { module, func })
        .filter(|r| crate::mangle::mangle_fn(ws, &ws.module(r.module).functions[r.func]) == name)
        .collect()
}

/// THE declaration a conditionally-synthesized call to `name` resolves to.
///
/// Canonicalize every candidate through the one backend-variant grouping oracle
/// and demand unanimity -- the same rule
/// `skull::instance_graph::stdlib_entry_representative` applies to the named
/// runtime entries. Lowering and the instance walk call THIS function, not two
/// narrowings of the set above: a target-specific pick on one side and a
/// representative on the other is exactly the disagreement the join exists to
/// detect, and it would report as a callee mismatch rather than as a resolver
/// bug.
pub fn dispatched_callee_decl(ws: &Workspace, name: &str) -> Option<crate::mir::FuncRef> {
    let mut reps = dispatched_callee_decls(ws, name)
        .into_iter()
        .map(|r| super::canonical_variant(ws, r));
    let first = reps.next()?;
    reps.all(|r| r == first).then_some(first)
}

/// How a `{x:spec}` hole renders a value that has no printf conversion.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StructuralRender {
    /// `inspect_formatted(v, prec)` -- the spec asked for a precision.
    Formatted,
    /// The ordinary by-type render, then padded.
    Plain(StringifyRoute),
}

/// The helper CALLS a `{x:spec}` hole makes, plus the flags that decide them.
///
/// Lowering RENDERS this plan and the instance walk reads its
/// [`Self::obligations`]. Built once, in one place: a second derivation of
/// "which helper does this spec call" is free to be weaker, and silently would
/// be -- the printf rendering below it is a dozen interacting flags.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FormatPlan {
    /// The conversion letter implied by the value's type when the spec omits
    /// one (`{x:5}`).
    pub inferred: Option<char>,
    /// Set iff the value has no printf conversion at all.
    pub structural: Option<StructuralRender>,
    /// `__rock_fmt_percent` / `__rock_fmt_binary`, when the conversion has no
    /// printf equivalent.
    pub special: Option<&'static str>,
    /// Separator and digit-group size for `__rock_fmt_grouping`.
    pub grouping: Option<(char, i64)>,
    /// Whether `__rock_fmt_pad` runs after the conversion.
    pub needs_pad: bool,
    /// Whether the printf conversion string omits the width (the pad helper
    /// takes it instead).
    pub skip_width: bool,
    /// The value's type is `float`, which floors before an integer conversion.
    pub val_is_float: bool,
    /// The effective conversion letter: the spec's, else [`Self::inferred`].
    pub ty: Option<char>,
}

impl FormatPlan {
    /// Every stdlib entry this plan calls, in lowering's emission order.
    ///
    /// The structural branch pads UNCONDITIONALLY -- `pad_to_width` with no
    /// width is a no-op on the string, not a skipped call -- so the pad
    /// obligation is present there whatever the spec says.
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

/// The complete render plan for ONE interpolation hole.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HolePlan {
    /// No spec: render by the hole's own type.
    Plain(StringifyRoute),
    /// A format spec.
    Formatted(FormatPlan),
}

impl HolePlan {
    /// Every synthesized callee obligation this hole carries, each with the
    /// path that identifies it within the hole.
    pub fn obligations(&self) -> Vec<(Vec<u32>, &'static str)> {
        match self {
            Self::Plain(route) => route.obligations(),
            Self::Formatted(plan) => plan.obligations(),
        }
    }
}

impl<'a> DispatchCtx<'a> {
    // --- type resolution --------------------------------------------------
    //
    // The `Builder` methods of these names delegate here; this is the only
    // implementation.

    /// The stored type node, WITHOUT the specialization substitution.
    pub fn raw_ty_node(&self, ty: TyId) -> Option<TypeExpr> {
        Some(match ty {
            TyId::Static(mid, idx) => self.ws.module(mid).ty_arena[idx],
            TyId::Local(idx) => self.inf?.ty_arena[idx],
        })
    }

    /// The concrete logical type for a plan-owned slot. This is deliberately
    /// separate from `TypeExpr`: a `MonoTy` owns all children and does not need
    /// an HIR arena to fabricate a partial node.
    pub fn concrete_ty(&self, ty: TyId) -> Option<Rc<crate::monoty::MonoTy>> {
        let id = self.concrete_types?.ty_type(ty)?;
        self.concrete_mono(id)
    }

    pub fn concrete_expr_ty(&self, expr: Idx<Expr>) -> Option<Rc<crate::monoty::MonoTy>> {
        let id = self.concrete_types?.expr_type(expr)?;
        self.concrete_mono(id)
    }

    pub(crate) fn concrete_mono(
        &self,
        id: crate::monoty::MonoTyId,
    ) -> Option<Rc<crate::monoty::MonoTy>> {
        let timing = self.lower_attribution.is_some() && self.lower_timing_enabled;
        if let Some(stats) = self.lower_attribution {
            stats
                .borrow_mut()
                .record_concrete_mono_request(super::ConcreteMonoConsumer::Dispatch);
        }
        let interner = self.interner?;
        if let Some(cache) = self.concrete_type_cache {
            let cache_lookup_start = timing.then(bone_core::prelude::Instant::now);
            let hit = cache.borrow().get(&id).map(Rc::clone);
            let cache_lookup_ns = cache_lookup_start
                .map(|start| start.elapsed().as_nanos() as u64)
                .unwrap_or_default();
            if let Some(ty) = hit {
                if let Some(stats) = self.lower_attribution {
                    let mut stats = stats.borrow_mut();
                    stats.concrete_mono_shared_handle_requests += 1;
                    if timing {
                        stats.concrete_mono_timed_requests += 1;
                    }
                    stats.record_concrete_mono_cache_hit(
                        super::ConcreteMonoConsumer::Dispatch,
                        0,
                        0,
                        cache_lookup_ns,
                        0,
                    );
                }
                return Some(ty);
            }
            let lookup_start = timing.then(bone_core::prelude::Instant::now);
            let (ty, interner_cold) = interner.get_shared_with_status(id);
            let lookup_ns = lookup_start
                .map(|start| start.elapsed().as_nanos() as u64)
                .unwrap_or_default();
            let nodes = if interner_cold {
                super::mono_ty_node_count(&ty)
            } else {
                Default::default()
            };
            cache.borrow_mut().insert(id, Rc::clone(&ty));
            if let Some(stats) = self.lower_attribution {
                let mut stats = stats.borrow_mut();
                stats.concrete_mono_shared_handle_requests += 1;
                if timing {
                    stats.concrete_mono_timed_requests += 1;
                }
                stats.record_concrete_mono_derivation(
                    super::ConcreteMonoConsumer::Dispatch,
                    nodes,
                    0,
                    lookup_ns,
                    cache_lookup_ns,
                    0,
                );
            }
            return Some(ty);
        }
        if timing {
            if let Some(stats) = self.lower_attribution {
                stats.borrow_mut().concrete_mono_timed_requests += 1;
            }
        }
        let lookup_start = timing.then(bone_core::prelude::Instant::now);
        let (ty, interner_cold) = interner.get_shared_with_status(id);
        let lookup_ns = lookup_start
            .map(|start| start.elapsed().as_nanos() as u64)
            .unwrap_or_default();
        if let Some(stats) = self.lower_attribution {
            let mut stats = stats.borrow_mut();
            stats.concrete_mono_shared_handle_requests += 1;
            let nodes = if interner_cold {
                super::mono_ty_node_count(&ty)
            } else {
                Default::default()
            };
            stats.record_concrete_mono_derivation(
                super::ConcreteMonoConsumer::Dispatch,
                nodes,
                0,
                lookup_ns,
                0,
                0,
            );
        }
        Some(ty)
    }

    /// The stored HIR node. Concrete plan consumers use `concrete_ty` or
    /// `concrete_expr_ty` for resolved facts; this accessor never fabricates a
    /// partial HIR node for a concrete generic argument.
    pub fn ty_node(&self, ty: TyId) -> Option<TypeExpr> {
        self.raw_ty_node(ty)
    }

    /// Chase `Infer` and peel `Nullable`.
    pub fn resolve_ty(&self, ty: TyId) -> Option<TypeExpr> {
        match self.ty_node(ty)? {
            TypeExpr::Infer(var, _) => self.resolve_ty(self.inf?.substitutions.get(var)?),
            TypeExpr::Nullable(inner) => self.resolve_ty(ty.with_idx(inner)),
            other => Some(other),
        }
    }

    /// Chase `Infer` WITHOUT peeling `Nullable`, keeping the `TyId`.
    pub fn resolve_tyid(&self, ty: TyId) -> Option<TyId> {
        match self.ty_node(ty)? {
            TypeExpr::Infer(var, _) => self.resolve_tyid(self.inf?.substitutions.get(var)?),
            _ => Some(ty),
        }
    }

    pub fn expr_ty(&self, e: Idx<Expr>) -> Option<TypeExpr> {
        self.resolve_ty(*self.inf?.expr_types.get(e)?)
    }

    pub fn is_nullable_ty(&self, ty: TyId) -> bool {
        if let Some(ty) = self.concrete_ty(ty) {
            return ty.peel_nullable().1;
        }
        if self.concrete_plan.is_some() {
            return false;
        }
        // A generic bound to `E?` is nullable and `ty_node` cannot say so: it
        // substitutes the PAYLOAD. Read the raw node before that erases the `?`.
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

    pub fn expr_is_nullable(&self, e: Idx<Expr>) -> bool {
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

    pub fn is_null_lit(&self, e: Idx<Expr>) -> bool {
        matches!(self.body.expr_arena[e], Expr::Null)
    }

    pub fn is_nobox_struct(&self, name: Ustr) -> bool {
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

    pub fn nobox_value_name(&self, e: Idx<Expr>) -> Option<Ustr> {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let mut payload = &*ty;
            while let crate::monoty::MonoTy::Nullable(inner) = payload {
                payload = inner.as_ref();
            }
            return match payload {
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

    /// Whether `name` declares a `value` struct (any storage tier -- cdata,
    /// GC-carrying table, or leaf table). Every tier gets an unconditionally
    /// emitted per-type `__eq` (`value_struct_metatypes` for cdata,
    /// `value_struct_gc_defs` for the GC-carrying and leaf-table tiers in
    /// `skull/src/lib.rs`), so a bare `a == b` on two same-named `value`
    /// operands already invokes exactly that metamethod -- native `==` is not
    /// a weaker answer than the structural walk here, it IS the walk's own
    /// authority for this case (`stdlib/unified.rock`'s `__op_eq` comment:
    /// "cdata value structs ... hit the primitive `==` fast path", and its
    /// GC-carrying branch documents the per-type metatable as authoritative).
    pub fn is_value_struct(&self, name: Ustr) -> bool {
        self.ws.struct_by_name.get(&name).is_some_and(|sid| {
            crate::lower::has_value_plan(&self.ws.modules[sid.module.index()].structs[sid.def])
        })
    }

    /// The declared `value` struct name of `e`'s (nullable-peeled) static
    /// type, or `None` when it is not a `value` struct. Unlike
    /// [`Self::nobox_value_name`] this does NOT require the `nobox`
    /// annotation -- it is the equality-only question "does this type already
    /// carry a native-`==`-reachable `__eq`", which every `value` struct
    /// answers yes to regardless of storage tier or ABI opt-in.
    pub fn value_struct_name(&self, e: Idx<Expr>) -> Option<Ustr> {
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

    /// Whether `e`'s static type is an enum ALL of whose variants are unit (no
    /// payload). Those variants lower to interned singleton globals --
    /// reference-equal across uses -- so `==`/`!=` is native Lua `==`/`~=`, not
    /// the dynamic `__op_eq` (`type()`/`pairs()`) walk.
    pub fn is_payloadless_enum(&self, e: Idx<Expr>) -> bool {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let (payload, _) = ty.peel_nullable();
            let crate::monoty::MonoTy::App {
                ctor: crate::monoty::MonoCtor::Nominal(name),
                ..
            } = payload
            else {
                return false;
            };
            let Some(eid) = self.ws.enums_by_name.get(name) else {
                return false;
            };
            let ed = &self.ws.modules[eid.module.index()].enums[eid.def];
            return ed
                .variants
                .iter()
                .all(|v| matches!(v.payload, bone_semantic::hir::VariantPayload::Unit));
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
        let Some(eid) = self.ws.enums_by_name.get(&name) else {
            return false;
        };
        let ed = &self.ws.modules[eid.module.index()].enums[eid.def];
        ed.variants
            .iter()
            .all(|v| matches!(v.payload, bone_semantic::hir::VariantPayload::Unit))
    }

    pub fn eq_is_scalar(&self, e: Idx<Expr>) -> bool {
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

    pub fn expr_is_flat_enum(&self, e: Idx<Expr>) -> bool {
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

    // --- the two dispatch oracles ------------------------------------------

    /// The route a `{x}` hole with NO format spec takes, for a value already
    /// known present. The nullable guard wraps this rather than changing it --
    /// `expr_ty` peels `Nullable`, so the arm is picked from the non-null shape
    /// on both sides.
    pub fn stringify_route(&self, expr: Idx<Expr>) -> StringifyRoute {
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
                | crate::monoty::MonoTy::Bool
                | crate::monoty::MonoTy::String => StringifyRoute::ToString,
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
                } if payload_free_enum_variants(self.ws, *name).is_some() => {
                    StringifyRoute::EnumNames(*name)
                }
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
            Some(TypeExpr::Builtin(
                BuiltinTy::Int | BuiltinTy::Uint | BuiltinTy::Bool | BuiltinTy::String,
            )) => StringifyRoute::ToString,
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
            }) if payload_free_enum_variants(self.ws, name).is_some() => {
                StringifyRoute::EnumNames(name)
            }
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

    /// The structural rendering of `expr`'s non-null type, when that type is
    /// one the structural formatter owns (an enum with a payload, a value
    /// record, a handle). `None` leaves the hole to the by-type routes.
    pub fn structural_route(&self, expr: Idx<Expr>) -> Option<StringifyRoute> {
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

    /// `tostring(x)` / `to_string(x)` where `x` formats structurally: the
    /// argument and its route. Only the stdlib's extern `tostring` and the
    /// `to_string(any)` fallback qualify -- a user function or a typed overload
    /// of either name is an ordinary call.
    pub fn tostring_route(&self, call: Idx<Expr>) -> Option<(Idx<Expr>, StringifyRoute)> {
        let Expr::Call {
            callee: bone_semantic::hir::Callee::Unresolved { name, .. },
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
        let resolved = match self.inf.and_then(|i| i.resolved_overload_calls.get(&call)) {
            Some(&(module, func)) => vec![(module, func)],
            None => self.ws.functions_by_name.get(name)?.to_vec(),
        };
        let [(module, func)] = resolved[..] else {
            return None;
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

    /// The logical type of `expr` from whichever plan this context carries.
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

    /// Whether `e`'s type is a composite (tuple/array/map/struct/enum) -- what
    /// `inspect`/`inspect_formatted` render structurally.
    pub fn is_composite_expr(&self, e: Idx<Expr>) -> bool {
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

    /// THE plan for one interpolation hole. `spec` is the hole's format spec.
    pub fn hole_plan(&self, hole: Idx<Expr>, spec: Option<&InterpSpec>) -> HolePlan {
        let Some(spec) = spec else {
            return HolePlan::Plain(self.stringify_route(hole));
        };
        HolePlan::Formatted(self.format_plan(hole, spec))
    }

    /// THE plan for a `{x:spec}` hole. Every branch that decides a stdlib call
    /// is here; the printf conversion STRING is not, because it emits no call.
    pub fn format_plan(&self, hole: Idx<Expr>, spec: &InterpSpec) -> FormatPlan {
        let inferred = if self.concrete_plan.is_some() {
            match self.concrete_expr_ty(hole).as_deref() {
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
        // A composite has no printf conversion. The numeric-presentation flags
        // have no meaning on a structural rendering, so a spec carrying one
        // falls through to the printf path and gaps there rather than being
        // silently ignored.
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
                self.concrete_expr_ty(hole).as_deref(),
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

    /// The route `lhs == rhs` / `lhs != rhs` takes.
    ///
    /// A null-literal side and a matched nobox pair are answered `Primitive`
    /// here for the same reason lowering returns early on them: neither can
    /// reach the structural helper. Stating it means the walk needs no
    /// knowledge of the order lowering tests them in.
    pub fn eq_route(&self, lhs: Idx<Expr>, rhs: Idx<Expr>) -> EqRoute {
        if self.is_null_lit(lhs) || self.is_null_lit(rhs) {
            return EqRoute::Primitive;
        }
        if let (Some(ln), Some(rn)) = (self.nobox_value_name(lhs), self.nobox_value_name(rhs)) {
            if ln == rn {
                return EqRoute::Primitive;
            }
        }
        // Lua/C value structs have native equality. JS objects reach their
        // per-type equality registry through the structural helper instead.
        // The target-neutral graph retains that JS callee obligation too.
        // Different names retain the helper's type-mismatch check.
        if let (Some(ln), Some(rn)) = (self.value_struct_name(lhs), self.value_struct_name(rhs)) {
            if ln == rn {
                return if matches!(
                    self.target,
                    bone_semantic::hir::FnBackend::Any | bone_semantic::hir::FnBackend::Js
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
