//! ALIASING: a
//! Vec or Map may not grow, shrink, reorder or rehash while a reference into
//! one of its elements is live, because the element moves and a write through
//! the old reference lands in the superseded buffer.
//!
//! Inference already rejects retaining an element reference (a `ref` local, a
//! ref return), so one is live only while the call it was passed to runs. The
//! check is therefore at that call: every function gets a summary of the
//! collections whose structure it may change, and a call that receives an
//! element reference is rejected when its summary, or a later argument, may
//! change a collection of that element type. Summaries follow parameters,
//! fields, globals and call chains by element type; closures and foreign code
//! are unknown and reject.
//!
//! The structural primitives are the stdlib functions marked `@relocates`
//! (their first parameter changes structure) plus Map index assignment and a
//! Vec `len` write.

use std::collections::BTreeMap;

use bone_core::modules::is_stdlib_module_file;
use bone_core::workspace::BodyInference;

use crate::hir::{BinOp, BindingInfo, BindingKind, Body, Expr, Function, Module, Statement, TypeCtor, TypeExpr, VarId};
use crate::prelude::*;

/// The compiler-reserved stdlib marker of a primitive that changes the
/// structure of the Vec or Map passed as its first parameter.
pub const RELOCATES_ANNOTATION: &str = "relocates";

/// Every rejection starts with this, so a session can replace the previous
/// run's rejections wholesale.
const REJECTION_PREFIX: &str = "cannot pass a reference into a relocatable Vec or Map element";

const ANNOTATION_MESSAGE: &str = "`@relocates` is reserved for the standard Vec and Map primitives";

/// A key for an unknown type: it matches every key.
const ANY_KEY: &str = "_";

/// Keys longer than this (polymorphic recursion) degrade to [`ANY_KEY`].
const MAX_KEY_LEN: usize = 256;

type FnKey = (ModuleId, Idx<Function>);

/// What a call may do to collection structure. Each entry keeps the first
/// site that established it, for the diagnostic.
#[derive(Clone, Default)]
struct Effect {
    /// Runs a closure or foreign code whose effect is not known.
    unknown: Option<Span>,
    /// Changes the structure of the collection passed as this parameter.
    params: BTreeMap<u16, Span>,
    /// Changes the structure of some other reachable collection with this
    /// element-type key (the function's generics as `$i$`).
    elements: BTreeMap<String, Span>,
    /// Changes the structure of this fresh local of the calling body. Only
    /// that body can reach it, so it never enters a summary.
    locals: BTreeMap<Idx<BindingInfo>, Span>,
}

impl Effect {
    fn add_unknown(&mut self, at: Span) {
        self.unknown.get_or_insert(at);
    }

    fn add_element(&mut self, key: String, at: Span) {
        self.elements.entry(key).or_insert(at);
    }

    /// Equal apart from witnesses. Witnesses can shift while a cycle
    /// settles; only the keys decide convergence.
    fn same_shape(&self, other: &Effect) -> bool {
        self.unknown.is_some() == other.unknown.is_some()
            && self.params.keys().eq(other.params.keys())
            && self.elements.keys().eq(other.elements.keys())
    }

    fn union(&mut self, other: &Effect) {
        if let Some(at) = other.unknown {
            self.add_unknown(at);
        }
        for (&p, &at) in &other.params {
            self.params.entry(p).or_insert(at);
        }
        for (key, &at) in &other.elements {
            self.elements.entry(key.clone()).or_insert(at);
        }
        for (&local, &at) in &other.locals {
            self.locals.entry(local).or_insert(at);
        }
    }
}

/// One argument of a call, as the caller sees it.
#[derive(Clone)]
struct Arg {
    /// A local Vec/Map built fresh in this body: only this body can reach it.
    fresh: Option<Idx<BindingInfo>>,
    /// The caller's own parameter, passed through directly.
    param: Option<u16>,
    /// The element-type key when the argument is a Vec or Map.
    element: Option<String>,
}

/// A resolved call, with what substituting the callee's summary needs.
struct Edge {
    callee: FnKey,
    /// The callee's generics, as keys in the caller.
    generics: Vec<String>,
    /// Indexed by the callee's parameter.
    args: Vec<Option<Arg>>,
}

/// The collection an element reference points into.
struct Root {
    key: String,
    local: Option<Idx<BindingInfo>>,
}

enum CallSite {
    Known(Edge),
    Unknown,
}

/// A function body's calls and its own structural writes.
struct FnFacts {
    direct: Effect,
    calls: Vec<(Idx<Expr>, CallSite)>,
}

struct BodyCtx<'a> {
    ws: &'a Workspace,
    body: &'a Body,
    bi: &'a BodyInference,
    generics: Vec<VarId>,
    params: FxHashMap<Idx<BindingInfo>, u16>,
    fresh_locals: FxHashSet<Idx<BindingInfo>>,
}


impl<'a> BodyCtx<'a> {
    fn new(ws: &'a Workspace, function: &Function, body: &'a Body, bi: &'a BodyInference) -> Self {
        let generics = function
            .generics
            .iter()
            .filter_map(|&g| match resolve(ws, None, g) {
                Some((_, TypeExpr::Generic(var, _))) => Some(var),
                _ => None,
            })
            .collect();
        let params = body
            .param_bindings
            .into_iter()
            .enumerate()
            .map(|(i, b)| (b, i as u16))
            .collect();
        let mut ctx = Self {
            ws,
            body,
            bi,
            generics,
            params,
            fresh_locals: FxHashSet::default(),
        };
        ctx.fresh_locals = ctx.collect_fresh_locals();
        ctx
    }

    /// `let` bindings whose every value is a new collection.
    fn collect_fresh_locals(&self) -> FxHashSet<Idx<BindingInfo>> {
        let mut fresh = FxHashSet::default();
        let mut stale = FxHashSet::default();
        for (_, statement) in self.body.statement_arena.iter() {
            if let Statement::Let { name, value } = *statement {
                if self.body.binding_arena[name].kind == BindingKind::Let && self.is_new_collection(value) {
                    fresh.insert(name);
                } else {
                    stale.insert(name);
                }
            }
        }
        for (_, expr) in self.body.expr_arena.iter() {
            if let Expr::Assign { target, value } = *expr
                && let Expr::Var(binding) = self.body.expr_arena[target]
                && !self.is_new_collection(value)
            {
                stale.insert(binding);
            }
        }
        fresh.retain(|b| !stale.contains(b));
        fresh
    }

    fn is_new_collection(&self, e: Idx<Expr>) -> bool {
        match self.body.expr_arena[e] {
            Expr::Array(_) => true,
            Expr::Call { .. } => self.resolved(e).is_some_and(|(mid, fid)| {
                let module = self.ws.module(mid);
                is_stdlib_module_file(module.file.as_str())
                    && matches!(module.functions[fid].name.as_str(), "vec_new" | "map_new")
            }),
            _ => false,
        }
    }

    fn resolved(&self, call: Idx<Expr>) -> Option<FnKey> {
        self.bi.resolved_overload_calls.get(&call).copied()
    }

    fn ty(&self, e: Idx<Expr>) -> Option<TyId> {
        self.bi.expr_types.get(e).copied()
    }

    fn key(&self, ty: TyId) -> String {
        let mut out = String::new();
        render_key(self.ws, Some(self.bi), &self.generics, ty, &mut out);
        if out.len() > MAX_KEY_LEN {
            ANY_KEY.to_string()
        } else {
            out
        }
    }

    /// The element-type key of a Vec or Map expression.
    fn element_key(&self, e: Idx<Expr>) -> Option<String> {
        let element = collection_element(self.ws, Some(self.bi), self.ty(e)?)?;
        Some(self.key(element))
    }

    fn arg(&self, expr: Idx<Expr>) -> Arg {
        let (fresh, param) = match self.body.expr_arena[expr] {
            Expr::Var(binding) => (
                self.fresh_locals.contains(&binding).then_some(binding),
                self.params.get(&binding).copied(),
            ),
            _ => (None, None),
        };
        Arg {
            fresh,
            param,
            element: self.element_key(expr),
        }
    }

    /// A structural write to the collection `target`.
    fn write(&self, effect: &mut Effect, target: Idx<Expr>, at: Span) {
        let arg = self.arg(target);
        if let Some(local) = arg.fresh {
            effect.locals.entry(local).or_insert(at);
        } else if let Some(p) = arg.param {
            effect.params.entry(p).or_insert(at);
        } else if let Some(key) = arg.element {
            effect.add_element(key, at);
        } else {
            effect.add_unknown(at);
        }
    }

    fn call_args(&self, call: Idx<Expr>) -> Option<Vec<Idx<Expr>>> {
        match &self.body.expr_arena[call] {
            Expr::Call { args, .. } => Some(args.clone().into_iter().collect()),
            Expr::SafeNavCall { receiver, args, .. } => {
                Some(std::iter::once(*receiver).chain(args.clone()).collect())
            }
            _ => None,
        }
    }

    /// The callee's parameter each argument fills: by name when named,
    /// positionally otherwise.
    fn bind_args(&self, callee: &Function, args: &[Idx<Expr>]) -> Vec<Option<Arg>> {
        let mut bound: Vec<Option<Arg>> = vec![None; callee.param_decls.len()];
        let mut next = 0;
        for &arg in args {
            let slot = match self.body.arg_names.get(&arg) {
                Some(name) => callee.param_decls.iter().position(|p| p.name == *name),
                None => {
                    let slot = next;
                    next += 1;
                    (slot < bound.len()).then_some(slot)
                }
            };
            if let Some(slot) = slot {
                bound[slot] = Some(self.arg(arg));
            }
        }
        bound
    }

    fn facts(&self) -> FnFacts {
        let mut direct = Effect::default();
        let mut calls = Vec::new();
        for (id, expr) in self.body.expr_arena.iter() {
            match expr {
                Expr::Call { .. } | Expr::SafeNavCall { .. } => {
                    calls.push((id, self.call_site(id)));
                }
                Expr::Assign { .. } => self.assign_write(id, &mut direct),
                _ => {}
            }
        }
        FnFacts { direct, calls }
    }

    /// Map index assignment inserts; a Vec `len` write resizes.
    fn assign_write(&self, e: Idx<Expr>, effect: &mut Effect) {
        let Expr::Assign { target, .. } = self.body.expr_arena[e] else { return };
        let at = self.body.expr_spans[e];
        match self.body.expr_arena[target] {
            Expr::Index { base, .. } if self.is_map(base) => self.write(effect, base, at),
            Expr::FieldAccess { base, field } if field.as_str() == "len" && self.is_vec(base) => {
                self.write(effect, base, at)
            }
            _ => {}
        }
    }

    fn is_map(&self, e: Idx<Expr>) -> bool {
        self.collection_ctor(e) == Some(TypeCtor::Map)
    }

    fn is_vec(&self, e: Idx<Expr>) -> bool {
        self.collection_ctor(e) == Some(TypeCtor::Array)
    }

    fn collection_ctor(&self, e: Idx<Expr>) -> Option<TypeCtor> {
        match strip_ref(self.ws, Some(self.bi), self.ty(e)?)? {
            (_, TypeExpr::App { ctor, .. }) => Some(ctor),
            _ => None,
        }
    }

    fn call_site(&self, call: Idx<Expr>) -> CallSite {
        if self.bi.fn_field_calls.contains_key(&call) || self.bi.bound_calls.contains_key(&call) {
            return CallSite::Unknown;
        }
        let Some(callee) = self.resolved(call) else {
            return CallSite::Unknown;
        };
        let function = &self.ws.module(callee.0).functions[callee.1];
        let generics = match self.bi.call_generic_bindings.get(&call) {
            Some(bindings) => bindings.iter().map(|&ty| self.key(ty)).collect(),
            None => Vec::new(),
        };
        let args = self.bind_args(function, &self.call_args(call).unwrap_or_default());
        CallSite::Known(Edge { callee, generics, args })
    }

    /// The collections an argument points into an element of, with their
    /// element keys. Retained element references are rejected by inference,
    /// so only a place expression or a ref-returning call can carry one.
    fn element_roots(&self, e: Idx<Expr>, out: &mut Vec<Root>) {
        match &self.body.expr_arena[e] {
            Expr::FieldAccess { base, .. } | Expr::SafeNav { base, .. } => self.element_roots(*base, out),
            Expr::TupleIndex { tuple, .. } => self.element_roots(*tuple, out),
            Expr::As { value, .. } => self.element_roots(*value, out),
            Expr::Index { base, .. } => match self.element_key(*base) {
                Some(key) => out.push(Root {
                    key,
                    local: self.arg(*base).fresh,
                }),
                None => self.element_roots(*base, out),
            },
            Expr::BinOp {
                op: BinOp::NullCoalesce,
                lhs,
                rhs,
            } => {
                self.element_roots(*lhs, out);
                self.element_roots(*rhs, out);
            }
            Expr::Block { tail: Some(tail), .. } => self.element_roots(*tail, out),
            Expr::If {
                then_branch,
                else_branch,
                ..
            }
            | Expr::IfLet {
                then_branch,
                else_branch,
                ..
            } => {
                self.element_roots(*then_branch, out);
                if let Some(else_branch) = else_branch {
                    self.element_roots(*else_branch, out);
                }
            }
            Expr::Match { arms, .. } => {
                for arm in arms.clone() {
                    self.element_roots(self.body.match_arm_arena[arm].body, out);
                }
            }
            Expr::Call { .. } | Expr::SafeNavCall { .. } => {
                for &arg in self.bi.ref_call_args.get(&e).into_iter().flatten() {
                    self.element_roots(arg, out);
                }
            }
            _ => {}
        }
    }
}

fn resolve(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> Option<(TyId, TypeExpr)> {
    let mut cur = ty;
    loop {
        let expr = match cur {
            TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
            TyId::Local(idx) => bi?.ty_arena[idx],
        };
        match expr {
            TypeExpr::Infer(var, _) => match bi.and_then(|bi| bi.substitutions.get(var)) {
                Some(next) => cur = next,
                None => return Some((cur, expr)),
            },
            _ => return Some((cur, expr)),
        }
    }
}

/// `ty` under any `ref` and `?`.
fn strip_ref(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> Option<(TyId, TypeExpr)> {
    let (base, expr) = resolve(ws, bi, ty)?;
    match expr {
        TypeExpr::Nullable(inner) => strip_ref(ws, bi, base.with_idx(inner)),
        TypeExpr::App {
            ctor: TypeCtor::Ref,
            params,
        } => strip_ref(ws, bi, base.with_idx(params.into_iter().next()?)),
        _ => Some((base, expr)),
    }
}

/// The element type of a Vec, or the value type of a Map.
fn collection_element(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> Option<TyId> {
    match strip_ref(ws, bi, ty)? {
        (base, TypeExpr::App {
            ctor: TypeCtor::Array | TypeCtor::Map,
            params,
        }) => Some(base.with_idx(params.into_iter().last()?)),
        _ => None,
    }
}

fn render_key(ws: &Workspace, bi: Option<&BodyInference>, generics: &[VarId], ty: TyId, out: &mut String) {
    if out.len() > MAX_KEY_LEN {
        return;
    }
    let Some((base, expr)) = resolve(ws, bi, ty) else {
        out.push_str(ANY_KEY);
        return;
    };
    let list = |out: &mut String, open: &str, items: IdxRange<TypeExpr>, close: &str| {
        out.push_str(open);
        for (i, item) in items.into_iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            render_key(ws, bi, generics, base.with_idx(item), out);
        }
        out.push_str(close);
    };
    match expr {
        TypeExpr::Builtin(builtin) => out.push_str(&format!("{builtin:?}")),
        TypeExpr::Generic(var, _) => match generics.iter().position(|&g| g == var) {
            Some(i) => out.push_str(&format!("${i}$")),
            None => out.push_str(ANY_KEY),
        },
        TypeExpr::Infer(..) => out.push_str(ANY_KEY),
        TypeExpr::Nullable(inner) => {
            render_key(ws, bi, generics, base.with_idx(inner), out);
            out.push('?');
        }
        TypeExpr::App { ctor, params } => {
            let name = match ctor {
                TypeCtor::Array => "Vec",
                TypeCtor::Map => "Map",
                TypeCtor::Ref => "ref",
                TypeCtor::Nominal(name) => name.as_str(),
            };
            list(out, &format!("{name}<"), params, ">");
        }
        TypeExpr::Function { params, ret } => {
            list(out, "fn(", params, ")->");
            render_key(ws, bi, generics, base.with_idx(ret), out);
        }
        TypeExpr::Tuple { elements } => list(out, "(", elements, ")"),
    }
}

/// `key` with the callee's `$i$` replaced by the caller's bindings.
fn substitute(key: &str, generics: &[String]) -> String {
    let mut out = String::new();
    let mut rest = key;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let Some(end) = tail.find('$') else {
            return ANY_KEY.to_string();
        };
        let binding = tail[..end].parse::<usize>().ok().and_then(|i| generics.get(i));
        out.push_str(binding.map_or(ANY_KEY, String::as_str));
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    if out.len() > MAX_KEY_LEN {
        ANY_KEY.to_string()
    } else {
        out
    }
}

/// Keys may alias when equal or when either is only partly known (an
/// unresolved type, or a generic of the caller).
fn keys_may_alias(a: &str, b: &str) -> bool {
    a == b || a.contains(ANY_KEY) || b.contains(ANY_KEY) || a.contains('$') || b.contains('$')
}

/// The callee's summary as seen from one call.
fn map_edge(edge: &Edge, callee: &Effect, at: Span) -> Effect {
    let mut mapped = Effect::default();
    if let Some(witness) = callee.unknown {
        mapped.add_unknown(witness);
    }
    for (&p, &witness) in &callee.params {
        match edge.args.get(p as usize) {
            Some(Some(Arg { fresh: Some(local), .. })) => {
                mapped.locals.entry(*local).or_insert(witness);
            }
            Some(Some(arg)) => {
                if let Some(q) = arg.param {
                    mapped.params.entry(q).or_insert(witness);
                }
                match &arg.element {
                    Some(key) => mapped.add_element(key.clone(), witness),
                    None if arg.param.is_none() => mapped.add_unknown(witness),
                    None => {}
                }
            }
            // A defaulted parameter is a value the callee builds itself.
            Some(None) => {}
            None => mapped.add_unknown(at),
        }
    }
    for (key, &witness) in &callee.elements {
        mapped.add_element(substitute(key, &edge.generics), witness);
    }
    mapped
}

fn bodiless_summary(ws: &Workspace, key: FnKey) -> Effect {
    let module = ws.module(key.0);
    let function = &module.functions[key.1];
    let mut effect = Effect::default();
    if function.annotations.iter().any(|a| a.is_builtin(RELOCATES_ANNOTATION)) {
        effect.params.insert(0, function.span);
    } else if !is_stdlib_module_file(module.file.as_str())
        || function
            .param_decls
            .iter()
            .any(|p| matches!(resolve(ws, None, p.ty), Some((_, TypeExpr::Function { .. }))))
    {
        // Foreign code, or a stdlib primitive that calls back into a closure.
        effect.add_unknown(function.span);
    }
    effect
}

/// The element keys worth tracking: those some element reference points
/// into, plus keys still generic or unknown.
#[derive(Default)]
struct Relevance {
    keys: FxHashSet<String>,
    all: bool,
}

impl Relevance {
    fn add(&mut self, key: &str) {
        if key.contains('$') || key.contains(ANY_KEY) {
            self.all = true;
        }
        self.keys.insert(key.to_string());
    }

    fn keeps(&self, key: &str) -> bool {
        self.all || key.contains('$') || key.contains(ANY_KEY) || self.keys.contains(key)
    }

    /// An unknown effect already rejects every reference, so the rest is
    /// dropped with it.
    fn trim(&self, effect: &mut Effect) {
        if effect.unknown.is_some() {
            effect.params.clear();
            effect.elements.clear();
            effect.locals.clear();
        } else {
            effect.elements.retain(|key, _| self.keeps(key));
        }
    }
}

/// Summaries are computed on demand: only for the callees, transitively, of
/// bodies that pass an element reference somewhere.
struct Program<'a> {
    ws: &'a Workspace,
    relevance: Relevance,
    sites: Vec<FnKey>,
    facts: FxHashMap<FnKey, FnFacts>,
    summaries: FxHashMap<FnKey, Effect>,
}

fn inferred_body<'a>(ws: &'a Workspace, key: FnKey) -> Option<(&'a Function, &'a Body, &'a BodyInference)> {
    let module = ws.module(key.0);
    let function = &module.functions[key.1];
    if function.inline_body.is_some() || function.is_extern {
        return None;
    }
    let owner = BodyOwner::Function(key.1);
    let body = module.bodies.get(&owner)?;
    let bi = ws.inference(key.0)?.per_fn.get(&owner)?;
    Some((function, body, bi))
}

impl<'a> Program<'a> {
    fn build(ws: &'a Workspace) -> Self {
        let mut program = Self {
            ws,
            relevance: Relevance::default(),
            sites: Vec::new(),
            facts: FxHashMap::default(),
            summaries: FxHashMap::default(),
        };
        for module in ws.modules.iter() {
            for (fid, _) in module.functions.iter() {
                let key = (module.id, fid);
                let Some((function, body, bi)) = inferred_body(ws, key) else { continue };
                if bi.ref_call_args.is_empty() {
                    continue;
                }
                let ctx = BodyCtx::new(ws, function, body, bi);
                let mut roots = Vec::new();
                for &arg in bi.ref_call_args.values().flatten() {
                    ctx.element_roots(arg, &mut roots);
                }
                if roots.is_empty() {
                    continue;
                }
                for root in &roots {
                    program.relevance.add(&root.key);
                }
                program.sites.push(key);
            }
        }
        let mut pending = program.sites.clone();
        while let Some(key) = pending.pop() {
            if program.facts.contains_key(&key) {
                continue;
            }
            let Some((function, body, bi)) = inferred_body(ws, key) else { continue };
            let facts = BodyCtx::new(ws, function, body, bi).facts();
            pending.extend(facts.calls.iter().filter_map(|(_, site)| match site {
                CallSite::Known(edge) => Some(edge.callee),
                CallSite::Unknown => None,
            }));
            program.facts.insert(key, facts);
        }
        program.solve();
        program
    }

    fn summary(&self, key: FnKey) -> Effect {
        match self.summaries.get(&key) {
            Some(effect) => effect.clone(),
            None if self.facts.contains_key(&key) => Effect::default(),
            None => bodiless_summary(self.ws, key),
        }
    }

    fn call_effect(&self, body: &Body, call: Idx<Expr>, site: &CallSite) -> Effect {
        let at = body.expr_spans[call];
        let mut effect = match site {
            CallSite::Known(edge) => map_edge(edge, &self.summary(edge.callee), at),
            CallSite::Unknown => {
                let mut effect = Effect::default();
                effect.add_unknown(at);
                effect
            }
        };
        self.relevance.trim(&mut effect);
        effect
    }

    /// Summaries only grow, and keys are bounded, so the worklist drains.
    fn solve(&mut self) {
        let mut callers: FxHashMap<FnKey, Vec<FnKey>> = FxHashMap::default();
        for (&caller, facts) in &self.facts {
            for (_, site) in &facts.calls {
                if let CallSite::Known(edge) = site {
                    callers.entry(edge.callee).or_default().push(caller);
                }
            }
        }
        let mut pending: Vec<FnKey> = self.facts.keys().copied().collect();
        let mut queued: FxHashSet<FnKey> = pending.iter().copied().collect();
        while let Some(key) = pending.pop() {
            queued.remove(&key);
            let body = &self.ws.module(key.0).bodies[&BodyOwner::Function(key.1)];
            let facts = &self.facts[&key];
            let mut effect = facts.direct.clone();
            for (call, site) in &facts.calls {
                if effect.unknown.is_some() {
                    break;
                }
                effect.union(&self.call_effect(body, *call, site));
            }
            effect.locals.clear();
            self.relevance.trim(&mut effect);
            if self.summaries.get(&key).is_some_and(|old| old.same_shape(&effect)) {
                continue;
            }
            self.summaries.insert(key, effect);
            for &caller in callers.get(&key).into_iter().flatten() {
                if queued.insert(caller) {
                    pending.push(caller);
                }
            }
        }
    }

    fn rejections(&self, key: FnKey) -> Vec<CompilerError> {
        let (Some(facts), Some((function, body, bi))) = (self.facts.get(&key), inferred_body(self.ws, key)) else {
            return Vec::new();
        };
        let ctx = BodyCtx::new(self.ws, function, body, bi);
        let effects: FxHashMap<Idx<Expr>, Effect> = facts
            .calls
            .iter()
            .map(|(call, site)| (*call, self.call_effect(body, *call, site)))
            .collect();
        let mut errors = Vec::new();
        for (call, _) in &facts.calls {
            let Some(ref_args) = bi.ref_call_args.get(call) else { continue };
            for &ref_arg in ref_args {
                let mut roots = Vec::new();
                ctx.element_roots(ref_arg, &mut roots);
                if roots.is_empty() {
                    continue;
                }
                let effect = self.effect_while_live(&ctx, &effects, *call, ref_arg);
                if let Some(error) = rejection(&ctx, *call, ref_arg, &roots, &effect) {
                    errors.push(error);
                }
            }
        }
        errors
    }

    /// What may run while `ref_arg` is live: the call itself and the
    /// arguments evaluated after it.
    fn effect_while_live(
        &self,
        ctx: &BodyCtx,
        effects: &FxHashMap<Idx<Expr>, Effect>,
        call: Idx<Expr>,
        ref_arg: Idx<Expr>,
    ) -> Effect {
        let mut effect = effects[&call].clone();
        let taken = ctx.body.expr_spans[ref_arg];
        for arg in ctx.call_args(call).unwrap_or_default() {
            let span = ctx.body.expr_spans[arg];
            if span.offset_range.0 <= taken.offset_range.0 {
                continue;
            }
            for (id, _) in ctx.body.expr_arena.iter() {
                let inner = ctx.body.expr_spans[id];
                if !contains(span, inner) {
                    continue;
                }
                if let Some(nested) = effects.get(&id) {
                    effect.union(nested);
                }
            }
            for (id, _) in ctx.body.expr_arena.iter() {
                if contains(span, ctx.body.expr_spans[id]) {
                    ctx.assign_write(id, &mut effect);
                }
            }
        }
        effect
    }
}

fn contains(outer: Span, inner: Span) -> bool {
    outer.file == inner.file
        && outer.offset_range.0 <= inner.offset_range.0
        && inner.offset_range.1 <= outer.offset_range.1
}

fn callee_name(ctx: &BodyCtx, call: Idx<Expr>) -> String {
    match ctx.resolved(call) {
        Some((mid, fid)) => ctx.ws.module(mid).functions[fid].name.to_string(),
        None => "this callee".to_string(),
    }
}

fn rejection(
    ctx: &BodyCtx,
    call: Idx<Expr>,
    ref_arg: Idx<Expr>,
    roots: &[Root],
    effect: &Effect,
) -> Option<CompilerError> {
    let at = ctx.body.expr_spans[ref_arg];
    let callee = callee_name(ctx, call);
    let remedy = "copy the element out and write it back after the call, or keep data that is referenced across calls in an Arena";
    if let Some(witness) = effect.unknown {
        return Some(
            CompilerError::new_with_label(
                at,
                format!(
                    "{REJECTION_PREFIX} to `{callee}`: the call runs a closure or foreign function whose effect on the collection is unknown; {remedy}"
                ),
            )
            .with_label(witness, "the call may run unknown code here".to_string()),
        );
    }
    for root in roots {
        let local = root.local.and_then(|local| effect.locals.get(&local));
        let element = effect.elements.iter().find(|(key, _)| keys_may_alias(&root.key, key));
        let (key, witness) = match (local, element) {
            (Some(&witness), _) => (root.key.as_str(), witness),
            (None, Some((key, &witness))) => (key.as_str(), witness),
            (None, None) => continue,
        };
        return Some(
            CompilerError::new_with_label(
                at,
                format!(
                    "{REJECTION_PREFIX} to `{callee}`: the call may grow, shrink, reorder or rehash a collection of `{key}` while the reference is live; {remedy}"
                ),
            )
            .with_label(witness, format!("a collection of `{key}` may change structure here")),
        );
    }
    None
}

fn annotation_errors(module: &Module) -> Vec<(BodyOwner, CompilerError)> {
    if is_stdlib_module_file(module.file.as_str()) {
        return Vec::new();
    }
    let mut errors = Vec::new();
    for (fid, function) in module.functions.iter() {
        for annotation in function.annotations.iter().filter(|a| a.is_builtin(RELOCATES_ANNOTATION)) {
            errors.push((
                BodyOwner::Function(fid),
                CompilerError::new_with_label(annotation.span, ANNOTATION_MESSAGE.to_string()),
            ));
        }
    }
    errors
}

/// Replace the previous run's rejections with this one's, on the calling
/// body. Whole-program: a callee's summary can change a caller's verdict.
pub fn check_relocation(ws: &mut Workspace) {
    for inference in ws.inferences.iter_mut().flatten() {
        inference.errors.retain(|_, errors| {
            errors.retain(|e| !e.message.starts_with(REJECTION_PREFIX) && e.message != ANNOTATION_MESSAGE);
            !errors.is_empty()
        });
    }
    let found: Vec<(ModuleId, BodyOwner, CompilerError)> = {
        let program = Program::build(ws);
        let mut found = Vec::new();
        for module in ws.modules.iter() {
            for (owner, error) in annotation_errors(module) {
                found.push((module.id, owner, error));
            }
        }
        for &key in &program.sites {
            for error in program.rejections(key) {
                found.push((key.0, BodyOwner::Function(key.1), error));
            }
        }
        found
    };
    for (mid, owner, error) in found {
        if let Some(Some(inference)) = ws.inferences.get_mut(mid.index()) {
            inference.errors.entry(owner).or_default().push(error);
        }
    }
}
