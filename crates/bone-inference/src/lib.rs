use bone_core::hir::{Expr, Module, Statement};
use bone_core::suspension::{SuspensionPolicy, uses_after_suspension};

use bone_core::{
    hir::{
        AsKind, BindingInfo, BindingKind, BuiltinTy, CallFamily, Callee, DecoratorDef, Function,
        GlobalDef, InterpPart, Pattern, StructDef, StructFieldDef, TyId, TypeCtor, TypeExpr, VarId,
        VariantArgs, VariantPayload,
    },
    prelude::*,
};

use ustr::ustr;

pub use bone_core::modules::{AccessSite, NsVisibility, PackageId};
pub use bone_core::workspace::{
    BodyInference, ComptimeInputs, ComptimeVarValue, InferenceResult, Substitutions, Workspace,
};
pub use fields::struct_fields_of;
pub use iter::{iterable_elem, iterator_elem};

mod narrowing;

use narrowing::{Narrow, Root, detect_null_narrow, infer_branch, path_key, stmt_guard_narrows};

mod calls;
mod defer;
mod embed;
mod exhaustive;
mod expr;
mod fields;
mod iter;
mod lambda;
mod patterns;
mod purity;
mod unify;

pub(crate) use calls::*;
pub(crate) use defer::*;
pub(crate) use expr::*;
pub(crate) use fields::*;
pub(crate) use iter::*;
pub(crate) use lambda::*;
pub(crate) use patterns::*;
pub(crate) use purity::*;
pub use unify::*;

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

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RefProvenance {
    Stable,
    Local,
    Relocatable,
    Unknown,
}

impl RefProvenance {
    pub(crate) fn merge(self, other: Self) -> Self {
        self.max(other)
    }

    pub(crate) fn retainable(self) -> bool {
        matches!(self, Self::Stable | Self::Local)
    }

    pub(crate) fn returnable(self) -> bool {
        self == Self::Stable
    }
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
    /// A `defer` body runs while the function is already leaving, so `return`
    /// has nothing to return from.
    pub in_defer: bool,

    /// Path refinements expire on effects that can mutate fields or globals.
    pub(crate) refined_nonnull: FxHashSet<Narrow>,
    /// A call can revoke whole-local refinements of bindings a lambda writes.
    pub(crate) closure_assigned: FxHashSet<Idx<BindingInfo>>,
    /// Whole-local narrows of `closure_assigned` bindings in force, with the
    /// type each replaced; drained by [`Gamma::invalidate_mutable_refinements`].
    pub(crate) revocable_locals: Vec<(Idx<BindingInfo>, TyId)>,
    pub(crate) refinement_epoch: u64,
    pub sibling_fields: Vec<FxHashMap<Ustr, TyId>>,

    pub dep_acc: RefCell<FxHashSet<Ustr>>,
    pub body_errors: Vec<CompilerError>,

    pub quiet: bool,

    /// (Generic variable, generic index, element type) for each Iterator bound.
    pub fn_bounds: Vec<(VarId, usize, TyId)>,

    // Trials never queue pending constraints, so their rollback needn't track them.
    pub(crate) deferrable_vars: FxHashMap<VarId, DeferOrigin>,
    pub(crate) pending: Vec<Option<Pending>>,
    pub(crate) pending_by_var: FxHashMap<VarId, SmallVec<[usize; 2]>>,

    pub(crate) allow_frame_ref_return_place: bool,
    pub(crate) ref_binding_provenance: FxHashMap<Idx<BindingInfo>, RefProvenance>,

    pub(crate) iter_result_lambda: Option<Idx<Expr>>,
    pub(crate) lambda_returns: Option<Vec<Idx<Expr>>>,

    pub out: BodyInference,
}

#[derive(Copy, Clone, Debug)]
pub struct BodyCtx<'a> {
    pub module: &'a Module,
    pub body: &'a Body,
}

impl<'a> Gamma<'a> {
    pub(crate) fn invalidate_mutable_refinements(&mut self) {
        self.refined_nonnull.clear();
        self.refinement_epoch += 1;
        // A path can narrow its root more than once. Restore the outermost
        // type last, rather than retaining an inner, already narrowed type.
        for (b, old) in self.revocable_locals.drain(..).rev() {
            self.out.binding_types.insert(b, old);
        }
    }

    pub(crate) fn restore_local_narrow(&mut self, b: Idx<BindingInfo>, old: TyId, epoch: u64) {
        // Leaving an inner scope must not resurrect an outer narrow that a
        // callback invalidated while the inner scope was being inferred.
        if epoch == self.refinement_epoch || !self.closure_assigned.contains(&b) {
            self.out.binding_types.insert(b, old);
        }
    }

    pub(crate) fn narrow_local(&mut self, b: Idx<BindingInfo>, span: Span) -> Option<TyId> {
        let old = self.out.binding_types.get(b).copied()?;
        let narrowed = unify::peel_nullable(self, old, span);
        self.out.binding_types.insert(b, narrowed);
        if self.closure_assigned.contains(&b) {
            self.revocable_locals.push((b, old));
        }
        Some(old)
    }

    pub fn new(workspace: &'a Workspace, body: &'a Body, mid: ModuleId) -> Gamma<'a> {
        Self {
            workspace,
            body,
            current_module_id: mid,
            next_var_id: workspace.module(mid).lower_var_high_water,
            declared_ret_ty: None,
            coroutine_ret: None,
            loop_depth: 0,
            in_defer: false,

            refined_nonnull: Default::default(),
            closure_assigned: narrowing::closure_assigned_locals(body),
            revocable_locals: Vec::new(),
            refinement_epoch: 0,
            sibling_fields: Default::default(),

            dep_acc: Default::default(),
            body_errors: Vec::new(),

            quiet: false,

            fn_bounds: Vec::new(),

            deferrable_vars: Default::default(),
            pending: Vec::new(),
            pending_by_var: Default::default(),

            allow_frame_ref_return_place: false,
            ref_binding_provenance: Default::default(),

            iter_result_lambda: None,
            lambda_returns: None,

            out: BodyInference {
                body_generation: body.generation,

                ty_arena: body.lowered_tys.clone(),
                ty_spans: body.lowered_ty_spans.clone(),

                ..Default::default()
            },
        }
    }

    pub(crate) fn access_site(&self) -> AccessSite<'_> {
        let file = self.workspace.module(self.current_module_id).file;
        if let Some(frag) = self.workspace.fragment_scopes.get(&file) {
            AccessSite {
                package: Some(frag.package),
                module_path: &frag.module_path,
            }
        } else {
            AccessSite {
                package: None,
                module_path: &[],
            }
        }
    }

    pub fn for_query(
        workspace: &'a Workspace,
        body: &'a Body,
        mid: ModuleId,
        seed: &BodyInference,
    ) -> Gamma<'a> {
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

    pub(crate) fn body_ctx(&self) -> BodyCtx<'a> {
        let ws: &'a Workspace = self.workspace;
        BodyCtx {
            module: &ws.modules[self.current_module_id.index()],
            body: self.body,
        }
    }

    pub fn render(&self, ty: TyId) -> String {
        self.workspace.render(Some(&self.out), ty)
    }

    pub(crate) fn fresh_callsite_var(&mut self, hint: Ustr, span: Span) -> TyId {
        let var_id = VarId(self.next_var_id);
        self.next_var_id += 1;

        let idx = self.out.ty_arena.alloc(TypeExpr::Infer(var_id, hint));
        let ty = TyId::Local(idx);

        self.out.ty_spans.insert(idx, span);

        ty
    }

    pub(crate) fn fn_bound_for(&self, v: VarId) -> Option<(usize, TyId)> {
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

    pub fn get(&self, ty: TyId) -> &TypeExpr {
        match ty {
            TyId::Static(mid, idx) => &self.workspace.module(mid).ty_arena[idx],
            TyId::Local(idx) => &self.out.ty_arena[idx],
        }
    }

    pub(crate) fn span_of(&self, ty: TyId) -> Option<Span> {
        match ty {
            TyId::Static(mid, idx) => self.workspace.module(mid).ty_spans.get(idx).copied(),
            TyId::Local(idx) => self.out.ty_spans.get(idx).copied(),
        }
    }

    pub(crate) fn instantiate_generics(
        &mut self,
        generics: &[TyId],
        span: Span,
    ) -> Result<(FxHashMap<VarId, TyId>, Vec<TyId>), CompilerError> {
        let mut subs = FxHashMap::default();
        let mut fresh = Vec::with_capacity(generics.len());

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

    pub(crate) fn app_with_ctor(&mut self, ctor: TypeCtor, args: &[TyId], span: Span) -> TyId {
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

    pub(crate) fn assign_expr_type(
        &mut self,
        c: BodyCtx,
        e: Idx<Expr>,
        ty: TyId,
    ) -> Result<(), CompilerError> {
        let span = c.body.expr_spans.get(e).copied().unwrap_or(Span::unknown());
        let ty = resolve(self, ty, span);
        let old_ty = self.out.expr_types.insert(e, ty);
        self.check_reassigned_type("expression", old_ty, ty, span)
    }

    pub(crate) fn assign_named_type(
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
        let old_ty = self.out.binding_types.insert(name, ty);
        self.check_reassigned_type("binding", old_ty, ty, span)
    }

    fn check_reassigned_type(
        &mut self,
        what: &str,
        old_ty: Option<TyId>,
        ty: TyId,
        span: Span,
    ) -> Result<(), CompilerError> {
        let Some(old_ty) = old_ty else {
            return Ok(());
        };
        let old_ty = resolve(self, old_ty, span);
        if types_equal(self, ty, old_ty) {
            return Ok(());
        }

        let old_str = self.render(old_ty);
        let new_str = self.render(ty);

        let mut err = CompilerError::new_with_label(
            span,
            format!("{what} inferred twice: was `{old_str}`, now `{new_str}`"),
        );

        if let Some(s) = self.span_of(old_ty) {
            err = err.with_label(s, format!("`{old_str}` from here"));
        }

        if let Some(s) = self.span_of(ty) {
            err = err.with_label(s, format!("`{new_str}` from here"));
        }

        Err(err)
    }
}

/// Foreign wrappers can materialize only literal defaults.
fn is_literal_default(body: &bone_core::hir::Body, e: Idx<Expr>) -> bool {
    match body.expr_arena[e] {
        Expr::Null
        | Expr::Unit
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_) => true,
        Expr::UnaryMinus(inner, _) => is_literal_default(body, inner),
        _ => false,
    }
}

pub fn check_body(
    ws: &Workspace,
    c: BodyCtx,
    module_id: ModuleId,
    owner: BodyOwner,
) -> Option<Result<(BodyInference, Vec<CompilerError>), CompilerError>> {
    let root = c.body.root;
    if matches!(owner, BodyOwner::Global(_)) && root.is_none() {
        return None;
    }

    let _span = match owner {
        BodyOwner::Function(id) => {
            let f = &c.module.functions[id];
            Some(tracing::info_span!("check_function", name = %f.name).entered())
        }
        _ => None,
    };
    let mut gamma = Gamma::new(ws, c.body, module_id);
    let result = match owner {
        BodyOwner::Function(id) => check_function(c, &mut gamma, &c.module.functions[id]),
        BodyOwner::Decorator(id) => check_decorator(c, &mut gamma, &c.module.decorators[id]),
        BodyOwner::Global(id) => {
            check_global(c, &mut gamma, &c.module.globals[id], root.unwrap());
            Ok(())
        }
        BodyOwner::StructDefaults(id) => {
            check_struct_defaults(c, &mut gamma, &c.module.structs[id])
        }
        BodyOwner::Enum(_) | BodyOwner::Newtype(_) => return None,
    };
    Some(result.map(|()| {
        if root.is_some() || !matches!(owner, BodyOwner::Function(_)) {
            report_unpinned_lambda_params(&mut gamma);
        }
        (gamma.out, gamma.body_errors)
    }))
}

fn check_function(c: BodyCtx, gamma: &mut Gamma, f: &Function) -> Result<(), CompilerError> {
    let body = c.body;
    let frame_ref_source = f
        .annotations
        .iter()
        .find(|annotation| annotation.is_builtin("frame_ref_source"));
    let is_arena_stdlib =
        bone_core::modules::stdlib_file_name(c.module.file.as_str()) == Some("arena.rock");
    if let Some(annotation) = frame_ref_source
        && !is_arena_stdlib
    {
        comp_error!(
            annotation.span,
            "`@frame_ref_source` is reserved for the standard arena storage primitive"
        );
    }
    gamma.allow_frame_ref_return_place = frame_ref_source.is_some();

    for (idx, (g, b)) in f.generics.iter().zip(f.generic_bounds.iter()).enumerate() {
        let Some(elem) = *b else { continue };
        let TypeExpr::Generic(var_id, _) = *gamma.get(*g) else {
            continue;
        };
        gamma.fn_bounds.push((var_id, idx, elem));
    }

    if f.is_coroutine {
        if let Some(ret) = f.return_ty {
            let (_y, r, _n) = coroutine_yrn(gamma, ret, f.span)?;
            gamma.coroutine_ret = Some(ret);
            gamma.declared_ret_ty = Some(r);
        }
    } else if let Some(ret) = f.return_ty {
        gamma.declared_ret_ty = Some(ret);
    } else {
        // An unannotated top-level function discards its tail and rejects a value return.
        gamma.declared_ret_ty = Some(gamma.workspace.builtins.unit());
    }

    for param_binding in body.param_bindings {
        let binding = &c.body.binding_arena[param_binding];

        match (&binding.kind, binding.declared_ty) {
            (BindingKind::Param, Some(ty)) => {
                gamma.assign_named_type(param_binding, ty)?;
                if contains_frame_ref(gamma, ty) {
                    gamma
                        .ref_binding_provenance
                        .insert(param_binding, RefProvenance::Stable);
                }
            }

            (BindingKind::Param, None) => {
                comp_error!(
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

    let is_foreign = f.inline_body.is_some() || f.is_extern;
    for (i, pb) in body.param_bindings.into_iter().enumerate() {
        if let Some(Some(dexpr)) = body.param_defaults.get(i) {
            // Inline and foreign calls have no Rock prologue to evaluate defaults.
            if is_foreign && !is_literal_default(c.body, *dexpr) {
                gamma.body_errors.push(CompilerError::new_with_label(
                    c.body.expr_spans[*dexpr],
                    format!(
                        "default of `{}` on a foreign function must be a literal; \
                         declare it nullable with a `null` default and resolve it \
                         in a Rock wrapper",
                        c.body.binding_arena[pb].name
                    ),
                ));
            }
            let dty = infer_expr(c, gamma, *dexpr)?;

            if let Some(declared) = c.body.binding_arena[pb].declared_ty {
                coerce(gamma, dty, declared, f.span)?;
            }
        }
    }

    let Some(body) = body.root else {
        return Ok(());
    };

    if let (Some(ret_ty), false) = (f.return_ty, f.is_coroutine) {
        // Keep partial inference for the editor and report return mismatches at the tail.
        let tail = match c.body.expr_arena[body] {
            Expr::Block { tail, .. } => tail,
            _ => Some(body),
        };
        if let Some(tail) = tail {
            if let Err(e) = check_expr(c, gamma, body, ret_ty, c.body.expr_spans[tail]) {
                gamma.body_errors.push(e);
            } else if let Err(e) = validate_frame_ref_return(c, gamma, body, ret_ty) {
                gamma.body_errors.push(e);
            }
        } else {
            let body_ty = infer_expr(c, gamma, body)?;
            let body_ty = resolve(gamma, body_ty, f.span);
            if coerce(gamma, body_ty, ret_ty, f.span).is_err() {
                let ret_render = gamma.render(ret_ty);
                gamma.body_errors.push(CompilerError::new_with_label(
                    f.name_span,
                    format!(
                        "function `{}` must return `{}`, but its body has no return value",
                        f.name, ret_render
                    ),
                ));
            }
        }
    } else {
        infer_expr_used(c, gamma, body, Use::Discard)?;
    }

    gamma.out.deps = gamma.dep_acc.take();

    if fn_has_anno(f, "pure") {
        if fn_has_anno(f, "debug_impure") {
            gamma.body_errors.push(CompilerError::new_with_label(
                f.span,
                "`@pure` and `@debug_impure` on the same function are contradictory".to_string(),
            ));
        }
        let ws_ref = gamma.workspace;
        let mut purity_errors = Vec::new();
        verify_pure(
            c,
            ws_ref,
            &gamma.out.resolved_overload_calls,
            bone_core::hir_walk::HirChild::Expr(body),
            &mut purity_errors,
        );
        gamma.body_errors.append(&mut purity_errors);
    }

    embed::verify_embed_projection(c, gamma);

    for (binding, ty) in gamma.out.binding_types.iter() {
        if persistent_type_contains_frame_ref(gamma, *ty) {
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
        let policy = RefSuspensionPolicy { gamma };
        for at in uses_after_suspension(c.body, &policy) {
            gamma.body_errors.push(CompilerError::new_with_label(
                c.body.expr_spans[at],
                "frame borrow is used after a coroutine suspension; retain an `Entity<T>` and resolve it after the yield"
                    .to_string(),
            ));
        }
    }

    Ok(())
}

fn check_decorator(c: BodyCtx, gamma: &mut Gamma, d: &DecoratorDef) -> Result<(), CompilerError> {
    let body = c.body;
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

    if let Err(e) = infer_expr_used(c, gamma, root, Use::Discard) {
        gamma.body_errors.push(e);
    }

    embed::verify_embed_projection(c, gamma);
    Ok(())
}

fn check_global(c: BodyCtx, gamma: &mut Gamma, g: &GlobalDef, value: Idx<Expr>) {
    if let Err(error) = infer_expr(c, gamma, value).and_then(|ty| coerce(gamma, ty, g.ty, g.span)) {
        gamma.body_errors.push(error);
    }
    embed::verify_embed_projection(c, gamma);
    gamma.out.deps = gamma.dep_acc.take();
}

fn check_struct_defaults(
    c: BodyCtx,
    gamma: &mut Gamma,
    def: &StructDef,
) -> Result<(), CompilerError> {
    let body = c.body;
    let (subs, arg_tys) = gamma.instantiate_generics(&def.generics, def.span)?;

    // Published so a consumer holding `Box<int>` can bind the defaults body's
    // inference class; `subs` alone is keyed by `VarId` and dies with the check.
    gamma.out.struct_default_generic_slots = arg_tys;

    let mut sib = FxHashMap::default();
    for f in def.fields.iter() {
        sib.insert(f.name, substitute(gamma, f.ty, &subs, f.span)?);
    }

    gamma.sibling_fields.push(sib);

    for f in def.fields.iter() {
        if let Some(dexpr) = body.field_defaults.get(&f.name) {
            if let Err(e) = (|| -> Result<(), CompilerError> {
                let dty = infer_expr(c, gamma, *dexpr)?;
                let fty = substitute(gamma, f.ty, &subs, f.span)?;
                coerce(gamma, dty, fty, f.span)?;
                if def.is_value {
                    check_value_int_field_literal(c, gamma, *dexpr, fty)?;
                }

                Ok(())
            })() {
                gamma.body_errors.push(e);
            }
        }
    }

    gamma.sibling_fields.pop();

    embed::verify_embed_projection(c, gamma);

    Ok(())
}
