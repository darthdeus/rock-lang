//! `row_escape` -- a row place reaching storage that outlives its arena.
//!
//! A *row* is the `T` an `Arena<T>` stores, or the root `T` a `Scene<T>` owns.
//! Both are handed out today as ordinary references: `Entity<T>` is
//! generation-checked, the `T` it resolves to is not. Once the Scene unloads or
//! the slot is recycled, a retained row reads dead state, and under the SoA
//! backing `docs/scene-system-plan.md` P4 wants there is no row object to
//! retain at all.
//!
//! This is the transitional census the plan's P6 rests on: it measures the
//! escape POPULATION so the decision to build (or delete) the checked
//! scoped-place property is made against a number. It is deliberately `Advice`
//! and opt-in -- a lint cannot prove the absence of an escape, so it must not
//! pose as the boundary. What it cannot see is enumerated in
//! `docs/lint-row-escape.md`; read that before reading a zero as evidence.

use crate::{head_type, EscapeKind, RowEscape};
use bone_core::hir::{
    BinOp, BindingInfo, BuiltinTy, Callee, Expr, Function, InterpPart, Pattern,
    Statement, TyId, TypeCtor, TypeExpr, VarId, VariantArgs,
};
use bone_core::prelude::*;
use bone_core::workspace::BodyInference;

/// Handle types whose element/root `T` is a row. Each is a WEAK scalar identity
/// (`stdlib/unified.rock`): the handle survives what it names, the `T` does not.
const HANDLES: [&str; 3] = ["Arena", "Entity", "Scene"];

/// Where a row sits in a row-source call's result.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum RowShape {
    /// The result IS the row (`get`, `remove`): `T` or `T?`.
    Direct,
    /// The result is a tuple carrying the row at this index (`random`:
    /// `(Entity<T>, T)?`).
    TupleElem(usize),
}

/// `tail_sink` is where this body's TRAILING expression goes -- the enclosing
/// function's return, a global's initial value, `None` for a body that
/// discards it. Without it the rule cannot tell `fn f() -> Mob? { e.get() }`
/// from `fn f() { e.remove() }`, and the second is not an escape at all.
pub fn row_escapes_in_body(
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

/// What the census EXAMINED. A finding count on its own cannot be read: zero
/// escapes in a project with no `Arena` at all says nothing about escapes,
/// and the P6 decision this lint feeds turns on distinguishing those two zeros.
#[derive(Copy, Clone, Debug, Default)]
pub struct RowPopulation {
    /// Bodies carrying at least one row.
    pub bodies: usize,
    /// Row SOURCES: call sites reading a row out of a handle, plus Arena loop
    /// variables. The denominator -- what a finding count is a fraction of.
    pub sources: usize,
    /// Locals that hold a row, after aliasing is closed over.
    pub bindings: usize,
}

/// The row population of the bodies a run REPORTS on. `report` is the same file
/// set the findings are restricted to, so the numerator and denominator are
/// measured over one population; passing an empty set measures every user
/// module (tests and whole-workspace runs).
pub fn row_population(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    report: &FxHashSet<Ustr>,
) -> RowPopulation {
    let mut pop = RowPopulation::default();
    for module in crate::user_modules(ws, warm) {
        if !report.is_empty() && !report.contains(&module.file) {
            continue;
        }
        let Some(inf) = ws.inference(module.id) else { continue };
        for (owner, body) in module.bodies.iter() {
            let Some(bi) = inf.per_fn.get(owner) else { continue };
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

/// The row-valued expressions and bindings of one body, closed under aliasing.
struct Rows {
    exprs: FxHashSet<Idx<Expr>>,
    bindings: FxHashSet<Idx<BindingInfo>>,
    /// Expressions whose value is a TUPLE carrying a row, and at which index.
    tuples: FxHashMap<Idx<Expr>, usize>,
    /// Row SOURCES before aliasing closure -- the census denominator. Counted
    /// separately from `exprs`, which grows to include every alias.
    seeds: usize,
}

impl Rows {
    /// Seed from the row SOURCES, then close over aliasing until nothing new
    /// appears. The fixpoint matters because a row reaches its escape through
    /// an unwrap and one or more plain `let`s far more often than directly.
    fn resolve(ws: &Workspace, bi: &BodyInference, body: &Body) -> Rows {
        let mut r = Rows {
            exprs: FxHashSet::default(),
            bindings: FxHashSet::default(),
            tuples: FxHashMap::default(),
            seeds: 0,
        };
        for (idx, expr) in body.expr_arena.iter() {
            let Expr::Call { .. } = expr else { continue };
            let Some(f) = callee_fn(ws, bi, idx) else { continue };
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

    /// Loop variables bound by iterating an Arena. Which leaves are rows is
    /// decided by the leaf's own TYPE rather than the loop's arity, so
    /// `for m in a`, `for (e, m) in a` and `for e in a.keys()` all fall out of
    /// one rule -- an `Entity<T>` leaf is the handle, never the row.
    fn seed_loops(&mut self, ws: &Workspace, bi: &BodyInference, body: &Body) {
        for (_, stmt) in body.statement_arena.iter() {
            let Statement::For { vars, iter, .. } = stmt else { continue };
            if !iterates_an_arena(ws, bi, body, *iter) {
                continue;
            }
            for leaf in vars.leaves() {
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
                // `e.get() ?? return` / `?? fallback`: the value that flows on
                // is the row whenever the left side produced one.
                Expr::BinOp { op: BinOp::NullCoalesce, lhs, .. } if self.exprs.contains(lhs) => {
                    self.exprs.insert(idx);
                }
                // A cast does not change what the value IS. `as any` is an
                // escape, reported separately -- it is still the same row.
                Expr::As { value, .. } if self.exprs.contains(value) => {
                    self.exprs.insert(idx);
                }
                Expr::Block { tail: Some(t), .. } if self.exprs.contains(t) => {
                    self.exprs.insert(idx);
                }
                Expr::TupleIndex { tuple, index } if self.tuples.get(tuple) == Some(index) => {
                    self.exprs.insert(idx);
                }
                // An unwrapper -- declared `(X?) -> X` -- forwards its argument.
                // Keyed on the declared signature, not the name, so `unwrap`,
                // `expect` and any project-local sibling are covered alike.
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
                Statement::Let { name, value } | Statement::Static { name, value } => {
                    if self.exprs.contains(value) {
                        self.bindings.insert(*name);
                    }
                }
                Statement::LetTuple { vars, value } => {
                    if let Some(&i) = self.tuples.get(value) {
                        let leaves = vars.leaves();
                        if let Some(&leaf) = leaves.get(i) {
                            self.bindings.insert(leaf);
                        }
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
                Expr::SafeNavLet { receiver, binding, .. } if self.exprs.contains(receiver) => {
                    self.bindings.insert(*binding);
                }
                // `m = e.get()` into an ordinary local makes that local a row;
                // only a store into a PLACE that outlives the row is an escape.
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

    /// Stores into a place that outlives the row: a global, a `static` local, a
    /// struct field, a collection slot.
    fn stores(
        &self,
        ws: &Workspace,
        bi: &BodyInference,
        body: &Body,
        tail_sink: Option<EscapeKind>,
        out: &mut Vec<RowEscape>,
    ) {
        let mut push = |kind, e: Idx<Expr>| {
            out.push(RowEscape { kind, row_type: self.render(ws, bi, e), span: body.expr_spans[e] })
        };

        for (idx, expr) in body.expr_arena.iter() {
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
            let _ = idx;
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

        // A trailing expression escapes only where it actually goes somewhere.
        if let (Some(root), Some(sink)) = (body.root, tail_sink) {
            if let Expr::Block { tail: Some(t), .. } = body.expr_arena[root] {
                if self.exprs.contains(&t) {
                    push(sink, t);
                }
            }
        }
    }

    /// Rows passed as call arguments. Three positions are escapes; every other
    /// call is an ordinary Rock-bodied helper, which this lint cannot judge
    /// (no interprocedural retain summary -- see the doc).
    fn calls(&self, ws: &Workspace, bi: &BodyInference, body: &Body, out: &mut Vec<RowEscape>) {
        for (idx, expr) in body.expr_arena.iter() {
            let Expr::Call { args, .. } = expr else { continue };
            let args: Vec<Idx<Expr>> = (*args).into_iter().collect();
            let row_args: Vec<(usize, Idx<Expr>)> =
                args.iter().copied().enumerate().filter(|(_, a)| self.exprs.contains(a)).collect();
            if row_args.is_empty() {
                continue;
            }
            let f = callee_fn(ws, bi, idx);

            // `bag.push(m)` / `map.insert(k, m)`: receiver is a container and
            // the row is a later argument, so the row is being STORED in it.
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
                out.push(RowEscape {
                    kind,
                    row_type: self.render(ws, bi, a),
                    span: body.expr_spans[a],
                });
            }
        }
    }

    /// A row read from inside a lambda that was bound outside it. The lambda
    /// outlives the statement that built it, so the row it closed over is read
    /// at a time nothing has re-validated. `mag/glyphs.rock`'s `move_to`
    /// captures the `Entity` and re-`get()`s inside -- which is why the handle
    /// is excluded from `bindings` and this stays quiet on the correct idiom.
    fn captures(&self, ws: &Workspace, bi: &BodyInference, body: &Body, out: &mut Vec<RowEscape>) {
        for (lam, expr) in body.expr_arena.iter() {
            let Expr::Lambda { .. } = expr else { continue };
            let lam_span = body.expr_spans[lam];
            for (idx, e) in body.expr_arena.iter() {
                let Expr::Var(b) = e else { continue };
                if !self.bindings.contains(b) || !contains(lam_span, body.expr_spans[idx]) {
                    continue;
                }
                if contains(lam_span, body.binding_spans[*b]) {
                    continue;
                }
                out.push(RowEscape {
                    kind: EscapeKind::ClosureCapture,
                    row_type: self.render(ws, bi, idx),
                    span: body.expr_spans[idx],
                });
            }
        }
    }

    /// A row still read after a suspension. Between the `yield` and the read,
    /// arbitrary game code ran: the row may name a despawned entity, and
    /// nothing re-binds a suspended body's locals. The failure mode is a
    /// VACUOUS GREEN rather than a wrong value -- `Arena.insert` rebinds a
    /// reused slot's `_data` instead of mutating in place, so a stale row never
    /// aliases the new occupant. It freezes at the dead entity's values and
    /// writes through it reach nothing, which is why "the mob did not move"
    /// passes trivially. See `docs/entity-coroutine-binding.md`.
    ///
    /// An evaluation-order walk carrying two binding sets: `live` (fetched,
    /// nothing suspended since) and `crossed` (fetched, then suspended). A read
    /// of a `crossed` binding is the finding. Liveness, not scope -- a row
    /// finished with before the `yield` stays quiet.
    fn suspensions(&self, ws: &Workspace, bi: &BodyInference, body: &Body, out: &mut Vec<RowEscape>) {
        let Some(root) = body.root else { return };
        // No suspension point, nothing to be stale across. A flat arena scan
        // over the overwhelming majority of bodies (nothing but a `co fn`
        // suspends) is far cheaper than the ordered walk it skips.
        if !body.expr_arena.iter().any(|(_, e)| matches!(e, Expr::Yield(_) | Expr::YieldDelegate(_)))
        {
            return;
        }
        let mut walk =
            YieldWalk { rows: self, ws, bi, body, out, reported: FxHashSet::default() };
        walk.expr(root, &mut Flow::default());
    }

    fn render(&self, ws: &Workspace, bi: &BodyInference, e: Idx<Expr>) -> String {
        match bi.expr_types.get(e).copied() {
            Some(ty) => ws.render(Some(bi), ty),
            None => "?".to_string(),
        }
    }
}

/// One finding per (kind, site). A row reached through several aliases lands on
/// the same escape expression more than once; the site is the thing an author
/// fixes, so reporting it twice is noise, and a census that double-counts it
/// overstates the population it exists to measure.
fn dedup(mut found: Vec<RowEscape>) -> Vec<RowEscape> {
    let mut seen: FxHashSet<(EscapeKind, (usize, usize))> = FxHashSet::default();
    found.retain(|f| seen.insert((f.kind, f.span.offset_range)));
    found
}

fn contains(outer: Span, inner: Span) -> bool {
    outer.offset_range.0 <= inner.offset_range.0 && inner.offset_range.1 <= outer.offset_range.1
}

fn callee_fn<'a>(ws: &'a Workspace, bi: &BodyInference, call: Idx<Expr>) -> Option<&'a Function> {
    let (mid, fid) = bi.resolved_overload_calls.get(&call)?;
    Some(&ws.module(*mid).functions[*fid])
}

/// Whether `f` reads a row OUT of a handle, judged from its DECLARATION: param
/// 0 is `Handle<G>` and the result is `G`, `G?`, or a tuple carrying `G`.
///
/// Structural rather than name-keyed on purpose. `get` names a method on a
/// dozen unrelated types, and keying on it would both miss `remove` (which
/// hands out a row whose slot is already free -- strictly worse to retain) and
/// falsely match `Map.get`. The generic identity is what carries the meaning:
/// `Arena<G>.len() -> int` mentions no `G` and is not a source.
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

/// `Some(G)` iff `ty` is `Arena<G>` / `Entity<G>` / `Scene<G>` for a declared
/// generic `G`.
fn handle_generic(ws: &Workspace, ty: TyId) -> Option<VarId> {
    let (mid, _) = static_parts(ty)?;
    let TypeExpr::App { ctor: TypeCtor::Nominal(name), params } = static_te(ws, ty)? else {
        return None;
    };
    if !HANDLES.contains(&name.as_str()) || params.len() != 1 {
        return None;
    }
    let p = params.into_iter().next()?;
    match ws.module(mid).ty_arena[p] {
        TypeExpr::Generic(v, _) => Some(v),
        _ => None,
    }
}

/// Whether `f` forwards its argument out of a `?` -- declared `(X?) -> X` for
/// the same `X`. `unwrap`, `expect`, and any project-local sibling: the value
/// that comes out is the value that went in, so row-ness passes through.
fn is_unwrapper(ws: &Workspace, f: &Function) -> bool {
    let Some(p0) = f.param_decls.first() else { return false };
    let (Some(ret), Some((mid, _))) = (f.return_ty, static_parts(p0.ty)) else { return false };
    let (Some(pte), Some(rte)) = (static_te(ws, p0.ty), static_te(ws, ret)) else { return false };
    let TypeExpr::Nullable(inner) = pte else { return false };
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

/// Whether an inferred type is one of the weak handles -- the thing an author
/// is SUPPOSED to store, so never a row.
fn is_handle(ws: &Workspace, bi: &BodyInference, ty: Option<TyId>) -> bool {
    matches!(
        ty.map(|t| head_type(ws, bi, t)),
        Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) if HANDLES.contains(&n.as_str())
    )
}

fn is_container(ws: &Workspace, bi: &BodyInference, ty: Option<TyId>) -> bool {
    matches!(
        ty.map(|t| head_type(ws, bi, t)),
        Some(TypeExpr::App { ctor: TypeCtor::Array | TypeCtor::Map, .. })
    )
}

fn is_any(ws: &Workspace, bi: &BodyInference, ty: Option<TyId>) -> bool {
    matches!(ty.map(|t| head_type(ws, bi, t)), Some(TypeExpr::Builtin(BuiltinTy::Any)))
}

/// Whether a `for`'s iterable is an Arena, directly (`for m in mobs`) or
/// through one of its iterator constructors (`mobs.values()`, `mobs.pairs()`).
/// A combinator chain between the two is not followed -- see the doc's
/// not-covered list.
fn iterates_an_arena(
    ws: &Workspace,
    bi: &BodyInference,
    body: &Body,
    iter: Idx<Expr>,
) -> bool {
    let is_arena = |e: Idx<Expr>| {
        matches!(
            bi.expr_types.get(e).copied().map(|t| head_type(ws, bi, t)),
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) if n.as_str() == "Arena"
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

/// Bindings holding a row, split by whether a suspension has happened since the
/// fetch. A read of a `crossed` binding is the finding.
#[derive(Clone, Default)]
struct Flow {
    /// Fetched, no suspension since -- still naming a live slot.
    live: FxHashSet<Idx<BindingInfo>>,
    /// Fetched, then suspended. Any read from here on is stale.
    crossed: FxHashSet<Idx<BindingInfo>>,
}

impl Flow {
    fn suspend(&mut self) {
        self.crossed.extend(self.live.drain());
    }

    /// Union both halves. Merging a branch that suspended with one that didn't
    /// leaves the binding `crossed`, which is the answer we want: one path
    /// through the code reads a recycled slot.
    fn merge(&mut self, other: Flow) {
        self.live.extend(other.live);
        self.crossed.extend(other.crossed);
    }

    /// Total tracked bindings. The taint lattice only grows, so an unchanged
    /// size means an unchanged state -- the fixpoint test.
    fn size(&self) -> usize {
        self.live.len() + self.crossed.len()
    }

    /// A re-assignment (or a re-fetch) invalidates whatever was known.
    fn rebind(&mut self, b: Idx<BindingInfo>, fetched: bool) {
        self.live.remove(&b);
        self.crossed.remove(&b);
        if fetched {
            self.live.insert(b);
        }
    }
}

struct YieldWalk<'a> {
    rows: &'a Rows,
    ws: &'a Workspace,
    bi: &'a BodyInference,
    body: &'a Body,
    out: &'a mut Vec<RowEscape>,
    /// Loops are walked twice to reach the back-edge fixpoint, so the same read
    /// can be visited more than once. One finding per read expression.
    reported: FxHashSet<Idx<Expr>>,
}

impl YieldWalk<'_> {
    fn report(&mut self, at: Idx<Expr>) {
        if !self.reported.insert(at) {
            return;
        }
        let row_type = match self.bi.expr_types.get(at).copied() {
            Some(ty) => self.ws.render(Some(self.bi), ty),
            None => "?".to_string(),
        };
        self.out.push(RowEscape {
            kind: EscapeKind::AcrossYield,
            row_type,
            span: self.body.expr_spans[at],
        });
    }

    /// Whether a binding of `value` is worth tracking. Row-ness comes from the
    /// whole-body [`Rows`] fixpoint, so an Arena loop variable and a
    /// `Scene<T>` root track alongside `e.get()` -- the reason
    /// `los_blocks_aggro`'s `summon`, bound by `for (me, m) in MOBS`, is seen.
    fn bind(&mut self, b: Idx<BindingInfo>, value: Idx<Expr>, f: &mut Flow) {
        f.rebind(b, self.rows.exprs.contains(&value) && self.holds_payload(b));
    }

    /// A scalar copied out of a row is safe to hold: `Entity<int>` yields the
    /// value itself, and nothing points back at the arena.
    fn holds_payload(&self, b: Idx<BindingInfo>) -> bool {
        let Some(ty) = self.bi.binding_types.get(b).copied() else { return false };
        !matches!(head_type(self.ws, self.bi, ty), TypeExpr::Builtin(_))
    }

    fn block_expr(&mut self, e: Idx<Expr>, f: &mut Flow) {
        self.expr(e, f);
    }

    /// Walk a branch from a copy of `f` and fold the result back in, so both
    /// arms see the same entry state and the exit state is their union.
    fn branch(&mut self, e: Option<Idx<Expr>>, entry: &Flow, out: &mut Flow) {
        let Some(e) = e else { return };
        let mut b = entry.clone();
        self.expr(e, &mut b);
        out.merge(b);
    }

    /// A loop body, walked to fixpoint. The taint lattice only grows, so the
    /// second pass -- entered with the first pass's exit state -- is the
    /// fixpoint. This is what makes the back-edge a suspension point: a binding
    /// fetched at the top and read after a `yield` LATER in the body is fine,
    /// but one fetched OUTSIDE and read after the body's `yield` is stale from
    /// the second iteration on.
    fn loop_body(&mut self, body: Idx<Expr>, f: &mut Flow) {
        let before = f.size();
        let mut pass = f.clone();
        self.expr(body, &mut pass);
        f.merge(pass);
        // Already at the fixpoint -- the second pass would see the identical
        // entry state and find nothing new. Skipping it is what keeps NESTED
        // loops from costing 2^depth walks in the common case (a body with no
        // tracked binding at all never grows).
        if f.size() == before {
            return;
        }
        let mut second = f.clone();
        self.expr(body, &mut second);
        f.merge(second);
    }

    fn stmt(&mut self, s: Idx<Statement>, f: &mut Flow) {
        match &self.body.statement_arena[s] {
            Statement::Let { name, value } => {
                self.expr(*value, f);
                self.bind(*name, *value, f);
            }
            Statement::LetNewtype { var, value, .. } => {
                self.expr(*value, f);
                self.bind(*var, *value, f);
            }
            Statement::LetTuple { vars, value } => {
                self.expr(*value, f);
                for b in vars.leaves() {
                    f.rebind(b, self.rows.bindings.contains(&b));
                }
            }
            Statement::Static { name, value } => {
                self.expr(*value, f);
                f.rebind(*name, false);
            }
            // A nested `fn` cannot capture enclosing locals (rejected in
            // inference), so its body is unreachable from this flow.
            Statement::Fn { .. } => {}
            Statement::Return(v) => {
                if let Some(v) = v {
                    self.expr(*v, f);
                }
            }
            Statement::Expr(e) => self.expr(*e, f),
            // A `defer` body runs at scope exit, after every suspension the
            // scope contains -- so it is walked with the exit state, not here.
            Statement::Defer(_) => {}
            Statement::For { vars, iter, body } => {
                let (iter, body) = (*iter, *body);
                let leaves = vars.leaves();
                self.expr(iter, f);
                // An Arena loop variable IS a row, re-fetched each iteration.
                for b in leaves {
                    let is_row = self.rows.bindings.contains(&b);
                    f.rebind(b, is_row);
                }
                self.loop_body(body, f);
            }
            Statement::While { cond, body } => {
                let (cond, body) = (*cond, *body);
                self.expr(cond, f);
                self.loop_body(body, f);
                // The re-test runs after the body, so it sees the back-edge state.
                self.expr(cond, f);
            }
            Statement::WhileLet { pattern, value, body } => {
                let (pattern, value, body) = (*pattern, *value, *body);
                self.expr(value, f);
                if let Pattern::Bind(b) = self.body.pattern_arena[pattern] {
                    self.bind(b, value, f);
                }
                self.loop_body(body, f);
            }
            Statement::Comment(_) | Statement::Break | Statement::Continue => {}
        }
    }

    fn expr(&mut self, e: Idx<Expr>, f: &mut Flow) {
        match self.body.expr_arena[e] {
            Expr::Var(b) => {
                if f.crossed.contains(&b) {
                    self.report(e);
                }
            }

            Expr::Yield(v) => {
                if let Some(v) = v {
                    self.expr(v, f);
                }
                f.suspend();
            }
            Expr::YieldDelegate(v) => {
                self.expr(v, f);
                f.suspend();
            }

            // A lambda body is deferred, and bone rejects `yield` inside one, so
            // it can neither read at this point in the flow nor suspend. A
            // capture that outlives a suspension is `closure_capture`'s job.
            Expr::Lambda { .. } => {}

            Expr::Block { statements, tail } => {
                for s in statements {
                    self.stmt(s, f);
                }
                if let Some(t) = tail {
                    self.block_expr(t, f);
                }
                self.run_defers(statements, f);
            }

            Expr::If { cond, then_branch, else_branch } => {
                self.expr(cond, f);
                let entry = f.clone();
                let mut out = Flow::default();
                self.branch(Some(then_branch), &entry, &mut out);
                match else_branch {
                    Some(_) => self.branch(else_branch, &entry, &mut out),
                    None => out.merge(entry),
                }
                *f = out;
            }

            Expr::IfLet { pattern, value, then_branch, else_branch } => {
                self.expr(value, f);
                let mut entry = f.clone();
                if let Pattern::Bind(b) = self.body.pattern_arena[pattern] {
                    self.bind(b, value, &mut entry);
                }
                let mut out = Flow::default();
                self.branch(Some(then_branch), &entry, &mut out);
                match else_branch {
                    Some(_) => self.branch(else_branch, f, &mut out),
                    None => out.merge(f.clone()),
                }
                *f = out;
            }

            Expr::Match { scrutinee, arms } => {
                self.expr(scrutinee, f);
                let entry = f.clone();
                let mut out = Flow::default();
                for arm in arms {
                    let arm = self.body.match_arm_arena[arm];
                    let mut a = entry.clone();
                    if let Some(g) = arm.guard {
                        self.expr(g, &mut a);
                    }
                    if let Pattern::Bind(b) = self.body.pattern_arena[arm.pattern] {
                        self.bind(b, scrutinee, &mut a);
                    }
                    self.expr(arm.body, &mut a);
                    out.merge(a);
                }
                *f = out;
            }

            Expr::Assign { target, value } => {
                self.expr(value, f);
                match self.body.expr_arena[target] {
                    Expr::Var(b) => self.bind(b, value, f),
                    _ => self.expr(target, f),
                }
            }
            Expr::CompoundAssign { target, value, .. } => {
                self.expr(value, f);
                self.expr(target, f);
            }

            Expr::Call { callee, args } => {
                if let Callee::Value(v) = callee {
                    self.expr(v, f);
                }
                if let Callee::Local(b) = callee {
                    if f.crossed.contains(&b) {
                        self.report(e);
                    }
                }
                for a in args {
                    self.expr(a, f);
                }
            }
            Expr::SafeNavCall { receiver, args, .. } => {
                self.expr(receiver, f);
                for a in args {
                    self.expr(a, f);
                }
            }
            Expr::SafeNavLet { receiver, binding, body } => {
                self.expr(receiver, f);
                self.bind(binding, receiver, f);
                self.expr(body, f);
            }

            Expr::BinOp { op: BinOp::NullCoalesce, lhs, rhs } => {
                self.expr(lhs, f);
                let entry = f.clone();
                let mut out = Flow::default();
                self.branch(Some(rhs), &entry, &mut out);
                out.merge(entry);
                *f = out;
            }
            Expr::BinOp { lhs, rhs, .. } => {
                self.expr(lhs, f);
                self.expr(rhs, f);
            }
            Expr::Range { low, high } => {
                self.expr(low, f);
                self.expr(high, f);
            }

            Expr::Not(x)
            | Expr::BitNot(x)
            | Expr::UnaryMinus(x)
            | Expr::Spread(x)
            | Expr::As { value: x, .. }
            | Expr::FieldAccess { base: x, .. }
            | Expr::SafeNav { base: x, .. }
            | Expr::TupleIndex { tuple: x, .. }
            | Expr::IncDec { target: x, .. } => self.expr(x, f),

            Expr::Index { base, index } => {
                self.expr(base, f);
                self.expr(index, f);
            }

            Expr::Tuple(xs) | Expr::Array(xs) => {
                for x in xs {
                    self.expr(x, f);
                }
            }

            Expr::StructLiteral { fields, bases, .. } => {
                for b in bases {
                    self.expr(b, f);
                }
                for fd in fields {
                    self.expr(self.body.struct_lit_field_arena[fd].value, f);
                }
            }
            Expr::EnumVariant { args, .. } => match args {
                VariantArgs::Unit => {}
                VariantArgs::Tuple(xs) => {
                    for x in xs {
                        self.expr(x, f);
                    }
                }
                VariantArgs::Record(fs) => {
                    for fd in fs {
                        self.expr(self.body.struct_lit_field_arena[fd].value, f);
                    }
                }
            },

            Expr::InterpolatedString(parts) => {
                for p in parts {
                    if let InterpPart::Value { expr, spec } = self.body.interp_part_arena[p] {
                        self.expr(expr, f);
                        if let Some(x) = spec.and_then(|s| s.precision_expr) {
                            self.expr(x, f);
                        }
                    }
                }
            }

            Expr::Return(v) => {
                if let Some(v) = v {
                    self.expr(v, f);
                }
            }

            Expr::Unit
            | Expr::Null
            | Expr::Hole(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::String(_)
            | Expr::Global(_)
            | Expr::SiblingRef(_)
            | Expr::Break
            | Expr::Continue => {}
        }
    }

    /// A block's `defer`s run at scope exit -- after every suspension the block
    /// contains -- so they are walked last, with the block's exit state.
    fn run_defers(&mut self, statements: IdxRange<Statement>, f: &mut Flow) {
        for s in statements {
            if let Statement::Defer(d) = self.body.statement_arena[s] {
                let mut d_flow = f.clone();
                self.expr(d, &mut d_flow);
            }
        }
    }
}
