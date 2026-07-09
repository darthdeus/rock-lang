use bone_core::hir::{Expr, Module, Statement};
use bone_core::{
    hir::{
        BindingInfo, BindingKind, BindingPattern, BuiltinTy, Callee, DecoratorDef, Function,
        GlobalDef, InterpPart, Pattern, StructDef, StructFieldDef, TyId, TypeCtor, TypeExpr, VarId,
        VariantArgs, VariantPayload,
    },
    prelude::*,
};
use ustr::ustr;

// Compilation storage + inference result structs now live in bone-core; re-export
// them through the historical `bone::inference::*` path so consumers don't churn.
pub use bone_core::workspace::{BodyInference, InferenceResult, Substitutions, Workspace};

mod narrowing;

use narrowing::{detect_null_narrow, infer_branch, path_key, stmt_guard_narrows, Narrow, Root};

static NOT: LazyLock<Ustr> = LazyLock::new(|| ustr("__not"));
static UNM: LazyLock<Ustr> = LazyLock::new(|| ustr("__unm"));
static NULL: LazyLock<Ustr> = LazyLock::new(|| ustr("null"));
static Q_E: LazyLock<Ustr> = LazyLock::new(|| ustr("?E"));
static Q_R: LazyLock<Ustr> = LazyLock::new(|| ustr("?R"));
static Q_LAM: LazyLock<Ustr> = LazyLock::new(|| ustr("?lam"));
static NATIVE_ARRAY: LazyLock<Ustr> = LazyLock::new(|| ustr("NativeArray"));
static VEC: LazyLock<Ustr> = LazyLock::new(|| ustr("Vec"));
static ENTITY: LazyLock<Ustr> = LazyLock::new(|| ustr("Entity"));
static HOLE: LazyLock<Ustr> = LazyLock::new(|| ustr("hole"));

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Use {
    Value,
    Discard,
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

            let r_resolved = resolve(&mut gamma, r, f.span);

            // TODO: this should go once we finish the port and unify Unit and ()
            let is_unit_struct = matches!(
                *gamma.get(r_resolved),
                TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } if n == "Unit"
            );

            gamma.declared_ret_ty = Some(if is_unit_struct {
                gamma.workspace.builtins.unit()
            } else {
                r
            });
        }
    } else if let Some(ret) = f.return_ty {
        gamma.declared_ret_ty = Some(ret);
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
        let body_ty = infer_expr(c, &mut gamma, body)?;
        let body_ty = resolve(&mut gamma, body_ty, f.span);

        // Return-type mismatch is recorded, but not propagated, so gamma.out is well formed
        // and partial inference survives for the LSP. Anchor it at the value the body
        // actually offers -- its tail expression -- not the whole `fn { .. }` span. A body
        // that ends in a statement offers no value at all: a clearer, distinct message.
        let tail = match c.body.expr_arena[body] {
            Expr::Block { tail, .. } => tail,
            _ => Some(body),
        };
        if let Some(tail) = tail {
            if let Err(e) = coerce(&mut gamma, body_ty, ret_ty, c.body.expr_spans[tail]) {
                gamma.body_errors.push(e);
            }
        } else if coerce(&mut gamma, body_ty, ret_ty, f.span).is_err() {
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

    let errors = std::mem::take(&mut gamma.body_errors);

    Ok(Some((BodyOwner::Function(f_id), gamma.out, errors)))
}

/// True if `e` reaches a `return`/`break`/`continue` (statement or expr form)
/// through block / if / match / loop bodies. Port of rock's
/// `block_contains_control_flow`
/// (rock-semantic/src/semantic/control_flow_subexpr_validation.rs).
fn expr_contains_cf(c: BodyCtx, e: Idx<Expr>) -> bool {
    match c.body.expr_arena[e] {
        Expr::Return(_) | Expr::Break | Expr::Continue => true,
        Expr::Block { statements, tail } => {
            statements.into_iter().any(|s| stmt_contains_cf(c, s))
                || tail.is_some_and(|t| expr_contains_cf(c, t))
        }
        Expr::If {
            then_branch,
            else_branch,
            ..
        } => {
            expr_contains_cf(c, then_branch) || else_branch.is_some_and(|b| expr_contains_cf(c, b))
        }
        Expr::Match { arms, .. } => arms
            .into_iter()
            .any(|a| expr_contains_cf(c, c.body.match_arm_arena[a].body)),
        _ => false,
    }
}

fn stmt_contains_cf(c: BodyCtx, s: Idx<Statement>) -> bool {
    match c.body.statement_arena[s] {
        Statement::Return(_) | Statement::Break | Statement::Continue => true,
        Statement::Expr(e) => expr_contains_cf(c, e),
        Statement::For { body, .. }
        | Statement::While { body, .. }
        | Statement::WhileLet { body, .. } => expr_contains_cf(c, body),
        _ => false,
    }
}

/// Reject control flow (`return`/`break`/`continue`) inside an `if` that is a
/// tuple-literal element of a destructure (`let (..)` / newtype) RHS: the
/// cond-false path can't materialise a value to destructure. Port of rock's
/// `ControlFlowInSubexpression` rule.
fn reject_cf_in_destructure_rhs(c: BodyCtx, value: Idx<Expr>) -> Result<(), CompilerError> {
    if let Expr::Tuple(elems) = c.body.expr_arena[value] {
        for elem in elems {
            if matches!(c.body.expr_arena[elem], Expr::If { .. }) && expr_contains_cf(c, elem) {
                return Err(CompilerError::new_with_label(
                    c.body.expr_spans[elem],
                    "control flow (return/break/continue) is not allowed in an `if` used as a \
                     tuple element of a destructuring assignment"
                        .to_string(),
                ));
            }
        }
    }
    Ok(())
}

/// Does function `f` carry annotation `@name`?
fn fn_has_anno(f: &Function, name: &str) -> bool {
    f.annotations.iter().any(|a| a.name.name == name)
}

/// Verify a `@pure` function body: push an error at each construct that is not
/// referentially transparent -- a global read, an assignment / inc-dec, or a
/// call to a callee that is itself neither `@pure` nor `@debug_impure`.
/// Permissive on everything else (value-struct literals, field reads,
/// operators) so a genuinely-pure body passes. This is the genuine-side-effect
/// subset of rock-optimizations/src/purity.rs; rock's conservative "accepted
/// limitations" (heap struct/array literal, match-bind, ...) are deliberately
/// NOT flagged -- documented divergence, see docs/parked/unfuck.md.
/// A `static` initializer runs ONCE, not per call, so it may not read an
/// enclosing param/local or a sibling static -- that would silently snapshot
/// the first reaching call's values. A `Var` whose declaration span is
/// contained in the initializer (a lambda param, an inner `let`) is local to
/// the init and allowed; one declared outside is the forbidden capture.
/// Which hoisted construct is being checked -- they forbid different capture
/// sets. A static INITIALIZER runs once so it may read nothing declared outside
/// itself (params, locals, and sibling statics all snapshot the first reaching
/// call). A nested fn BODY is hoisted to module scope with no enclosing frame,
/// so it may not read enclosing params/locals -- but it MAY reference in-scope
/// `static`s and sibling nested `fn`s, which are themselves module-level.
#[derive(Copy, Clone)]
enum CaptureCtx {
    StaticInit,
    NestedFnBody,
}

fn reject_static_init_captures(
    c: BodyCtx,
    root: Idx<Expr>,
    site: Span,
) -> Result<(), CompilerError> {
    let root_span = c.body.expr_spans[root];
    reject_captures_expr(c, root, root_span, site, CaptureCtx::StaticInit)
}

/// A nested fn's body may not capture the enclosing frame. `body` is the body
/// block expr; `decl_span` spans the whole `fn ..(..) { .. }` statement so the
/// fn's OWN params (declared in the signature, before the body block) count as
/// inside -- only bindings declared outside the declaration are captures.
fn reject_nested_fn_captures(
    c: BodyCtx,
    body: Idx<Expr>,
    decl_span: Span,
) -> Result<(), CompilerError> {
    reject_captures_expr(c, body, decl_span, decl_span, CaptureCtx::NestedFnBody)
}

/// Reject a binding reference (a `Var` read or a `Callee::Local` call) that
/// escapes the hoisted construct. A binding whose declaration span is contained
/// in `root_span` is local to the construct (a param, an inner `let`, a lambda
/// param) and always allowed.
fn reject_captured_binding(
    c: BodyCtx,
    b: Idx<hir::BindingInfo>,
    root_span: Span,
    site: Span,
    mode: CaptureCtx,
) -> Result<(), CompilerError> {
    let bspan = c.body.binding_arena[b].span;
    let inside = bspan.offset_range.0 >= root_span.offset_range.0
        && bspan.offset_range.1 <= root_span.offset_range.1;
    if inside {
        return Ok(());
    }
    let name = c.body.binding_arena[b].name;
    match mode {
        CaptureCtx::StaticInit => comp_error!(
            site,
            "static initializer may not read local `{}`: the initializer runs once, not per call",
            name
        ),
        CaptureCtx::NestedFnBody => {
            if matches!(
                c.body.binding_arena[b].kind,
                hir::BindingKind::Let | hir::BindingKind::Param
            ) {
                comp_error!(
                    site,
                    "nested function may not read enclosing local `{}`: it is hoisted to module scope with no access to the enclosing frame",
                    name
                );
            }
        }
    }
    Ok(())
}

fn reject_captures_expr(
    c: BodyCtx,
    e: Idx<Expr>,
    root_span: Span,
    site: Span,
    mode: CaptureCtx,
) -> Result<(), CompilerError> {
    macro_rules! go {
        ($x:expr) => {
            reject_captures_expr(c, $x, root_span, site, mode)?
        };
    }
    match c.body.expr_arena[e] {
        Expr::Var(b) => reject_captured_binding(c, b, root_span, site, mode)?,

        Expr::Unit
        | Expr::Null
        | Expr::Hole(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::Break
        | Expr::Continue
        | Expr::Global(_)
        | Expr::SiblingRef(_)
        | Expr::Return(None)
        | Expr::Yield(None) => {}

        Expr::Not(i)
        | Expr::BitNot(i)
        | Expr::UnaryMinus(i)
        | Expr::Spread(i)
        | Expr::YieldDelegate(i)
        | Expr::Return(Some(i))
        | Expr::Yield(Some(i))
        | Expr::As { value: i, .. }
        | Expr::TupleIndex { tuple: i, .. }
        | Expr::FieldAccess { base: i, .. }
        | Expr::SafeNav { base: i, .. }
        | Expr::IncDec { target: i, .. } => go!(i),

        Expr::Index { base: a, index: b }
        | Expr::Range { low: a, high: b }
        | Expr::BinOp { lhs: a, rhs: b, .. }
        | Expr::Assign { target: a, value: b }
        | Expr::CompoundAssign { target: a, value: b, .. } => {
            go!(a);
            go!(b);
        }

        Expr::Tuple(es) | Expr::Array(es) | Expr::NativeArray(es) => {
            for x in es {
                go!(x);
            }
        }

        Expr::InterpolatedString(parts) => {
            for p in parts {
                if let InterpPart::Value { expr, .. } = c.body.interp_part_arena[p] {
                    go!(expr);
                }
            }
        }

        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            go!(cond);
            go!(then_branch);
            if let Some(eb) = else_branch {
                go!(eb);
            }
        }
        Expr::IfLet {
            value,
            then_branch,
            else_branch,
            ..
        } => {
            go!(value);
            go!(then_branch);
            if let Some(eb) = else_branch {
                go!(eb);
            }
        }

        Expr::Call { callee, args } => {
            match callee {
                Callee::Value(ve) => go!(ve),
                // Calling an enclosing local (a captured lambda) is a capture;
                // calling a sibling nested fn / in-scope static is not.
                Callee::Local(b) => reject_captured_binding(c, b, root_span, site, mode)?,
                Callee::Unresolved { .. } => {}
            }
            for a in args {
                go!(a);
            }
        }
        Expr::SafeNavCall { receiver, args, .. } => {
            go!(receiver);
            for a in args {
                go!(a);
            }
        }
        Expr::SafeNavLet { receiver, body, .. } => {
            go!(receiver);
            go!(body);
        }

        Expr::Lambda { body, .. } => go!(body),

        Expr::Block { statements, tail } => {
            for s in statements {
                reject_captures_stmt(c, s, root_span, site, mode)?;
            }
            if let Some(t) = tail {
                go!(t);
            }
        }

        Expr::Match { scrutinee, arms } => {
            go!(scrutinee);
            for a in arms {
                let arm = c.body.match_arm_arena[a];
                if let Some(g) = arm.guard {
                    go!(g);
                }
                go!(arm.body);
            }
        }

        Expr::StructLiteral { fields, bases, .. } => {
            for b in bases {
                go!(b);
            }
            for fld in fields {
                go!(c.body.struct_lit_field_arena[fld].value);
            }
        }

        Expr::EnumVariant { args, .. } => match args {
            VariantArgs::Unit => {}
            VariantArgs::Tuple(es) => {
                for x in es {
                    go!(x);
                }
            }
            VariantArgs::Record(fs) => {
                for fld in fs {
                    go!(c.body.struct_lit_field_arena[fld].value);
                }
            }
        },
    }
    Ok(())
}

fn reject_captures_stmt(
    c: BodyCtx,
    s: Idx<Statement>,
    root_span: Span,
    site: Span,
    mode: CaptureCtx,
) -> Result<(), CompilerError> {
    macro_rules! go {
        ($x:expr) => {
            reject_captures_expr(c, $x, root_span, site, mode)?
        };
    }
    match c.body.statement_arena[s] {
        Statement::Let { value, .. }
        | Statement::LetTuple { value, .. }
        | Statement::LetNewtype { value, .. }
        | Statement::Expr(value)
        | Statement::Defer(value)
        | Statement::Return(Some(value))
        | Statement::Static { value, .. } => go!(value),
        // A doubly-nested fn's body sits inside `root_span`, so any enclosing-frame
        // read it makes is caught by ITS own reject pass; walking the body here is
        // harmless (every ref is span-inside `root_span`).
        Statement::Fn { body, .. } => go!(body),
        Statement::For { iter, body, .. } => {
            go!(iter);
            go!(body);
        }
        Statement::While { cond, body } => {
            go!(cond);
            go!(body);
        }
        Statement::WhileLet { value, body, .. } => {
            go!(value);
            go!(body);
        }
        Statement::Return(None)
        | Statement::Break
        | Statement::Continue
        | Statement::Comment(_) => {}
    }
    Ok(())
}

fn verify_pure_expr(
    c: BodyCtx,
    ws: &Workspace,
    resolved: &FxHashMap<Idx<Expr>, (ModuleId, Idx<Function>)>,
    e: Idx<Expr>,
    errors: &mut Vec<CompilerError>,
) {
    let span = c.body.expr_spans[e];
    macro_rules! go {
        ($x:expr) => {
            verify_pure_expr(c, ws, resolved, $x, errors)
        };
    }
    match c.body.expr_arena[e] {
        Expr::Global(name) => {
            // const globals are read-only (assignment/incdec rejected,
            // re-initialized unconditionally on hot reload); reading one
            // is pure. Only mutable globals break purity.
            if !ws.const_globals.contains(&name) {
                errors.push(CompilerError::new_with_label(
                    span,
                    format!("`@pure` function reads global `{}` (mutable state)", name),
                ))
            }
        }
        Expr::Assign { target, value } => {
            errors.push(CompilerError::new_with_label(
                span,
                "`@pure` function performs an assignment (mutation)".to_string(),
            ));
            go!(target);
            go!(value);
        }
        Expr::CompoundAssign { target, value, .. } => {
            errors.push(CompilerError::new_with_label(
                span,
                "`@pure` function performs a compound assignment (mutation)".to_string(),
            ));
            go!(target);
            go!(value);
        }
        Expr::IncDec { target, .. } => {
            errors.push(CompilerError::new_with_label(
                span,
                "`@pure` function performs an increment/decrement (mutation)".to_string(),
            ));
            go!(target);
        }
        Expr::Call { callee, args } => {
            if let Some(&(mid, fid)) = resolved.get(&e) {
                let callee_fn = &ws.module(mid).functions[fid];
                if !fn_has_anno(callee_fn, "pure") && !fn_has_anno(callee_fn, "debug_impure") {
                    errors.push(CompilerError::new_with_label(
                        span,
                        format!(
                            "`@pure` function calls `{}` which is not provably pure",
                            callee_fn.name
                        ),
                    ));
                }
            }
            if let Callee::Value(ve) = callee {
                go!(ve);
            }
            for a in args {
                go!(a);
            }
        }
        Expr::Block { statements, tail } => {
            for s in statements {
                verify_pure_stmt(c, ws, resolved, s, errors);
            }
            if let Some(t) = tail {
                go!(t);
            }
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            go!(cond);
            go!(then_branch);
            if let Some(eb) = else_branch {
                go!(eb);
            }
        }
        Expr::IfLet {
            value,
            then_branch,
            else_branch,
            ..
        } => {
            go!(value);
            go!(then_branch);
            if let Some(eb) = else_branch {
                go!(eb);
            }
        }
        Expr::Match { scrutinee, arms } => {
            go!(scrutinee);
            for a in arms {
                let arm = c.body.match_arm_arena[a];
                if let Some(g) = arm.guard {
                    go!(g);
                }
                go!(arm.body);
            }
        }
        Expr::BinOp { lhs, rhs, .. } => {
            go!(lhs);
            go!(rhs);
        }
        Expr::Index { base, index } => {
            errors.push(CompilerError::new_with_label(
                span,
                "`@pure` function indexes a collection (reads potentially-mutable state)"
                    .to_string(),
            ));
            go!(base);
            go!(index);
        }
        Expr::Range { low, high } => {
            go!(low);
            go!(high);
        }
        Expr::Not(i)
        | Expr::BitNot(i)
        | Expr::UnaryMinus(i)
        | Expr::Spread(i)
        | Expr::YieldDelegate(i)
        | Expr::Return(Some(i))
        | Expr::Yield(Some(i))
        | Expr::As { value: i, .. }
        | Expr::TupleIndex { tuple: i, .. }
        | Expr::FieldAccess { base: i, .. }
        | Expr::SafeNav { base: i, .. } => go!(i),
        // Tuples are value-like (no heap identity) -- pure.
        Expr::Tuple(es) => {
            for x in es {
                go!(x);
            }
        }
        // Array literals allocate a heap table -- identity observable across
        // re-evaluation, so not dup-safe.
        Expr::Array(es) | Expr::NativeArray(es) => {
            errors.push(CompilerError::new_with_label(
                span,
                "`@pure` function builds an array literal (allocates a heap table, identity observable)"
                    .to_string(),
            ));
            for x in es {
                go!(x);
            }
        }
        // A `value` struct literal is byte-value (pure); a heap struct literal
        // has observable identity.
        Expr::StructLiteral { name, fields, bases } => {
            let is_value = ws
                .struct_by_name
                .get(&name)
                .is_some_and(|sid| ws.module(sid.module).structs[sid.def].is_value);
            if !is_value {
                errors.push(CompilerError::new_with_label(
                    span,
                    "`@pure` function constructs a heap struct literal (identity observable); only `value` structs are allowed"
                        .to_string(),
                ));
            }
            for b in bases {
                go!(b);
            }
            for fld in fields {
                go!(c.body.struct_lit_field_arena[fld].value);
            }
        }
        Expr::EnumVariant { args, .. } => match args {
            VariantArgs::Unit => {}
            VariantArgs::Tuple(es) => {
                for x in es {
                    go!(x);
                }
            }
            VariantArgs::Record(fs) => {
                for fld in fs {
                    go!(c.body.struct_lit_field_arena[fld].value);
                }
            }
        },
        // A lambda captures and has closure identity; its body isn't vetted
        // for purity here.
        Expr::Lambda { .. } => errors.push(CompilerError::new_with_label(
            span,
            "`@pure` function creates a lambda (closure identity / captured state not vetted)"
                .to_string(),
        )),
        // Match (incl. with-binding) and self-recursion are accepted: they are
        // referentially transparent. rock rejects match-bind / self-recursion
        // as scope-walker limitations; bone implements the intended rule.
        // Remaining leaves (interpolation, safe-nav-let/call) carry no
        // pinned side effect.
        _ => {}
    }
}

fn verify_pure_stmt(
    c: BodyCtx,
    ws: &Workspace,
    resolved: &FxHashMap<Idx<Expr>, (ModuleId, Idx<Function>)>,
    s: Idx<Statement>,
    errors: &mut Vec<CompilerError>,
) {
    macro_rules! go {
        ($x:expr) => {
            verify_pure_expr(c, ws, resolved, $x, errors)
        };
    }
    match c.body.statement_arena[s] {
        Statement::Let { value, .. }
        | Statement::LetTuple { value, .. }
        | Statement::LetNewtype { value, .. }
        | Statement::Expr(value)
        | Statement::Defer(value)
        | Statement::Return(Some(value)) => go!(value),
        Statement::For { iter, body, .. } => {
            go!(iter);
            go!(body);
        }
        Statement::While { cond, body } => {
            go!(cond);
            go!(body);
        }
        Statement::WhileLet { value, body, .. } => {
            go!(value);
            go!(body);
        }
        Statement::Static { .. } => {
            errors.push(CompilerError::new_with_label(
                c.body.statement_spans[s],
                "`@pure` function declares a `static` (writes module state)".to_string(),
            ));
        }
        // Conservative, like the lambda case: a nested fn's body is not vetted
        // for purity here. Put the helper at top level if it must be `@pure`.
        Statement::Fn { .. } => {
            errors.push(CompilerError::new_with_label(
                c.body.statement_spans[s],
                "`@pure` function declares a nested `fn` (its body is not vetted for purity)".to_string(),
            ));
        }
        Statement::Return(None)
        | Statement::Break
        | Statement::Continue
        | Statement::Comment(_) => {}
    }
}

fn infer_statement(c: BodyCtx, gamma: &mut Gamma, s: Idx<Statement>) -> Result<(), CompilerError> {
    let span = c.body.statement_spans[s];

    match &c.body.statement_arena[s] {
        Statement::Expr(e) => {
            infer_expr_used(c, gamma, *e, Use::Discard)?;
        }

        Statement::Defer(e) => {
            infer_expr_used(c, gamma, *e, Use::Discard)?;
        }

        Statement::Return(e) => {
            let e_ty = match e {
                Some(e) => infer_expr(c, gamma, *e)?,
                None => gamma.workspace.builtins.unit(),
            };

            if let Some(declared_ret_ty) = gamma.declared_ret_ty {
                coerce(gamma, e_ty, declared_ret_ty, c.body.statement_spans[s])?;
            }
        }

        Statement::Let { name, value } => {
            let declared = c.body.binding_arena[*name].declared_ty;

            let binding_ty = match declared {
                Some(ann) => {
                    let span = c
                        .body
                        .expr_spans
                        .get(*value)
                        .copied()
                        .unwrap_or(Span::unknown());

                    // Bind to the annotation in case RHS fails, helps LSP
                    gamma.assign_named_type(*name, ann)?;
                    check_expr(c, gamma, *value, ann, span)?;

                    ann
                }

                None => match infer_expr(c, gamma, *value) {
                    Ok(ty) => ty,
                    Err(e) => {
                        // RHS failed, position name with "error" so downstream can still do something
                        let poison = gamma.workspace.builtins.error();
                        gamma.assign_named_type(*name, poison)?;
                        return Err(e);
                    }
                },
            };

            gamma.assign_named_type(*name, binding_ty)?;

            // ctx.assign_named_type(*name, inf_ty);
            // ctx.assign_type(name, ty)?;
            // ctx.assign_type(e, ty)
        }

        Statement::Static { name, value } => {
            if gamma.coroutine_ret.is_some() {
                comp_error!(
                    c.body.statement_spans[s],
                    "`static` is not allowed in a `co fn`: a shared cell across yields corrupts"
                );
            }

            let declared = c.body.binding_arena[*name].declared_ty;
            let binding_ty = match declared {
                Some(ann) => {
                    let vspan = c.body.expr_spans.get(*value).copied().unwrap_or(Span::unknown());
                    gamma.assign_named_type(*name, ann)?;
                    check_expr(c, gamma, *value, ann, vspan)?;
                    ann
                }

                None => match infer_expr(c, gamma, *value) {
                    Ok(ty) => ty,
                    Err(e) => {
                        let poison = gamma.workspace.builtins.error();
                        gamma.assign_named_type(*name, poison)?;
                        return Err(e);
                    }
                },
            };

            gamma.assign_named_type(*name, binding_ty)?;

            reject_static_init_captures(c, *value, c.body.statement_spans[s])?;
        }

        Statement::Fn { name, params, ret_ty, body } => {
            let (name, params, ret_ty, body) = (*name, *params, *ret_ty, *body);

            // Params always carry a declared type (the signature is mandatory).
            let mut param_tys = Vec::with_capacity(params.len());
            for pb in params {
                let ty = c.body.binding_arena[pb]
                    .declared_ty
                    .unwrap_or_else(|| gamma.workspace.builtins.error());
                gamma.assign_named_type(pb, ty)?;
                param_tys.push(ty);
            }

            // The nested body is hoisted and runs at call time, so no enclosing
            // GLOBAL refinement holds inside it (mirrors `infer_lambda`); the
            // enclosing return-context is saved and restored around it.
            let saved_ret = gamma.declared_ret_ty.take();
            let saved_coro = gamma.coroutine_ret.take();
            let saved_globals: Vec<Narrow> = gamma
                .refined_nonnull
                .iter()
                .filter(|(root, _)| root.is_global())
                .cloned()
                .collect();
            for g in &saved_globals {
                gamma.refined_nonnull.remove(g);
            }

            let result = (|| -> Result<(), CompilerError> {
                match ret_ty {
                    Some(ret) => {
                        // Concrete signature -> assign the name its fn type BEFORE
                        // the body so a self-recursive call resolves.
                        let fn_ty = make_fn_ty(gamma, &param_tys, ret, span);
                        gamma.assign_named_type(name, fn_ty)?;
                        gamma.declared_ret_ty = Some(ret);

                        let body_ty = infer_expr(c, gamma, body)?;
                        let body_ty = resolve(gamma, body_ty, span);
                        let tail = match c.body.expr_arena[body] {
                            Expr::Block { tail, .. } => tail,
                            _ => Some(body),
                        };
                        let cspan = tail.map(|t| c.body.expr_spans[t]).unwrap_or(span);
                        if let Err(e) = coerce(gamma, body_ty, ret, cspan) {
                            gamma.body_errors.push(e);
                        }
                    }
                    None => {
                        // Inferred return: the body's tail is the return type (like
                        // a lambda). A self-recursive call needs an explicit `-> T`;
                        // an explicit `return X` in an inferred-return nested fn is
                        // not type-linked (v1 -- annotate the return type instead).
                        gamma.declared_ret_ty = None;
                        let body_ty = infer_expr(c, gamma, body)?;
                        let body_ty = resolve(gamma, body_ty, span);
                        let fn_ty = make_fn_ty(gamma, &param_tys, body_ty, span);
                        gamma.assign_named_type(name, fn_ty)?;
                    }
                }
                Ok(())
            })();

            gamma.refined_nonnull.extend(saved_globals);
            gamma.coroutine_ret = saved_coro;
            gamma.declared_ret_ty = saved_ret;
            result?;

            reject_nested_fn_captures(c, body, span)?;
        }

        Statement::Comment(_) => (),

        Statement::For { vars, iter, body } => {
            let (iter, body) = (*iter, *body);

            let (elem_ty, deferred) = match infer_for_elem(c, gamma, iter, span) {
                Ok(t) => (t, None),
                Err(e) => (gamma.workspace.builtins.error(), Some(e)),
            };

            destructure_pattern(gamma, vars, elem_ty, span)?;
            gamma.loop_depth += 1;
            let body_res = infer_expr_used(c, gamma, body, Use::Discard);
            gamma.loop_depth -= 1;
            body_res?;

            if let Some(err) = deferred {
                return Err(err);
            }
        }

        Statement::While { cond, body } => {
            let (cond, body) = (*cond, *body);
            let cond_ty = infer_expr(c, gamma, cond)?;

            unify(gamma, cond_ty, gamma.workspace.builtins.bool(), span)?;

            let (narrow_then, _) = detect_null_narrow(gamma, c, cond);
            gamma.loop_depth += 1;
            let body_res = infer_branch(c, gamma, body, &narrow_then, span, Use::Discard);
            gamma.loop_depth -= 1;
            body_res?;
        }

        Statement::WhileLet {
            pattern,
            value,
            body,
        } => {
            let (pattern, value, body) = (*pattern, *value, *body);

            let (unwrapped, deferred) = match infer_expr(c, gamma, value) {
                Ok(t) => (peel_nullable(gamma, t, span), None),
                Err(e) => (gamma.workspace.builtins.error(), Some(e)),
            };

            check_pattern(c, gamma, pattern, unwrapped, span)?;
            gamma.loop_depth += 1;
            let body_res = infer_expr_used(c, gamma, body, Use::Discard);
            gamma.loop_depth -= 1;
            body_res?;

            if let Some(err) = deferred {
                return Err(err);
            }
        }

        Statement::Break | Statement::Continue => {
            if gamma.loop_depth == 0 {
                comp_error!(span, "`break`/`continue` is only valid inside a loop body");
            }
        }

        Statement::LetTuple { vars, value } => {
            reject_cf_in_destructure_rhs(c, *value)?;
            match infer_expr(c, gamma, *value) {
                Ok(t) => destructure_pattern(gamma, vars, t, span)?,
                Err(e) => {
                    destructure_pattern(gamma, vars, gamma.workspace.builtins.error(), span)?;
                    return Err(e);
                }
            }
        }

        Statement::LetNewtype {
            newtype_name,
            var,
            value,
        } => {
            reject_cf_in_destructure_rhs(c, *value)?;
            let value_ty = infer_expr(c, gamma, *value)?;
            let resolved = resolve(gamma, value_ty, span);

            match *gamma.get(resolved) {
                TypeExpr::App {
                    ctor: TypeCtor::Nominal(name),
                    ..
                } if name == *newtype_name => {}

                TypeExpr::Infer(..) | TypeExpr::Generic(..) => {}

                _ => {
                    comp_error!(span, "cannot destructure this value with newtype pattern `{}` - the value is not a newtype `{}`", newtype_name, newtype_name)
                }
            };

            let NewtypeId { module: ntm, def } =
                resolve_newtype(gamma, *newtype_name).ok_or_else(|| {
                    CompilerError::new_with_label(
                        span,
                        format!("unknown newtype `{}`", newtype_name),
                    )
                })?;

            let nt = gamma.workspace.module(ntm).newtypes[def].clone();

            let mut subs = FxHashMap::default();
            if let TypeExpr::App {
                ctor: TypeCtor::Nominal(_),
                params,
            } = *gamma.get(resolved)
            {
                for (g, arg) in nt.generics.iter().zip(params) {
                    if let TypeExpr::Generic(var_id, _) = gamma.get(*g) {
                        subs.insert(*var_id, resolved.with_idx(arg));
                    }
                }
            }

            let wt = substitute(gamma, nt.wrapped, &subs, span)?;
            gamma.assign_named_type(*var, wt)?;
        }
    };

    Ok(())
}

pub fn infer_expr_used(
    c: BodyCtx,
    gamma: &mut Gamma,
    e: Idx<Expr>,
    used: Use,
) -> Result<TyId, CompilerError> {
    let expr_span = c.body.expr_spans[e];

    if let Some(&cached) = gamma.out.expr_types.get(e) {
        return Ok(resolve(gamma, cached, expr_span));
    }

    let ty = match &c.body.expr_arena[e] {
        Expr::Int(_) => gamma.workspace.builtins.int(),
        Expr::Float(_) => gamma.workspace.builtins.float(),
        Expr::Bool(_) => gamma.workspace.builtins.bool(),
        Expr::String(_) => gamma.workspace.builtins.string(),
        Expr::Unit => gamma.workspace.builtins.unit(),

        Expr::Hole(_) => gamma.fresh_callsite_var(*HOLE, expr_span),

        Expr::InterpolatedString(parts) => {
            for p in *parts {
                if let InterpPart::Value { expr, spec } = c.body.interp_part_arena[p] {
                    infer_expr(c, gamma, expr)?;

                    if let Some(s) = spec {
                        if let Some(we) = s.width_expr {
                            infer_expr(c, gamma, we)?;
                        }
                        if let Some(pe) = s.precision_expr {
                            infer_expr(c, gamma, pe)?;
                        }
                    }
                }
            }

            gamma.workspace.builtins.string()
        }
        Expr::Not(inner) => {
            let inner_ty = infer_expr(c, gamma, *inner)?;
            let resolved = resolve(gamma, inner_ty, c.body.expr_spans[*inner]);

            if matches!(gamma.get(resolved), TypeExpr::Builtin(BuiltinTy::Bool)) {
                gamma.workspace.builtins.bool()
            } else {
                resolve_named_call(
                    gamma,
                    c,
                    *NOT,
                    None,
                    IdxRange::new_inclusive(*inner..=*inner),
                    e,
                    expr_span,
                )?
            }
        }

        Expr::BitNot(inner) => {
            let int_ty = gamma.workspace.builtins.int();
            let inner_ty = infer_expr(c, gamma, *inner)?;

            unify(gamma, inner_ty, int_ty, c.body.expr_spans[*inner])?;

            int_ty
        }

        Expr::Spread(..) => comp_error!(expr_span, "spread is only valid inside an array literal"),

        Expr::Yield(opt) => {
            let opt = *opt;
            let ret = gamma.coroutine_ret.ok_or_else(|| {
                CompilerError::new_with_label(
                    expr_span,
                    "yield is only allowed in a coroutine (co fn)".to_string(),
                )
            })?;

            // let ret = gamma.declared_ret_ty.ok_or_else(|| {
            //     CompilerError::new_with_label(
            //         expr_span,
            //         "yield is only allowed in functions with an annotated return type".to_string(),
            //     )
            // })?;

            let (y, _r, n) = coroutine_yrn(gamma, ret, expr_span)?;

            if let Some(e) = opt {
                let et = infer_expr(c, gamma, e)?;
                coerce(gamma, et, y, c.body.expr_spans[e])?;
            }

            // Suspension hands control to the resumer, who may reassign any
            // global; drop all global refinements (the yielded value above was
            // computed before suspending, so it still saw them).
            gamma.refined_nonnull.retain(|(root, _)| !root.is_global());

            n
        }

        Expr::YieldDelegate(inner) => {
            let inner = *inner;
            let ret = gamma.coroutine_ret.ok_or_else(|| {
                CompilerError::new_with_label(
                    expr_span,
                    "yield is only allowed in functions with an annotated return type".to_string(),
                )
            })?;

            let (outer_y, _, _) = coroutine_yrn(gamma, ret, expr_span)?;
            let inner_ty = infer_expr(c, gamma, inner)?;
            let (inner_y, inner_r, _) = coroutine_yrn(gamma, inner_ty, c.body.expr_spans[inner])?;

            coerce(gamma, inner_y, outer_y, c.body.expr_spans[e])?;

            // `yield*` suspends repeatedly into the delegate; the resumer may
            // reassign globals between resumptions.
            gamma.refined_nonnull.retain(|(root, _)| !root.is_global());

            inner_r
        }

        Expr::UnaryMinus(inner) => {
            let inner_ty = infer_expr(c, gamma, *inner)?;
            // TODO: do we actually want to call resolve after every infer? shouldn't that be part
            // of infer? figure that out later
            let resolved = resolve(gamma, inner_ty, c.body.expr_spans[*inner]);

            match *gamma.get(resolved) {
                TypeExpr::Builtin(BuiltinTy::Int) | TypeExpr::Builtin(BuiltinTy::Float) => resolved,

                TypeExpr::App {
                    ctor: TypeCtor::Nominal(name),
                    ..
                } if resolve_newtype(gamma, name).is_some() => resolved,

                _ => resolve_named_call(
                    gamma,
                    c,
                    *UNM,
                    None,
                    IdxRange::new_inclusive(*inner..=*inner),
                    e,
                    expr_span,
                )?,
            }
        }

        Expr::IncDec { target, .. } => {
            // Same const-rebind rejection as Expr::Assign/CompoundAssign;
            // `CONST++` is a write with extra steps.
            if let Expr::Global(name) = c.body.expr_arena[*target] {
                if gamma.workspace.const_globals.contains(&name) {
                    comp_error!(
                        expr_span,
                        "cannot assign to const `{}`; consts are read-only",
                        name
                    );
                }
            }
            let inner_ty = infer_expr(c, gamma, *target)?;
            resolve(gamma, inner_ty, c.body.expr_spans[*target])
        }

        Expr::As { value, target } => {
            infer_expr(c, gamma, *value)?;
            resolve(gamma, *target, expr_span)
        }

        Expr::Lambda { params, body } => {
            infer_lambda(c, gamma, *params, *body, None, None, expr_span)?
        }

        Expr::Index { base, index } => {
            let (base, index) = (*base, *index);

            let base_ty = infer_expr(c, gamma, base)?;
            let base_ty = resolve(gamma, base_ty, expr_span);

            let index_ty = infer_expr(c, gamma, index)?;

            match *gamma.get(base_ty) {
                TypeExpr::App {
                    ctor: TypeCtor::Array,
                    params,
                } => {
                    let elem = params.into_iter().next().ok_or_else(|| {
                        CompilerError::new_with_label(
                            expr_span,
                            "array type must have one parameter".to_string(),
                        )
                    })?;

                    unify(
                        gamma,
                        index_ty,
                        gamma.workspace.builtins.int(),
                        c.body.expr_spans[index],
                    )?;
                    resolve(gamma, base_ty.with_idx(elem), expr_span)
                }

                TypeExpr::App {
                    ctor: TypeCtor::Map,
                    params,
                } => {
                    let mut it = params.into_iter();

                    let key = it.next().ok_or_else(|| {
                        CompilerError::new_with_label(
                            expr_span,
                            "map type must have two parameters".to_string(),
                        )
                    })?;

                    let val = it.next().ok_or_else(|| {
                        CompilerError::new_with_label(
                            expr_span,
                            "map type must have two parameters".to_string(),
                        )
                    })?;

                    let key_ty = resolve(gamma, base_ty.with_idx(key), expr_span);
                    unify(gamma, index_ty, key_ty, c.body.expr_spans[index])?;
                    resolve(gamma, base_ty.with_idx(val), expr_span)
                }

                TypeExpr::App {
                    ctor: TypeCtor::Nominal(name),
                    params,
                } => {
                    unify(
                        gamma,
                        index_ty,
                        gamma.workspace.builtins.int(),
                        c.body.expr_spans[index],
                    )?;

                    let elem = match name.as_str() {
                        "NativeBuffer" | "NativeArray" => {
                            let p = params.into_iter().next().ok_or_else(|| {
                                CompilerError::new_with_label(
                                    expr_span,
                                    "NativeBuffer type must have one parameter".to_string(),
                                )
                            })?;

                            base_ty.with_idx(p)
                        }

                        "NativeBufferU8" | "NativeBufferI8" | "NativeBufferU16"
                        | "NativeBufferI16" | "NativeBufferU32" | "NativeBufferI32" => {
                            gamma.workspace.builtins.int()
                        }

                        _ => comp_error!(
                            expr_span,
                            "cannot index into type {}",
                            gamma.workspace.render(Some(&gamma.out), base_ty)
                        ),
                    };

                    resolve(gamma, elem, expr_span)
                }

                _ => comp_error!(
                    expr_span,
                    "cannot index into type {}",
                    gamma.workspace.render(Some(&gamma.out), base_ty)
                ),
            }
        }

        Expr::Range { low, high } => {
            let int_ty = gamma.workspace.builtins.int();
            let low_ty = infer_expr(c, gamma, *low)?;
            unify(gamma, low_ty, int_ty, c.body.expr_spans[*low])?;

            let high_ty = infer_expr(c, gamma, *high)?;
            unify(gamma, high_ty, int_ty, c.body.expr_spans[*high])?;

            let int_texp = *gamma.get(int_ty);
            let params = alloc_contiguous(&mut gamma.out.ty_arena, [int_texp]);

            let arr = gamma.out.ty_arena.alloc(TypeExpr::App {
                ctor: TypeCtor::Array,
                params,
            });

            resolve(gamma, TyId::Local(arr), expr_span)
        }

        Expr::Var(binding) => gamma
            .out
            .binding_types
            .get(*binding)
            .cloned()
            .ok_or_else(|| {
                CompilerError::new_with_label(
                    c.body.expr_spans[e],
                    format!(
                        "Undefined variable `{}`",
                        c.body.binding_arena[*binding].name
                    ),
                )
            })?,

        Expr::Global(name) => match resolve_global(gamma, *name) {
            // A global guarded by `if G != null` (with no intervening write /
            // impure call / yield since) reads as its non-null inner type.
            Some(ty)
                if gamma
                    .refined_nonnull
                    .contains(&(Root::Global(*name), vec![])) =>
            {
                peel_nullable(gamma, ty, c.body.expr_spans[e])
            }

            Some(ty) => ty,

            None => match resolve_callee(gamma, *name).as_slice() {
                [(mid, fid)] => {
                    let function = gamma.workspace.module(*mid).functions[*fid].clone();

                    // A bounded fn only exists monomorphized-by-inlining at its
                    // call sites; as a value it would need a runtime identity
                    // its placeholder bound-calls can't have.
                    if function.generic_bounds.iter().any(Option::is_some) {
                        comp_error!(
                            expr_span,
                            "`{}` has `Iterator` bounds and cannot be used as a value; call it directly",
                            name
                        );
                    }

                    let param_tys = function.param_decls.iter().map(|p| p.ty).collect_vec();

                    let mut texps = Vec::with_capacity(param_tys.len());

                    for pty in param_tys {
                        let r = TyId::Local(reify(gamma, pty, expr_span));
                        texps.push(*gamma.get(r));
                    }

                    let params = alloc_contiguous(&mut gamma.out.ty_arena, texps);
                    let ret_ty = function
                        .return_ty
                        .unwrap_or_else(|| gamma.workspace.builtins.unit());
                    let ret = reify(gamma, ret_ty, expr_span);

                    TyId::Local(gamma.out.ty_arena.alloc(TypeExpr::Function { params, ret }))
                }

                [] => comp_error!(expr_span, "Undefined global `{}`", name),
                _ => comp_error!(
                    expr_span,
                    "ambiguous fn-value reference `{}` (overloaded)",
                    name
                ),
            },
        },

        Expr::Null => {
            let TyId::Local(idx) = gamma.fresh_callsite_var(*NULL, expr_span) else {
                comp_error!(expr_span, "fresh_callsite_var returned non-local idx");
            };

            TyId::Local(gamma.out.ty_arena.alloc(TypeExpr::Nullable(idx)))
        }

        Expr::BinOp { op, lhs, rhs } => match op {
            hir::BinOp::Eq | hir::BinOp::NotEq => {
                let lhs_ty = infer_expr(c, gamma, *lhs)?;
                let rhs_ty = infer_expr(c, gamma, *rhs)?;

                // Comparing a value KNOWN to be non-null against `null` is a
                // category error: `null` can't inhabit it, so the check is dead.
                // "Known non-null" is flow-sensitive -- it covers a declared
                // non-nullable type AND a nullable refined to non-null by a guard
                // or assignment (`redundant_optional_on` reads the inferred type,
                // which reflects both). Exempts nullable / `any` / unresolved
                // generics. Consistent with bone's `??`/`?.` rejection on a
                // non-nullable; stricter than rock, which only narrows via guards.
                let non_null_ty = match (c.body.expr_arena[*lhs], c.body.expr_arena[*rhs]) {
                    (_, Expr::Null) => Some(lhs_ty),
                    (Expr::Null, _) => Some(rhs_ty),
                    _ => None,
                };
                if let Some(ty) = non_null_ty {
                    if redundant_optional_on(gamma, ty, expr_span) {
                        let rendered = gamma.workspace.render(Some(&gamma.out), ty);
                        comp_error!(
                            expr_span,
                            "Cannot compare non-nullable type '{}' with null",
                            rendered
                        );
                    }
                }

                join(gamma, lhs_ty, rhs_ty, expr_span)?;
                gamma.workspace.builtins.bool()
            }

            hir::BinOp::BitAnd
            | hir::BinOp::BitOr
            | hir::BinOp::BitXor
            | hir::BinOp::Shl
            | hir::BinOp::Shr => {
                let int_ty = gamma.workspace.builtins.int();
                let lhs_ty = infer_expr(c, gamma, *lhs)?;

                unify(gamma, lhs_ty, int_ty, c.body.expr_spans[*lhs])?;

                let rhs_ty = infer_expr(c, gamma, *rhs)?;
                unify(gamma, rhs_ty, int_ty, c.body.expr_spans[*rhs])?;

                int_ty
            }

            hir::BinOp::Lt | hir::BinOp::Leq | hir::BinOp::Gt | hir::BinOp::Geq => {
                let lhs_ty = infer_expr(c, gamma, *lhs)?;
                let rhs_ty = infer_expr(c, gamma, *rhs)?;

                let joined = join(gamma, lhs_ty, rhs_ty, expr_span)?;
                // unify(gamma, lhs_ty, rhs_ty, expr_span)?;

                let resolved = resolve(gamma, joined, expr_span);

                let orderable = match *gamma.get(resolved) {
                    TypeExpr::Builtin(BuiltinTy::Int)
                    | TypeExpr::Builtin(BuiltinTy::Float)
                    | TypeExpr::Generic(..) => true,

                    TypeExpr::App {
                        ctor: TypeCtor::Nominal(name),
                        ..
                    } if resolve_newtype(gamma, name).is_some() => {
                        let inner = tuple_or_newtype_index_ty(gamma, resolved, 0, expr_span)?;
                        let inner = resolve(gamma, inner, expr_span);

                        matches!(
                            gamma.get(inner),
                            TypeExpr::Builtin(BuiltinTy::Int) | TypeExpr::Builtin(BuiltinTy::Float)
                        )
                    }

                    _ => false,
                };

                if !orderable {
                    comp_error!(
                        expr_span,
                        "ordering operators require numeric operands, got {}",
                        gamma.workspace.render(Some(&gamma.out), resolved)
                    );
                }

                gamma.workspace.builtins.bool()
            }

            hir::BinOp::And => {
                let bool_ty = gamma.workspace.builtins.bool();
                let lhs_ty = infer_expr(c, gamma, *lhs)?;
                unify(gamma, lhs_ty, bool_ty, c.body.expr_spans[*lhs])?;

                let (then_narrows, _) = detect_null_narrow(gamma, c, *lhs);

                let rhs_ty = infer_branch(
                    c,
                    gamma,
                    *rhs,
                    &then_narrows,
                    c.body.expr_spans[*rhs],
                    Use::Value,
                )?;

                unify(gamma, rhs_ty, bool_ty, c.body.expr_spans[*rhs])?;

                bool_ty
            }

            hir::BinOp::Or => {
                let bool_ty = gamma.workspace.builtins.bool();
                let lhs_ty = infer_expr(c, gamma, *lhs)?;
                unify(gamma, lhs_ty, bool_ty, c.body.expr_spans[*lhs])?;

                // The RHS of `||` runs only when the LHS is false, so it sees the
                // LHS's ELSE narrowings (dual of `&&`, which uses THEN narrowings).
                let (_, else_narrows) = detect_null_narrow(gamma, c, *lhs);

                let rhs_ty = infer_branch(
                    c,
                    gamma,
                    *rhs,
                    &else_narrows,
                    c.body.expr_spans[*rhs],
                    Use::Value,
                )?;

                unify(gamma, rhs_ty, bool_ty, c.body.expr_spans[*rhs])?;

                bool_ty
            }

            hir::BinOp::NullCoalesce => {
                let lhs_ty = infer_expr(c, gamma, *lhs)?;
                if redundant_optional_on(gamma, lhs_ty, expr_span) {
                    comp_error!(
                        c.body.expr_spans[*lhs],
                        "left operand of `??` is not nullable -- the `?? default` is redundant"
                    );
                }
                let inner = peel_nullable(gamma, lhs_ty, expr_span);
                let rhs_ty = infer_expr(c, gamma, *rhs)?;

                join(gamma, inner, rhs_ty, expr_span)?
            }
        },

        Expr::Block { statements, tail } => {
            let mut g_saved: Vec<(Idx<BindingInfo>, TyId)> = vec![];
            // Snapshot the path-refinement set on entry; restore on exit. This
            // scopes to the block BOTH escaping guard narrows AND
            // assignment-refinements (`x.f = nonNull` marks `x.f` non-null):
            // flow-sensitively, a refinement established inside a block must not
            // survive past it (the other control path didn't run it). Whole-local
            // narrows peel `binding_types` and are restored via `g_saved`. The set
            // is usually empty/small, so the clone is cheap (an empty FxHashSet
            // clone doesn't allocate).
            let refined_snapshot = gamma.refined_nonnull.clone();

            for statement in *statements {
                if let Err(e) = infer_statement(c, gamma, statement) {
                    gamma.body_errors.push(e);
                    continue;
                };

                for (root, fields) in stmt_guard_narrows(gamma, c, statement) {
                    match root {
                        Root::Local(b) if fields.is_empty() => {
                            if let Some(cur) = gamma.out.binding_types.get(b).copied() {
                                let narrowed = peel_nullable(gamma, cur, expr_span);
                                gamma.out.binding_types.insert(b, narrowed);
                                g_saved.push((b, cur));
                            }
                        }
                        _ => {
                            gamma.refined_nonnull.insert((root, fields));
                        }
                    }
                }
            }

            let result = match tail {
                Some(tail) => match infer_expr_used(c, gamma, *tail, used) {
                    Ok(t) => t,
                    Err(err) => {
                        gamma.body_errors.push(err);
                        gamma.workspace.builtins.error()
                    }
                },
                None => {
                    let diverges = (*statements)
                        .into_iter()
                        .rfind(|&s| !matches!(c.body.statement_arena[s], Statement::Comment(_)))
                        .is_some_and(|s| matches!(c.body.statement_arena[s], Statement::Return(_)));

                    if diverges {
                        gamma.workspace.builtins.never()
                    } else {
                        gamma.workspace.builtins.unit()
                    }
                }
            };

            for (b, old) in g_saved {
                gamma.out.binding_types.insert(b, old);
            }

            gamma.refined_nonnull = refined_snapshot;

            result
        }

        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let cond_ty = infer_expr(c, gamma, *cond)?;
            unify(gamma, cond_ty, gamma.workspace.builtins.bool(), expr_span)?;

            let (narrow_then, narrow_else) = detect_null_narrow(gamma, c, *cond);

            match used {
                Use::Discard => {
                    infer_branch(
                        c,
                        gamma,
                        *then_branch,
                        &narrow_then,
                        expr_span,
                        Use::Discard,
                    )?;

                    if let Some(else_branch) = else_branch {
                        infer_branch(
                            c,
                            gamma,
                            *else_branch,
                            &narrow_else,
                            expr_span,
                            Use::Discard,
                        )?;
                    }

                    gamma.workspace.builtins.unit()
                }

                Use::Value => {
                    let then_ty =
                        infer_branch(c, gamma, *then_branch, &narrow_then, expr_span, Use::Value)?;

                    let Some(else_branch) = else_branch else {
                        comp_error!(
                            expr_span,
                            "`if` without a final `else` cannot be used as a value"
                        );
                    };

                    let else_ty =
                        infer_branch(c, gamma, *else_branch, &narrow_else, expr_span, Use::Value)?;
                    join(gamma, then_ty, else_ty, expr_span)?;

                    then_ty
                }
            }
        }

        Expr::TupleIndex { tuple, index } => {
            let tuple_ty = infer_expr(c, gamma, *tuple)?;
            let tuple_ty = resolve(gamma, tuple_ty, expr_span);

            tuple_or_newtype_index_ty(gamma, tuple_ty, *index, expr_span)?
        }

        Expr::Array(elements) | Expr::NativeArray(elements) => {
            let mut elem_ty: Option<TyId> = None;

            for elem in *elements {
                let elem_span = c.body.expr_spans[elem];
                let tys: Vec<TyId> = if let Expr::Spread(operand) = c.body.expr_arena[elem] {
                    let op_ty = infer_expr(c, gamma, operand)?;
                    spread_constituents(gamma, op_ty, elem_span)?
                } else {
                    vec![infer_expr(c, gamma, elem)?]
                };

                for t in tys {
                    elem_ty = Some(match elem_ty {
                        None => t,
                        Some(acc) => join(gamma, acc, t, elem_span)?,
                    });
                }
            }

            let elem_ty = match elem_ty {
                Some(t) => resolve(gamma, t, expr_span),
                None => gamma.fresh_callsite_var(*Q_E, expr_span),
            };

            let elem_idx = reify(gamma, elem_ty, expr_span);
            let elem_texp = *gamma.get(TyId::Local(elem_idx));
            let params = alloc_contiguous(&mut gamma.out.ty_arena, [elem_texp]);

            let ctor = match &c.body.expr_arena[e] {
                Expr::NativeArray(_) => TypeCtor::Nominal(*NATIVE_ARRAY),
                Expr::Array(_) => TypeCtor::Array,
                _ => comp_error!(expr_span, "expected array expression"),
            };

            let arr_ty = gamma.out.ty_arena.alloc(TypeExpr::App { ctor, params });

            let ret_ty = TyId::Local(arr_ty);

            resolve(gamma, ret_ty, expr_span)
        }

        Expr::Tuple(items) => {
            // TODO: can we get rid of the allocations here?
            let tys = items.map(|x| infer_expr(c, gamma, x)).try_vec()?;
            tuple_of(gamma, &tys, expr_span)
        }

        Expr::Call { callee, args } => {
            let args = *args;

            match callee {
                hir::Callee::Unresolved { name, .. } => {
                    resolve_named_call(gamma, c, *name, None, args, e, expr_span)?
                }

                hir::Callee::Local(binding) => {
                    let binding = *binding;

                    let fn_ty = gamma
                        .out
                        .binding_types
                        .get(binding)
                        .cloned()
                        .ok_or_else(|| {
                            CompilerError::new_with_label(
                                expr_span,
                                format!(
                                    "Undefined function `{}`",
                                    c.body.binding_arena[binding].name
                                ),
                            )
                        })?;

                    call_fn_value(c, gamma, fn_ty, args, expr_span)?
                }

                hir::Callee::Value(callee) => {
                    let fn_ty = infer_expr(c, gamma, *callee)?;
                    call_fn_value(c, gamma, fn_ty, args, expr_span)?
                }
            }
        }

        Expr::StructLiteral { name, fields, bases } => {
            let name = *name;
            let fields = *fields;
            let bases = *bases;

            let sid = resolve_struct(gamma, name).ok_or_else(|| {
                CompilerError::new_with_label(expr_span, format!("unknown struct `{}`", name))
            })?;

            let def = &gamma.workspace.module(sid.module).structs[sid.def];

            let (subs, arg_tys) = gamma.instantiate_generics(&def.generics, expr_span)?;
            let provided: Vec<(Ustr, Idx<Expr>)> = fields
                .into_iter()
                .map(|fi| {
                    let f = c.body.struct_lit_field_arena[fi];
                    (f.name, f.value)
                })
                .collect();

            let what = format!("struct `{}`", name);

            let ctor = match name.as_str() {
                "Vec" => TypeCtor::Array,
                _ => TypeCtor::Nominal(name),
            };
            let result_ty = gamma.app_with_ctor(ctor, &arg_tys, expr_span);

            // Functional update `Ty { ..base, .. }`: each base must have the same
            // nominal type as the literal (rule 6). A single base covers every
            // unwritten field. Multiple bases of the same type would all supply
            // every field (rule 5 collision), so only one is allowed.
            let has_base = !bases.is_empty();
            if bases.into_iter().count() > 1 {
                comp_error!(
                    expr_span,
                    "a functional-update literal takes at most one `..base`"
                );
            }
            for b in bases {
                check_expr(c, gamma, b, result_ty, c.body.expr_spans[b])?;
            }

            check_named_fields(
                c, gamma, &def.fields, &provided, &subs, expr_span, &what, has_base,
            )?;

            result_ty
        }

        Expr::FieldAccess { base, field } => {
            let (base, field) = (*base, *field);

            let base_ty = infer_expr(c, gamma, base)?;
            let base_ty = resolve(gamma, base_ty, expr_span);

            let fty = struct_field_ty(gamma, base_ty, field, expr_span)?;

            match path_key(c, e) {
                Some(p) if gamma.refined_nonnull.contains(&p) => {
                    peel_nullable(gamma, fty, expr_span)
                }
                _ => fty,
            }
        }

        Expr::SiblingRef(name) => {
            let name = *name;

            *gamma
                .sibling_fields
                .last()
                .and_then(|m| m.get(&name))
                .ok_or_else(|| CompilerError::new_with_label(expr_span, format!("field reference `.{name}` is only valid against an expected `FieldOf<T, V>` or in a struct-field default")))?
        }

        Expr::SafeNav { base, field } => {
            let base = *base;
            let field = *field;

            let base_ty = infer_expr(c, gamma, base)?;
            if redundant_optional_on(gamma, base_ty, expr_span) {
                comp_error!(
                    c.body.expr_spans[base],
                    "receiver of `?.` is not nullable -- use `.` instead"
                );
            }
            let inner = peel_nullable(gamma, base_ty, expr_span);

            let field_ty = if let Ok(index) = field.as_str().parse::<usize>() {
                let inner = resolve(gamma, inner, expr_span);
                tuple_or_newtype_index_ty(gamma, inner, index, expr_span)?
            } else {
                struct_field_ty(gamma, inner, field, expr_span)?
            };

            nullable_of(gamma, field_ty, expr_span)
        }

        Expr::SafeNavLet {
            receiver,
            binding,
            body,
        } => {
            let (receiver, binding, body) = (*receiver, *binding, *body);

            let (inner, deferred) = match infer_expr(c, gamma, receiver) {
                Ok(t) => (peel_nullable(gamma, t, expr_span), None),
                Err(err) => (gamma.workspace.builtins.error(), Some(err)),
            };

            gamma.assign_named_type(binding, inner)?;

            let body_ty = infer_expr(c, gamma, body)?;

            if let Some(err) = deferred {
                return Err(err);
            }

            nullable_of(gamma, body_ty, expr_span)
        }

        Expr::SafeNavCall {
            receiver,
            method,
            args,
        } => {
            let (receiver, method, args) = (*receiver, *method, *args);
            let recv_ty = infer_expr(c, gamma, receiver)?;
            if redundant_optional_on(gamma, recv_ty, expr_span) {
                comp_error!(
                    c.body.expr_spans[receiver],
                    "receiver of `?.` is not nullable -- use `.` instead"
                );
            }
            let inner = peel_nullable(gamma, recv_ty, expr_span);

            let ret = resolve_named_call(gamma, c, method, Some(inner), args, e, expr_span)?;

            nullable_of(gamma, ret, expr_span)
        }

        Expr::EnumVariant {
            enum_name,
            variant,
            args,
            ..
        } => {
            let (enum_name, variant, args) = (*enum_name, *variant, *args);

            let eid = resolve_enum(gamma, enum_name).ok_or_else(|| {
                CompilerError::new_with_label(expr_span, format!("unknown enum `{}`", enum_name))
            })?;

            // TODO: maybe make this Copy and then no alloc here?
            let def = gamma.workspace.module(eid.module).enums[eid.def].clone();
            let (subs, arg_tys) = gamma.instantiate_generics(&def.generics, expr_span)?;

            let vdef = def
                .variants
                .iter()
                .find(|v| v.name == variant)
                .cloned()
                .ok_or_else(|| {
                    CompilerError::new_with_label(
                        expr_span,
                        format!("enum `{}` has no variant `{}`", enum_name, variant),
                    )
                })?;

            let what = format!("variant: `{}::{}`", enum_name, variant);

            match (&vdef.payload, args) {
                (VariantPayload::Unit, VariantArgs::Unit) => {}

                (VariantPayload::Tuple(tys), VariantArgs::Tuple(range)) => {
                    check_positional_args(
                        c,
                        gamma,
                        tys,
                        range,
                        tys.len(),
                        &subs,
                        expr_span,
                        &what,
                    )?;
                }

                (VariantPayload::Record(fields), VariantArgs::Record(range)) => {
                    let provided: Vec<(Ustr, Idx<Expr>)> = range
                        .into_iter()
                        .map(|fi| {
                            let f = c.body.struct_lit_field_arena[fi];
                            (f.name, f.value)
                        })
                        .collect();

                    check_named_fields(c, gamma, fields, &provided, &subs, expr_span, &what, false)?;
                }

                _ => comp_error!(
                    expr_span,
                    "mismatched enum variant payload and arguments for {}",
                    what
                ),
            }

            gamma.nominal_app(enum_name, &arg_tys, expr_span)
        }

        Expr::Assign { target, value } => {
            let (target, value) = (*target, *value);
            // A `const` global is read-only: reject a whole-global rebind. Field/
            // index writes through it (`Expr::Field`/`Expr::Index` target) are out
            // of scope and fall through.
            if let Expr::Global(name) = c.body.expr_arena[target] {
                if gamma.workspace.const_globals.contains(&name) {
                    comp_error!(
                        expr_span,
                        "cannot assign to const `{}`; consts are read-only",
                        name
                    );
                }
            }
            // Writing any refined path (local field, global, or global field)
            // drops its refinement; re-established below if the stored value is
            // non-null. A whole local is handled via `binding_types` instead.
            let path = path_key(c, target);

            if let Some(p) = &path {
                gamma.refined_nonnull.remove(p);
            }

            let target_ty = match c.body.expr_arena[target] {
                Expr::Var(b) => match c.body.binding_arena[b].declared_ty {
                    Some(t) => resolve(gamma, t, expr_span),
                    None => infer_expr(c, gamma, target)?,
                },

                _ => infer_expr(c, gamma, target)?,
            };

            let value_ty = infer_expr(c, gamma, value)?;
            coerce(gamma, value_ty, target_ty, expr_span)?;

            let vr = resolve(gamma, value_ty, expr_span);
            let value_nonnull = !matches!(gamma.get(vr), TypeExpr::Nullable(_));

            if let Some(p) = path {
                match p.0 {
                    Root::Local(b) if p.1.is_empty() => {
                        if !value_nonnull {
                            gamma.out.binding_types.insert(b, target_ty);
                        }
                    }
                    _ if value_nonnull => {
                        gamma.refined_nonnull.insert(p);
                    }
                    _ => {}
                }
            };

            gamma.workspace.builtins.unit()
        }

        Expr::Match { scrutinee, arms } => {
            let (scrutinee, arms) = (*scrutinee, *arms);
            let scrut_ty = infer_expr(c, gamma, scrutinee)?;
            let scrut_ty = resolve(gamma, scrut_ty, expr_span);

            let mut result: Option<TyId> = None;
            let mut seen_unguarded_wildcard = false;
            // Exhaustiveness bookkeeping: only UNGUARDED arms contribute, since a
            // guarded arm may not fire. An unguarded `_`/binding arm catches the rest.
            let mut exhaustive_via_catch_all = false;
            let mut covered: FxHashSet<Ustr> = FxHashSet::default();

            for arm_idx in arms {
                let arm = c.body.match_arm_arena[arm_idx];

                if matches!(c.body.pattern_arena[arm.pattern], Pattern::Wildcard)
                    && arm.guard.is_none()
                {
                    if seen_unguarded_wildcard {
                        comp_error!(c.body.expr_spans[arm.body], "duplicate wildcard pattern in match expression - only one unguarded `_` arm is allowed");
                    }

                    seen_unguarded_wildcard = true;
                }

                check_pattern(c, gamma, arm.pattern, scrut_ty, expr_span)?;

                if let Some(guard) = arm.guard {
                    let guard_ty = infer_expr(c, gamma, guard)?;
                    unify(gamma, guard_ty, gamma.workspace.builtins.bool(), expr_span)?;
                } else {
                    if matches!(
                        c.body.pattern_arena[arm.pattern],
                        Pattern::Wildcard | Pattern::Bind(_)
                    ) {
                        exhaustive_via_catch_all = true;
                    }
                    collect_covered_enum_variants(c, arm.pattern, &mut covered);
                }

                let body_ty = infer_expr_used(c, gamma, arm.body, used)?;

                if used == Use::Value {
                    match result {
                        None => result = Some(body_ty),
                        Some(rt) => result = Some(join(gamma, body_ty, rt, expr_span)?),
                    }
                }
            }

            // A `match` over an enum must cover every variant unless an unguarded
            // `_`/binding arm catches the rest. Recovered (body_errors, not a hard
            // `?`) so partial inference survives for the LSP, like the return-type
            // mismatch. Message keeps the "Non-exhaustive match" prefix the LSP
            // fill-arms code action links against.
            if !exhaustive_via_catch_all {
                if let TypeExpr::App {
                    ctor: TypeCtor::Nominal(ename),
                    ..
                } = *gamma.get(scrut_ty)
                {
                    if let Some(eid) = resolve_enum(gamma, ename) {
                        let missing: Vec<Ustr> = gamma.workspace.module(eid.module).enums[eid.def]
                            .variants
                            .iter()
                            .map(|v| v.name)
                            .filter(|n| !covered.contains(n))
                            .collect();
                        if !missing.is_empty() {
                            let names = missing
                                .iter()
                                .map(|n| n.as_str())
                                .collect::<Vec<_>>()
                                .join(", ");
                            gamma.body_errors.push(CompilerError::new_with_label(
                                expr_span,
                                format!(
                                    "Non-exhaustive match expression. Missing patterns: {names}"
                                ),
                            ));
                        }
                    }
                }
            }

            match used {
                Use::Discard => gamma.workspace.builtins.unit(),
                Use::Value => result.unwrap_or_else(|| gamma.workspace.builtins.unit()),
            }
        }

        Expr::IfLet {
            pattern,
            value,
            then_branch,
            else_branch,
        } => {
            let (unwrapped, deferred) = match infer_expr(c, gamma, *value) {
                // if let x = 0 ... x: T
                Ok(t) => (peel_nullable(gamma, t, expr_span), None),
                Err(err) => (gamma.workspace.builtins.error(), Some(err)),
            };

            check_pattern(c, gamma, *pattern, unwrapped, expr_span)?;

            let result = match used {
                Use::Discard => {
                    infer_expr_used(c, gamma, *then_branch, Use::Discard)?;

                    if let Some(e) = else_branch {
                        infer_expr_used(c, gamma, *e, Use::Discard)?;
                    }

                    gamma.workspace.builtins.unit()
                }

                Use::Value => {
                    let then_ty = infer_expr(c, gamma, *then_branch)?;

                    let Some(e) = else_branch else {
                        comp_error!(
                            expr_span,
                            "`if let` without an `else` cannot be used as a value"
                        );
                    };

                    let else_ty = infer_expr(c, gamma, *e)?;
                    join(gamma, then_ty, else_ty, expr_span)?
                }
            };

            if let Some(err) = deferred {
                return Err(err);
            }

            result
        }

        Expr::CompoundAssign {
            target,
            op_fn,
            value,
        } => {
            let (target, op_fn, value) = (*target, *op_fn, *value);

            // `const` globals are read-only (mirrors the `Expr::Assign` check).
            if let Expr::Global(name) = c.body.expr_arena[target] {
                if gamma.workspace.const_globals.contains(&name) {
                    comp_error!(
                        expr_span,
                        "cannot assign to const `{}`; consts are read-only",
                        name
                    );
                }
            }

            let target_ty = infer_expr(c, gamma, target)?;
            let unwrapped = peel_nullable(gamma, target_ty, expr_span);
            let value_ty = infer_expr(c, gamma, value)?;

            let candidates = resolve_callee(gamma, op_fn);

            if candidates.is_empty() {
                comp_error!(expr_span, "Function `{}` doens't exist", op_fn);
            }

            let arg_tys = [
                resolve(gamma, unwrapped, expr_span),
                resolve(gamma, value_ty, expr_span),
            ];

            let (mid, fid) = if candidates.len() == 1 {
                candidates[0]
            } else {
                let mut arg_ty: FxHashMap<Idx<Expr>, TyId> = FxHashMap::default();
                arg_ty.insert(target, arg_tys[0]);
                arg_ty.insert(value, arg_tys[1]);

                select_overload(
                    c,
                    gamma,
                    &candidates,
                    None,
                    &[target, value],
                    &arg_ty,
                    &arg_tys,
                    op_fn,
                    expr_span,
                )?
            };

            gamma.out.resolved_overload_calls.insert(e, (mid, fid));

            let function = gamma.workspace.module(mid).functions[fid].clone();
            let (subs, _) = gamma.instantiate_generics(&function.generics, expr_span)?;

            let ret_ty = function
                .return_ty
                .unwrap_or_else(|| gamma.workspace.builtins.unit());

            let sub = substitute(gamma, ret_ty, &subs, expr_span)?;
            let ret_ty = resolve(gamma, sub, expr_span);

            coerce(gamma, ret_ty, unwrapped, expr_span)?;

            gamma.workspace.builtins.unit()
        }

        Expr::Return(e) => {
            let e_ty = match e {
                Some(e) => infer_expr(c, gamma, *e)?,
                None => gamma.workspace.builtins.unit(),
            };

            if let Some(declared) = gamma.declared_ret_ty {
                coerce(gamma, e_ty, declared, expr_span)?;
            }

            gamma.workspace.builtins.never()
        }

        Expr::Break | Expr::Continue => {
            if gamma.loop_depth == 0 {
                comp_error!(
                    expr_span,
                    "`break`/`continue` is only valid inside a loop body"
                );
            }
            gamma.workspace.builtins.never()
        }
    };

    gamma.assign_expr_type(c, e, ty)?;

    Ok(ty)
}

pub fn infer_expr(c: BodyCtx, gamma: &mut Gamma, e: Idx<Expr>) -> Result<TyId, CompilerError> {
    infer_expr_used(c, gamma, e, Use::Value)
}

fn substitute(
    gamma: &mut Gamma,
    ty: TyId,
    subs: &FxHashMap<VarId, TyId>,
    span: Span,
) -> Result<TyId, CompilerError> {
    let texp = gamma.get(ty);

    match *texp {
        TypeExpr::Builtin(_) => Ok(ty),
        TypeExpr::Generic(var_id, ustr) => {
            if let Some(sub) = subs.get(&var_id) {
                Ok(*sub)
            } else {
                comp_error!(span, "No sub for generic {ustr} in {:?}", texp);
            }
        }

        TypeExpr::Nullable(child) => {
            let child_ty = ty.with_idx(child);
            let sub_child = substitute(gamma, child_ty, subs, span)?;

            if sub_child == child_ty {
                Ok(ty)
            } else {
                Ok(nullable_of(gamma, sub_child, span))
            }
        }

        TypeExpr::App { ctor, params } => {
            if let Some(new_elements) = map_substitute(gamma, subs, span, ty, params)? {
                let idx = gamma.out.ty_arena.alloc(TypeExpr::App {
                    ctor,
                    params: new_elements,
                });
                gamma.out.ty_spans.insert(idx, span);

                Ok(TyId::Local(idx))
            } else {
                Ok(ty)
            }
        }

        TypeExpr::Tuple { elements } => {
            if let Some(new_elements) = map_substitute(gamma, subs, span, ty, elements)? {
                let idx = gamma.out.ty_arena.alloc(TypeExpr::Tuple {
                    elements: new_elements,
                });
                gamma.out.ty_spans.insert(idx, span);

                Ok(TyId::Local(idx))
            } else {
                Ok(ty)
            }

            // let mut subbed = Vec::with_capacity(elements.len());
            // let mut changed = false;
            //
            // for child in elements {
            //     let child_ty = ty.with_idx(child);
            //     let s = substitute(gamma, child_ty, subs, span)?;
            //     changed |= s != child_ty;
            //
            //     subbed.push(s);
            // }
            //
            // if !changed {
            //     return Ok(ty);
            // }
            //
            // let nodes = subbed
            //     .into_iter()
            //     .map(|x| gamma.get(x))
            //     .copied()
            //     .collect_vec();
            //
            // let new_elements = alloc_contiguous(&mut gamma.out.ty_arena, nodes);
        }

        TypeExpr::Function { params, ret } => {
            let new_params = map_substitute(gamma, subs, span, ty, params)?;
            let ret_ty = ty.with_idx(ret);
            let sub_ret = substitute(gamma, ret_ty, subs, span)?;

            if new_params.is_none() && sub_ret == ret_ty {
                Ok(ty)
            } else {
                let params = match new_params {
                    Some(p) => p,
                    None => reify_range(gamma, ty, params, span),
                };

                // let params = new_params.unwrap_or(params);
                let ret_idx = reify(gamma, sub_ret, span);

                let idx = gamma.out.ty_arena.alloc(TypeExpr::Function {
                    params,
                    ret: ret_idx,
                });
                gamma.out.ty_spans.insert(idx, span);

                Ok(TyId::Local(idx))
            }
        }

        TypeExpr::Infer(..) => {
            comp_error!(span, "Unsupported type expr {:?}", texp)
        }
    }
}

pub fn unify(gamma: &mut Gamma, a: TyId, b: TyId, span: Span) -> Result<(), CompilerError> {
    let a = resolve(gamma, a, span);
    let b = resolve(gamma, b, span);

    match (gamma.get(a), gamma.get(b)) {
        (TypeExpr::Builtin(x), TypeExpr::Builtin(y)) if x == y => {}

        (TypeExpr::Builtin(BuiltinTy::Never), _) => {}
        (_, TypeExpr::Builtin(BuiltinTy::Never)) => {}

        (TypeExpr::Builtin(BuiltinTy::Error), _) => {}
        (_, TypeExpr::Builtin(BuiltinTy::Error)) => {}

        (TypeExpr::Generic(x, _), TypeExpr::Generic(y, _)) if x == y => {}
        // (TypeExpr::Named(x), TypeExpr::Named(y)) if x == y => {}
        (TypeExpr::Infer(va, _), TypeExpr::Infer(vb, _)) if va == vb => {}

        (TypeExpr::Infer(va, _), _) => {
            gamma.out.bind_var(*va, b);
        }

        (_, TypeExpr::Infer(vb, _)) => {
            gamma.out.bind_var(*vb, a);
        }

        (
            TypeExpr::App {
                ctor: ctor_a,
                params: params_a,
            },
            TypeExpr::App {
                ctor: ctor_b,
                params: params_b,
            },
        ) => {
            if ctor_a != ctor_b {
                let a_str = gamma.workspace.render(Some(&gamma.out), a);
                let b_str = gamma.workspace.render(Some(&gamma.out), b);

                // Both sides are plain nominals (structs/enums with no type
                // args) far more often than generics; "different generic
                // constructors" is nonsense for those. Only mention generics
                // when a side actually has type parameters.
                if params_a.is_empty() && params_b.is_empty() {
                    unify_fail!(gamma, span, "type mismatch: expected `{b_str}`, found `{a_str}`")
                } else {
                    unify_fail!(
                        gamma,
                        span,
                        "type mismatch: expected `{b_str}`, found `{a_str}` (different type constructors)"
                    )
                }
            }

            if params_a.len() != params_b.len() {
                unify_fail!(
                    gamma,
                    span,
                    "Failed to unify {} and {}, generics of different length",
                    gamma.workspace.render(Some(&gamma.out), a),
                    gamma.workspace.render(Some(&gamma.out), b)
                )
            }

            for (ea, eb) in (*params_a).zip(*params_b) {
                unify(gamma, a.with_idx(ea), b.with_idx(eb), span)?;
            }
        }

        (
            TypeExpr::Tuple {
                elements: elements_a,
            },
            TypeExpr::Tuple {
                elements: elements_b,
            },
        ) => {
            if elements_a.len() != elements_b.len() {
                unify_fail!(
                    gamma,
                    span,
                    "Failed to unify {} and {}, tuples of different length",
                    gamma.workspace.render(Some(&gamma.out), a),
                    gamma.workspace.render(Some(&gamma.out), b)
                )
            }

            for (ea, eb) in (*elements_a).zip(*elements_b) {
                unify(gamma, a.with_idx(ea), b.with_idx(eb), span)?;
            }
        }

        (TypeExpr::Nullable(ia), TypeExpr::Nullable(ib)) => {
            let (ia, ib) = (*ia, *ib);

            unify(gamma, a.with_idx(ia), b.with_idx(ib), span)?;
        }

        (_, TypeExpr::Nullable(ib)) => {
            let ib = *ib;

            unify(gamma, a, b.with_idx(ib), span)?;
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
            if pa.len() != pb.len() {
                unify_fail!(
                    gamma,
                    span,
                    "Failed to unify {} and {}, functions with different arity",
                    gamma.workspace.render(Some(&gamma.out), a),
                    gamma.workspace.render(Some(&gamma.out), b)
                )
            }

            let (ra, rb) = (*ra, *rb);

            for (ea, eb) in (*pa).zip(*pb) {
                unify(gamma, a.with_idx(ea), b.with_idx(eb), span)?;
            }

            unify(gamma, a.with_idx(ra), b.with_idx(rb), span)?;
        }

        _ => unify_fail!(
            gamma,
            span,
            "Failed to unify {} and {}",
            gamma.workspace.render(Some(&gamma.out), a),
            gamma.workspace.render(Some(&gamma.out), b)
        ),
    }

    Ok(())
}

fn resolve(gamma: &mut Gamma, ty: TyId, span: Span) -> TyId {
    match *gamma.get(ty) {
        TypeExpr::Infer(var_id, _) => match gamma.out.substitutions.get(var_id) {
            Some(next) => {
                let root = resolve(gamma, next, span);

                if root != next {
                    gamma.out.bind_var(var_id, root);
                }

                root
            }
            None => ty,
        },

        TypeExpr::Nullable(inner) => {
            let inner_ty = ty.with_idx(inner);
            let resolved = resolve(gamma, inner_ty, span);

            if resolved == inner_ty {
                ty
            } else {
                nullable_of(gamma, resolved, span)
            }
        }

        TypeExpr::App { ctor, params } => {
            if let Some(new_params) = map_resolve(gamma, span, ty, params) {
                let idx = gamma.out.ty_arena.alloc(TypeExpr::App {
                    ctor,
                    params: new_params,
                });

                gamma.out.ty_spans.insert(idx, span);

                TyId::Local(idx)
            } else {
                ty
            }
        }

        TypeExpr::Tuple { elements } => {
            if let Some(new_elements) = map_resolve(gamma, span, ty, elements) {
                let idx = gamma.out.ty_arena.alloc(TypeExpr::Tuple {
                    elements: new_elements,
                });

                gamma.out.ty_spans.insert(idx, span);

                TyId::Local(idx)
            } else {
                ty
            }
        }

        TypeExpr::Function { params, ret } => {
            let new_params = map_resolve(gamma, span, ty, params);
            let ret_ty = ty.with_idx(ret);
            let resolved_ret = resolve(gamma, ret_ty, span);

            if new_params.is_none() && resolved_ret == ret_ty {
                ty
            } else {
                let params = new_params.unwrap_or(params);
                let ret_idx = reify(gamma, resolved_ret, span);

                let idx = gamma.out.ty_arena.alloc(TypeExpr::Function {
                    params,
                    ret: ret_idx,
                });

                gamma.out.ty_spans.insert(idx, span);

                TyId::Local(idx)
            }
        }

        _ => ty,
    }
}

fn map_resolve(
    gamma: &mut Gamma,
    span: Span,
    root_ty: TyId,
    elements: IdxRange<TypeExpr>,
) -> Option<IdxRange<TypeExpr>> {
    // This case is actually infallible
    map_children(gamma, root_ty, elements, span, |g, c| {
        Ok(resolve(g, c, span))
    })
    .unwrap()
}

fn map_substitute(
    gamma: &mut Gamma,
    subs: &FxHashMap<VarId, TyId>,
    span: Span,
    root_ty: TyId,
    elements: IdxRange<TypeExpr>,
) -> Result<Option<IdxRange<TypeExpr>>, CompilerError> {
    map_children(gamma, root_ty, elements, span, |g, c| {
        substitute(g, c, subs, span)
    })
}

fn map_children(
    gamma: &mut Gamma,
    root_ty: TyId,
    elements: IdxRange<TypeExpr>,
    span: Span,
    mut f: impl FnMut(&mut Gamma, TyId) -> Result<TyId, CompilerError>,
) -> Result<Option<IdxRange<TypeExpr>>, CompilerError> {
    let mut out: SmallVec<[TyId; 4]> = SmallVec::with_capacity(elements.len());
    let mut changed = false;

    for child in elements {
        let child_ty = root_ty.with_idx(child);
        let s = f(gamma, child_ty)?;

        changed |= s != child_ty;
        out.push(s);
    }

    if !changed {
        return Ok(None);
    }

    let mut nodes: SmallVec<[TypeExpr; 4]> = SmallVec::with_capacity(out.len());

    for x in out {
        let tyid = TyId::Local(reify(gamma, x, span));
        let t = *gamma.get(tyid);
        nodes.push(t);
    }

    Ok(Some(alloc_contiguous(&mut gamma.out.ty_arena, nodes)))
}

// Allocates a fresh local nullable wrapper
fn nullable_of(gamma: &mut Gamma, inner: TyId, span: Span) -> TyId {
    // collapse T?? -> T?
    if matches!(gamma.get(inner), TypeExpr::Nullable(_)) {
        return inner;
    }

    let inner_idx = reify(gamma, inner, span);
    let idx = gamma.out.ty_arena.alloc(TypeExpr::Nullable(inner_idx));
    gamma.out.ty_spans.insert(idx, span);

    TyId::Local(idx)
}

// If `ty` resolves to Nullable, return the resolved inner
fn peel_nullable(gamma: &mut Gamma, ty: TyId, span: Span) -> TyId {
    let ty = resolve(gamma, ty, span);

    match *gamma.get(ty) {
        TypeExpr::Nullable(inner) => resolve(gamma, ty.with_idx(inner), span),
        _ => ty,
    }
}

// If `cond` is a null check on a variable, return the binding and whether we narrow on
// `then` or `else` (x != null -> then, x == null -> else)
// fn detect_null_narrow(module: &Module, cond: Idx<Expr>) -> Option<(Idx<BindingInfo>, bool)> {
//     let Expr::BinOp { op, lhs, rhs } = c.body.expr_arena[cond] else {
//         return None;
//     };
//
//     let narrow_in_then = match op {
//         hir::BinOp::NotEq => true,
//         hir::BinOp::Eq => false,
//         _ => return None,
//     };
//
//     let binding = match (c.body.expr_arena[lhs], c.body.expr_arena[rhs]) {
//         (Expr::Var(b), Expr::Null) => b,
//         (Expr::Null, Expr::Var(b)) => b,
//         _ => return None,
//     };
//
//     Some((binding, narrow_in_then))
// }

fn types_equal(gamma: &Gamma, a: TyId, b: TyId) -> bool {
    if a == b {
        return true;
    }

    match (gamma.get(a), gamma.get(b)) {
        (TypeExpr::Builtin(x), TypeExpr::Builtin(y)) => x == y,
        (TypeExpr::Generic(x, _), TypeExpr::Generic(y, _)) => x == y,
        (TypeExpr::Infer(x, _), TypeExpr::Infer(y, _)) => x == y,
        // (TypeExpr::Named(x), TypeExpr::Named(y)) => x == y,
        (TypeExpr::Nullable(ia), TypeExpr::Nullable(ib)) => {
            types_equal(gamma, a.with_idx(*ia), b.with_idx(*ib))
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
        ) => {
            ca == cb
                && pa.len() == pb.len()
                && (*pa)
                    .zip(*pb)
                    .all(|(ea, eb)| types_equal(gamma, a.with_idx(ea), b.with_idx(eb)))
        }

        (TypeExpr::Tuple { elements: ea }, TypeExpr::Tuple { elements: eb }) => {
            ea.len() == eb.len()
                && (*ea)
                    .zip(*eb)
                    .all(|(ta, tb)| types_equal(gamma, a.with_idx(ta), b.with_idx(tb)))
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
            pa.len() == pb.len()
                && types_equal(gamma, a.with_idx(*ra), b.with_idx(*rb))
                && (*pa)
                    .zip(*pb)
                    .all(|(ea, eb)| types_equal(gamma, a.with_idx(ea), b.with_idx(eb)))
        }

        _ => false,
    }
}

fn resolve_callee(gamma: &Gamma, name: Ustr) -> FnList {
    let _span = tracing::info_span!("resolve_callee", name = %name).entered();

    gamma.dep_acc.borrow_mut().insert(name);

    gamma
        .workspace
        .functions_by_name
        .get(&name)
        .cloned()
        .unwrap_or_default()

    // gamma
    //     .workspace
    //     .modules
    //     .iter()
    //     .flat_map(|(mid, m)| {
    //         m.functions
    //             .iter()
    //             .filter(|(_, f)| f.name == name)
    //             .map(|(idx, _)| (*mid, idx))
    //     })
    //     .collect()
}

fn fit_cost(gamma: &mut Gamma, arg: TyId, param: TyId, span: Span) -> Option<usize> {
    let snap = gamma.out.checkpoint();

    if gamma.trial(|g| unify(g, arg, param, span).is_ok()) {
        return Some(0);
    }

    gamma.out.rollback_to(snap);

    let a = resolve(gamma, arg, span);
    let p = resolve(gamma, param, span);

    match (gamma.get(a), gamma.get(p)) {
        (TypeExpr::Builtin(BuiltinTy::Int), TypeExpr::Builtin(BuiltinTy::Float)) => Some(1),
        (_, TypeExpr::Builtin(BuiltinTy::Any)) => Some(2),
        _ => {
            let p_is_iface = match *gamma.get(p) {
                TypeExpr::App {
                    ctor: TypeCtor::Nominal(n),
                    ..
                } => gamma.workspace.interface_names.contains(&n),
                _ => false,
            };

            if p_is_iface {
                let snap = gamma.out.checkpoint();
                if gamma.trial(|g| coerce(g, a, p, span).is_ok()) {
                    return Some(3);
                }

                gamma.out.rollback_to(snap);
            }

            None
        }
    }
}

fn select_overload(
    c: BodyCtx,
    gamma: &mut Gamma,
    candidates: &[(ModuleId, Idx<Function>)],
    recv_ty: Option<TyId>,
    args: &[Idx<Expr>],
    arg_ty: &FxHashMap<Idx<Expr>, TyId>,
    arg_tys_msg: &[TyId],
    name: Ustr,
    span: Span,
) -> Result<(ModuleId, Idx<Function>), CompilerError> {
    let mut best: Option<(usize, (ModuleId, Idx<Function>))> = None;
    let mut tied = false;
    // The bound-conformance failure of an otherwise-fitting `Iterator`-bounded
    // candidate (params fit, bounds did not). When EVERY candidate rejects,
    // this is the near-miss the user needs to see -- the generic "No overload"
    // headline alone hides e.g. "EnumIter's `next` yields multiple values".
    let mut bound_fail: Option<CompilerError> = None;

    for &(mid, fid) in candidates {
        if let Some(cost) =
            score_candidate(c, gamma, mid, fid, recv_ty, args, arg_ty, span, &mut bound_fail)
        {
            match best {
                None => {
                    best = Some((cost, (mid, fid)));
                    tied = false;
                }
                Some((bc, _)) if cost < bc => {
                    best = Some((cost, (mid, fid)));
                    tied = false;
                }
                Some((bc, _)) if cost == bc => {
                    tied = true;
                }
                _ => {}
            }
        }
    }

    match best {
        Some((_, pick)) if !tied => Ok(pick),
        Some(_) => comp_error!(
            span,
            "Ambiguous overload for `{name}` with argument types ({})",
            arg_tys_msg
                .iter()
                .map(|t| gamma.workspace.render(Some(&gamma.out), *t))
                .join(", ")
        ),

        None => {
            let args_msg = arg_tys_msg
                .iter()
                .map(|t| gamma.workspace.render(Some(&gamma.out), *t))
                .join(", ");
            match bound_fail {
                Some(be) => comp_error!(
                    span,
                    "No overload for `{name}` with argument types ({args_msg}); the `Iterator`-bounded candidate was rejected: {}",
                    be.message
                ),
                None => comp_error!(
                    span,
                    "No overload for `{name}` with argument types ({args_msg})"
                ),
            }
        }
    }
}

fn resolve_struct(gamma: &Gamma, name: Ustr) -> Option<StructId> {
    gamma.dep_acc.borrow_mut().insert(name);
    gamma.workspace.struct_by_name.get(&name).copied()
}

fn struct_arg_subs(
    gamma: &Gamma,
    def: &StructDef,
    app_ty: TyId,
    params: IdxRange<TypeExpr>,
) -> FxHashMap<VarId, TyId> {
    let mut subs = FxHashMap::default();

    for (g, arg) in def.generics.iter().zip(params) {
        if let TypeExpr::Generic(var_id, _) = gamma.get(*g) {
            subs.insert(*var_id, app_ty.with_idx(arg));
        }
    }

    subs
}

fn resolve_enum(gamma: &Gamma, name: Ustr) -> Option<EnumId> {
    gamma.dep_acc.borrow_mut().insert(name);
    gamma.workspace.enums_by_name.get(&name).copied()
}

fn check_positional_args(
    c: BodyCtx,
    gamma: &mut Gamma,
    declared: &[TyId],
    args: IdxRange<Expr>,
    num_required: usize,
    subs: &FxHashMap<VarId, TyId>,
    span: Span,
    what: &str,
) -> Result<(), CompilerError> {
    // if args.len() != declared.len() {
    //     comp_error!(
    //         span,
    //         "Expected {} arguments in {}, but got {}",
    //         declared.len(),
    //         what,
    //         args.len()
    //     );
    // }
    //
    // for (arg, dty) in args.zip(declared.iter()) {
    //     let aspan = c.body.expr_spans[arg];
    //     let pty = substitute(gamma, *dty, subs, span)?;
    //     let aty = infer_expr(c, gamma, arg)?;
    //
    //     coerce(gamma, aty, pty, aspan)?;
    // }

    let params: Vec<TyId> = declared
        .iter()
        .map(|d| substitute(gamma, *d, subs, span))
        .collect::<Result<_, _>>()?;

    check_args(c, gamma, &params, args, num_required, span, what)
}

fn check_named_fields(
    c: BodyCtx,
    gamma: &mut Gamma,
    fields: &[StructFieldDef],
    provided: &[(Ustr, Idx<Expr>)],
    subs: &FxHashMap<VarId, TyId>,
    span: Span,
    what: &str,
    // A functional-update base (`{ ..base }`) copies every unwritten field, so
    // missing-field errors are suppressed when a base is present.
    base_covers_all: bool,
) -> Result<(), CompilerError> {
    let mut seen: Vec<Ustr> = Vec::new();

    for (fname, fvalue) in provided {
        let vspan = c.body.expr_spans[*fvalue];

        let Some(fdef) = fields.iter().find(|f| f.name == *fname) else {
            comp_error!(vspan, "{} has no field named `{}`", what, fname);
        };

        if seen.contains(fname) {
            comp_error!(vspan, "Duplicate field `{}` in {} literal", fname, what);
        }

        seen.push(*fname);

        let fty = substitute(gamma, fdef.ty, subs, vspan)?;
        check_expr(c, gamma, *fvalue, fty, vspan)?
    }

    if !base_covers_all {
        for f in fields {
            if !seen.contains(&f.name) && !f.has_default {
                comp_error!(span, "Missing field `{}` in {} literal", f.name, what);
            }
        }
    }

    Ok(())
}

/// Enum-variant names an arm pattern covers (for exhaustiveness). A variant /
/// record pattern covers its variant; an or-pattern covers the union of its
/// alternatives; everything else (wildcard/binding/literal/...) covers no named
/// variant. Guard handling is the caller's job -- only unguarded arms are fed in.
fn collect_covered_enum_variants(c: BodyCtx, pat: Idx<Pattern>, out: &mut FxHashSet<Ustr>) {
    match c.body.pattern_arena[pat] {
        Pattern::EnumVariant { variant, .. } => {
            out.insert(variant);
        }
        Pattern::Record { name: Some(n), .. } => {
            out.insert(n);
        }
        Pattern::Or { alternatives } => {
            for alt in alternatives {
                collect_covered_enum_variants(c, alt, out);
            }
        }
        _ => {}
    }
}

fn check_pattern(
    c: BodyCtx,
    gamma: &mut Gamma,
    pat: Idx<Pattern>,
    expected: TyId,
    span: Span,
) -> Result<(), CompilerError> {
    let er = resolve(gamma, expected, span);

    if matches!(gamma.get(er), TypeExpr::Builtin(BuiltinTy::Error)) {
        return bind_pattern_leaves_to(c, gamma, pat, er, span);
    }

    match c.body.pattern_arena[pat] {
        Pattern::Wildcard => Ok(()),
        Pattern::Bind(b) => gamma.assign_named_type(b, expected),

        Pattern::Literal(lit) => {
            let lit_ty = infer_expr(c, gamma, lit)?;
            coerce(gamma, lit_ty, expected, span)
        }

        Pattern::Range { start, end, .. } => {
            for bound in [start, end].into_iter().flatten() {
                let bty = infer_expr(c, gamma, bound)?;
                coerce(gamma, bty, expected, span)?;
            }

            Ok(())
        }

        Pattern::EnumVariant {
            enum_name,
            variant,
            sub,
            ..
        } => {
            let expected = resolve(gamma, expected, span);
            let (scrut_name, params) = match *gamma.get(expected) {
                TypeExpr::App {
                    ctor: TypeCtor::Nominal(scrut_name),
                    params,
                } => (scrut_name, params),
                _ => comp_error!(
                    span,
                    "Expected a nominal type in pattern, got {}",
                    gamma.workspace.render(Some(&gamma.out), expected)
                ),
            };

            let ename = enum_name.unwrap_or(scrut_name);

            let eid = resolve_enum(gamma, ename).ok_or_else(|| {
                CompilerError::new_with_label(span, format!("unknown enum `{}`", ename))
            })?;

            // TODO: maybe don't need to clone
            let def = gamma.workspace.module(eid.module).enums[eid.def].clone();

            let mut subs = FxHashMap::default();
            for (g, arg) in def.generics.iter().zip(params) {
                if let TypeExpr::Generic(var_id, _) = gamma.get(*g) {
                    subs.insert(*var_id, expected.with_idx(arg));
                }
            }

            let vdef = def
                .variants
                .iter()
                .find(|v| v.name == variant)
                .cloned()
                .ok_or_else(|| {
                    CompilerError::new_with_label(
                        span,
                        format!("enum `{}` has no variant `{}`", ename, variant),
                    )
                })?;

            match vdef.payload {
                VariantPayload::Unit => {
                    if sub.len() != 0 {
                        comp_error!(
                            span,
                            "variant `{}` has no payload, but pattern has subpattern",
                            variant
                        );
                    }
                }

                VariantPayload::Tuple(tys) => {
                    if sub.len() != tys.len() {
                        comp_error!(
                            span,
                            "variant `{}` expected {} subpatterns, but got {}",
                            variant,
                            tys.len(),
                            sub.len()
                        );
                    }

                    for (subpat, ty) in sub.zip(tys.iter()) {
                        let pty = substitute(gamma, *ty, &subs, span)?;
                        check_pattern(c, gamma, subpat, pty, span)?;
                    }
                }

                VariantPayload::Record(_) => comp_error!(
                    span,
                    "Record variants not supported in patterns yet (variant `{}`)",
                    variant
                ),
            }

            Ok(())
        }

        Pattern::Record {
            enum_name,
            name,
            fields,
            ..
        } => {
            let expected = resolve(gamma, expected, span);

            if let TypeExpr::App {
                ctor: TypeCtor::Nominal(scrut),
                params,
            } = *gamma.get(expected)
            {
                let ename = enum_name.unwrap_or(scrut);

                if let Some(eid) = resolve_enum(gamma, ename) {
                    // TODO: another clone we might not need?
                    let def = gamma.workspace.module(eid.module).enums[eid.def].clone();

                    let mut subs = FxHashMap::default();
                    for (g, arg) in def.generics.iter().zip(params) {
                        if let TypeExpr::Generic(var_id, _) = gamma.get(*g) {
                            subs.insert(*var_id, expected.with_idx(arg));
                        }
                    }

                    let vname = name.ok_or_else(|| {
                        CompilerError::new_with_label(
                            span,
                            "record-variant pattern requires a variant name".to_string(),
                        )
                    })?;

                    let vdef = def
                        .variants
                        .iter()
                        .find(|v| v.name == vname)
                        .cloned()
                        .ok_or_else(|| {
                            CompilerError::new_with_label(
                                span,
                                format!("enum `{}` has no variant `{}`", ename, vname),
                            )
                        })?;

                    let rec = match vdef.payload {
                        VariantPayload::Record(fs) => fs,
                        _ => comp_error!(span, "variant `{}` is not a record variant", vname),
                    };

                    for f in fields {
                        let pf = c.body.record_pat_field_arena[f];
                        let fdef = rec.iter().find(|sf| sf.name == pf.name).ok_or_else(|| {
                            CompilerError::new_with_label(
                                span,
                                format!("record variant `{}` has no field `{}`", vname, pf.name),
                            )
                        })?;

                        let fty = substitute(gamma, fdef.ty, &subs, span)?;
                        check_pattern(c, gamma, pf.pat, fty, span)?;
                    }

                    return Ok(());
                }
            }

            for f in fields {
                let field = c.body.record_pat_field_arena[f];
                let fty = struct_field_ty(gamma, expected, field.name, span)?;
                check_pattern(c, gamma, field.pat, fty, span)?;
            }

            Ok(())
        }

        Pattern::Or { alternatives } => {
            for alt in alternatives {
                if pattern_binds(c, alt) {
                    comp_error!(
                        span,
                        "or-patterns cannot bind variables, use `_` for payload"
                    );
                }

                check_pattern(c, gamma, alt, expected, span)?;
            }

            Ok(())
        }

        Pattern::Tuple { elements } => {
            let expected = resolve(gamma, expected, span);

            let tuple_elems = match *gamma.get(expected) {
                TypeExpr::Tuple { elements } => elements,

                _ => comp_error!(
                    span,
                    "expected a tuple type in pattern, got {}",
                    gamma.render(expected)
                ),
            };

            if elements.len() != tuple_elems.len() {
                comp_error!(
                    span,
                    "tuple pattern expected {} elements, but got {}",
                    tuple_elems.len(),
                    elements.len()
                );
            }

            for (subpat, elem) in elements.zip(tuple_elems) {
                let elem_ty = resolve(gamma, expected.with_idx(elem), span);
                check_pattern(c, gamma, subpat, elem_ty, span)?;
            }

            Ok(())
        }
    }
}

// Resolve the type of `field` on a nominal struct, with generic subst.
// `base_ty` must be resolved and peeled nullable for safe nav.
fn struct_field_ty(
    gamma: &mut Gamma,
    base_ty: TyId,
    field: Ustr,
    span: Span,
) -> Result<TyId, CompilerError> {
    let (struct_name, params) = match *gamma.get(base_ty) {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } => {
            gamma.dep_acc.borrow_mut().insert(name);
            (name, params)
        }

        TypeExpr::App {
            ctor: TypeCtor::Array,
            params,
        } => (*VEC, params),

        TypeExpr::Infer(..) => {
            let mut matches: Vec<StructId> = Vec::new();

            for m in gamma.workspace.modules.iter() {
                let mid = m.module_id();

                for (def, sdef) in m.structs.iter() {
                    if sdef.fields.iter().any(|f| f.name == field) {
                        matches.push(StructId { module: mid, def });
                    }
                }
            }

            let sid = match matches.as_slice() {
                [one] => *one,
                [] => comp_error!(span, "no struct has a field `{}`", field),
                _ => comp_error!(
                    span,
                    "ambiguous field `{}`: multiple structs define it; annotate the parameter",
                    field
                ),
            };

            let def = gamma.workspace.module(sid.module).structs[sid.def].clone();
            let (subs, fresh) = gamma.instantiate_generics(&def.generics, span)?;

            let struct_ty = gamma.nominal_app(def.name, &fresh, span);
            unify(gamma, base_ty, struct_ty, span)?;

            // `sid` was selected because it has this field (scan above), so the
            // re-find succeeds -- but return an error rather than unwrap, so a
            // future change to the scan degrades to a diagnostic, never a panic.
            let fty = def
                .fields
                .iter()
                .find(|f| f.name == field)
                .ok_or_else(|| {
                    CompilerError::new_with_label(
                        span,
                        format!("struct `{}` has no field `{}`", def.name, field),
                    )
                })?
                .ty;
            return substitute(gamma, fty, &subs, span);
        }

        _ => comp_error!(
            span,
            "cannot access field `{}` on non-struct type {}",
            field,
            gamma.workspace.render(Some(&gamma.out), base_ty)
        ),
    };

    let sid = resolve_struct(gamma, struct_name).ok_or_else(|| {
        CompilerError::new_with_label(span, format!("unknown struct `{}`", struct_name))
    })?;

    let def = &gamma.workspace.module(sid.module).structs[sid.def];
    let subs = struct_arg_subs(gamma, def, base_ty, params);

    let field_ty = def
        .fields
        .iter()
        .find(|f| f.name == field)
        .map(|f| f.ty)
        .ok_or_else(|| {
            CompilerError::new_with_label(
                span,
                format!("struct `{}` has no field `{}`", struct_name, field),
            )
        })?;

    substitute(gamma, field_ty, &subs, span)
}

fn tuple_or_newtype_index_ty(
    gamma: &mut Gamma,
    tuple_ty: TyId,
    index: usize,
    span: Span,
) -> Result<TyId, CompilerError> {
    let ty = match *gamma.get(tuple_ty) {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } if gamma.workspace.newtypes_by_name.contains_key(&name) => {
            let NewtypeId { module: ntm, def } = resolve_newtype(gamma, name).unwrap();
            let nt = gamma.workspace.module(ntm).newtypes[def].clone();

            let mut subs = FxHashMap::default();

            for (g, arg) in nt.generics.iter().zip(params) {
                if let TypeExpr::Generic(var_id, _) = gamma.get(*g) {
                    subs.insert(*var_id, tuple_ty.with_idx(arg));
                }
            }

            if index != 0 {
                comp_error!(
                    span,
                    "newtype `{}` has no field .{} (newtypes wrap one value, indexed .0)",
                    name,
                    index
                );
            }

            substitute(gamma, nt.wrapped, &subs, span)?
        }

        TypeExpr::Tuple { elements } => {
            let elem = elements.into_iter().nth(index).ok_or_else(|| {
                CompilerError::new_with_label(
                    span,
                    format!(
                        "tuple index {} out of bounds (len {})",
                        index,
                        elements.len()
                    ),
                )
            })?;

            let elem_ty = tuple_ty.with_idx(elem);
            resolve(gamma, elem_ty, span)
        }

        _ => comp_error!(
            span,
            "cannot index a non-tuple type {}",
            gamma.workspace.render(Some(&gamma.out), tuple_ty),
        ),
    };

    Ok(ty)
}

fn score_candidate(
    c: BodyCtx,
    gamma: &mut Gamma,
    mid: ModuleId,
    fid: Idx<Function>,
    recv_ty: Option<TyId>,
    args: &[Idx<Expr>],
    arg_ty: &FxHashMap<Idx<Expr>, TyId>,
    span: Span,
    bound_fail: &mut Option<CompilerError>,
) -> Option<usize> {
    let snap = gamma.out.checkpoint();

    let f = &gamma.workspace.module(mid).functions[fid];

    let mut declared: SmallVec<[TyId; 8]> = SmallVec::with_capacity(c.body.param_bindings.len());
    let mut pnames: SmallVec<[Ustr; 8]> = SmallVec::with_capacity(c.body.param_bindings.len());

    for p in f.param_decls.iter() {
        declared.push(p.ty);
        pnames.push(p.name);
    }

    let subs = match gamma.instantiate_generics(&f.generics, span) {
        Ok((s, _)) => s,
        Err(_) => {
            gamma.out.rollback_to(snap);
            return None;
        }
    };

    let offset = if recv_ty.is_some() { 1 } else { 0 };
    if offset > declared.len() {
        gamma.out.rollback_to(snap);

        return None;
    }

    // if arg_tys.len() < f.num_required || arg_tys.len() > declared.len() {
    //     gamma.out.substitutions = outer;
    //
    //     return None;
    // }

    let mut total = 0usize;

    // Receiver (fills slot 0) - named args never name it
    if let Some(r) = recv_ty {
        let pty = if subs.is_empty() {
            declared[0]
        } else {
            match substitute(gamma, declared[0], &subs, span) {
                Ok(t) => t,
                Err(_) => {
                    gamma.out.rollback_to(snap);
                    return None;
                }
            }
        };

        match fit_cost(gamma, r, pty, span) {
            Some(c) => total += c,
            None => {
                gamma.out.rollback_to(snap);
                return None;
            }
        }
    }

    // Map named + positional args onto THIS candidate's slots

    let slots = match reorder_call_args(
        c,
        gamma,
        args,
        &pnames[offset..],
        f.num_required.saturating_sub(offset),
        span,
    ) {
        Ok(s) => s,
        Err(_) => {
            gamma.out.rollback_to(snap);
            return None;
        }
    };

    for (i, slot) in slots.iter().enumerate() {
        let pty = if subs.is_empty() {
            declared[offset + i]
        } else {
            match substitute(gamma, declared[offset + i], &subs, span) {
                Ok(t) => t,
                Err(_) => {
                    gamma.out.rollback_to(snap);
                    return None;
                }
            }
        };

        match slot {
            Some(fill) => {
                let aty = match fill {
                    SlotFill::Direct(arg) => match arg_ty.get(arg) {
                        Some(&t) => t,
                        None => {
                            gamma.out.rollback_to(snap);
                            return None;
                        }
                    },
                    SlotFill::SpreadField(_, fty) => *fty,
                };

                match fit_cost(gamma, aty, pty, span) {
                    Some(c) => total += c,
                    None => {
                        gamma.out.rollback_to(snap);
                        return None;
                    }
                }
            }

            None => total += 1, // omitted default param
        }
    }

    // An `Iterator`-bounded candidate only fits if its bounds hold for this
    // instantiation: a bare `it: I` unifies with ANY arg (`[int]`, Arena, ..),
    // so without this check a bounded combinator would tie with -- or shadow --
    // the concrete overloads (`map([T], f)`, `map(Arena<T>, f)`).
    let f = &gamma.workspace.module(mid).functions[fid];
    if f.generic_bounds.iter().any(Option::is_some) {
        let mut tree = BoundTree::default();
        let mut done = FxHashSet::default();
        let mut berr: Option<CompilerError> = None;
        let conforms = gamma.trial(|g| {
            match check_iterator_bounds(c, g, f, &subs, &mut tree, &mut done, false, 0, span) {
                Ok(()) => true,
                Err(e) => {
                    berr = Some(e);
                    false
                }
            }
        });
        if !conforms {
            // Keep the FIRST bound failure for the all-rejected diagnostic.
            if bound_fail.is_none() {
                *bound_fail = berr;
            }
            gamma.out.rollback_to(snap);
            return None;
        }
        // Bounds that fit only structurally still rank below an exact concrete
        // overload -- prefer the specialized fn when both match.
        total += 1;
    }

    // for (dty, &aty) in declared.iter().zip(arg_tys) {
    //     let pty = match substitute(gamma, *dty, &subs, span) {
    //         Ok(t) => t,
    //         Err(_) => {
    //             gamma.out.substitutions = outer;
    //             return None;
    //         }
    //     };
    //
    //     match fit_cost(gamma, aty, pty, span) {
    //         Some(c) => total += c,
    //         None => {
    //             gamma.out.substitutions = outer;
    //             return None;
    //         }
    //     }
    // }
    //
    // total += declared.len() - arg_tys.len();

    gamma.out.rollback_to(snap);
    Some(total)
}

pub fn coerce(gamma: &mut Gamma, from: TyId, to: TyId, span: Span) -> Result<(), CompilerError> {
    let from_r = resolve(gamma, from, span);
    let to_r = resolve(gamma, to, span);

    if matches!(gamma.get(from_r), TypeExpr::Builtin(BuiltinTy::Never)) {
        return Ok(());
    }

    if matches!(gamma.get(from_r), TypeExpr::Builtin(BuiltinTy::Error))
        || matches!(gamma.get(to_r), TypeExpr::Builtin(BuiltinTy::Error))
    {
        return Ok(());
    }

    if let TypeExpr::Infer(_, _) = *gamma.get(from_r) {
        return unify(gamma, from_r, to_r, span);
    }

    // `null` (a `Nullable` whose inner is still an unconstrained `Infer`) may
    // not be assigned to bare `any` -- `any` excludes null; use `any?`. A
    // nullable VALUE with a concrete inner (e.g. `int?`) IS allowed (rock
    // parity: only the null literal is rejected here).
    if matches!(gamma.get(to_r), TypeExpr::Builtin(BuiltinTy::Any)) {
        if let TypeExpr::Nullable(inner) = *gamma.get(from_r) {
            let inner_r = resolve(gamma, from_r.with_idx(inner), span);
            if matches!(gamma.get(inner_r), TypeExpr::Infer(..)) {
                return Err(CompilerError::new_with_label(
                    span,
                    "`null` cannot be assigned to bare `any` (which excludes null) -- use `any?`"
                        .to_string(),
                ));
            }
        }
    }

    let from_ty = gamma.get(from_r);
    let to_ty = gamma.get(to_r);

    if matches!(to_ty, TypeExpr::Builtin(BuiltinTy::Any)) {
        return Ok(());
    }

    if matches!(from_ty, TypeExpr::Builtin(BuiltinTy::Int))
        && matches!(to_ty, TypeExpr::Builtin(BuiltinTy::Float))
    {
        return Ok(());
    }

    if matches!(from_ty, TypeExpr::Builtin(BuiltinTy::Float))
        && matches!(to_ty, TypeExpr::Builtin(BuiltinTy::Int))
    {
        if let Some(v) = bound_var(gamma, to) {
            gamma.out.bind_var(v, from_r);
            return Ok(());
        }
    }

    if let TypeExpr::App {
        ctor: TypeCtor::Nominal(iname),
        ..
    } = *gamma.get(to_r)
    {
        if gamma.workspace.interface_names.contains(&iname) {
            return coerce_to_interface(gamma, from_r, iname, span);
        }
    }

    let snap = gamma.out.checkpoint();
    if gamma.trial(|g| unify(g, from_r, to_r, span).is_ok()) {
        return Ok(());
    }

    gamma.out.rollback_to(snap);

    if let TypeExpr::Nullable(ti) = *gamma.get(to_r) {
        let to_inner = to_r.with_idx(ti);
        let from_inner = match *gamma.get(from_r) {
            TypeExpr::Nullable(fi) => from_r.with_idx(fi),
            _ => from_r,
        };

        return coerce(gamma, from_inner, to_inner, span);
    }

    unify(gamma, from_r, to_r, span)
}

fn check_args(
    c: BodyCtx,
    gamma: &mut Gamma,
    params: &[TyId],
    args: IdxRange<Expr>,
    num_required: usize,
    span: Span,
    what: &str,
) -> Result<(), CompilerError> {
    if args.len() < num_required || args.len() > params.len() {
        comp_error!(
            span,
            "Expected {} to {} arguments in {}, but got {}",
            num_required,
            params.len(),
            what,
            args.len()
        );
    }

    for (arg, pty) in args.zip(params.iter()) {
        let aspan = c.body.expr_spans[arg];
        check_expr(c, gamma, arg, *pty, aspan)?;
    }

    Ok(())
}

pub fn join(gamma: &mut Gamma, a: TyId, b: TyId, span: Span) -> Result<TyId, CompilerError> {
    let ar = resolve(gamma, a, span);
    let br = resolve(gamma, b, span);

    if matches!(
        gamma.get(ar),
        TypeExpr::Builtin(BuiltinTy::Never) | TypeExpr::Builtin(BuiltinTy::Error)
    ) {
        return Ok(br);
    }

    if matches!(
        gamma.get(br),
        TypeExpr::Builtin(BuiltinTy::Never) | TypeExpr::Builtin(BuiltinTy::Error)
    ) {
        return Ok(ar);
    }

    let a_inner = match *gamma.get(ar) {
        TypeExpr::Nullable(i) => Some(ar.with_idx(i)),
        _ => None,
    };

    let b_inner = match *gamma.get(br) {
        TypeExpr::Nullable(i) => Some(br.with_idx(i)),
        _ => None,
    };

    if a_inner.is_some() || b_inner.is_some() {
        let inner = join(gamma, a_inner.unwrap_or(ar), b_inner.unwrap_or(br), span)?;
        return Ok(nullable_of(gamma, inner, span));
    }

    match (gamma.get(ar), gamma.get(br)) {
        (TypeExpr::Builtin(BuiltinTy::Int), TypeExpr::Builtin(BuiltinTy::Float))
        | (TypeExpr::Builtin(BuiltinTy::Float), TypeExpr::Builtin(BuiltinTy::Int)) => {
            Ok(gamma.workspace.builtins.float())
        }

        _ => {
            let snap = gamma.out.checkpoint();
            if gamma.trial(|g| coerce(g, ar, br, span).is_ok()) {
                return Ok(br);
            }

            gamma.out.rollback_to(snap);

            if gamma.trial(|g| coerce(g, br, ar, span).is_ok()) {
                return Ok(ar);
            }

            gamma.out.rollback_to(snap);

            unify(gamma, ar, br, span)?;
            Ok(resolve(gamma, ar, span))
        }
    }
}

// Deep-copy `ty` (any arena) into gamma.out, remapping child indices. Only safe way to
// move possibly Static into a local arena. Shallow copy leaves dangling indices.
fn reify(gamma: &mut Gamma, ty: TyId, span: Span) -> Idx<TypeExpr> {
    let node = match *gamma.get(ty) {
        TypeExpr::App { ctor, params } => TypeExpr::App {
            ctor,
            params: reify_range(gamma, ty, params, span),
        },

        TypeExpr::Tuple { elements } => TypeExpr::Tuple {
            elements: reify_range(gamma, ty, elements, span),
        },

        TypeExpr::Function { params, ret } => {
            let params = reify_range(gamma, ty, params, span);
            let ret = reify(gamma, ty.with_idx(ret), span);

            TypeExpr::Function { params, ret }
        }

        TypeExpr::Nullable(inner) => TypeExpr::Nullable(reify(gamma, ty.with_idx(inner), span)),

        leaf => leaf,
    };

    let idx = gamma.out.ty_arena.alloc(node);
    gamma.out.ty_spans.insert(idx, span);
    idx
}

// TODO: keep this and A/B test against the no-alloc version
fn reify_range(
    gamma: &mut Gamma,
    parent: TyId,
    range: IdxRange<TypeExpr>,
    span: Span,
) -> IdxRange<TypeExpr> {
    let mut children = Vec::with_capacity(range.len());

    for c in range {
        let tyid = TyId::Local(reify(gamma, parent.with_idx(c), span));
        let texp = *gamma.get(tyid);
        children.push(texp);
    }

    alloc_contiguous(&mut gamma.out.ty_arena, children)
}

// fn reify_range(
//     gamma: &mut Gamma,
//     parent: TyId,
//     range: IdxRange<TypeExpr>,
//     span: Span,
// ) -> IdxRange<TypeExpr> {
//     let mut idx_start = None;
//     let mut idx_end = None;
//
//     for c in range {
//         let tyid = TyId::Local(reify(gamma, parent.with_idx(c), span));
//         let texp = *gamma.get(tyid);
//
//         // TODO: span?
//         idx_end = Some(gamma.out.ty_arena.alloc(texp));
//         idx_start = idx_start.or(idx_end);
//     }
//
//     if let (Some(a), Some(b)) = (idx_start, idx_end) {
//         IdxRange::new_inclusive(a..=b)
//     } else {
//         IdxRange::new_empty()
//     }
// }

fn tuple_of(gamma: &mut Gamma, elems: &[TyId], span: Span) -> TyId {
    let idxs: Vec<TyId> = elems
        .iter()
        .map(|&t| TyId::Local(reify(gamma, t, span)))
        .collect();
    let nodes: Vec<TypeExpr> = idxs.iter().map(|&id| *gamma.get(id)).collect();
    let range = alloc_contiguous(&mut gamma.out.ty_arena, nodes);

    let idx = gamma
        .out
        .ty_arena
        .alloc(TypeExpr::Tuple { elements: range });
    gamma.out.ty_spans.insert(idx, span);

    TyId::Local(idx)
}

fn coerce_to_interface(
    gamma: &mut Gamma,
    from: TyId,
    iname: Ustr,
    span: Span,
) -> Result<(), CompilerError> {
    let isid = resolve_struct(gamma, iname).ok_or_else(|| {
        CompilerError::new_with_label(span, format!("unknown interface `{iname}`"))
    })?;

    let ifields: Vec<(Ustr, TyId)> = gamma.workspace.module(isid.module).structs[isid.def]
        .fields
        .iter()
        .map(|f| (f.name, f.ty))
        .collect();

    for (fname, fty) in ifields {
        let actual = struct_field_ty(gamma, from, fname, span).map_err(|_| {
            CompilerError::new_with_label(
                span,
                format!(
                    "type {} does not have required field `{}` of interface `{}`",
                    gamma.workspace.render(Some(&gamma.out), from),
                    fname,
                    iname
                ),
            )
        })?;

        coerce(gamma, actual, fty, span)?;
    }

    Ok(())
}

fn infer_lambda(
    c: BodyCtx,
    gamma: &mut Gamma,
    params: IdxRange<BindingInfo>,
    body: Idx<Expr>,
    expected_params: Option<&[TyId]>,
    expected_ret: Option<TyId>,
    expr_span: Span,
) -> Result<TyId, CompilerError> {
    let mut param_tys = Vec::with_capacity(params.len());

    for (i, pb) in params.into_iter().enumerate() {
        let binding = &c.body.binding_arena[pb];

        let ty = match binding.declared_ty {
            Some(ty) => ty,

            None => match expected_params.and_then(|ps| ps.get(i)) {
                Some(&et) => et,
                None => gamma.fresh_callsite_var(binding.name, expr_span),
            },
        };

        gamma.assign_named_type(pb, ty)?;
        param_tys.push(ty)
    }

    // let body_ty = infer_expr(c, gamma, body)?;
    // let body_ty = resolve(gamma, body_ty, expr_span);

    let ret_var = gamma.fresh_callsite_var(*Q_R, expr_span);

    let expects_unit = expected_ret.is_some_and(|r| {
        let r = resolve(gamma, r, expr_span);
        matches!(*gamma.get(r), TypeExpr::Builtin(BuiltinTy::Unit))
    });

    let saved_ret = gamma.declared_ret_ty.replace(ret_var);
    let saved_coro = gamma.coroutine_ret.take();
    // A lambda body runs at CALL time, after arbitrary intervening effects, and a
    // global is read fresh on each call -- so no enclosing GLOBAL refinement holds
    // inside it. Save + clear the global entries for the body, restore after.
    // Locals are captured by the closure, so they keep their refinement.
    let saved_globals: Vec<Narrow> = gamma
        .refined_nonnull
        .iter()
        .filter(|(root, _)| root.is_global())
        .cloned()
        .collect();
    for g in &saved_globals {
        gamma.refined_nonnull.remove(g);
    }
    let body_res = if expects_unit {
        infer_expr_used(c, gamma, body, Use::Discard)
    } else {
        infer_expr(c, gamma, body)
    };
    gamma.refined_nonnull.extend(saved_globals);
    gamma.coroutine_ret = saved_coro;
    gamma.declared_ret_ty = saved_ret;
    let body_ty = body_res?;

    unify(gamma, ret_var, body_ty, expr_span)?;
    let ret_ty = resolve(gamma, ret_var, expr_span);

    let mut texps = Vec::with_capacity(param_tys.len());

    for pt in param_tys.iter() {
        let r = resolve(gamma, *pt, expr_span);
        let r = TyId::Local(reify(gamma, r, expr_span));
        let texp = *gamma.get(r);
        texps.push(texp);
    }

    let params_range = alloc_contiguous(&mut gamma.out.ty_arena, texps);

    let ret_idx = reify(gamma, ret_ty, expr_span);

    let idx = gamma.out.ty_arena.alloc(TypeExpr::Function {
        params: params_range,
        ret: ret_idx,
    });

    Ok(TyId::Local(idx))
}

/// Build a `fn(P..) -> R` TyId from resolved param/ret types, reifying each into
/// `gamma.out.ty_arena` (the same construction `infer_lambda` uses for a lambda's
/// type). Used to type a nested `fn` name binding.
fn make_fn_ty(gamma: &mut Gamma, param_tys: &[TyId], ret: TyId, span: Span) -> TyId {
    let mut texps = Vec::with_capacity(param_tys.len());
    for pt in param_tys {
        let r = resolve(gamma, *pt, span);
        let r = TyId::Local(reify(gamma, r, span));
        texps.push(*gamma.get(r));
    }
    let params_range = alloc_contiguous(&mut gamma.out.ty_arena, texps);
    let r = resolve(gamma, ret, span);
    let ret_idx = reify(gamma, r, span);
    let idx = gamma
        .out
        .ty_arena
        .alloc(TypeExpr::Function { params: params_range, ret: ret_idx });
    TyId::Local(idx)
}

/// True when `ty` is a concrete non-nullable type, so `??` / `?.` on it is
/// redundant (dead). Skips `Nullable` (the meaningful case), unresolved `Infer`,
/// and `Error`/`Never`/`Any` -- don't reject on uncertainty.
fn redundant_optional_on(gamma: &mut Gamma, ty: TyId, span: Span) -> bool {
    let r = resolve(gamma, ty, span);
    !matches!(
        gamma.get(r),
        TypeExpr::Nullable(_)
            | TypeExpr::Infer(..)
            | TypeExpr::Builtin(BuiltinTy::Error)
            | TypeExpr::Builtin(BuiltinTy::Never)
            | TypeExpr::Builtin(BuiltinTy::Any)
    )
}

fn check_expr(
    c: BodyCtx,
    gamma: &mut Gamma,
    e: Idx<Expr>,
    expected: TyId,
    span: Span,
) -> Result<(), CompilerError> {
    if let Expr::Lambda { params, body } = c.body.expr_arena[e] {
        let exp_r = resolve(gamma, expected, span);

        // Lambda literal is a valid value for a NULLABLe function param `(() ->())?`,
        // so look through a nullable wrapper to reach the function type.
        let fn_ty = match *gamma.get(exp_r) {
            TypeExpr::Nullable(inner) => exp_r.with_idx(inner),
            _ => exp_r
        };

        if let TypeExpr::Function { params: ep, ret } = *gamma.get(fn_ty) {
            let expected_params: Vec<TyId> = ep.into_iter().map(|p| fn_ty.with_idx(p)).collect();
            let lam_ty = infer_lambda(
                c,
                gamma,
                params,
                body,
                Some(&expected_params),
                Some(fn_ty.with_idx(ret)),
                c.body.expr_spans[e],
            )?;
            return coerce(gamma, lam_ty, expected, span);
        }
    }

    if let Expr::Array(elements) | Expr::NativeArray(elements) = c.body.expr_arena[e] {
        let exp_r = resolve(gamma, expected, span);
        let has_spread = elements
            .into_iter()
            .any(|el| matches!(c.body.expr_arena[el], Expr::Spread(_)));

        if !has_spread {
            if let TypeExpr::App {
                ctor: TypeCtor::Array,
                params,
            } = *gamma.get(exp_r)
            {
                let elem_ty = exp_r.with_idx(params.into_iter().next().unwrap());

                for el in elements {
                    check_expr(c, gamma, el, elem_ty, c.body.expr_spans[el])?;
                }

                gamma.assign_expr_type(c, e, exp_r)?;
                return Ok(());
            }
        }
    }

    if let Expr::SiblingRef(field) = c.body.expr_arena[e] {
        let exp_r = resolve(gamma, expected, span);

        if let TypeExpr::App {
            ctor: TypeCtor::Nominal(former),
            params,
        } = *gamma.get(exp_r)
        {
            if former.as_str() == "FieldOf" {
                let mut it = params.into_iter();
                let t = exp_r.with_idx(it.next().unwrap());
                let v = exp_r.with_idx(it.next().unwrap());

                let t_r = resolve(gamma, t, span);
                let fty = struct_field_ty(gamma, t_r, field, span)?;

                unify(gamma, fty, v, span)?;

                gamma.assign_expr_type(c, e, exp_r)?;

                return Ok(());
            }
        }
    }

    let aty = infer_expr(c, gamma, e)?;

    // `/` is float division; its `float` result cannot fill an int-shaped hole.
    // When coercion actually rejects a `float` against an `int` (NOT a
    // re-bindable numeric var that coerce would widen, e.g. `min(1, x)`), and
    // the expression is (or contains, through arithmetic operators / unary
    // minus) a `/`, replace the bare unify error with a pointer at that `/`
    // suggesting `./` (integer floor division) (docs/float-division-plan.md).
    if let Err(coerce_err) = coerce(gamma, aty, expected, span) {
        let exp_r = resolve(gamma, expected, span);
        let aty_r = resolve(gamma, aty, span);
        if matches!(gamma.get(exp_r), TypeExpr::Builtin(BuiltinTy::Int))
            && matches!(gamma.get(aty_r), TypeExpr::Builtin(BuiltinTy::Float))
        {
            if let Some(div_span) = find_float_div_span(c, e) {
                return Err(CompilerError::new_with_label(
                    div_span,
                    "`/` is float division; its `float` result does not fit `int` -- use `./` for integer floor division"
                        .to_string(),
                ));
            }
        }
        return Err(coerce_err);
    }

    Ok(())
}

// Find the `/` (float division) responsible for a `float` value flowing into an
// `int` hole. Float-ness only propagates into that hole through arithmetic
// operators (`__op_*`) and unary minus, so the walk descends only those; a
// `float` from anywhere else (a call's return type, a field) is not a misplaced
// `/` and yields None. Returns the offending `/`'s span for the diagnostic.
fn find_float_div_span(c: BodyCtx, e: Idx<Expr>) -> Option<Span> {
    match &c.body.expr_arena[e] {
        Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
        } => {
            let name = *name;
            let args = *args;
            if name == "__op_div" {
                return Some(c.body.expr_spans[e]);
            }
            if name.as_str().starts_with("__op_") {
                for a in args {
                    if let Some(s) = find_float_div_span(c, a) {
                        return Some(s);
                    }
                }
            }
            None
        }
        Expr::UnaryMinus(x) => find_float_div_span(c, *x),
        _ => None,
    }
}

fn coroutine_yrn(
    gamma: &mut Gamma,
    ret: TyId,
    span: Span,
) -> Result<(TyId, TyId, TyId), CompilerError> {
    let r = resolve(gamma, ret, span);

    match *gamma.get(r) {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            params,
        } if n.as_str() == "Coroutine" => {
            let mut it = params.into_iter();
            let (y, rr, nn) = (it.next(), it.next(), it.next());

            match (y, rr, nn) {
                (Some(y), Some(rr), Some(nn)) => {
                    Ok((r.with_idx(y), r.with_idx(rr), r.with_idx(nn)))
                }
                _ => comp_error!(
                    span,
                    "Coroutine type must have 3 parameters (yield, return, next), got {}",
                    params.len()
                ),
            }
        }

        _ => comp_error!(
            span,
            "Expected a Coroutine type, got {}",
            gamma.workspace.render(Some(&gamma.out), r)
        ),
    }
}

/// How a callee parameter slot is filled after `..spread` expansion.
#[derive(Clone, Copy)]
enum SlotFill {
    /// A written positional or named argument.
    Direct(Idx<Expr>),
    /// A field pulled from a spread operand: (operand expr, field type). The
    /// param slot index encodes which field; skull-mir re-derives the name.
    SpreadField(Idx<Expr>, TyId),
}

/// The (name, type) of every field of a call-site spread operand. The operand
/// must be a nominal struct with statically known fields (rule: not tuple, not
/// array, not Map/any).
fn spread_struct_fields(
    gamma: &mut Gamma,
    op_ty: TyId,
    span: Span,
) -> Result<Vec<(Ustr, TyId)>, CompilerError> {
    let r = resolve(gamma, op_ty, span);

    match *gamma.get(r) {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } => {
            let sid = resolve_struct(gamma, name).ok_or_else(|| {
                CompilerError::new_with_label(span, format!("cannot spread `{}`: not a struct", name))
            })?;

            let def = gamma.workspace.module(sid.module).structs[sid.def].clone();
            let subs = struct_arg_subs(gamma, &def, r, params);
            def.fields
                .iter()
                .map(|f| Ok((f.name, substitute(gamma, f.ty, &subs, span)?)))
                .collect()
        }

        _ => comp_error!(
            span,
            "cannot spread `{}` into call arguments: expected a nominal struct",
            gamma.workspace.render(Some(&gamma.out), r)
        ),
    }
}

fn reorder_call_args(
    c: BodyCtx,
    gamma: &mut Gamma,
    args: &[Idx<Expr>],
    param_names: &[Ustr],
    num_required: usize,
    span: Span,
) -> Result<Vec<Option<SlotFill>>, CompilerError> {
    let mut slots: Vec<Option<SlotFill>> = vec![None; param_names.len()];
    let mut pos = 0usize;

    for &arg in args {
        // Struct spread `..op`: each field of the operand fills the param slot
        // of the same name (rules 2/4/5).
        if c.body.spread_args.contains(&arg) {
            let op_ty = infer_expr(c, gamma, arg)?;
            let aspan = c.body.expr_spans[arg];
            for (fname, fty) in spread_struct_fields(gamma, op_ty, aspan)? {
                let pi = param_names.iter().position(|p| *p == fname).ok_or_else(|| {
                    CompilerError::new_with_label(
                        span,
                        format!("spread field `{}` names no parameter of the callee", fname),
                    )
                })?;
                match slots[pi] {
                    // rule 5: two spreads supplying the same param.
                    Some(SlotFill::SpreadField(..)) => {
                        comp_error!(span, "two spreads both supply parameter `{}`", fname)
                    }
                    // rule 4: a named/positional arg written BEFORE the spread.
                    Some(SlotFill::Direct(_)) => comp_error!(
                        span,
                        "parameter `{}` is given before the spread that also supplies it",
                        fname
                    ),
                    None => slots[pi] = Some(SlotFill::SpreadField(arg, fty)),
                }
            }
            continue;
        }

        match c.body.arg_names.get(&arg) {
            // positional
            None => {
                if pos >= param_names.len() {
                    comp_error!(
                        span,
                        "Too many positional arguments, expected at most {}",
                        param_names.len()
                    );
                }

                if slots[pos].is_some() {
                    comp_error!(span, "Multiple values provided for parameter `{}`", param_names[pos]);
                }
                slots[pos] = Some(SlotFill::Direct(arg));
                pos += 1;
            }

            Some(nm) => {
                let pi = param_names.iter().position(|p| p == nm).ok_or_else(|| {
                    CompilerError::new_with_label(span, format!("unknown parameter `{}`", nm))
                })?;

                match slots[pi] {
                    // rule 3: a named arg AFTER a spread overrides that field.
                    Some(SlotFill::SpreadField(..)) | None => {
                        slots[pi] = Some(SlotFill::Direct(arg))
                    }
                    Some(SlotFill::Direct(_)) => {
                        comp_error!(span, "Multiple values provided for parameter `{}`", nm)
                    }
                }
            }
        }
    }

    for i in 0..num_required {
        if slots[i].is_none() {
            comp_error!(
                span,
                "Missing required argument for parameter `{}`",
                param_names[i]
            );
        }
    }

    Ok(slots)
}

fn spread_constituents(
    gamma: &mut Gamma,
    op_ty: TyId,
    span: Span,
) -> Result<Vec<TyId>, CompilerError> {
    let r = resolve(gamma, op_ty, span);

    match *gamma.get(r) {
        TypeExpr::Tuple { elements } => Ok(elements.into_iter().map(|e| r.with_idx(e)).collect()),
        TypeExpr::App {
            ctor: TypeCtor::Array,
            params,
        } => {
            let e = params.into_iter().next().ok_or_else(|| {
                CompilerError::new_with_label(
                    span,
                    "Array type missing element type parameter".to_string(),
                )
            })?;

            Ok(vec![r.with_idx(e)])
        }

        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } => {
            let sid = resolve_struct(gamma, name).ok_or_else(|| {
                CompilerError::new_with_label(span, format!("cannot spread `{}`", name))
            })?;

            let def = gamma.workspace.module(sid.module).structs[sid.def].clone();
            // mirror struct_field_ty; empty for generics
            let subs = struct_arg_subs(gamma, &def, r, params);
            def.fields
                .iter()
                .map(|f| substitute(gamma, f.ty, &subs, span))
                .collect()
        }

        _ => comp_error!(
            span,
            "Cannot spread type {}, expected a tuple, array, or nominal struct",
            gamma.workspace.render(Some(&gamma.out), r)
        ),
    }
}

fn resolve_global(gamma: &Gamma, name: Ustr) -> Option<TyId> {
    gamma.dep_acc.borrow_mut().insert(name);
    gamma.workspace.globals_by_name.get(&name).copied()
}

fn resolve_newtype(gamma: &Gamma, name: Ustr) -> Option<NewtypeId> {
    gamma.dep_acc.borrow_mut().insert(name);
    gamma.workspace.newtypes_by_name.get(&name).copied()
}

fn resolve_named_call(
    gamma: &mut Gamma,
    c: BodyCtx,
    name: Ustr,
    recv_override: Option<TyId>,
    args: IdxRange<Expr>,
    call_expr: Idx<Expr>,
    span: Span,
) -> Result<TyId, CompilerError> {
    let res = resolve_named_call_inner(gamma, c, name, recv_override, args, call_expr, span);

    let Err(e) = res else { return res };
    if gamma.quiet {
        return Err(e);
    }

    // `recv.name(...)` that failed to resolve: if `recv` has a field `name`
    // that isn't callable, the `()` is almost certainly a slip (a field read
    // written as a call). Gate on the failure being about the receiver or the
    // call itself, so a later-arg mismatch under a legit UFCS call keeps its
    // real error.
    let (recv_ty, recv_span) = match recv_override {
        Some(r) => (resolve(gamma, r, span), None),
        None if !args.is_empty() && c.body.method_call_recvs.contains(&args.start()) => {
            match infer_expr(c, gamma, args.start()) {
                Ok(t) => (resolve(gamma, t, span), Some(c.body.expr_spans[args.start()])),
                Err(_) => return Err(e),
            }
        }
        None => return Err(e),
    };

    let err_span = e.labels.first().map(|l| l.span);
    let about_receiver = err_span == Some(span) || (recv_span.is_some() && err_span == recv_span);
    if about_receiver {
        if let Ok(fty) = struct_field_ty(gamma, recv_ty, name, span) {
            let fty = resolve(gamma, fty, span);
            if !matches!(gamma.get(fty), TypeExpr::Function { .. }) {
                let recv_str = gamma.workspace.render(Some(&gamma.out), recv_ty);
                let fty_str = gamma.workspace.render(Some(&gamma.out), fty);
                return Err(CompilerError::new_with_label(
                    span,
                    format!(
                        "`{name}` is a field of `{recv_str}` (type `{fty_str}`), not a method -- remove the `()` to read the field"
                    ),
                ));
            }
        }
    }

    Err(e)
}

fn resolve_named_call_inner(
    gamma: &mut Gamma,
    c: BodyCtx,
    name: Ustr,
    recv_override: Option<TyId>,
    args: IdxRange<Expr>,
    call_expr: Idx<Expr>,
    span: Span,
) -> Result<TyId, CompilerError> {
    let _span = tracing::info_span!("named_call", name = %name).entered();
    let candidates = resolve_callee(gamma, name);

    // Method-syntax `recv.name(args)` / `recv?.name(args)`:
    // - a fn-typed FIELD on the receiver wins over free fns (UFCS is only a fallback)
    // - recv_override is Some exactly for SafeNavCall
    // - plain method path carries receiver as args[0]
    let method_recv: Option<TyId> = match recv_override {
        Some(r) => Some(resolve(gamma, r, span)),

        None if !args.is_empty() && c.body.method_call_recvs.contains(&args.start()) => {
            let recv_ty = infer_expr(c, gamma, args.start())?;
            Some(resolve(gamma, recv_ty, span))
        }

        None => None,
    };

    if let Some(recv_ty) = method_recv {
        if let Ok(field_ty) = struct_field_ty(gamma, recv_ty, name, span) {
            let field_ty = resolve(gamma, field_ty, span);

            if matches!(gamma.get(field_ty), TypeExpr::Function { .. }) {
                let call_args = if recv_override.is_some() {
                    // safe-nav: receiver isn't in args, pass them as is
                    args
                } else {
                    // plain method call ... drop the receiver
                    let r = args.make_range();

                    IdxRange::new_exclusive(
                        Idx::from_raw(RawIdx::from_u32(r.start + 1))
                            ..Idx::from_raw(RawIdx::from_u32(r.end)),
                    )
                };

                return call_fn_value(c, gamma, field_ty, call_args, span);
            }
        }
    }

    // Bound-calls: `next(x)` / `iter(x)` (either spelling incl. UFCS) where
    // `x: I` and the enclosing fn declares `I: Iterator<E>`. There is no
    // concrete callee at check time -- the body is checked ONCE against the
    // bound; skull lowers these as placeholders and the instantiation-time
    // BoundTree substitutes the concrete `next` per splice. `iter(x)` is
    // IDENTITY (types as `I`, lowers to the argument, no call).
    if !gamma.fn_bounds.is_empty()
        && recv_override.is_none()
        && args.len() == 1
        && matches!(name.as_str(), "next" | "iter")
    {
        let aty = infer_expr(c, gamma, args.start())?;
        let aty = resolve(gamma, aty, span);
        if let TypeExpr::Generic(var_id, _) = *gamma.get(aty) {
            if let Some((idx, elem)) = gamma.fn_bound_for(var_id) {
                if name.as_str() == "iter" {
                    gamma.out.bound_identity_calls.insert(call_expr);
                    return Ok(aty);
                }
                gamma.out.bound_calls.insert(call_expr, idx);
                let b = gamma.workspace.builtins.bool();
                return Ok(tuple_of(gamma, &[b, elem], span));
            }
        }
    }

    if candidates.is_empty() {
        if recv_override.is_none() {
            if let Some(gty) = resolve_global(gamma, name) {
                let gty = resolve(gamma, gty, span);

                if matches!(gamma.get(gty), TypeExpr::Function { .. }) {
                    return call_fn_value(c, gamma, gty, args, span);
                }
            }
        }

        if recv_override.is_none() {
            if let Some(NewtypeId { module: ntm, def }) = resolve_newtype(gamma, name) {
                let nt = gamma.workspace.module(ntm).newtypes[def].clone();
                let (subs, fresh) = gamma.instantiate_generics(&nt.generics, span)?;

                if args.len() != 1 {
                    comp_error!(
                        span,
                        "newtype `{}` constructor expects exactly 1 argument, got {}",
                        name,
                        args.len()
                    );
                }

                // TODO: do tests still pass without this?
                for arg in args.into_iter() {
                    let aspan = c.body.expr_spans[arg];
                    let aty = infer_expr(c, gamma, arg)?;
                    let w_sub = substitute(gamma, nt.wrapped, &subs, aspan)?;

                    coerce(gamma, aty, w_sub, aspan)?;
                }

                let arg_tys: Vec<TyId> = fresh.iter().map(|&f| resolve(gamma, f, span)).collect();
                let result = gamma.nominal_app(name, &arg_tys, span);

                return Ok(resolve(gamma, result, span));
            }
        }

        comp_error!(span, "Function `{}` doesn't exist", name);
    }

    // let (callee_mid, callee_fn) = resolve_callee(gamma, name).ok_or_else(|| {
    //     CompilerError::new_with_label(
    //         span,
    //         format!("Function `{}` doens't exist", name),
    //     )
    // })?;

    let mut preinferred: FxHashMap<Idx<Expr>, TyId> = FxHashMap::default();

    let (callee_mid, callee_fn) = if candidates.len() == 1 {
        candidates[0]
    } else {
        let mut arg_tys: Vec<TyId> = Vec::new();
        let mut arg_ty: FxHashMap<Idx<Expr>, TyId> = FxHashMap::default();
        let mut arg_list: Vec<Idx<Expr>> = Vec::new();

        if let Some(r) = recv_override {
            arg_tys.push(resolve(gamma, r, span));
        }

        for a in args {
            let t = if matches!(c.body.expr_arena[a], Expr::Lambda { .. }) {
                gamma.fresh_callsite_var(*Q_LAM, span)
            } else {
                let t = infer_expr(c, gamma, a)?;
                let t = resolve(gamma, t, span);
                preinferred.insert(a, t);
                t
                // Ok::<_, CompilerError>(t)
            };

            arg_tys.push(t);
            arg_ty.insert(a, t);
            arg_list.push(a);
        }

        match select_overload(
            c,
            gamma,
            &candidates,
            recv_override,
            &arg_list,
            &arg_ty,
            &arg_tys,
            name,
            span,
        ) {
            Ok(pick) => pick,

            Err(e) => {
                if name.as_str().starts_with("__op_") {
                    if let Some(&first) = arg_tys.first() {
                        if arg_tys.iter().all(|&t| {
                            matches!(*gamma.get(t), TypeExpr::Generic(..) | TypeExpr::Infer(..))
                        }) {
                            return Ok(first);
                        }
                    }
                }

                return Err(e);
            }
        }
    };

    gamma
        .out
        .record_resolved_call(call_expr, callee_mid, callee_fn, span)?;

    let ws = gamma.workspace;
    let function = &ws.module(callee_mid).functions[callee_fn];
    let (subs, _arg_tys) = gamma.instantiate_generics(&function.generics, span)?;

    // Caller, for arg exprs
    let callee_module = &ws.module(callee_mid);
    let function = &callee_module.functions[callee_fn];

    // `Iterator`-bounded callee: conformance is (re)tried after every arg
    // unification -- resolving `next(C)` is also what binds the bound's elem
    // generic, and a LATER lambda arg (`map(it, x => ..)`) must see it bound.
    let has_bounds = function.generic_bounds.iter().any(Option::is_some);
    let mut bound_tree = BoundTree::default();
    let mut bounds_done: FxHashSet<usize> = FxHashSet::default();

    let mut param_tys = Vec::with_capacity(c.body.param_bindings.len());
    let mut param_names = Vec::with_capacity(c.body.param_bindings.len());

    for param in function.param_decls.iter() {
        param_tys.push(param.ty);
        param_names.push(param.name);
    }

    // let what = format!("function `{}`", name);
    // check_positional_args(
    //     module,
    //     gamma,
    //     &param_tys,
    //     args,
    //     function.num_required,
    //     &subs,
    //     span,
    //     &what,
    // )?;

    let offset = if let Some(r) = recv_override {
        if param_tys.is_empty() {
            comp_error!(span, "method `{}` has no receiver parameter", name);
        }

        let pty = substitute(gamma, param_tys[0], &subs, span)?;
        coerce(gamma, r, pty, span)?;

        1
    } else {
        0
    };

    let arg_list: Vec<Idx<Expr>> = args.into_iter().collect();

    let pairs: Vec<(SlotFill, TyId)> = if function.is_variadic {
        let velem = param_tys[param_tys.len() - 1];
        let nfixed = (param_tys.len() - 1).saturating_sub(offset);

        if arg_list.len() < nfixed {
            comp_error!(
                span,
                "`{}` expects at least {} argument(s), got {}",
                name,
                nfixed,
                arg_list.len()
            );
        }

        arg_list
            .iter()
            .enumerate()
            .map(|(i, &arg)| {
                (
                    SlotFill::Direct(arg),
                    if i < nfixed {
                        param_tys[offset + 1]
                    } else {
                        velem
                    },
                )
            })
            .collect()
    } else {
        let slots = reorder_call_args(
            c,
            gamma,
            &arg_list,
            &param_names[offset..],
            function.num_required.saturating_sub(offset),
            span,
        )?;

        slots
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.map(|fill| (fill, param_tys[offset + i])))
            .collect()
    };

    for (fill, pty) in pairs {
        if has_bounds {
            check_iterator_bounds(
                c, gamma, function, &subs, &mut bound_tree, &mut bounds_done, false, 0, span,
            )?;
        }
        let pty = substitute(gamma, pty, &subs, span)?;
        match fill {
            SlotFill::Direct(arg) => match preinferred.get(&arg) {
                Some(&aty) => coerce(gamma, aty, pty, c.body.expr_spans[arg])?,
                None => check_expr(c, gamma, arg, pty, c.body.expr_spans[arg])?,
            },
            // A spread-supplied field: check the field's type against the param.
            SlotFill::SpreadField(op, fty) => {
                coerce(gamma, fty, pty, c.body.expr_spans[op])?
            }
        }
    }

    if has_bounds {
        check_iterator_bounds(
            c, gamma, function, &subs, &mut bound_tree, &mut bounds_done, true, 0, span,
        )?;
        gamma.out.bound_call_trees.insert(call_expr, bound_tree);
    }

    // A non-`@pure` callee may reassign a global, so its call invalidates every
    // global refinement. Args + receiver above were checked while the
    // refinements still held (the call's effect is sequenced after them); a
    // `@pure` callee provably can't write globals (verify_pure_expr), so it
    // preserves them. Done here, after arg checking, so nested impure calls in
    // earlier args have already cleared in evaluation order.
    let callee_pure = fn_has_anno(
        &gamma.workspace.module(callee_mid).functions[callee_fn],
        "pure",
    );

    let ret_ty = function
        .return_ty
        .unwrap_or_else(|| gamma.workspace.builtins.unit());

    if !callee_pure {
        gamma.refined_nonnull.retain(|(root, _)| !root.is_global());
    }

    let ret_ty = substitute(gamma, ret_ty, &subs, span)?;

    Ok(resolve(gamma, ret_ty, span))
}

fn call_fn_value(
    c: BodyCtx,
    gamma: &mut Gamma,
    fn_ty: TyId,
    args: IdxRange<Expr>,
    span: Span,
) -> Result<TyId, CompilerError> {
    let fn_ty = resolve(gamma, fn_ty, span);

    let (params, ret) = match *gamma.get(fn_ty) {
        TypeExpr::Function { params, ret } => (params, ret),
        _ => comp_error!(
            span,
            "Attempted to call a non-function type {}",
            gamma.workspace.render(Some(&gamma.out), fn_ty)
        ),
    };

    let param_tys: Vec<TyId> = params.into_iter().map(|p| fn_ty.with_idx(p)).collect();
    let what = format!(
        "call to value of type {}",
        gamma.workspace.render(Some(&gamma.out), fn_ty)
    );
    check_args(c, gamma, &param_tys, args, param_tys.len(), span, &what)?;

    // A fn-value callee is opaque -- we can't prove it doesn't write globals, so
    // conservatively drop all global refinements (args above were checked while
    // they still held).
    gamma.refined_nonnull.retain(|(root, _)| !root.is_global());

    Ok(resolve(gamma, fn_ty.with_idx(ret), span))
}

fn bound_var(gamma: &Gamma, ty: TyId) -> Option<VarId> {
    match *gamma.get(ty) {
        TypeExpr::Infer(var_id, _) => match gamma.out.substitutions.get(var_id) {
            Some(next) => bound_var(gamma, next).or(Some(var_id)),
            None => None,
        },

        _ => None,
    }
}

fn destructure_pattern(
    gamma: &mut Gamma,
    pat: &BindingPattern,
    ty: TyId,
    span: Span,
) -> Result<(), CompilerError> {
    // A poisoned scrutinee has no tuple/nominal shape.
    // Bind every slot to `error` so a failed RHS doesn't force a shape error
    // or leave vars unbound
    let r = resolve(gamma, ty, span);
    if matches!(gamma.get(r), TypeExpr::Builtin(BuiltinTy::Error)) {
        return match pat {
            BindingPattern::Leaf(b) => gamma.assign_named_type(*b, r),
            BindingPattern::Tuple(subs) => {
                for sub in subs {
                    destructure_pattern(gamma, sub, r, span)?;
                }

                Ok(())
            }
        };
    }

    match pat {
        BindingPattern::Leaf(b) => gamma.assign_named_type(*b, ty),

        BindingPattern::Tuple(subs) => {
            let resolved = resolve(gamma, ty, span);

            match *gamma.get(resolved) {
                TypeExpr::Tuple { elements } => {
                    let es: Vec<_> = elements.into_iter().collect();
                    if es.len() != subs.len() {
                        comp_error!(
                            span,
                            "tuple binding count mismatch: expected {}, got {}",
                            subs.len(),
                            es.len()
                        );
                    }

                    for (sub, e) in subs.iter().zip(es) {
                        let et = resolve(gamma, resolved.with_idx(e), span);
                        // recurse
                        destructure_pattern(gamma, sub, et, span)?;
                    }

                    Ok(())
                }

                _ => comp_error!(
                    span,
                    "expected tuple type, got {}",
                    gamma.workspace.render(Some(&gamma.out), resolved)
                ),
            }
        }
    }
}

fn pattern_binds(c: BodyCtx, pat: Idx<Pattern>) -> bool {
    match c.body.pattern_arena[pat] {
        Pattern::Bind(_) => true,
        Pattern::EnumVariant { sub, .. } => sub.into_iter().any(|p| pattern_binds(c, p)),
        Pattern::Record { fields, .. } => fields
            .into_iter()
            .any(|f| pattern_binds(c, c.body.record_pat_field_arena[f].pat)),

        Pattern::Or { alternatives } => alternatives.into_iter().any(|p| pattern_binds(c, p)),
        Pattern::Tuple { elements } => elements.into_iter().any(|p| pattern_binds(c, p)),

        _ => false,
    }
}

fn for_elem_ty(gamma: &mut Gamma, iter_ty: TyId, span: Span) -> Result<TyId, CompilerError> {
    Ok(match *gamma.get(iter_ty) {
        // [T]
        TypeExpr::App {
            ctor: TypeCtor::Array,
            params,
        } => {
            let elem = or_fail!(
                params.into_iter().next(),
                span,
                "array type must have one parameter"
            )?;
            resolve(gamma, iter_ty.with_idx(elem), span)
        }

        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } if matches!(name.as_str(), "NativeArray" | "NativeBuffer") => {
            let elem = or_fail!(
                params.into_iter().next(),
                span,
                "array type must have one parameter"
            )?;
            resolve(gamma, iter_ty.with_idx(elem), span)
        }

        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } if matches!(
            name.as_str(),
            "NativeBufferU8"
                | "NativeBufferI8"
                | "NativeBufferU16"
                | "NativeBufferI16"
                | "NativeBufferU32"
                | "NativeBufferI32"
        ) =>
        {
            gamma.workspace.builtins.int()
        }

        TypeExpr::App {
            ctor: TypeCtor::Map,
            params,
        } => {
            let mut it = params.into_iter();
            let k = or_fail!(it.next(), span, "map type must have two parameters")?;
            let v = or_fail!(it.next(), span, "map type must have two parameters")?;

            let kr = resolve(gamma, iter_ty.with_idx(k), span);
            let vr = resolve(gamma, iter_ty.with_idx(v), span);

            tuple_of(gamma, &[kr, vr], span)
        }

        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } if name.as_str() == "Arena" => {
            // Arena<T> yields (Entity<T>, T)
            let elem = or_fail!(
                params.into_iter().next(),
                span,
                "Arena type must have one parameter"
            )?;

            let elem = resolve(gamma, iter_ty.with_idx(elem), span);
            let entity = gamma.nominal_app(*ENTITY, &[elem], span);

            tuple_of(gamma, &[entity, elem], span)
        }

        _ => comp_error!(span, "type is not iterable: {}", gamma.render(iter_ty)),
    })
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

    let (subs, _arg_tys) = gamma.instantiate_generics(&def.generics, def.span)?;

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

    let errors = std::mem::take(&mut gamma.body_errors);

    Ok(Some((BodyOwner::StructDefaults(sidx), gamma.out, errors)))
}

fn bind_pattern_leaves_to(
    c: BodyCtx,
    gamma: &mut Gamma,
    pat: Idx<Pattern>,
    ty: TyId,
    _span: Span,
) -> Result<(), CompilerError> {
    match c.body.pattern_arena[pat] {
        Pattern::Bind(b) => gamma.assign_named_type(b, ty)?,

        Pattern::EnumVariant { sub, .. } => {
            for p in sub {
                bind_pattern_leaves_to(c, gamma, p, ty, _span)?;
            }
        }

        Pattern::Record { fields, .. } => {
            for f in fields {
                bind_pattern_leaves_to(c, gamma, c.body.record_pat_field_arena[f].pat, ty, _span)?;
            }
        }

        Pattern::Tuple { elements } => {
            for p in elements {
                bind_pattern_leaves_to(c, gamma, p, ty, _span)?;
            }
        }

        Pattern::Or { alternatives } => {
            for p in alternatives {
                bind_pattern_leaves_to(c, gamma, p, ty, _span)?;
            }
        }

        Pattern::Wildcard | Pattern::Literal(..) | Pattern::Range { .. } => {}
    }

    Ok(())
}

/// Whether `enumerate(coll)` in for-position keeps its builtin template
/// lowering (the types `for_elem_ty` knows). Everything else routes through
/// the stdlib `EnumIter` combinator.
fn builtin_enumerable(gamma: &Gamma, ty: TyId) -> bool {
    match *gamma.get(ty) {
        TypeExpr::App { ctor: TypeCtor::Array, .. } | TypeExpr::App { ctor: TypeCtor::Map, .. } => {
            true
        }
        TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => matches!(
            n.as_str(),
            "Arena"
                | "NativeArray"
                | "NativeBuffer"
                | "NativeBufferU8"
                | "NativeBufferI8"
                | "NativeBufferU16"
                | "NativeBufferI16"
                | "NativeBufferU32"
                | "NativeBufferI32"
        ),
        _ => false,
    }
}

fn infer_for_elem(
    c: BodyCtx,
    gamma: &mut Gamma,
    iter: Idx<Expr>,
    span: Span,
) -> Result<TyId, CompilerError> {
    match c.body.expr_arena[iter] {
        Expr::Range { low, high } => {
            let low_ty = infer_expr(c, gamma, low)?;
            unify(gamma, low_ty, gamma.workspace.builtins.int(), span)?;

            let high_ty = infer_expr(c, gamma, high)?;
            unify(gamma, high_ty, gamma.workspace.builtins.int(), span)?;

            Ok(gamma.workspace.builtins.int())
        }

        // Builtin-collection `enumerate(coll)` keeps its template lowering
        // (Map has no protocol iterator; the counted paths are co-fn safe).
        // `enumerate` over anything else -- an iterator chain -- falls through
        // to the default arm: the call resolves to the stdlib `EnumIter`
        // combinator and the result iterates via the protocol.
        Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
        } if name.as_str() == "enumerate"
            && args.len() == 1
            && {
                let coll_ty = infer_expr(c, gamma, args.start())?;
                let coll_ty = resolve(gamma, coll_ty, span);
                builtin_enumerable(gamma, coll_ty)
            } =>
        {
            let coll_ty = infer_expr(c, gamma, args.start())?;
            let coll_ty = resolve(gamma, coll_ty, span);
            let elem = for_elem_ty(gamma, coll_ty, span)?;

            let int_ty = gamma.workspace.builtins.int();

            Ok(tuple_of(gamma, &[int_ty, elem], span))
        }

        _ => {
            let iter_ty = infer_expr(c, gamma, iter)?;
            let iter_ty = resolve(gamma, iter_ty, span);

            // `[T]` routes through the stdlib iterator protocol (`VecIter`):
            // the desugar + @inline + SROA + FORL recovery emit the same
            // native numeric `for` the deleted skull template did. `Arena<T>`
            // likewise routes through `ArenaIter` (fetch-outside skip shape).
            // `for_elem_ty` keeps its Array/Arena arms -- `enumerate(..)`
            // above still calls it directly for the element type.
            let is_protocol_builtin = match *gamma.get(iter_ty) {
                TypeExpr::App { ctor: TypeCtor::Array, .. } => true,
                TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => n.as_str() == "Arena",
                _ => false,
            };
            if is_protocol_builtin {
                return resolve_for_iter_protocol(c, gamma, iter, iter_ty, span);
            }

            match for_elem_ty(gamma, iter_ty, span) {
                Ok(elem) => Ok(elem),
                // Not a builtin collection: try the user iterator protocol
                // `iter(coll) -> It`, `next(It) -> (bool, E)`. The recorded
                // (iter_fn, next_fn) drive skull's for-loop desugar.
                Err(_not_builtin) => resolve_for_iter_protocol(c, gamma, iter, iter_ty, span),
            }
        }
    }
}

/// `for x in coll` where `coll` is not a builtin collection: resolve the user
/// iterator protocol and record it keyed by the For's `iter` expr (for skull's
/// desugar), returning the element type E. E is a single type for
/// `next -> (bool, E)`, or a tuple for `next -> (bool, A, B, ...)` so
/// `for (a, b) in coll` destructures.
///
/// Resolution order:
/// - a bounded generic (`coll: I`, `I: Iterator<E>`) iterates as itself with a
///   placeholder `next` (resolved per splice at concrete roots);
/// - `iter(coll) -> It` when it resolves;
/// - otherwise IDENTITY: a next-able type iterates as itself (bare combinator
///   chains -- `for y in v.iter().map(f)` -- need no blanket `iter<I>`).
fn resolve_for_iter_protocol(
    c: BodyCtx,
    gamma: &mut Gamma,
    iter_expr: Idx<Expr>,
    coll_ty: TyId,
    span: Span,
) -> Result<TyId, CompilerError> {
    if let TypeExpr::Generic(var_id, gname) = *gamma.get(coll_ty) {
        let Some((idx, elem)) = gamma.fn_bound_for(var_id) else {
            comp_error!(
                span,
                "`{}` is a generic type with no `Iterator` bound, so it cannot be iterated; declare it `{}: Iterator<T>`",
                gname,
                gname
            );
        };
        gamma.out.for_iter_protocol.insert(
            iter_expr,
            ForIterInfo { iter_fn: None, next: BoundNext::CallerGeneric(idx) },
        );
        return Ok(elem);
    }

    let (iter_fn, it_ty) =
        match resolve_unary_by_recv(c, gamma, Ustr::from("iter"), coll_ty, span) {
            Ok((f, ret, _subs)) => {
                let (fm, ff) = f;
                if gamma.workspace.module(fm).functions[ff]
                    .generic_bounds
                    .iter()
                    .any(Option::is_some)
                {
                    comp_error!(
                        span,
                        "`iter({})` resolves to a fn with `Iterator` bounds; bounded `iter` fns are not supported",
                        gamma.render(coll_ty)
                    );
                }
                (Some(f), ret)
            }
            // Identity: the collection IS the iterator if `next` resolves on it.
            Err(_) => (None, coll_ty),
        };

    let (next_fn, next_ret, next_subs) =
        match resolve_unary_by_recv(c, gamma, Ustr::from("next"), it_ty, span) {
            Ok(x) => x,
            Err(_) if iter_fn.is_some() => comp_error!(
                span,
                "`iter` yields `{}`, but there is no `next({}) -> (bool, E)` function to drive the loop",
                gamma.render(it_ty),
                gamma.render(it_ty)
            ),
            Err(_) => comp_error!(
                span,
                "type `{}` is not iterable: not a builtin collection, and neither `iter({})` nor `next({}) -> (bool, E)` is in scope",
                gamma.render(coll_ty),
                gamma.render(coll_ty),
                gamma.render(coll_ty)
            ),
        };

    let elem = for_iter_next_elem(gamma, next_ret, span)?;

    // A bounded `next` (combinator chain) resolves its inner bounds HERE -- the
    // for-loop is the mono root; the tree drives skull's placeholder
    // substitution when the chain inlines.
    let next_callee = gamma.workspace.module(next_fn.0).functions[next_fn.1].clone();
    let mut sub = BoundTree::default();
    if next_callee.generic_bounds.iter().any(Option::is_some) {
        let mut done = FxHashSet::default();
        check_iterator_bounds(
            c, gamma, &next_callee, &next_subs, &mut sub, &mut done, true, 0, span,
        )?;
    }

    gamma.out.for_iter_protocol.insert(
        iter_expr,
        ForIterInfo { iter_fn, next: BoundNext::Concrete { func: next_fn, sub } },
    );
    Ok(elem)
}

/// Check the `Iterator` bounds of `function` at an instantiation site and
/// build the resolution tree: for each `I: Iterator<E>` with instantiation
/// `C`, conformance is STRUCTURAL -- `next(C) -> (bool, E')` must resolve and
/// `E'` unifies with `E` (which is HOW `E` gets inferred from `C`). The
/// resolved `next` may itself be bounded (nested chains) -- recurse with its
/// own substitution. An instantiation that is the enclosing fn's bounded
/// generic records a `CallerGeneric` leaf (bound propagation).
///
/// `done` tracks generics already resolved: the call-site loop retries after
/// each argument unification so a later lambda argument sees `E` already
/// bound. `finalize` errors on bounds still unresolvable.
#[allow(clippy::too_many_arguments)]
fn check_iterator_bounds(
    c: BodyCtx,
    gamma: &mut Gamma,
    function: &Function,
    subs: &FxHashMap<VarId, TyId>,
    tree: &mut BoundTree,
    done: &mut FxHashSet<usize>,
    finalize: bool,
    depth: usize,
    span: Span,
) -> Result<(), CompilerError> {
    if depth > 64 {
        comp_error!(span, "`Iterator` bound resolution exceeded depth 64 (recursive iterator type?)");
    }

    for (idx, b) in function.generic_bounds.iter().enumerate() {
        let Some(elem_decl) = *b else { continue };
        if done.contains(&idx) {
            continue;
        }

        let inst = substitute(gamma, function.generics[idx], subs, span)?;
        let inst = resolve(gamma, inst, span);

        match *gamma.get(inst) {
            TypeExpr::Infer(..) => {
                if finalize {
                    comp_error!(
                        span,
                        "cannot resolve the `Iterator` bound on `{}`: the iterator type is not determined by the arguments",
                        function.name
                    );
                }
                // Not yet determined -- the caller retries after more args unify.
            }

            TypeExpr::Generic(var_id, gname) => {
                let Some((caller_idx, caller_elem)) = gamma.fn_bound_for(var_id) else {
                    comp_error!(
                        span,
                        "`{}` does not satisfy `Iterator<{}>`: it is a generic type with no `Iterator` bound",
                        gname,
                        gamma.render(elem_decl)
                    );
                };
                let elem_expected = substitute(gamma, elem_decl, subs, span)?;
                unify(gamma, caller_elem, elem_expected, span)?;
                tree.next.insert(idx, BoundNext::CallerGeneric(caller_idx));
                done.insert(idx);
            }

            _ => {
                let (next_fn, next_ret, next_subs) =
                    match resolve_unary_by_recv(c, gamma, Ustr::from("next"), inst, span) {
                        Ok(x) => x,
                        Err(_) => comp_error!(
                            span,
                            "`{}` does not satisfy `Iterator<{}>`: no `next({}) -> (bool, E)` function in scope",
                            gamma.render(inst),
                            gamma.render(elem_decl),
                            gamma.render(inst)
                        ),
                    };
                let elem_actual = bound_next_elem(gamma, next_ret, inst, span)?;
                let elem_expected = substitute(gamma, elem_decl, subs, span)?;
                if unify(gamma, elem_actual, elem_expected, span).is_err() {
                    comp_error!(
                        span,
                        "`{}` satisfies `Iterator<{}>`, but the bound requires `Iterator<{}>`",
                        gamma.render(inst),
                        gamma.render(elem_actual),
                        gamma.render(elem_expected)
                    );
                }

                let next_callee =
                    gamma.workspace.module(next_fn.0).functions[next_fn.1].clone();
                let mut sub = BoundTree::default();
                if next_callee.generic_bounds.iter().any(Option::is_some) {
                    let mut sub_done = FxHashSet::default();
                    check_iterator_bounds(
                        c, gamma, &next_callee, &next_subs, &mut sub, &mut sub_done,
                        true, depth + 1, span,
                    )?;
                }
                tree.next.insert(idx, BoundNext::Concrete { func: next_fn, sub });
                done.insert(idx);
            }
        }
    }

    Ok(())
}

/// Element type for BOUND conformance: `next` must return exactly `(bool, E)`.
/// Multi-value iterators (`(bool, A, B)` -- ArenaIter's entity/data pair,
/// enumerate/zip outputs) can be CONSUMED by a `for` loop but not re-wrapped
/// by a combinator: a generic body cannot destructure an unknown-arity splat.
fn bound_next_elem(
    gamma: &mut Gamma,
    next_ret: TyId,
    inst: TyId,
    span: Span,
) -> Result<TyId, CompilerError> {
    let next_ret = resolve(gamma, next_ret, span);
    let TypeExpr::Tuple { elements } = *gamma.get(next_ret) else {
        comp_error!(
            span,
            "`{}` does not satisfy `Iterator<..>`: its `next` returns `{}`, not `(bool, E)`",
            gamma.render(inst),
            gamma.render(next_ret)
        );
    };
    let elems: Vec<_> = elements.into_iter().collect();
    if elems.len() != 2 {
        comp_error!(
            span,
            "`{}` does not satisfy `Iterator<..>`: its `next` returns `{}`; a bound requires exactly `(bool, E)` -- multi-value iterators can drive a `for` loop but cannot be wrapped by combinators",
            gamma.render(inst),
            gamma.render(next_ret)
        );
    }
    let first = resolve(gamma, next_ret.with_idx(elems[0]), span);
    if !matches!(gamma.get(first), TypeExpr::Builtin(BuiltinTy::Bool)) {
        comp_error!(
            span,
            "`{}` does not satisfy `Iterator<..>`: `next`'s first return must be `bool` (the done flag), got `{}`",
            gamma.render(inst),
            gamma.render(first)
        );
    }
    Ok(resolve(gamma, next_ret.with_idx(elems[1]), span))
}

/// Resolve a unary call `name(recv)` by the receiver's TYPE alone (no arg expr) --
/// the iterator protocol calls `iter(coll)` / `next(it)` on synthesized values.
/// Reuses `select_overload` via the `recv_ty` slot, then instantiates generics
/// and returns the resolved `(callee, return_ty, generic subs)`. Does NOT
/// record into `resolved_overload_calls` (the caller stores it in
/// `for_iter_protocol` / the bound tree).
fn resolve_unary_by_recv(
    c: BodyCtx,
    gamma: &mut Gamma,
    name: Ustr,
    recv_ty: TyId,
    span: Span,
) -> Result<((ModuleId, Idx<Function>), TyId, FxHashMap<VarId, TyId>), CompilerError> {
    let candidates = resolve_callee(gamma, name);
    if candidates.is_empty() {
        comp_error!(span, "no `{}` function in scope", name);
    }
    let recv_ty = resolve(gamma, recv_ty, span);

    let (mid, fid) = if candidates.len() == 1 {
        candidates[0]
    } else {
        select_overload(
            c,
            gamma,
            &candidates,
            Some(recv_ty),
            &[],
            &FxHashMap::default(),
            &[recv_ty],
            name,
            span,
        )?
    };

    let function = &gamma.workspace.module(mid).functions[fid];
    let generics = function.generics.clone();
    let n_params = function.param_decls.len();
    let num_required = function.num_required;
    let param0 = function.param_decls.first().map(|p| p.ty);
    let ret = function.return_ty;

    // The protocol is unary: exactly one required receiver parameter.
    if n_params != 1 || num_required != 1 {
        comp_error!(span, "`{}` must take exactly one argument to drive a `for` loop", name);
    }
    let Some(p0) = param0 else {
        comp_error!(span, "`{}` has no receiver parameter", name);
    };

    let (subs, _) = gamma.instantiate_generics(&generics, span)?;
    let p0_sub = substitute(gamma, p0, &subs, span)?;
    coerce(gamma, recv_ty, p0_sub, span)?;

    let ret = ret.unwrap_or_else(|| gamma.workspace.builtins.unit());
    let ret_sub = substitute(gamma, ret, &subs, span)?;
    let ret_res = resolve(gamma, ret_sub, span);
    Ok(((mid, fid), ret_res, subs))
}

/// Element type of a `next() -> (bool, E)` return: the tuple tail. `(bool, E)`
/// yields `E`; `(bool, A, B, ...)` yields the tuple `(A, B, ...)`. The first
/// element must be `bool` (the done flag) and there must be at least one element.
fn for_iter_next_elem(gamma: &mut Gamma, next_ret: TyId, span: Span) -> Result<TyId, CompilerError> {
    let next_ret = resolve(gamma, next_ret, span);
    let TypeExpr::Tuple { elements } = *gamma.get(next_ret) else {
        comp_error!(span, "`next` must return `(bool, E)`, got `{}`", gamma.render(next_ret));
    };
    let elems: Vec<_> = elements.into_iter().collect();
    if elems.len() < 2 {
        comp_error!(
            span,
            "`next` must return `(bool, E)` (a done-flag plus at least one element), got `{}`",
            gamma.render(next_ret)
        );
    }

    let first = resolve(gamma, next_ret.with_idx(elems[0]), span);
    if !matches!(gamma.get(first), TypeExpr::Builtin(BuiltinTy::Bool)) {
        comp_error!(
            span,
            "`next`'s first return must be `bool` (the done flag), got `{}`",
            gamma.render(first)
        );
    }

    if elems.len() == 2 {
        Ok(resolve(gamma, next_ret.with_idx(elems[1]), span))
    } else {
        let tail: Vec<TyId> = elems[1..]
            .iter()
            .map(|&e| resolve(gamma, next_ret.with_idx(e), span))
            .collect();
        Ok(tuple_of(gamma, &tail, span))
    }
}

pub fn instantiate_param(gamma: &mut Gamma, generics: &[TyId], param: TyId, span: Span) -> Result<TyId, CompilerError> {
    let (subs, _fresh) = gamma.instantiate_generics(generics, span)?;
    substitute(gamma, param, &subs, span)
}

pub fn instantiate_params(gamma: &mut Gamma, generics: &[TyId], params: &[TyId], span: Span) -> Result<Vec<TyId>, CompilerError> {
    let (subs, _fresh) = gamma.instantiate_generics(generics, span)?;
    params.iter().map(|&p| substitute(gamma, p, &subs, span)).collect()
}
