use bone_core::hir::{Expr, Module, Statement};
use bone_core::suspension::{uses_after_suspension, SuspensionPolicy};

use bone_core::{
    hir::{
        AsKind, BindingInfo, BindingKind, BindingPattern, BuiltinTy, Callee, DecoratorDef,
        Function, GlobalDef, InterpPart, Pattern, StructDef, StructFieldDef, TyId, TypeCtor,
        TypeExpr, VarId, VariantArgs, VariantPayload,
    },
    prelude::*,
};

use ustr::ustr;

// Compilation storage + inference result structs now live in bone-core; re-export
// them through the historical `bone::inference::*` path so consumers don't churn.
pub use bone_core::workspace::{
    BodyInference, ComptimeInputs, ComptimeVarValue, InferenceResult, Substitutions, Workspace,
};
pub use fields::vec_swizzle_components;

mod narrowing;

use narrowing::{detect_null_narrow, infer_branch, path_key, stmt_guard_narrows, Narrow, Root};

mod defer;
mod embed;
mod purity;
mod expr;
mod lambda;
mod unify;
mod calls;
mod fields;
mod patterns;
mod iter;

pub(crate) use defer::*;
pub(crate) use purity::*;
pub use expr::*;
pub(crate) use lambda::*;
pub use unify::*;
pub(crate) use calls::*;
pub(crate) use fields::*;
pub(crate) use patterns::*;
pub(crate) use iter::*;

static NOT: LazyLock<Ustr> = LazyLock::new(|| ustr("__not"));

static UNM: LazyLock<Ustr> = LazyLock::new(|| ustr("__unm"));

static NULL: LazyLock<Ustr> = LazyLock::new(|| ustr("null"));

static Q_E: LazyLock<Ustr> = LazyLock::new(|| ustr("?E"));

static Q_R: LazyLock<Ustr> = LazyLock::new(|| ustr("?R"));

static Q_LAM: LazyLock<Ustr> = LazyLock::new(|| ustr("?lam"));

static Q_FIELDREF: LazyLock<Ustr> = LazyLock::new(|| ustr("?fieldref"));

static VEC: LazyLock<Ustr> = LazyLock::new(|| ustr("Vec"));

static ENTITY: LazyLock<Ustr> = LazyLock::new(|| ustr("Entity"));

static HOLE: LazyLock<Ustr> = LazyLock::new(|| ustr("hole"));

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Use {
    Value,
    Discard,
}

struct RefSuspensionPolicy<'a> {
    gamma: &'a Gamma<'a>,
}

impl SuspensionPolicy for RefSuspensionPolicy<'_> {
    fn binding_holds_frame_data(
        &self,
        binding: Idx<BindingInfo>,
        _value: Option<Idx<Expr>>,
    ) -> bool {
        self.gamma
            .out
            .binding_types
            .get(binding)
            .copied()
            .or(self.gamma.body.binding_arena[binding].declared_ty)
            .is_some_and(|ty| contains_frame_ref(self.gamma, ty))
    }
}

#[derive(Debug)]
pub struct Gamma<'a> {
    pub workspace: &'a Workspace,
    pub body: &'a Body,
    pub current_module_id: ModuleId,

    pub next_var_id: u32,
    pub declared_ret_ty: Option<TyId>,
    pub coroutine_ret: Option<TyId>,
    pub loop_depth: u32,

    // TODO: this feels slow?
    // Non-null refinements keyed on `(Root, field-path)` -- locals AND globals
    // share this set (whole locals peel `binding_types` instead). Globals are
    // invalidated by an intervening write / non-`@pure` call / `yield` (a callee
    // can reassign one); the call/yield sites clear only `Root::Global` entries.
    pub(crate) refined_nonnull: FxHashSet<Narrow>,
    pub sibling_fields: Vec<FxHashMap<Ustr, TyId>>,

    pub dep_acc: RefCell<FxHashSet<Ustr>>,
    pub body_errors: Vec<CompilerError>,

    pub quiet: bool,

    /// The enclosing fn's `Iterator` bounds: `(generic VarId, index into
    /// `Function.generics`, declared element type)`. Set by `check_function`;
    /// empty for unbounded fns and non-fn bodies.
    pub fn_bounds: Vec<(VarId, usize, TyId)>,

    // Defer-to-use for unconstrained lambda params (defer.rs). Body-local by
    // design; kept out of `BodyInference` so checkpoint/rollback (substitution
    // trail only) never sees it -- deferral never queues under `quiet`.
    pub(crate) deferrable_vars: FxHashMap<VarId, DeferOrigin>,
    pub(crate) pending: Vec<Option<Pending>>,
    pub(crate) pending_by_var: FxHashMap<VarId, SmallVec<[usize; 2]>>,

    pub out: BodyInference,
}

#[derive(Copy, Clone, Debug)]
pub struct BodyCtx<'a> {
    pub module: &'a Module,
    pub body: &'a Body,
}

impl<'a> Gamma<'a> {
    pub fn new(workspace: &'a Workspace, body: &'a Body, mid: ModuleId) -> Gamma<'a> {
        Self {
            workspace,
            body,
            current_module_id: mid,
            next_var_id: workspace.module(mid).lower_var_high_water,
            declared_ret_ty: None,
            coroutine_ret: None,
            loop_depth: 0,

            refined_nonnull: Default::default(),
            sibling_fields: Default::default(),

            dep_acc: Default::default(),
            body_errors: Vec::new(),

            quiet: false,

            fn_bounds: Vec::new(),

            deferrable_vars: Default::default(),
            pending: Vec::new(),
            pending_by_var: Default::default(),

            out: BodyInference {
                body_generation: body.generation,

                ty_arena: body.lowered_tys.clone(),
                ty_spans: body.lowered_ty_spans.clone(),

                ..Default::default()
            },
        }
    }

    pub fn for_query(workspace: &'a Workspace, body: &'a Body, mid: ModuleId, seed: &BodyInference) -> Gamma<'a> {
        let mut g = Gamma::new(workspace, body, mid);
        let mut high = g.next_var_id;

        for (_, te) in seed.ty_arena.iter() {
            if let TypeExpr::Infer(VarId(v), _) = *te {
                high = high.max(v + 1);
            }
        }

        g.next_var_id = high;
        g.out = seed.clone();
        g.quiet = true;
        g
    }

    pub fn module(&self) -> &Module {
        &self.workspace.modules[self.current_module_id.index()]
    }

    /// A `BodyCtx` for this Gamma's own body -- pending-constraint retries run
    /// outside the syntactic walk, where no `c` is threaded through.
    pub(crate) fn body_ctx(&self) -> BodyCtx<'a> {
        let ws: &'a Workspace = self.workspace;
        BodyCtx { module: &ws.modules[self.current_module_id.index()], body: self.body }
    }

    pub fn render(&self, ty: TyId) -> String {
        self.workspace.render(Some(&self.out), ty)
    }

    pub fn fresh_callsite_var(&mut self, hint: Ustr, span: Span) -> TyId {
        let var_id = VarId(self.next_var_id);
        self.next_var_id += 1;

        let idx = self.out.ty_arena.alloc(TypeExpr::Infer(var_id, hint));
        let ty = TyId::Local(idx);

        self.out.ty_spans.insert(idx, span);

        ty
    }

    /// The enclosing fn's `Iterator` bound on the generic with this VarId, if
    /// any: `(index into Function.generics, declared element type)`.
    pub fn fn_bound_for(&self, v: VarId) -> Option<(usize, TyId)> {
        self.fn_bounds
            .iter()
            .find(|(var, _, _)| *var == v)
            .map(|&(_, idx, elem)| (idx, elem))
    }

    pub fn trial<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let prev = std::mem::replace(&mut self.quiet, true);
        let r = f(self);
        self.quiet = prev;
        r
    }

    // Why returning &TypeExpr when it's copy now?
    pub fn get(&self, ty: TyId) -> &TypeExpr {
        match ty {
            TyId::Static(mid, idx) => &self.workspace.module(mid).ty_arena[idx],
            TyId::Local(idx) => &self.out.ty_arena[idx],
        }
    }

    pub fn span_of(&self, ty: TyId) -> Option<Span> {
        match ty {
            TyId::Static(mid, idx) => self.workspace.module(mid).ty_spans.get(idx).copied(),
            TyId::Local(idx) => self.out.ty_spans.get(idx).copied(),
        }
    }

    pub fn instantiate_generics(
        &mut self,
        generics: &[TyId],
        span: Span,
    ) -> Result<(FxHashMap<VarId, TyId>, Vec<TyId>), CompilerError> {
        // Each call site needs to substitute generics for real types.
        //
        // We begin by creating a substitution map, each TypeRef::Generic
        // gains a new TypeRef::Infer, that is later replaced by `substitute()`
        let mut subs = FxHashMap::default();
        let mut fresh = Vec::with_capacity(generics.len());

        // Only Generic TypeExprs which are Static (created during Lowering) are valid,
        // each one gets a Local(TypeRef::Infer).
        for &g in generics {
            let (var_id, hint) = match *self.get(g) {
                TypeExpr::Generic(var_id, hint) => (var_id, hint),
                other => comp_error!(span, "Expected generic type, got {:?}", other),
            };

            let f = self.fresh_callsite_var(hint, span);
            subs.insert(var_id, f);
            fresh.push(f);
        }

        Ok((subs, fresh))
    }

    pub fn nominal_app(&mut self, name: Ustr, args: &[TyId], span: Span) -> TyId {
        self.app_with_ctor(TypeCtor::for_name(name), args, span)
    }

    pub fn app_with_ctor(&mut self, ctor: TypeCtor, args: &[TyId], span: Span) -> TyId {
        let mut texps = Vec::with_capacity(args.len());

        for &t in args {
            let r = resolve(self, t, span);
            let id = reify(self, r, span);
            texps.push(*self.get(TyId::Local(id)));
        }

        let params = alloc_contiguous(&mut self.out.ty_arena, texps);
        let idx = self.out.ty_arena.alloc(TypeExpr::App { ctor, params });

        resolve(self, TyId::Local(idx), span)
    }

    pub fn assign_expr_type(
        &mut self,
        c: BodyCtx,
        e: Idx<Expr>,
        ty: TyId,
    ) -> Result<(), CompilerError> {
        let span = c.body.expr_spans.get(e).copied().unwrap_or(Span::unknown());

        let ty = resolve(self, ty, span);

        if let Some(old_ty) = self.out.expr_types.insert(e, ty) {
            let old_ty = resolve(self, old_ty, span);

            if !types_equal(self, ty, old_ty) {
                let old_str = self.workspace.render(Some(&self.out), old_ty);
                let new_str = self.workspace.render(Some(&self.out), ty);

                let mut err = CompilerError::new_with_label(
                    span,
                    format!("expression inferred twice: was `{old_str}`, now `{new_str}`"),
                );

                if let Some(s) = self.span_of(old_ty) {
                    err = err.with_label(s, format!("`{old_str}` from here"));
                }

                if let Some(s) = self.span_of(ty) {
                    err = err.with_label(s, format!("`{new_str}` from here"));
                }

                return Err(err);
            }
        }

        Ok(())
    }

    pub fn assign_named_type(
        &mut self,
        name: Idx<BindingInfo>,
        ty: TyId,
    ) -> Result<(), CompilerError> {
        let span = self
            .body
            .binding_spans
            .get(name)
            .copied()
            .unwrap_or(Span::unknown());

        let ty = resolve(self, ty, span);

        if let Some(old_ty) = self.out.binding_types.insert(name, ty) {
            let old_ty = resolve(self, old_ty, span);

            if !types_equal(self, ty, old_ty) {
                let old_str = self.workspace.render(Some(&self.out), old_ty);
                let new_str = self.workspace.render(Some(&self.out), ty);

                let mut err = CompilerError::new_with_label(
                    span,
                    format!("binding inferred twice: was `{old_str}`, now `{new_str}`"),
                );

                if let Some(s) = self.span_of(old_ty) {
                    err = err.with_label(s, format!("`{old_str}` from here"));
                }

                if let Some(s) = self.span_of(ty) {
                    err = err.with_label(s, format!("`{new_str}` from here"));
                }

                return Err(err);
            }
        }

        Ok(())
    }
}

pub fn check_function(
    ws: &Workspace,
    module: &Module,
    module_id: ModuleId,
    f_id: Idx<Function>,
    f: &Function,
) -> Result<Option<(BodyOwner, BodyInference, Vec<CompilerError>)>, CompilerError> {
    let _span = tracing::info_span!("check_function", name = %f.name).entered();
    let body = &module.bodies[&BodyOwner::Function(f_id)];
    let mut gamma = Gamma::new(ws, body, module_id);
    let c = BodyCtx { module, body };

    for (idx, (g, b)) in f.generics.iter().zip(f.generic_bounds.iter()).enumerate() {
        let Some(elem) = *b else { continue };
        let TypeExpr::Generic(var_id, _) = *gamma.get(*g) else { continue };
        gamma.fn_bounds.push((var_id, idx, elem));
    }

    if f.is_coroutine {
        if let Some(ret) = f.return_ty {
            let (_y, r, _n) = coroutine_yrn(&mut gamma, ret, f.span)?;
            gamma.coroutine_ret = Some(ret);
            gamma.declared_ret_ty = Some(r);
        }
    } else if let Some(ret) = f.return_ty {
        gamma.declared_ret_ty = Some(ret);
    } else if !f.is_coroutine {
        // No annotation: a top-level fn returns `()`. Its tail is discarded
        // (the `f.return_ty`-gated branch below takes the `else`), but an
        // explicit `return <value>` is a real error -- you cannot return a
        // value from a unit fn. (A NESTED un-annotated fn is inferred-return
        // instead: it takes/replaces `declared_ret_ty` in `infer_expr`, so
        // this default only governs the enclosing top-level body.)
        gamma.declared_ret_ty = Some(gamma.workspace.builtins.unit());
    }

    for param_binding in body.param_bindings {
        let binding = &c.body.binding_arena[param_binding];

        match (&binding.kind, binding.declared_ty) {
            (BindingKind::Param, Some(ty)) => {
                gamma.assign_named_type(param_binding, ty)?;
            }

            (BindingKind::Param, None) => {
                comp_error!(
                    // TODO: better span
                    f.span,
                    "Parameter `{}` must have an annotated type",
                    binding.name
                );
            }

            _ => comp_error!(
                f.span,
                "Only parameters can be declared in the function signature"
            ),
        }
    }

    for (i, pb) in body.param_bindings.into_iter().enumerate() {
        if let Some(Some(dexpr)) = body.param_defaults.get(i) {
            let dty = infer_expr(c, &mut gamma, *dexpr)?;

            if let Some(declared) = c.body.binding_arena[pb].declared_ty {
                coerce(&mut gamma, dty, declared, f.span)?;
            }
        }
    }

    let Some(body) = body.root else {
        return Ok(Some((BodyOwner::Function(f_id), gamma.out, Vec::new())));
    };

    if let (Some(ret_ty), false) = (f.return_ty, f.is_coroutine) {
        // Return-type mismatch is recorded, but not propagated, so gamma.out is well formed
        // and partial inference survives for the LSP. Anchor it at the value the body
        // actually offers -- its tail expression -- not the whole `fn { .. }` span. A body
        // that ends in a statement offers no value at all: a clearer, distinct message.
        let tail = match c.body.expr_arena[body] {
            Expr::Block { tail, .. } => tail,
            _ => Some(body),
        };
        if let Some(tail) = tail {
            // CHECKED against the declared return, like an explicit `return` and a
            // declared `let`: a body whose tail is a lambda is otherwise inferred
            // and merely coerced, so it records its own narrower type and the
            // fn-value ABI disagrees at the two ends. The coerce still happens --
            // it moved down to the tail, keeping this span.
            if let Err(e) = check_expr(c, &mut gamma, body, ret_ty, c.body.expr_spans[tail]) {
                gamma.body_errors.push(e);
            }
        } else if {
            let body_ty = infer_expr(c, &mut gamma, body)?;
            let body_ty = resolve(&mut gamma, body_ty, f.span);
            coerce(&mut gamma, body_ty, ret_ty, f.span).is_err()
        } {
            let ret_render = gamma.workspace.render(Some(&gamma.out), ret_ty);
            gamma.body_errors.push(CompilerError::new_with_label(
                f.name_span,
                format!(
                    "function `{}` must return `{}`, but its body has no return value",
                    f.name, ret_render
                ),
            ));
        }
    } else {
        // no declared return (or coroutine): the body tail is discarded
        infer_expr_used(c, &mut gamma, body, Use::Discard)?;
    }

    gamma.out.deps = gamma.dep_acc.take();

    // `@pure` verification. Opaque (`inline(lua)`) bodies never reach here, so
    // they're trusted -- matching rock. `@pure` + `@debug_impure` on one fn is a
    // contradiction; a pure body may not read globals, mutate, or call a
    // non-pure fn.
    if fn_has_anno(f, "pure") {
        if fn_has_anno(f, "debug_impure") {
            gamma.body_errors.push(CompilerError::new_with_label(
                f.span,
                "`@pure` and `@debug_impure` on the same function are contradictory".to_string(),
            ));
        }
        let ws_ref = gamma.workspace;
        let mut purity_errors = Vec::new();
        verify_pure_expr(
            c,
            ws_ref,
            &gamma.out.resolved_overload_calls,
            body,
            &mut purity_errors,
        );
        gamma.body_errors.append(&mut purity_errors);
    }

    embed::verify_embed_projection(c, &mut gamma);

    for (binding, ty) in gamma.out.binding_types.iter() {
        if persistent_type_contains_frame_ref(&gamma, *ty) {
            gamma.body_errors.push(CompilerError::new_with_label(
                c.body.binding_spans[binding],
                format!(
                    "`{}` cannot store a frame reference in a persistent container or closure type",
                    c.body.binding_arena[binding].name
                ),
            ));
        }
    }

    if f.is_coroutine {
        let policy = RefSuspensionPolicy { gamma: &gamma };
        for at in uses_after_suspension(c.body, &policy) {
            gamma.body_errors.push(CompilerError::new_with_label(
                c.body.expr_spans[at],
                "frame reference is used after a coroutine suspension; retain an `Entity<T>` and resolve it after the yield"
                    .to_string(),
            ));
        }
    }

    report_unpinned_lambda_params(&mut gamma);

    let errors = std::mem::take(&mut gamma.body_errors);

    Ok(Some((BodyOwner::Function(f_id), gamma.out, errors)))
}

pub fn check_decorator(
    ws: &Workspace,
    module: &Module,
    module_id: ModuleId,
    d_id: Idx<DecoratorDef>,
    d: &DecoratorDef,
) -> Result<Option<(BodyOwner, BodyInference, Vec<CompilerError>)>, CompilerError> {
    let body = &module.bodies[&BodyOwner::Decorator(d_id)];
    let mut gamma = Gamma::new(ws, body, module_id);
    let c = BodyCtx { module, body };

    let mut seed = |b: Idx<BindingInfo>| -> Result<(), CompilerError> {
        let binding = &c.body.binding_arena[b];

        match binding.declared_ty {
            Some(ty) => gamma.assign_named_type(b, ty),
            None => comp_error!(d.span, "decorator binding `{}` has no type", binding.name),
        }
    };

    for p in body.param_bindings {
        seed(p)?;
    }

    if let Some(t) = body.deco_target {
        seed(t)?;
    } else {
        comp_error!(d.span, "decorator has no target binding");
    }

    let Some(root) = body.root else {
        comp_error!(d.span, "decorator has no body");
    };

    if let Err(e) = infer_expr_used(c, &mut gamma, root, Use::Discard) {
        gamma.body_errors.push(e);
    }

    embed::verify_embed_projection(c, &mut gamma);

    report_unpinned_lambda_params(&mut gamma);

    let errors = std::mem::take(&mut gamma.body_errors);

    Ok(Some((BodyOwner::Decorator(d_id), gamma.out, errors)))
}

pub fn check_global(
    ws: &Workspace,
    module: &Module,
    module_id: ModuleId,
    g_id: Idx<GlobalDef>,
    g: &GlobalDef,
) -> Result<Option<(BodyOwner, BodyInference, Vec<CompilerError>)>, CompilerError> {
    let body = &module.bodies[&BodyOwner::Global(g_id)];

    let Some(value) = body.root else {
        return Ok(None);
    };

    let c = BodyCtx { module, body };

    let mut gamma = Gamma::new(ws, body, module_id);

    if let Err(e) = (|| -> Result<(), CompilerError> {
        let vty = infer_expr(c, &mut gamma, value)?;
        coerce(&mut gamma, vty, g.ty, g.span)?;
        Ok(())
    })() {
        gamma.body_errors.push(e);
    }

    embed::verify_embed_projection(c, &mut gamma);

    report_unpinned_lambda_params(&mut gamma);

    let errors = std::mem::take(&mut gamma.body_errors);

    // Capture the names this initializer references (same as `check_function`),
    // so a global's `deps` are tracked -- the const overlay reads them to find a
    // changed const's dependents, and the cascade gets consistent global deps.
    gamma.out.deps = gamma.dep_acc.take();

    Ok(Some((BodyOwner::Global(g_id), gamma.out, errors)))
}

pub fn check_struct_defaults(
    ws: &Workspace,
    module: &Module,
    mid: ModuleId,
    sidx: Idx<StructDef>,
) -> Result<Option<(BodyOwner, BodyInference, Vec<CompilerError>)>, CompilerError> {
    let def = &module.structs[sidx];
    let body = &module.bodies[&BodyOwner::StructDefaults(sidx)];

    let c = BodyCtx { module, body };

    let mut gamma = Gamma::new(ws, body, mid);

    let (subs, arg_tys) = gamma.instantiate_generics(&def.generics, def.span)?;

    // Published so a consumer holding `Box<int>` can bind the defaults body's
    // inference class; `subs` alone is keyed by `VarId` and dies with the check.
    gamma.out.struct_default_generic_slots = arg_tys;

    let mut sib = FxHashMap::default();
    for f in def.fields.iter() {
        sib.insert(f.name, substitute(&mut gamma, f.ty, &subs, f.span)?);
    }

    gamma.sibling_fields.push(sib);

    for f in def.fields.iter() {
        if let Some(dexpr) = body.field_defaults.get(&f.name) {
            if let Err(e) = (|| -> Result<(), CompilerError> {
                let dty = infer_expr(c, &mut gamma, *dexpr)?;
                let fty = substitute(&mut gamma, f.ty, &subs, f.span)?;
                coerce(&mut gamma, dty, fty, f.span)?;

                Ok(())
            })() {
                gamma.body_errors.push(e);
            }
        }
    }

    gamma.sibling_fields.pop();

    embed::verify_embed_projection(c, &mut gamma);

    report_unpinned_lambda_params(&mut gamma);

    let errors = std::mem::take(&mut gamma.body_errors);

    Ok(Some((BodyOwner::StructDefaults(sidx), gamma.out, errors)))
}
