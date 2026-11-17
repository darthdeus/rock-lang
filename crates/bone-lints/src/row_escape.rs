//! `row_escape` measures where values from `Arena<T>` or `Scene<T>` leave the
//! checked region. This opt-in, intraprocedural advice census is not a lifetime
//! checker.

use crate::{EscapeKind, RowEscape, head_type};
use bone_core::hir::{
    BinOp, BindingInfo, BuiltinTy, Expr, Function, Statement, TyId, TypeCtor, TypeExpr, VarId,
};
use bone_core::prelude::*;
use bone_core::suspension::{SuspensionPolicy, uses_after_suspension};
use bone_core::workspace::BodyInference;

/// Handles whose `T` is a row; the handle outlives the value it names.
const HANDLES: [&str; 3] = ["Arena", "Entity", "Scene"];

/// Where a row appears in a source call's result.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum RowShape {
    /// The result is `T` or `T?`.
    Direct,
    /// The row is the tuple element at this index.
    TupleElem(usize),
}

/// `tail_sink` identifies where the body's trailing expression goes. A returned
/// row escapes; a discarded tail does not.
pub(crate) fn row_escapes_in_body(
    ws: &Workspace,
    bi: Option<&BodyInference>,
    body: &Body,
    tail_sink: Option<EscapeKind>,
) -> Vec<RowEscape> {
    let Some(bi) = bi else { return Vec::new() };
    let rows = Rows::resolve(ws, bi, body);
    if rows.exprs.is_empty() && rows.bindings.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    rows.stores(ws, bi, body, tail_sink, &mut out);
    rows.calls(ws, bi, body, &mut out);
    rows.captures(ws, bi, body, &mut out);
    rows.suspensions(ws, bi, body, &mut out);
    dedup(out)
}

/// Census denominator, including whether the run found any row sources.
#[derive(Copy, Clone, Debug, Default)]
pub struct RowPopulation {
    /// Bodies containing rows.
    pub bodies: usize,
    /// Calls and loop variables that produce rows; denominator for findings.
    pub sources: usize,
    /// Locals holding rows after aliasing closure.
    pub bindings: usize,
}

/// Row population for the reported files, so counts share the findings'
/// denominator. An empty `report` set includes every user module.
pub fn row_population(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    report: &FxHashSet<Ustr>,
) -> RowPopulation {
    let mut pop = RowPopulation::default();
    for module in ws.user_modules(warm) {
        if !report.is_empty() && !report.contains(&module.file) {
            continue;
        }
        let Some(inf) = ws.inference(module.id) else {
            continue;
        };
        for (owner, body) in module.bodies.iter() {
            let Some(bi) = inf.per_fn.get(owner) else {
                continue;
            };
            let rows = Rows::resolve(ws, bi, body);
            if rows.seeds == 0 && rows.bindings.is_empty() {
                continue;
            }
            pop.bodies += 1;
            pop.sources += rows.seeds;
            pop.bindings += rows.bindings.len();
        }
    }
    pop
}

/// Row-valued expressions and bindings in one body.
struct Rows {
    exprs: FxHashSet<Idx<Expr>>,
    bindings: FxHashSet<Idx<BindingInfo>>,
    /// Tuple-valued expressions and the index carrying a row.
    tuples: FxHashMap<Idx<Expr>, usize>,
    /// Row sources before aliasing closure, kept separate from aliases.
    seeds: usize,
}

impl Rows {
    /// Find row sources, then close over aliases to a fixed point.
    fn resolve(ws: &Workspace, bi: &BodyInference, body: &Body) -> Rows {
        let mut r = Rows {
            exprs: FxHashSet::default(),
            bindings: FxHashSet::default(),
            tuples: FxHashMap::default(),
            seeds: 0,
        };
        for (idx, expr) in body.expr_arena.iter() {
            let Expr::Call { .. } = expr else { continue };
            let Some(f) = callee_fn(ws, bi, idx) else {
                continue;
            };
            match row_shape(ws, f) {
                Some(RowShape::Direct) => {
                    r.exprs.insert(idx);
                }
                Some(RowShape::TupleElem(i)) => {
                    r.tuples.insert(idx, i);
                }
                None => {}
            }
        }
        r.seeds = r.exprs.len() + r.tuples.len();
        r.seed_loops(ws, bi, body);
        r.seeds += r.bindings.len();
        while r.grow(ws, bi, body) {}
        r
    }

    /// Seed row bindings from Arena loop variables. Each pattern leaf's type
    /// distinguishes rows from `Entity<T>` handles.
    fn seed_loops(&mut self, ws: &Workspace, bi: &BodyInference, body: &Body) {
        for (_, stmt) in body.statement_arena.iter() {
            let Statement::For { pattern, iter, .. } = stmt else {
                continue;
            };
            if !iterates_an_arena(ws, bi, body, *iter) {
                continue;
            }
            for leaf in body.pattern_bindings(*pattern) {
                if !is_handle(ws, bi, bi.binding_types.get(leaf).copied()) {
                    self.bindings.insert(leaf);
                }
            }
        }
    }

    /// One aliasing round. Returns whether anything was added.
    fn grow(&mut self, ws: &Workspace, bi: &BodyInference, body: &Body) -> bool {
        let before = (self.exprs.len(), self.bindings.len(), self.tuples.len());

        for (idx, expr) in body.expr_arena.iter() {
            match expr {
                Expr::Var(b) if self.bindings.contains(b) => {
                    self.exprs.insert(idx);
                }
                // The left value flows through null coalescing when present.
                Expr::BinOp {
                    op: BinOp::NullCoalesce,
                    lhs,
                    ..
                } if self.exprs.contains(lhs) => {
                    self.exprs.insert(idx);
                }
                // Casts preserve row identity; `as any` is reported separately.
                Expr::As { value, .. } if self.exprs.contains(value) => {
                    self.exprs.insert(idx);
                }
                Expr::Block { tail: Some(t), .. } if self.exprs.contains(t) => {
                    self.exprs.insert(idx);
                }
                Expr::TupleIndex { tuple, index } if self.tuples.get(tuple) == Some(index) => {
                    self.exprs.insert(idx);
                }
                // Unwrappers forward their nullable argument, regardless of name.
                Expr::Call { args, .. } if args.len() == 1 => {
                    let arg = (*args).into_iter().next().expect("len == 1");
                    if self.exprs.contains(&arg)
                        && callee_fn(ws, bi, idx).is_some_and(|f| is_unwrapper(ws, f))
                    {
                        self.exprs.insert(idx);
                    }
                }
                _ => {}
            }
        }

        for (_, stmt) in body.statement_arena.iter() {
            match stmt {
                Statement::Static { name, value } => {
                    if self.exprs.contains(value) {
                        self.bindings.insert(*name);
                    }
                }
                Statement::Let { pattern, value, .. } => {
                    if self.exprs.contains(value)
                        && let Some(name) = body.pattern_single_bind(*pattern)
                    {
                        self.bindings.insert(name);
                    }
                    // Bind the row selected from the tuple.
                    if let Some(&i) = self.tuples.get(value)
                        && let bone_core::hir::Pattern::Tuple { elements } =
                            body.pattern_arena[*pattern]
                        && let Some(ep) = elements.into_iter().nth(i)
                        && let bone_core::hir::Pattern::Bind(leaf) = body.pattern_arena[ep]
                    {
                        self.bindings.insert(leaf);
                    }
                }
                _ => {}
            }
        }

        for (_, expr) in body.expr_arena.iter() {
            match expr {
                Expr::IfLet { pattern, value, .. } if self.exprs.contains(value) => {
                    if let bone_core::hir::Pattern::Bind(b) = body.pattern_arena[*pattern] {
                        self.bindings.insert(b);
                    }
                }
                Expr::SafeNavLet {
                    receiver, binding, ..
                } if self.exprs.contains(receiver) => {
                    self.bindings.insert(*binding);
                }
                // A local assignment aliases the row; stores are checked separately.
                Expr::Assign { target, value } if self.exprs.contains(value) => {
                    if let Expr::Var(b) = body.expr_arena[*target] {
                        self.bindings.insert(b);
                    }
                }
                _ => {}
            }
        }

        before != (self.exprs.len(), self.bindings.len(), self.tuples.len())
    }

    /// Stores into places that may outlive the row.
    fn stores(
        &self,
        ws: &Workspace,
        bi: &BodyInference,
        body: &Body,
        tail_sink: Option<EscapeKind>,
        out: &mut Vec<RowEscape>,
    ) {
        let mut push = |kind, e: Idx<Expr>| out.push(self.escape(ws, bi, body, kind, e));

        for (_, expr) in body.expr_arena.iter() {
            match expr {
                Expr::Assign { target, value } if self.exprs.contains(value) => {
                    match &body.expr_arena[*target] {
                        Expr::Global(_) => push(EscapeKind::StoreGlobal, *value),
                        Expr::FieldAccess { .. } | Expr::SafeNav { .. } => {
                            push(EscapeKind::StoreField, *value)
                        }
                        Expr::Index { .. } => push(EscapeKind::StoreCollection, *value),
                        _ => {}
                    }
                }
                Expr::StructLiteral { fields, .. } => {
                    for f in *fields {
                        let v = body.struct_lit_field_arena[f].value;
                        if self.exprs.contains(&v) {
                            push(EscapeKind::StoreField, v);
                        }
                    }
                }
                Expr::Array(elems) => {
                    for e in *elems {
                        if self.exprs.contains(&e) {
                            push(EscapeKind::StoreCollection, e);
                        }
                    }
                }
                Expr::Return(Some(v)) if self.exprs.contains(v) => push(EscapeKind::Return, *v),
                Expr::As { value, target, .. } if self.exprs.contains(value) => {
                    if is_any(ws, bi, Some(*target)) {
                        push(EscapeKind::AnyErasure, *value);
                    }
                }
                _ => {}
            }
        }

        for (_, stmt) in body.statement_arena.iter() {
            match stmt {
                Statement::Static { value, .. } if self.exprs.contains(value) => {
                    push(EscapeKind::StoreStatic, *value)
                }
                Statement::Return(Some(v)) if self.exprs.contains(v) => {
                    push(EscapeKind::Return, *v)
                }
                _ => {}
            }
        }

        if let (Some(root), Some(sink)) = (body.root, tail_sink) {
            if let Expr::Block { tail: Some(t), .. } = body.expr_arena[root] {
                if self.exprs.contains(&t) {
                    push(sink, t);
                }
            }
        }
    }

    /// Check row arguments to extern, `Any`, and container-storage calls. Other
    /// calls have no interprocedural retention summary.
    fn calls(&self, ws: &Workspace, bi: &BodyInference, body: &Body, out: &mut Vec<RowEscape>) {
        for (idx, expr) in body.expr_arena.iter() {
            let Expr::Call { args, .. } = expr else {
                continue;
            };
            let args: Vec<Idx<Expr>> = (*args).into_iter().collect();
            let row_args: Vec<(usize, Idx<Expr>)> = args
                .iter()
                .copied()
                .enumerate()
                .filter(|(_, a)| self.exprs.contains(a))
                .collect();
            if row_args.is_empty() {
                continue;
            }
            let f = callee_fn(ws, bi, idx);

            // A row in a later argument is stored when the receiver is a container.
            let into_container = args
                .first()
                .is_some_and(|a| is_container(ws, bi, bi.expr_types.get(*a).copied()))
                && row_args.iter().all(|(i, _)| *i > 0);

            for (i, a) in row_args {
                let kind = if f.is_some_and(|f| f.is_extern) {
                    EscapeKind::FfiArg
                } else if f.is_some_and(|f| param_is_any(ws, f, i)) {
                    EscapeKind::AnyErasure
                } else if into_container {
                    EscapeKind::StoreCollection
                } else {
                    continue;
                };
                out.push(self.escape(ws, bi, body, kind, a));
            }
        }
    }

    /// Capturing an outer row is unprovable without a retention summary. A
    /// closure that captures a handle and fetches the row inside remains safe.
    fn captures(&self, ws: &Workspace, bi: &BodyInference, body: &Body, out: &mut Vec<RowEscape>) {
        for (lam, expr) in body.expr_arena.iter() {
            let Expr::Lambda { .. } = expr else { continue };
            let lam_span = body.expr_spans[lam];
            for (idx, e) in body.expr_arena.iter() {
                let Expr::Var(b) = e else { continue };
                if !self.bindings.contains(b) || !lam_span.contains(body.expr_spans[idx]) {
                    continue;
                }
                if lam_span.contains(body.binding_spans[*b]) {
                    continue;
                }
                out.push(self.escape(ws, bi, body, EscapeKind::ClosureCapture, idx));
            }
        }
    }

    /// Report row reads after suspension. The walk tracks live and suspended
    /// bindings in evaluation order, so rows no longer used before suspension
    /// are not reported.
    fn suspensions(
        &self,
        ws: &Workspace,
        bi: &BodyInference,
        body: &Body,
        out: &mut Vec<RowEscape>,
    ) {
        let policy = RowSuspensionPolicy { rows: self, ws, bi };
        for at in uses_after_suspension(body, &policy) {
            out.push(self.escape(ws, bi, body, EscapeKind::AcrossYield, at));
        }
    }

    fn escape(
        &self,
        ws: &Workspace,
        bi: &BodyInference,
        body: &Body,
        kind: EscapeKind,
        e: Idx<Expr>,
    ) -> RowEscape {
        let row_type = match bi.expr_types.get(e).copied() {
            Some(ty) => ws.render(Some(bi), ty),
            None => "?".to_string(),
        };
        RowEscape {
            kind,
            row_type,
            span: body.expr_spans[e],
        }
    }
}

struct RowSuspensionPolicy<'a> {
    rows: &'a Rows,
    ws: &'a Workspace,
    bi: &'a BodyInference,
}

impl SuspensionPolicy for RowSuspensionPolicy<'_> {
    fn binding_holds_frame_data(
        &self,
        binding: Idx<BindingInfo>,
        value: Option<Idx<Expr>>,
    ) -> bool {
        match value {
            Some(value) => {
                self.rows.exprs.contains(&value)
                    && self
                        .bi
                        .binding_types
                        .get(binding)
                        .copied()
                        .is_some_and(|ty| {
                            !matches!(head_type(self.ws, self.bi, ty), TypeExpr::Builtin(_))
                        })
            }
            None => self.rows.bindings.contains(&binding),
        }
    }
}

/// Keep one finding per escape kind and source site.
fn dedup(mut found: Vec<RowEscape>) -> Vec<RowEscape> {
    let mut seen: FxHashSet<(EscapeKind, (usize, usize))> = FxHashSet::default();
    found.retain(|f| seen.insert((f.kind, f.span.offset_range())));
    found
}

fn callee_fn<'a>(ws: &'a Workspace, bi: &BodyInference, call: Idx<Expr>) -> Option<&'a Function> {
    let (mid, fid) = bi.resolved_overload_calls.get(&call)?;
    Some(&ws.module(*mid).functions[*fid])
}

/// Whether a declaration returns its handle's generic row, directly or inside
/// a tuple. Match the type structure rather than method names such as `get`.
fn row_shape(ws: &Workspace, f: &Function) -> Option<RowShape> {
    let g = handle_generic(ws, f.param_decls.first()?.ty)?;
    let ret = f.return_ty?;
    let (mid, _) = static_parts(ret)?;
    let te = strip_nullable(ws, mid, static_te(ws, ret)?);
    if is_generic(te, g) {
        return Some(RowShape::Direct);
    }
    if let TypeExpr::Tuple { elements } = te {
        for (i, el) in elements.into_iter().enumerate() {
            if is_generic(strip_nullable(ws, mid, ws.module(mid).ty_arena[el]), g) {
                return Some(RowShape::TupleElem(i));
            }
        }
    }
    None
}

/// Return the declared generic parameter of a row handle type.
fn handle_generic(ws: &Workspace, ty: TyId) -> Option<VarId> {
    let (mid, _) = static_parts(ty)?;
    let TypeExpr::App {
        ctor: TypeCtor::Nominal(name),
        params,
    } = static_te(ws, ty)?
    else {
        return None;
    };
    if !HANDLES.contains(&bone_core::modules::display_name(name.as_str())) || params.len() != 1 {
        return None;
    }
    let p = params.into_iter().next()?;
    match ws.module(mid).ty_arena[p] {
        TypeExpr::Generic(v, _) => Some(v),
        _ => None,
    }
}

/// Whether `f` forwards a nullable argument unchanged, regardless of its name.
fn is_unwrapper(ws: &Workspace, f: &Function) -> bool {
    let Some(p0) = f.param_decls.first() else {
        return false;
    };
    let (Some(ret), Some((mid, _))) = (f.return_ty, static_parts(p0.ty)) else {
        return false;
    };
    let (Some(pte), Some(rte)) = (static_te(ws, p0.ty), static_te(ws, ret)) else {
        return false;
    };
    let TypeExpr::Nullable(inner) = pte else {
        return false;
    };
    ws.module(mid).ty_arena[inner] == rte
}

fn param_is_any(ws: &Workspace, f: &Function, i: usize) -> bool {
    f.param_decls
        .get(i)
        .and_then(|p| static_te(ws, p.ty))
        .is_some_and(|te| matches!(te, TypeExpr::Builtin(BuiltinTy::Any)))
}

fn is_generic(te: TypeExpr, g: VarId) -> bool {
    matches!(te, TypeExpr::Generic(v, _) if v == g)
}

fn strip_nullable(ws: &Workspace, mid: ModuleId, te: TypeExpr) -> TypeExpr {
    match te {
        TypeExpr::Nullable(inner) => ws.module(mid).ty_arena[inner],
        other => other,
    }
}

fn static_parts(ty: TyId) -> Option<(ModuleId, Idx<TypeExpr>)> {
    match ty {
        TyId::Static(mid, i) => Some((mid, i)),
        TyId::Local(_) => None,
    }
}

fn static_te(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
    let (mid, i) = static_parts(ty)?;
    Some(ws.module(mid).ty_arena[i])
}

/// Whether an inferred type is a row handle rather than a row.
fn is_handle(ws: &Workspace, bi: &BodyInference, ty: Option<TyId>) -> bool {
    matches!(
        ty.map(|t| head_type(ws, bi, t)),
        Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) if HANDLES.contains(&bone_core::modules::display_name(n.as_str()))
    )
}

fn is_container(ws: &Workspace, bi: &BodyInference, ty: Option<TyId>) -> bool {
    matches!(
        ty.map(|t| head_type(ws, bi, t)),
        Some(TypeExpr::App {
            ctor: TypeCtor::Array | TypeCtor::Map,
            ..
        })
    )
}

fn is_any(ws: &Workspace, bi: &BodyInference, ty: Option<TyId>) -> bool {
    matches!(
        ty.map(|t| head_type(ws, bi, t)),
        Some(TypeExpr::Builtin(BuiltinTy::Any))
    )
}

/// Whether a `for` iterates an Arena directly or through one of its iterator
/// constructors. Combinator chains are not followed.
fn iterates_an_arena(ws: &Workspace, bi: &BodyInference, body: &Body, iter: Idx<Expr>) -> bool {
    let is_arena = |e: Idx<Expr>| {
        matches!(
            bi.expr_types.get(e).copied().map(|t| head_type(ws, bi, t)),
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) if bone_core::modules::display_name(n.as_str()) == "Arena"
        )
    };
    if is_arena(iter) {
        return true;
    }
    match &body.expr_arena[iter] {
        Expr::Call { args, .. } => (*args).into_iter().next().is_some_and(is_arena),
        _ => false,
    }
}
