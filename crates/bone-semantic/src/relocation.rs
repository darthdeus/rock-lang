//! Structural changes can invalidate live Vec or Map element references.
//! Opaque calls can change any collection.

use std::collections::{BTreeMap, BTreeSet};

use bone_core::hir_walk::HirChild;
use bone_core::modules::is_stdlib_module_file;
use bone_core::suspension::{
    SuspensionPoint::{self, Expression, IteratorNext},
    SuspensionPolicy, uses_after_suspension_in,
};
use bone_core::workspace::{BodyInference, BoundNext};

use crate::hir::{
    BinOp, BindingInfo, BindingKind, Body, BuiltinTy, Expr, Function, Module, Pattern, Statement,
    TypeCtor, TypeExpr, VarId, VariantPayload,
};
use crate::prelude::*;

/// The compiler-reserved stdlib marker of a primitive that changes the
/// structure of the Vec or Map passed as its first parameter.
pub(crate) const RELOCATES_ANNOTATION: &str = "relocates";

/// Every rejection starts with this, so a session can replace the previous
/// run's rejections wholesale.
const REJECTION_PREFIX: &str = "cannot pass a reference into a relocatable Vec or Map element";
const ARENA_REJECTION: &str = "Arena value borrow may be used after invalidation";

const ANNOTATION_MESSAGE: &str = "`@relocates` is reserved for the standard Vec and Map primitives";

/// A key for an unknown type: it matches every key. This and [`GENERIC`]
/// are control characters so that no type or package name can contain them.
const ANY_KEY: &str = "\x01";
/// Delimits the position of one of the function's generics.
const GENERIC: char = '\x02';

/// Keys longer than this (polymorphic recursion) degrade to [`ANY_KEY`].
const MAX_KEY_LEN: usize = 256;

type FnKey = (ModuleId, Idx<Function>);
type BodyKey = (ModuleId, BodyOwner);

fn function_body((module, function): FnKey) -> BodyKey {
    (module, BodyOwner::Function(function))
}

/// Each effect keeps the first site that established it, for the diagnostic.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum EffectOn {
    Unknown,
    Host,
    HostParam(u16),
    Retained(u16),
    Param(u16),
    Local(Idx<BindingInfo>),
    Element(String),
}

use EffectOn::{Element, HostParam, Local, Param, Retained, Unknown};

#[derive(Clone, Default)]
struct Effect(BTreeMap<EffectOn, Span>);

impl Effect {
    fn add(&mut self, effect: EffectOn, at: Span) {
        self.0.entry(effect).or_insert(at);
    }

    fn add_host_access(&mut self, origin: CollectionOrigin, at: Span) {
        match origin {
            CollectionOrigin::Rock => {}
            CollectionOrigin::Host => self.add(EffectOn::Host, at),
            CollectionOrigin::Param(p) => self.add(HostParam(p), at),
        }
    }

    fn union(&mut self, other: &Effect) {
        for (effect, &at) in &other.0 {
            self.add(effect.clone(), at);
        }
    }

    fn invalidating<'a>(&'a self, root: &Root) -> Option<(&'a EffectOn, Span)> {
        self.0.iter().find_map(|(effect, &witness)| {
            let applies = match effect {
                Unknown => true,
                EffectOn::Host | HostParam(_) => root.key.starts_with("Arena<"),
                Local(binding) => root.local == Some(*binding),
                Element(key) => keys_may_alias(&root.key, key),
                Retained(_) | Param(_) => false,
            };
            applies.then_some((effect, witness))
        })
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum CollectionOrigin {
    #[default]
    Rock,
    Param(u16),
    Host,
}

impl CollectionOrigin {
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Rock, origin) | (origin, Self::Rock) => origin,
            (a, b) if a == b => a,
            _ => Self::Host,
        }
    }
}

#[derive(Default)]
struct CollectionSources {
    values: FxHashMap<Idx<BindingInfo>, Vec<Idx<Expr>>>,
    stored: FxHashMap<Idx<BindingInfo>, Vec<(Idx<Expr>, Idx<Expr>)>>,
    escaped: FxHashMap<Idx<BindingInfo>, usize>,
    points: Option<ArenaMap<Idx<Expr>, usize>>,
}

impl CollectionSources {
    fn point(&self, expr: Idx<Expr>) -> Option<usize> {
        self.points.as_ref()?.get(expr).copied()
    }
}

fn storage_root(body: &Body, mut place: Idx<Expr>) -> Expr {
    loop {
        match body.expr_arena[place] {
            Expr::FieldAccess { base, .. }
            | Expr::SafeNav { base, .. }
            | Expr::Index { base, .. } => {
                place = base;
            }
            Expr::TupleIndex { tuple, .. } => place = tuple,
            root => return root,
        }
    }
}

#[derive(Clone)]
struct Arg {
    expr: Idx<Expr>,
    collection_origin: CollectionOrigin,
    ref_params: BTreeSet<u16>,
    /// A local Vec/Map built fresh in this body: only this body can reach it.
    fresh: Option<Idx<BindingInfo>>,
    /// The caller's own parameter, passed through directly.
    param: Option<u16>,
    /// The element-type key when the argument is a Vec or Map.
    element: Option<String>,
}

struct Edge {
    callee: BodyKey,
    /// The callee's generics, as keys in the caller.
    generics: Vec<String>,
    /// Indexed by the callee's parameter.
    args: Vec<Option<Arg>>,
}

#[derive(Clone)]
struct Root {
    key: String,
    local: Option<Idx<BindingInfo>>,
}

enum Operation {
    Call(Option<Edge>),
    Implicit(Option<Edge>),
    HostAccess(CollectionOrigin),
}

type EffectsAt = FxHashMap<SuspensionPoint, Effect>;

struct FnFacts {
    direct: Effect,
    operations: Vec<(SuspensionPoint, Operation)>,
}

impl FnFacts {
    fn callees(&self) -> impl Iterator<Item = BodyKey> {
        self.operations.iter().filter_map(|(_, op)| match op {
            Operation::Call(Some(edge)) | Operation::Implicit(Some(edge)) => Some(edge.callee),
            _ => None,
        })
    }
}

#[derive(Default)]
struct HostRecords {
    types: FxHashSet<Ustr>,
    host_boundary: bool,
    escaping_calls: FxHashSet<FnKey>,
}

impl HostRecords {
    fn collect(ws: &Workspace) -> Self {
        let mut records = Self::default();
        let mut pending = Vec::new();
        let user_modules = || {
            ws.modules
                .iter()
                .filter(|m| !is_stdlib_module_file(m.file.as_str()))
        };
        let host_entry_points: FxHashSet<_> = user_modules()
            .flat_map(|m| {
                m.functions
                    .iter()
                    .filter(|(_, f)| Self::has_host_boundary(f))
                    .map(|(f, _)| (m.id, f))
            })
            .collect();
        if host_entry_points.is_empty() {
            return records;
        }
        records.host_boundary = true;
        let mut callers: FxHashMap<FnKey, Vec<FnKey>> = FxHashMap::default();
        for module in &ws.modules {
            let trusted = is_stdlib_module_file(module.file.as_str());
            if !trusted {
                // Host entry points can reach the Lua global directory by name.
                for global in module.globals.values() {
                    records.add(ws, None, global.ty, &mut pending);
                }
            }
            for (id, function) in module.functions.iter() {
                let opaque = function.inline_body.is_some() || function.is_extern;
                let stores_alias = function.param_decls.len() > 1
                    && function.annotations.iter().any(|a| {
                        a.is_builtin(RELOCATES_ANNOTATION) || a.is_builtin("writes_elems")
                    });
                if opaque && (!trusted || stores_alias) {
                    records.escaping_calls.insert((module.id, id));
                }
                if host_entry_points.contains(&(module.id, id)) {
                    for ty in function
                        .param_decls
                        .iter()
                        .map(|p| p.ty)
                        .chain(function.return_ty)
                    {
                        records.add(ws, None, ty, &mut pending);
                    }
                }
            }
            let Some(inference) = ws.inference(module.id) else {
                continue;
            };
            for (&owner, bi) in &inference.per_fn {
                let Some(body) = module.bodies.get(&owner) else {
                    continue;
                };
                if let BodyOwner::Function(id) = owner
                    && body.expr_arena.values().any(|expr| {
                        matches!(*expr, Expr::Assign { target, value }
                            if !matches!(body.expr_arena[target], Expr::Var(_))
                                && bi.expr_types.get(value).is_some_and(|&ty| {
                                    records.carries_collection(ws, bi, ty)
                                        || matches!(ws.resolve_ty(Some(bi), ty), Some((_, TypeExpr::Generic(..))))
                                }))
                    })
                {
                    records.escaping_calls.insert((module.id, id));
                }
                if !trusted {
                    for &(_, ty) in &bi.any_erasures {
                        records.add(ws, Some(bi), ty, &mut pending);
                    }
                }
                for (&site, &(mid, fid)) in &bi.resolved_overload_calls {
                    let callee_module = ws.module(mid);
                    let callee = &callee_module.functions[fid];
                    let stdlib = is_stdlib_module_file(callee_module.file.as_str());
                    if let BodyOwner::Function(caller) = owner {
                        callers
                            .entry((mid, fid))
                            .or_default()
                            .push((module.id, caller));
                    }
                    let foreign = host_entry_points.contains(&(mid, fid));
                    let cast = stdlib && matches!(callee.name.as_str(), "cast" | "transmute");
                    if trusted || (!foreign && !cast) {
                        continue;
                    }
                    if let Some(&ty) = bi.expr_types.get(site) {
                        records.add(ws, Some(bi), ty, &mut pending);
                    }
                    let (receiver, args) = match body.expr_arena[site] {
                        Expr::Call { args, .. } => (None, args),
                        Expr::SafeNavCall { receiver, args, .. } => (Some(receiver), args),
                        _ => continue,
                    };
                    for arg in receiver.into_iter().chain(args) {
                        if let Some(&ty) = bi.expr_types.get(arg) {
                            records.add(ws, Some(bi), ty, &mut pending);
                        }
                    }
                }
            }
        }
        // An opaque call or an aggregate store can retain a supplied alias.
        let mut escaping_calls: Vec<_> = records.escaping_calls.iter().copied().collect();
        while let Some(callee) = escaping_calls.pop() {
            for &caller in callers.get(&callee).into_iter().flatten() {
                if records.escaping_calls.insert(caller) {
                    escaping_calls.push(caller);
                }
            }
        }
        while let Some(name) = pending.pop() {
            let mut add = |ty| records.add(ws, None, ty, &mut pending);
            match ws.nominals.get(&name) {
                Some(Nominal::Struct(id)) => {
                    for field in &ws.module(id.module).structs[id.def].fields {
                        add(field.ty);
                    }
                }
                Some(Nominal::Enum(id)) => {
                    for variant in &ws.module(id.module).enums[id.def].variants {
                        match &variant.payload {
                            VariantPayload::Unit => {}
                            VariantPayload::Tuple(types) => {
                                types.iter().copied().for_each(&mut add)
                            }
                            VariantPayload::Record(fields) => fields.iter().for_each(|f| add(f.ty)),
                        }
                    }
                }
                Some(Nominal::Newtype(id)) => add(ws.module(id.module).newtypes[id.def].wrapped),
                None => {}
            }
        }
        records
    }

    fn has_host_boundary(function: &Function) -> bool {
        function.is_extern
            || function.inline_body.is_some()
            || function
                .annotations
                .iter()
                .any(|a| a.is_builtin("nomangle"))
    }

    fn add(
        &mut self,
        ws: &Workspace,
        bi: Option<&BodyInference>,
        ty: TyId,
        pending: &mut Vec<Ustr>,
    ) {
        ws.type_any(bi, ty, &mut |node| {
            if let TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            } = node
                && self.types.insert(name)
            {
                pending.push(name);
            }
            false
        });
    }

    fn carries_collection(&self, ws: &Workspace, bi: &BodyInference, ty: TyId) -> bool {
        collection_or_any(ws, Some(bi), ty)
            || match strip_ref(ws, Some(bi), ty) {
                Some((_, TypeExpr::Tuple { .. } | TypeExpr::Function { .. })) => true,
                Some((
                    _,
                    TypeExpr::App {
                        ctor: TypeCtor::Nominal(name),
                        ..
                    },
                )) => self.types.contains(&name),
                _ => false,
            }
    }

    fn may_callback(&self, ws: &Workspace, bi: &BodyInference, ty: TyId) -> bool {
        !self.types.is_empty()
            && matches!(strip_ref(ws, Some(bi), ty),
                Some((_, TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }))
                    if self.types.contains(&name) && ws.is_identity_struct(name))
    }
}

struct BodyCtx<'a> {
    ws: &'a Workspace,
    body: &'a Body,
    bi: &'a BodyInference,
    generics: Vec<VarId>,
    params: FxHashMap<Idx<BindingInfo>, u16>,
    fresh_locals: FxHashSet<Idx<BindingInfo>>,
    ref_values: FxHashMap<Idx<BindingInfo>, Vec<Idx<Expr>>>,
    arena_remove: Option<Span>,
    arena_storage: bool,
    collection_sources: Option<CollectionSources>,
    root_cache: std::cell::RefCell<FxHashMap<Idx<BindingInfo>, Vec<Root>>>,
    return_origins: &'a ReturnOrigins,
}

impl<'a> BodyCtx<'a> {
    fn new(
        ws: &'a Workspace,
        function: Option<&Function>,
        body: &'a Body,
        bi: &'a BodyInference,
        return_origins: &'a ReturnOrigins,
    ) -> Self {
        let generics = function
            .into_iter()
            .flat_map(|f| &f.generics)
            .filter_map(|&g| match ws.resolve_ty(None, g) {
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
            ref_values: FxHashMap::default(),
            root_cache: Default::default(),
            collection_sources: None,
            arena_storage: function.is_some_and(|f| {
                bone_core::modules::stdlib_file_name(f.span.file.as_str()) == Some("arena.rock")
            }),
            return_origins,
            arena_remove: function
                .filter(|f| {
                    f.name.as_str() == "remove"
                        && is_stdlib_module_file(f.span.file.as_str())
                        && f.param_decls
                            .first()
                            .is_some_and(|p| arena_element(ws, None, p.ty).is_some())
                })
                .map(|f| f.span),
        };
        ctx.fresh_locals = ctx.collect_fresh_locals();
        ctx
    }

    /// `let` bindings whose every value is a new collection.
    fn collect_fresh_locals(&mut self) -> FxHashSet<Idx<BindingInfo>> {
        let mut fresh = FxHashSet::default();
        let mut stale = FxHashSet::default();
        for (_, statement) in self.body.statement_arena.iter() {
            if let Statement::Let { pattern, value, .. } = *statement {
                if let Some(name) = self.body.pattern_single_bind(pattern) {
                    self.record_ref(name, value);
                    if self.body.binding_arena[name].kind == BindingKind::Let
                        && self.is_new_collection(value)
                    {
                        fresh.insert(name);
                    } else {
                        stale.insert(name);
                    }
                }
            }
        }
        for (_, expr) in self.body.expr_arena.iter() {
            match *expr {
                Expr::Assign { target, value } => {
                    if let Expr::Var(binding) = self.body.expr_arena[target] {
                        if !self.is_new_collection(value) {
                            stale.insert(binding);
                        }
                        self.record_ref(binding, value);
                    }
                }
                _ => {}
            }
        }
        fresh.retain(|b| !stale.contains(b));
        fresh
    }

    fn record_ref(&mut self, binding: Idx<BindingInfo>, value: Idx<Expr>) {
        if self
            .bi
            .binding_types
            .get(binding)
            .is_some_and(|&ty| ref_target(self.ws, self.bi, ty).is_some())
        {
            self.ref_values.entry(binding).or_default().push(value);
        }
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
        if let Some(element) = arena_element(self.ws, Some(self.bi), self.ty(e)?) {
            return Some(format!("Arena<{}>", self.key(element)));
        }
        let element = collection_element(self.ws, Some(self.bi), self.ty(e)?)?;
        Some(self.key(element))
    }

    fn place_binding(&self, expr: Idx<Expr>) -> (Option<Idx<BindingInfo>>, Option<u16>) {
        match self.body.expr_arena[expr] {
            Expr::Var(binding) => (
                self.fresh_locals.contains(&binding).then_some(binding),
                self.params.get(&binding).copied(),
            ),
            _ => (None, None),
        }
    }

    fn arg(&self, expr: Idx<Expr>, at: Idx<Expr>) -> Arg {
        let (fresh, param) = self.place_binding(expr);
        let mut ref_params = BTreeSet::new();
        self.ref_params(expr, &mut ref_params, &mut FxHashSet::default());
        Arg {
            expr,
            collection_origin: self.collection_origin(expr, at),
            ref_params,
            fresh,
            param,
            element: self.element_key(expr),
        }
    }

    fn for_each_value_carrier(&self, e: Idx<Expr>, visit: &mut impl FnMut(Idx<Expr>)) {
        match &self.body.expr_arena[e] {
            Expr::FieldAccess { base, .. }
            | Expr::SafeNav { base, .. }
            | Expr::TupleIndex { tuple: base, .. }
            | Expr::As { value: base, .. } => visit(*base),
            Expr::Block {
                tail: Some(tail), ..
            } => visit(*tail),
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
                visit(*then_branch);
                if let Some(branch) = else_branch {
                    visit(*branch);
                }
            }
            Expr::Match { arms, .. } => {
                for arm in *arms {
                    visit(self.body.match_arm_arena[arm].body);
                }
            }
            Expr::BinOp {
                op: BinOp::NullCoalesce,
                lhs,
                rhs,
            } => {
                visit(*lhs);
                visit(*rhs);
            }
            _ => {}
        }
    }

    fn ref_params(&self, e: Idx<Expr>, out: &mut BTreeSet<u16>, seen: &mut FxHashSet<Idx<Expr>>) {
        if !seen.insert(e) {
            return;
        }
        match self.body.expr_arena[e] {
            Expr::Var(binding) => {
                if let Some(&param) = self.params.get(&binding) {
                    out.insert(param);
                }
                for &value in self.ref_values.get(&binding).into_iter().flatten() {
                    self.ref_params(value, out, seen);
                }
            }
            Expr::Call { .. } | Expr::SafeNavCall { .. } => {
                for &arg in self.bi.ref_call_args.get(&e).into_iter().flatten() {
                    self.ref_params(arg, out, seen);
                }
            }
            _ => self.for_each_value_carrier(e, &mut |child| self.ref_params(child, out, seen)),
        }
    }

    fn write(&self, effect: &mut Effect, target: Idx<Expr>, at: Span) {
        let (fresh, param) = self.place_binding(target);
        let target = fresh
            .map(Local)
            .or_else(|| param.map(Param))
            .or_else(|| self.element_key(target).map(Element));
        effect.add(target.unwrap_or(Unknown), at);
    }

    fn call_args(&self, call: Idx<Expr>) -> impl Iterator<Item = Idx<Expr>> {
        let (leading, args) = match self.body.expr_arena[call] {
            Expr::Call { args, .. } => ([None, None], args),
            Expr::SafeNavCall { receiver, args, .. } => ([Some(receiver), None], args),
            Expr::CompoundAssign { target, value, .. } => {
                ([Some(target), Some(value)], IdxRange::new_empty())
            }
            _ => unreachable!("call arguments requested for a non-call"),
        };
        leading.into_iter().flatten().chain(args)
    }

    /// The callee's parameter each argument fills: by name when named,
    /// positionally otherwise.
    fn bind_args(&self, callee: &Function, call: Idx<Expr>) -> Vec<Option<Arg>> {
        let mut bound: Vec<Option<Arg>> = vec![None; callee.param_decls.len()];
        let mut next = 0;
        for arg in self.call_args(call) {
            let slot = match self.body.arg_names.get(&arg) {
                Some(name) => callee.param_decls.iter().position(|p| p.name == *name),
                None => {
                    let slot = next;
                    next += 1;
                    (slot < bound.len()).then_some(slot)
                }
            };
            if let Some(slot) = slot {
                bound[slot] = Some(self.arg(arg, call));
            }
        }
        bound
    }

    fn collection_origin(&self, expr: Idx<Expr>, at: Idx<Expr>) -> CollectionOrigin {
        let Some(sources) = &self.collection_sources else {
            return CollectionOrigin::Rock;
        };
        if !self
            .ty(expr)
            .is_some_and(|ty| collection_or_any(self.ws, Some(self.bi), ty))
        {
            return CollectionOrigin::Rock;
        }
        self.collection_origin_inner(
            expr,
            sources,
            sources.point(at).unwrap_or(usize::MAX),
            &mut FxHashSet::default(),
        )
    }

    fn collection_origin_inner(
        &self,
        expr: Idx<Expr>,
        sources: &CollectionSources,
        at: usize,
        seen: &mut FxHashSet<Idx<Expr>>,
    ) -> CollectionOrigin {
        use CollectionOrigin::{Host, Param, Rock};
        if !seen.insert(expr) {
            return Rock;
        }
        match self.body.expr_arena[expr] {
            Expr::Var(binding) => {
                if sources
                    .escaped
                    .get(&binding)
                    .is_some_and(|&escape| escape <= at)
                {
                    return Host;
                }
                let param = self.params.get(&binding).copied().map(Param);
                match sources.values.get(&binding) {
                    Some(values) => values.iter().fold(param.unwrap_or(Rock), |origin, &value| {
                        origin.merge(self.collection_origin_inner(value, sources, at, seen))
                    }),
                    None => param.unwrap_or(Host),
                }
            }
            Expr::Array(_) | Expr::Null => Rock,
            Expr::Call { .. } | Expr::SafeNavCall { .. } => {
                if self.is_new_collection(expr) {
                    return Rock;
                }
                if self.resolved(expr).is_some_and(|(mid, fid)| {
                    let module = self.ws.module(mid);
                    is_stdlib_module_file(module.file.as_str())
                        && matches!(module.functions[fid].name.as_str(), "cast" | "transmute")
                }) && let Some(arg) = self.call_args(expr).next()
                {
                    return self.collection_origin_inner(arg, sources, at, seen);
                }
                Host
            }
            // The Arena implementation owns its store columns and initializes them itself.
            Expr::FieldAccess { .. } if self.arena_storage => Rock,
            Expr::FieldAccess { .. } | Expr::SafeNav { .. } | Expr::TupleIndex { .. } => Host,
            _ => {
                let mut origin = None;
                self.for_each_value_carrier(expr, &mut |child| {
                    let child = self.collection_origin_inner(child, sources, at, seen);
                    origin = Some(
                        origin.map_or(child, |previous: CollectionOrigin| previous.merge(child)),
                    );
                });
                origin.unwrap_or(Host)
            }
        }
    }

    fn collect_collection_sources(&self, host: &HostRecords) -> CollectionSources {
        let mut sources = CollectionSources::default();
        let mut record = |binding, value| {
            if self
                .bi
                .binding_types
                .get(binding)
                .is_some_and(|&ty| host.carries_collection(self.ws, self.bi, ty))
            {
                sources.values.entry(binding).or_default().push(value);
            }
        };
        for statement in self.body.statement_arena.values() {
            if let Statement::Let { pattern, value, .. } = *statement
                && let Some(binding) = self.body.pattern_single_bind(pattern)
            {
                record(binding, value);
            }
        }
        let mut sinks = Vec::new();
        for (site, expr) in self.body.expr_arena.iter() {
            let Expr::Assign { target, value } = *expr else {
                continue;
            };
            if let Expr::Var(binding) = self.body.expr_arena[target] {
                record(binding, value);
            } else if self
                .ty(value)
                .is_some_and(|ty| host.carries_collection(self.ws, self.bi, ty))
            {
                match storage_root(self.body, target) {
                    Expr::Var(binding) => {
                        sources
                            .stored
                            .entry(binding)
                            .or_default()
                            .push((site, value));
                    }
                    _ => sinks.push((site, value)),
                }
            }
        }
        for (call, expr) in self.body.expr_arena.iter() {
            if !matches!(expr, Expr::Call { .. } | Expr::SafeNavCall { .. })
                || self
                    .resolved(call)
                    .is_some_and(|key| !host.escaping_calls.contains(&key))
            {
                continue;
            }
            for arg in self.call_args(call) {
                if self
                    .ty(arg)
                    .is_some_and(|ty| host.carries_collection(self.ws, self.bi, ty))
                {
                    sinks.push((call, arg));
                }
            }
        }
        if !sinks.is_empty() {
            sources.points = Some(self.statement_points());
        }
        for (site, value) in sinks {
            let at = sources.point(site).unwrap_or_default();
            self.escape_collection(value, &mut sources, at, &mut FxHashMap::default());
        }
        sources
    }

    // Distinguish sequential statements. Uses inside a loop, branch or
    // argument share its statement point. Deferred and nested bodies can run
    // out of order and do not get a point in the enclosing body.
    fn statement_points(&self) -> ArenaMap<Idx<Expr>, usize> {
        let mut points = ArenaMap::default();
        let Some(root) = self.body.root else {
            return points;
        };
        let Expr::Block { statements, tail } = self.body.expr_arena[root] else {
            return points;
        };
        for (point, child) in statements
            .into_iter()
            .map(HirChild::Statement)
            .chain(tail.map(HirChild::Expr))
            .enumerate()
        {
            let mut visit = |node| {
                match node {
                    HirChild::Expr(e) => {
                        if matches!(self.body.expr_arena[e], Expr::Lambda { .. }) {
                            return false;
                        }
                        if points.get(e).is_none() {
                            points.insert(e, point + 1);
                        }
                    }
                    HirChild::Statement(s)
                        if matches!(
                            self.body.statement_arena[s],
                            Statement::Fn { .. } | Statement::Defer(_)
                        ) =>
                    {
                        return false;
                    }
                    _ => {}
                }
                true
            };
            if visit(child) {
                self.body.walk_descendants(child, &mut visit);
            }
        }
        points
    }

    fn escape_collection(
        &self,
        expr: Idx<Expr>,
        sources: &mut CollectionSources,
        at: usize,
        seen: &mut FxHashMap<Idx<Expr>, usize>,
    ) {
        if seen.get(&expr).is_some_and(|&previous| previous <= at) {
            return;
        }
        seen.insert(expr, at);
        if let Expr::Var(binding) = self.body.expr_arena[expr] {
            let first = sources.escaped.entry(binding).or_insert(at);
            *first = (*first).min(at);
            if let Some(values) = sources.values.get(&binding).cloned() {
                for value in values {
                    self.escape_collection(value, sources, at, seen);
                }
            }
            if let Some(stored) = sources.stored.get(&binding).cloned() {
                for (site, value) in stored {
                    let at = at.max(sources.point(site).unwrap_or_default());
                    self.escape_collection(value, sources, at, seen);
                }
            }
        } else if let Expr::Lambda { body, .. } = self.body.expr_arena[expr] {
            self.escape_collection(body, sources, at, seen);
            self.body
                .walk_descendants(HirChild::Expr(body), &mut |child| {
                    if let HirChild::Expr(child) = child
                        && matches!(self.body.expr_arena[child], Expr::Var(_))
                    {
                        self.escape_collection(child, sources, at, seen);
                    }
                    true
                });
        } else {
            self.body.for_each_child(HirChild::Expr(expr), |child| {
                if let HirChild::Expr(child) = child {
                    self.escape_collection(child, sources, at, seen);
                }
            });
        }
    }

    fn facts(mut self, host_records: &HostRecords) -> FnFacts {
        if host_records.host_boundary {
            self.collection_sources = Some(self.collect_collection_sources(host_records));
        }
        let mut direct = Effect::default();
        if let Some(at) = self.arena_remove {
            direct.add(Param(0), at);
        }
        let mut operations = Vec::new();
        let private_records = self.private_records(host_records);
        let may_callback = |base| {
            self.ty(base)
                .is_some_and(|ty| host_records.may_callback(self.ws, self.bi, ty))
                && !matches!(self.body.expr_arena[base], Expr::StructLiteral { .. })
                && !private_records.contains(&base)
        };
        for (id, expr) in self.body.expr_arena.iter() {
            match expr {
                Expr::Call { .. } | Expr::SafeNavCall { .. } => {
                    operations.push((Expression(id), Operation::Call(self.call_edge(id))));
                    if host_records.host_boundary
                        && let Some(base) = self.raw_table_receiver(id)
                        && !private_records.contains(&base)
                    {
                        let origin = if may_callback(base) {
                            CollectionOrigin::Host
                        } else {
                            self.collection_origin(base, id)
                        };
                        if origin != CollectionOrigin::Rock {
                            operations.push((Expression(id), Operation::HostAccess(origin)));
                        }
                    }
                }
                Expr::CompoundAssign { .. } if self.resolved(id).is_some() => {
                    operations.push((Expression(id), Operation::Call(self.call_edge(id))));
                }
                Expr::Index { base, .. }
                    if self
                        .collection_ctor(*base)
                        .is_some_and(|ctor| matches!(ctor, TypeCtor::Array | TypeCtor::Map)) =>
                {
                    let origin = self.collection_origin(*base, id);
                    if origin != CollectionOrigin::Rock {
                        operations.push((Expression(id), Operation::HostAccess(origin)));
                    }
                }
                Expr::FieldAccess { base, field } | Expr::SafeNav { base, field }
                    if field.as_str() == "len"
                        && self.collection_ctor(*base) == Some(TypeCtor::Array) =>
                {
                    let origin = self.collection_origin(*base, id);
                    if origin != CollectionOrigin::Rock {
                        operations.push((Expression(id), Operation::HostAccess(origin)));
                    }
                }
                Expr::Assign { .. } => self.assign_write(id, &mut direct),
                Expr::FieldAccess { base, .. }
                | Expr::SafeNav { base, .. }
                | Expr::Spread(base)
                    if may_callback(*base) =>
                {
                    operations.push((
                        Expression(id),
                        Operation::HostAccess(CollectionOrigin::Host),
                    ));
                }
                Expr::StructLiteral { bases, fields, .. } => {
                    if bases.into_iter().any(may_callback) {
                        operations.push((
                            Expression(id),
                            Operation::HostAccess(CollectionOrigin::Host),
                        ));
                    }
                    if let Some(edge) = self.default_fields(id, *fields) {
                        operations.push((Expression(id), Operation::Implicit(Some(edge))));
                    }
                }
                Expr::IfLet { pattern, value, .. }
                    if self.pattern_may_callback(host_records, *pattern) =>
                {
                    operations.push((
                        Expression(*value),
                        Operation::HostAccess(CollectionOrigin::Host),
                    ));
                }
                Expr::Match {
                    scrutinee, arms, ..
                } if arms.into_iter().any(|arm| {
                    self.pattern_may_callback(host_records, self.body.match_arm_arena[arm].pattern)
                }) =>
                {
                    operations.push((
                        Expression(*scrutinee),
                        Operation::HostAccess(CollectionOrigin::Host),
                    ));
                }
                _ => {}
            }
        }
        if host_records.host_boundary {
            for statement in self.body.statement_arena.values() {
                match *statement {
                    Statement::Let { pattern, value, .. }
                        if self.pattern_may_callback(host_records, pattern) =>
                    {
                        operations.push((
                            Expression(value),
                            Operation::HostAccess(CollectionOrigin::Host),
                        ));
                    }
                    Statement::For { pattern, iter, .. }
                        if self.pattern_may_callback(host_records, pattern) =>
                    {
                        operations.push((
                            IteratorNext(iter),
                            Operation::HostAccess(CollectionOrigin::Host),
                        ));
                    }
                    _ => {}
                }
            }
        }
        for (&arg, witness) in &self.bi.implicit_iter_args {
            let edge = self.unary_witness(witness.func, &witness.generics, Some(arg));
            operations.push((Expression(arg), Operation::Implicit(Some(edge))));
        }
        for (&iter, info) in &self.bi.for_iter_protocol {
            if let Some(witness) = &info.iter {
                let edge = self.unary_witness(witness.func, &witness.generics, Some(iter));
                operations.push((Expression(iter), Operation::Implicit(Some(edge))));
            }
            let next = match &info.next {
                BoundNext::Concrete {
                    func,
                    next_generics,
                    ..
                } => Some(self.unary_witness(
                    *func,
                    next_generics,
                    info.iter.is_none().then_some(iter),
                )),
                BoundNext::CallerGeneric(_) => None,
            };
            operations.push((IteratorNext(iter), Operation::Implicit(next)));
        }
        FnFacts { direct, operations }
    }

    fn default_fields(
        &self,
        literal: Idx<Expr>,
        fields: IdxRange<crate::hir::StructLitField>,
    ) -> Option<Edge> {
        let (
            ty,
            TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                params,
            },
        ) = strip_ref(self.ws, Some(self.bi), self.ty(literal)?)?
        else {
            return None;
        };
        let id = self.ws.nominals.struct_id(name)?;
        let owner = BodyOwner::StructDefaults(id.def);
        let module = self.ws.module(id.module);
        let defaults = module.bodies.get(&owner)?;
        if defaults.field_defaults.keys().all(|name| {
            fields
                .into_iter()
                .any(|field| self.body.struct_lit_field_arena[field].name == *name)
        }) {
            return None;
        }
        Some(Edge {
            callee: (id.module, owner),
            generics: params
                .into_iter()
                .map(|param| self.key(ty.with_idx(param)))
                .collect(),
            args: Vec::new(),
        })
    }

    fn pattern_may_callback(&self, host_records: &HostRecords, pattern: Idx<Pattern>) -> bool {
        fn check(ctx: &BodyCtx<'_>, host: &HostRecords, pattern: Idx<Pattern>) -> bool {
            ctx.bi
                .record_pattern_types
                .get(&pattern)
                .is_some_and(|&ty| host.may_callback(ctx.ws, ctx.bi, ty))
        }
        if !host_records.host_boundary
            || matches!(
                self.body.pattern_arena[pattern],
                Pattern::Bind(_) | Pattern::Wildcard
            )
        {
            return false;
        }
        let mut found = check(self, host_records, pattern);
        self.body
            .walk_descendants(HirChild::Pattern(pattern), &mut |child| {
                if let HirChild::Pattern(pattern) = child {
                    found |= check(self, host_records, pattern);
                }
                !found
            });
        found
    }

    fn raw_table_receiver(&self, expr: Idx<Expr>) -> Option<Idx<Expr>> {
        let (mid, fid) = self.resolved(expr)?;
        let module = self.ws.module(mid);
        (matches!(
            module.functions[fid].name.as_str(),
            "table_get" | "table_set"
        ) && is_stdlib_module_file(module.file.as_str()))
        .then(|| self.call_args(expr).next())
        .flatten()
    }

    fn private_records(&self, host_records: &HostRecords) -> FxHashSet<Idx<Expr>> {
        if !host_records.host_boundary {
            return FxHashSet::default();
        }
        let projected: FxHashSet<_> = self
            .body
            .expr_arena
            .iter()
            .filter_map(|(id, expr)| match *expr {
                Expr::Call { .. } | Expr::SafeNavCall { .. } => self.raw_table_receiver(id),
                Expr::FieldAccess { base, .. } | Expr::SafeNav { base, .. }
                    if matches!(self.body.expr_arena[base], Expr::Var(_))
                        && self
                            .ty(base)
                            .is_some_and(|ty| host_records.may_callback(self.ws, self.bi, ty)) =>
                {
                    Some(base)
                }
                _ => None,
            })
            .collect();
        if projected.is_empty() {
            return projected;
        }
        let mut private: FxHashSet<_> = self
            .body
            .statement_arena
            .values()
            .filter_map(|stmt| {
                if let Statement::Let { pattern, value, .. } = *stmt
                    && matches!(self.body.expr_arena[value], Expr::StructLiteral { .. })
                {
                    return self.body.pattern_single_bind(pattern);
                }
                None
            })
            .collect();
        for (id, expr) in self.body.expr_arena.iter() {
            if let Expr::Var(binding) = *expr
                && !projected.contains(&id)
            {
                private.remove(&binding);
            }
        }
        projected.into_iter().filter(|&id| {
            matches!(self.body.expr_arena[id], Expr::Var(binding) if private.contains(&binding))
        }).collect()
    }

    fn unary_witness(
        &self,
        callee: FnKey,
        subs: &FxHashMap<VarId, TyId>,
        receiver: Option<Idx<Expr>>,
    ) -> Edge {
        let function = &self.ws.module(callee.0).functions[callee.1];
        let generics = function
            .generics
            .iter()
            .map(|&generic| {
                if let Some((_, TypeExpr::Generic(var, _))) = self.ws.resolve_ty(None, generic)
                    && let Some(&ty) = subs.get(&var)
                {
                    return self.key(ty);
                }
                ANY_KEY.to_string()
            })
            .collect();
        Edge {
            callee: function_body(callee),
            generics,
            // A missing receiver here is the iterator returned by a preceding iter call.
            args: receiver
                .map(|arg| vec![Some(self.arg(arg, arg))])
                .unwrap_or_default(),
        }
    }

    /// Map index assignment inserts; a Vec `len` write resizes.
    fn assign_write(&self, e: Idx<Expr>, effect: &mut Effect) {
        let Expr::Assign { target, .. } = self.body.expr_arena[e] else {
            return;
        };
        let at = self.body.expr_spans[e];
        match self.body.expr_arena[target] {
            Expr::Index { base, .. } if self.collection_ctor(base) == Some(TypeCtor::Map) => {
                self.write(effect, base, at)
            }
            Expr::FieldAccess { base, field }
                if field.as_str() == "len"
                    && self.collection_ctor(base) == Some(TypeCtor::Array) =>
            {
                self.write(effect, base, at)
            }
            _ => {}
        }
    }

    fn collection_ctor(&self, e: Idx<Expr>) -> Option<TypeCtor> {
        match strip_ref(self.ws, Some(self.bi), self.ty(e)?)? {
            (_, TypeExpr::App { ctor, .. }) => Some(ctor),
            _ => None,
        }
    }

    fn call_edge(&self, call: Idx<Expr>) -> Option<Edge> {
        if self.bi.fn_field_calls.contains_key(&call) || self.bi.bound_calls.contains_key(&call) {
            return None;
        }
        let callee = self.resolved(call)?;
        let function = &self.ws.module(callee.0).functions[callee.1];
        Some(Edge {
            callee: function_body(callee),
            generics: self.generic_keys(call),
            args: self.bind_args(function, call),
        })
    }

    fn generic_keys(&self, call: Idx<Expr>) -> Vec<String> {
        self.bi
            .call_generic_bindings
            .get(&call)
            .into_iter()
            .flatten()
            .map(|&ty| self.key(ty))
            .collect()
    }

    /// The collections an argument points into an element of, with their
    /// element keys. Arena value references may also be retained in bindings.
    fn element_roots(&self, e: Idx<Expr>, out: &mut Vec<Root>) {
        let mut seen = FxHashSet::default();
        let start = out.len();
        self.element_roots_inner(e, out, &mut seen);
        // Cache only the completed query: sibling inputs of a conditional
        // alias can point into independent Arenas and must not inherit its union.
        if let Expr::Var(binding) = self.body.expr_arena[e]
            && self.ref_values.contains_key(&binding)
        {
            self.root_cache
                .borrow_mut()
                .entry(binding)
                .or_insert_with(|| out[start..].to_vec());
        }
    }

    fn element_roots_inner(
        &self,
        e: Idx<Expr>,
        out: &mut Vec<Root>,
        seen: &mut FxHashSet<Idx<Expr>>,
    ) {
        if !seen.insert(e) {
            return;
        }
        match &self.body.expr_arena[e] {
            Expr::Var(binding) => {
                if let Some(roots) = self.root_cache.borrow().get(binding) {
                    out.extend(roots.iter().cloned());
                    return;
                }
                for &value in self.ref_values.get(binding).into_iter().flatten() {
                    self.element_roots_inner(value, out, seen);
                }
            }
            Expr::Index { base, .. } => match self.element_key(*base) {
                Some(key) => out.push(Root {
                    key,
                    local: self.place_binding(*base).0,
                }),
                None => self.element_roots_inner(*base, out, seen),
            },
            Expr::Call { .. } | Expr::SafeNavCall { .. } => {
                if let Some(target) = self.ty(e).and_then(|ty| ref_target(self.ws, self.bi, ty)) {
                    // A generic summary may discover Arena<T> before T is
                    // instantiated. Reference-struct rows keep stable identity.
                    if !is_reference_struct(self.ws, self.bi, target)
                        && let Some(callee) = self.resolved(e)
                    {
                        let generics = self.generic_keys(e);
                        for key in self.return_origins.roots(self.ws, callee) {
                            out.push(Root {
                                key: substitute(&key, &generics),
                                local: None,
                            });
                        }
                    }
                    for arg in self.call_args(e) {
                        if let Some(element) = self
                            .ty(arg)
                            .and_then(|ty| arena_element(self.ws, Some(self.bi), ty))
                            && !is_reference_struct(self.ws, self.bi, element)
                        {
                            out.push(Root {
                                key: format!("Arena<{}>", self.key(element)),
                                local: None,
                            });
                        }
                        if self
                            .ty(arg)
                            .is_some_and(|ty| ref_target(self.ws, self.bi, ty).is_some())
                        {
                            self.element_roots_inner(arg, out, seen);
                        }
                    }
                }
                for &arg in self.bi.ref_call_args.get(&e).into_iter().flatten() {
                    self.element_roots_inner(arg, out, seen);
                }
            }
            _ => self
                .for_each_value_carrier(e, &mut |child| self.element_roots_inner(child, out, seen)),
        }
    }
}

fn is_reference_struct(ws: &Workspace, bi: &BodyInference, ty: TyId) -> bool {
    matches!(
        strip_ref(ws, Some(bi), ty),
        Some((_, TypeExpr::App { ctor: TypeCtor::Nominal(name), .. })) if ws.is_identity_struct(name)
    )
}

fn ref_target(ws: &Workspace, bi: &BodyInference, ty: TyId) -> Option<TyId> {
    match ws.resolve_ty(Some(bi), ty)? {
        (base, TypeExpr::Nullable(inner)) => ref_target(ws, bi, base.with_idx(inner)),
        (
            base,
            TypeExpr::App {
                ctor: TypeCtor::Ref,
                params,
            },
        ) => params.into_iter().next().map(|p| base.with_idx(p)),
        _ => None,
    }
}

fn arena_element(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> Option<TyId> {
    match strip_ref(ws, bi, ty)? {
        (
            base,
            TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                params,
            },
        ) if bone_core::modules::is_intrinsic_key_in(name, &["Arena", "Entity"]) => {
            params.into_iter().next().map(|p| base.with_idx(p))
        }
        _ => None,
    }
}

struct ArenaCrossings<'a> {
    ctx: &'a BodyCtx<'a>,
    returns_ref: bool,
    roots: FxHashMap<Idx<BindingInfo>, Vec<Root>>,
    active_values: FxHashSet<Idx<Expr>>,
    effects: &'a EffectsAt,
}

impl SuspensionPolicy for ArenaCrossings<'_> {
    fn binding_holds_frame_data(
        &self,
        binding: Idx<BindingInfo>,
        value: Option<Idx<Expr>>,
    ) -> bool {
        self.roots.contains_key(&binding)
            && value.is_none_or(|value| self.active_values.contains(&value))
    }
    fn is_barrier(&self, _body: &Body, at: SuspensionPoint) -> bool {
        self.effects.contains_key(&at)
    }
    fn crosses_binding(&self, binding: Idx<BindingInfo>, at: SuspensionPoint) -> bool {
        self.crosses(binding, &self.effects[&at])
    }

    fn crosses_return(&self, value: Idx<Expr>, at: SuspensionPoint) -> bool {
        if !self.returns_ref {
            return false;
        }
        let effect = &self.effects[&at];
        let mut roots = Vec::new();
        self.ctx.element_roots(value, &mut roots);
        roots
            .iter()
            .any(|root| root.key.starts_with("Arena<") && effect.invalidating(root).is_some())
    }
}

impl ArenaCrossings<'_> {
    fn crosses(&self, binding: Idx<BindingInfo>, effect: &Effect) -> bool {
        self.roots[&binding]
            .iter()
            .any(|root| effect.invalidating(root).is_some())
    }
}

/// `ty` under any `ref` and `?`.
pub(crate) fn strip_ref(
    ws: &Workspace,
    bi: Option<&BodyInference>,
    ty: TyId,
) -> Option<(TyId, TypeExpr)> {
    let (base, expr) = ws.resolve_ty(bi, ty)?;
    match expr {
        TypeExpr::Nullable(inner) => strip_ref(ws, bi, base.with_idx(inner)),
        TypeExpr::App {
            ctor: TypeCtor::Ref,
            params,
        } => strip_ref(ws, bi, base.with_idx(params.into_iter().next()?)),
        _ => Some((base, expr)),
    }
}

fn collection_or_any(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> bool {
    matches!(
        strip_ref(ws, bi, ty),
        Some((
            _,
            TypeExpr::App {
                ctor: TypeCtor::Map | TypeCtor::Array,
                ..
            } | TypeExpr::Builtin(BuiltinTy::Any)
        ))
    )
}

fn collection_element(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> Option<TyId> {
    match strip_ref(ws, bi, ty)? {
        (
            base,
            TypeExpr::App {
                ctor: TypeCtor::Array | TypeCtor::Map,
                params,
            },
        ) => Some(base.with_idx(params.into_iter().next_back()?)),
        _ => None,
    }
}

fn render_key(
    ws: &Workspace,
    bi: Option<&BodyInference>,
    generics: &[VarId],
    ty: TyId,
    out: &mut String,
) {
    if out.len() > MAX_KEY_LEN {
        return;
    }
    let Some((base, expr)) = ws.resolve_ty(bi, ty) else {
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
            Some(i) => out.push_str(&format!("{GENERIC}{i}{GENERIC}")),
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

/// `key` with the callee's generics replaced by the caller's bindings.
fn substitute(key: &str, generics: &[String]) -> String {
    let mut out = String::new();
    let mut rest = key;
    while let Some(start) = rest.find(GENERIC) {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let Some(end) = tail.find(GENERIC) else {
            return ANY_KEY.to_string();
        };
        let binding = tail[..end]
            .parse::<usize>()
            .ok()
            .and_then(|i| generics.get(i));
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

/// Only partly known: an unresolved type, or a generic of the function.
fn partial(key: &str) -> bool {
    key.contains(ANY_KEY) || key.contains(GENERIC)
}

fn keys_may_alias(a: &str, b: &str) -> bool {
    a == b || partial(a) || partial(b)
}

fn shown(key: &str) -> String {
    key.replace(ANY_KEY, "_").replace(GENERIC, "$")
}

fn map_edge(edge: &Edge, callee: &Effect, at: Span) -> Effect {
    let mut mapped = Effect::default();
    let arg = |p: u16| edge.args.get(p as usize);
    for (effect, &witness) in &callee.0 {
        match *effect {
            Unknown | EffectOn::Host => mapped.add(effect.clone(), witness),
            HostParam(p) => mapped.add_host_access(
                arg(p)
                    .and_then(Option::as_ref)
                    .map_or(CollectionOrigin::Host, |arg| arg.collection_origin),
                witness,
            ),
            Retained(p) => {
                if let Some(Some(arg)) = arg(p) {
                    for &q in &arg.ref_params {
                        mapped.add(Retained(q), witness);
                    }
                }
            }
            Param(p) => match arg(p) {
                Some(Some(arg)) => {
                    if let Some(local) = arg.fresh {
                        mapped.add(Local(local), witness);
                    } else {
                        if let Some(q) = arg.param {
                            mapped.add(Param(q), witness);
                        }
                        if let Some(key) = &arg.element {
                            mapped.add(Element(key.clone()), witness);
                        } else if arg.param.is_none() {
                            mapped.add(Unknown, witness);
                        }
                    }
                }
                // A defaulted parameter is a value the callee builds itself.
                Some(None) => {}
                None => mapped.add(Unknown, at),
            },
            Element(ref key) => mapped.add(Element(substitute(key, &edge.generics)), witness),
            Local(_) => {}
        }
    }
    mapped
}

fn bodiless_summary(ws: &Workspace, key: FnKey) -> Effect {
    let module = ws.module(key.0);
    let function = &module.functions[key.1];
    let mut effect = Effect::default();
    if function
        .annotations
        .iter()
        .any(|a| a.is_builtin(RELOCATES_ANNOTATION))
    {
        effect.add(Param(0), function.span);
    } else if !is_stdlib_module_file(module.file.as_str())
        || matches!(
            function.name.as_str(),
            "resume" | "kill" | "fire_hot_reload" | "__rock_coroutine_kill"
        )
        || function.param_decls.iter().any(|p| {
            matches!(
                ws.resolve_ty(None, p.ty),
                Some((_, TypeExpr::Function { .. }))
            )
        })
    {
        // Foreign code, or a stdlib primitive that calls back into a closure.
        effect.add(Unknown, function.span);
    }
    if is_stdlib_module_file(module.file.as_str())
        && function.inline_body.is_some()
        && function.param_decls.first().is_some_and(|param| {
            matches!(
                strip_ref(ws, None, param.ty),
                Some((
                    _,
                    TypeExpr::App {
                        ctor: TypeCtor::Map | TypeCtor::Array,
                        ..
                    }
                ))
            )
        })
    {
        effect.add(HostParam(0), function.span);
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
        if partial(key) {
            self.all = true;
        }
        self.keys.insert(key.to_string());
    }

    fn keeps(&self, key: &str) -> bool {
        self.all || partial(key) || self.keys.contains(key)
    }

    /// Unknown code subsumes structural effects; retention still propagates.
    fn trim(&self, effect: &mut Effect) {
        if effect.0.contains_key(&Unknown) {
            effect
                .0
                .retain(|effect, _| matches!(effect, Unknown | Retained(_)));
        } else {
            effect.0.retain(|effect, _| match effect {
                Element(key) => self.keeps(key),
                _ => true,
            });
        }
    }
}

fn evaluated(body: &Body, root: Idx<Expr>, mut visit: impl FnMut(HirChild)) {
    let mut visit = |node| {
        visit(node);
        match node {
            HirChild::Expr(e) => !matches!(body.expr_arena[e], Expr::Lambda { .. }),
            HirChild::Statement(s) => !matches!(body.statement_arena[s], Statement::Fn { .. }),
            HirChild::Pattern(_) => true,
        }
    };
    if visit(HirChild::Expr(root)) {
        body.walk_descendants(HirChild::Expr(root), &mut visit);
    }
}

/// Completed return-origin queries share the existing root walker and type keys.
/// A recursive query can return discovered origins, but none of its active
/// callers may cache a partial answer from the cycle.
#[derive(Default)]
struct ReturnOrigins {
    completed: RefCell<FxHashMap<FnKey, Vec<String>>>,
    active: RefCell<FxHashMap<FnKey, bool>>,
}

impl ReturnOrigins {
    fn roots(&self, ws: &Workspace, key: FnKey) -> Vec<String> {
        if let Some(roots) = self.completed.borrow().get(&key) {
            return roots.clone();
        }
        if self.active.borrow().contains_key(&key) {
            for cacheable in self.active.borrow_mut().values_mut() {
                *cacheable = false;
            }
            return Vec::new();
        }
        self.active.borrow_mut().insert(key, true);
        let mut roots = Vec::new();
        if let Some((function, body, bi)) = inferred_body(ws, key) {
            let ctx = BodyCtx::new(ws, Some(function), body, bi, self);
            if let Some(root) = body.root {
                ctx.element_roots(root, &mut roots);
                evaluated(body, root, |node| {
                    let value = match node {
                        HirChild::Expr(e) => match body.expr_arena[e] {
                            Expr::Return(value) => value,
                            _ => None,
                        },
                        HirChild::Statement(s) => match body.statement_arena[s] {
                            Statement::Return(value) => value,
                            _ => None,
                        },
                        HirChild::Pattern(_) => None,
                    };
                    if let Some(value) = value {
                        ctx.element_roots(value, &mut roots);
                    }
                });
            }
        }
        let roots: Vec<_> = roots
            .into_iter()
            .map(|root| root.key)
            .filter(|key| key.starts_with("Arena<"))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if self
            .active
            .borrow_mut()
            .remove(&key)
            .expect("active return query")
        {
            self.completed.borrow_mut().insert(key, roots.clone());
        }
        roots
    }
}

/// Summaries are computed on demand: only for the callees, transitively, of
/// bodies that pass an element reference somewhere.
struct Program<'a> {
    ws: &'a Workspace,
    relevance: Relevance,
    sites: Vec<BodyKey>,
    facts: FxHashMap<BodyKey, FnFacts>,
    summaries: FxHashMap<BodyKey, Effect>,
    return_origins: ReturnOrigins,
    host_records: HostRecords,
}

fn inferred_site(
    ws: &Workspace,
    (module, owner): BodyKey,
) -> Option<(Option<&Function>, &Body, &BodyInference)> {
    let module = ws.module(module);
    let function = match owner {
        BodyOwner::Function(id) => {
            let f = &module.functions[id];
            if f.inline_body.is_some() || f.is_extern {
                return None;
            }
            Some(f)
        }
        _ => None,
    };
    let body = module.bodies.get(&owner)?;
    let bi = ws.inference(module.id)?.per_fn.get(&owner)?;
    Some((function, body, bi))
}

fn inferred_body(
    ws: &Workspace,
    (module, function): FnKey,
) -> Option<(&Function, &Body, &BodyInference)> {
    let (f, body, bi) = inferred_site(ws, function_body((module, function)))?;
    Some((f?, body, bi))
}

impl<'a> Program<'a> {
    fn build(ws: &'a Workspace) -> Self {
        let mut program = Self {
            ws,
            relevance: Relevance::default(),
            sites: Vec::new(),
            facts: FxHashMap::default(),
            summaries: FxHashMap::default(),
            return_origins: ReturnOrigins::default(),
            host_records: HostRecords::default(),
        };
        // Few bodies pass a reference argument; finding them visits every
        // function, so the search runs per module in parallel.
        let candidates: Vec<Vec<_>> = ws
            .modules
            .par_iter()
            .map(|module| {
                module
                    .bodies
                    .keys()
                    .copied()
                    .filter(|&owner| {
                        inferred_site(ws, (module.id, owner)).is_some_and(|(_, _, bi)| {
                            !bi.ref_call_args.is_empty()
                                || (match owner {
                                    BodyOwner::Function(id) => module.functions[id]
                                        .return_ty
                                        .is_some_and(|ty| ref_target(ws, bi, ty).is_some()),
                                    _ => false,
                                })
                                || bi
                                    .binding_types
                                    .iter()
                                    .any(|(_, &ty)| ref_target(ws, bi, ty).is_some())
                        })
                    })
                    .map(|owner| (module.id, owner))
                    .collect()
            })
            .collect();
        for (mid, owner) in candidates.into_iter().flatten() {
            let Some((function, body, bi)) = inferred_site(ws, (mid, owner)) else {
                continue;
            };
            let ctx = BodyCtx::new(ws, function, body, bi, &program.return_origins);
            let mut roots = Vec::new();
            for &arg in bi.ref_call_args.values().flatten() {
                ctx.element_roots(arg, &mut roots);
            }
            for values in ctx.ref_values.values() {
                for &value in values {
                    ctx.element_roots(value, &mut roots);
                }
            }
            if let BodyOwner::Function(id) = owner
                && function
                    .and_then(|f| f.return_ty)
                    .is_some_and(|ty| ref_target(ws, bi, ty).is_some())
                && body
                    .statement_arena
                    .values()
                    .any(|s| matches!(s, Statement::Defer(_)))
            {
                roots.extend(
                    program
                        .return_origins
                        .roots(ws, (mid, id))
                        .into_iter()
                        .map(|key| Root { key, local: None }),
                );
            }
            if roots.is_empty() {
                continue;
            }
            for root in &roots {
                program.relevance.add(&root.key);
            }
            program.sites.push((mid, owner));
        }
        let mut pending = program.sites.clone();
        if program
            .relevance
            .keys
            .iter()
            .any(|key| key.starts_with("Arena<"))
        {
            program.host_records = HostRecords::collect(ws);
        }
        while let Some(key) = pending.pop() {
            if program.facts.contains_key(&key) {
                continue;
            }
            let Some((function, body, bi)) = inferred_site(ws, key) else {
                continue;
            };
            let facts = BodyCtx::new(ws, function, body, bi, &program.return_origins)
                .facts(&program.host_records);
            pending.extend(facts.callees());
            program.facts.insert(key, facts);
        }
        program.solve();
        program
    }

    fn summary(&self, key: BodyKey) -> Effect {
        let mut effect = match self.summaries.get(&key) {
            Some(summary) => summary.clone(),
            None => match key.1 {
                BodyOwner::Function(id) if !self.facts.contains_key(&key) => {
                    bodiless_summary(self.ws, (key.0, id))
                }
                _ => Effect::default(),
            },
        };
        let BodyOwner::Function(id) = key.1 else {
            return effect;
        };
        let function = &self.ws.module(key.0).functions[id];
        if function.is_coroutine {
            for (index, param) in function.param_decls.iter().enumerate() {
                if self.ws.type_contains_ref(None, param.ty) {
                    effect.add(Retained(index as u16), function.span);
                }
            }
        }
        effect
    }

    fn operation_effect(
        &self,
        body: &Body,
        point: SuspensionPoint,
        operation: &Operation,
    ) -> Effect {
        let (Expression(expr) | IteratorNext(expr)) = point;
        let at = body.expr_spans[expr];
        let mut effect = match operation {
            Operation::Call(Some(edge)) | Operation::Implicit(Some(edge)) => {
                map_edge(edge, &self.summary(edge.callee), at)
            }
            Operation::Call(None) | Operation::Implicit(None) => {
                let mut effect = Effect::default();
                effect.add(Unknown, at);
                effect
            }
            Operation::HostAccess(origin) => {
                let mut effect = Effect::default();
                effect.add_host_access(*origin, at);
                effect
            }
        };
        self.relevance.trim(&mut effect);
        effect
    }

    /// Summaries only grow, and keys are bounded, so the worklist drains.
    fn solve(&mut self) {
        let mut callers: FxHashMap<BodyKey, Vec<BodyKey>> = FxHashMap::default();
        for (&caller, facts) in &self.facts {
            for callee in facts.callees() {
                callers.entry(callee).or_default().push(caller);
            }
        }
        let mut pending: Vec<_> = self.facts.keys().copied().collect();
        let mut queued: FxHashSet<_> = pending.iter().copied().collect();
        while let Some(key) = pending.pop() {
            queued.remove(&key);
            let body = &self.ws.module(key.0).bodies[&key.1];
            let facts = &self.facts[&key];
            let mut effect = facts.direct.clone();
            for (point, op) in &facts.operations {
                effect.union(&self.operation_effect(body, *point, op));
            }
            effect.0.retain(|target, _| !matches!(target, Local(_)));
            self.relevance.trim(&mut effect);
            if self
                .summaries
                .get(&key)
                .is_some_and(|old| old.0.keys().eq(effect.0.keys()))
            {
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

    fn rejections(&self, mid: ModuleId, owner: BodyOwner) -> Vec<CompilerError> {
        let Some((function, body, bi)) = inferred_site(self.ws, (mid, owner)) else {
            return Vec::new();
        };
        let ctx = BodyCtx::new(self.ws, function, body, bi, &self.return_origins);
        let facts = &self.facts[&(mid, owner)];
        let mut effects = EffectsAt::default();
        for &(point, ref op) in &facts.operations {
            effects
                .entry(point)
                .or_default()
                .union(&self.operation_effect(body, point, op));
        }
        let mut errors = Vec::new();
        let mut borrowed = FxHashMap::default();
        let mut active_values = FxHashSet::default();
        for (&binding, values) in &ctx.ref_values {
            let mut roots = Vec::new();
            for &value in values {
                let start = roots.len();
                ctx.element_roots(value, &mut roots);
                if roots[start..]
                    .iter()
                    .any(|root| root.key.starts_with("Arena<"))
                {
                    active_values.insert(value);
                }
            }
            roots.retain(|root| root.key.starts_with("Arena<"));
            if !roots.is_empty() {
                borrowed.insert(binding, roots);
            }
        }
        let returns_ref = function
            .and_then(|f| f.return_ty)
            .is_some_and(|ty| ref_target(self.ws, bi, ty).is_some());
        if !effects.is_empty() && (!borrowed.is_empty() || returns_ref) {
            let mut policy = ArenaCrossings {
                ctx: &ctx,
                returns_ref,
                roots: borrowed,
                active_values,
                effects: &effects,
            };
            let mut crossings = Vec::new();
            if let Some(root) = body.root {
                crossings.extend(uses_after_suspension_in(
                    body,
                    root,
                    body.param_bindings,
                    &policy,
                ));
            }
            policy.returns_ref = false;
            for &default in body
                .param_defaults
                .iter()
                .flatten()
                .chain(body.field_defaults.values())
            {
                crossings.extend(uses_after_suspension_in(body, default, [], &policy));
            }
            for expr in body.expr_arena.values() {
                if let Expr::Lambda {
                    params,
                    body: nested,
                } = *expr
                {
                    crossings.extend(uses_after_suspension_in(body, nested, params, &policy));
                }
            }
            for statement in body.statement_arena.values() {
                if let Statement::Fn {
                    params,
                    body: nested,
                    ..
                } = *statement
                {
                    crossings.extend(uses_after_suspension_in(body, nested, params, &policy));
                }
            }
            for at in crossings {
                errors.push(CompilerError::new_with_label(
                    body.expr_spans[at],
                    format!("{ARENA_REJECTION}; borrow again after the call"),
                ));
            }
        }
        for (point, operation) in &facts.operations {
            let (Expression(call), Operation::Call(edge)) = (point, operation) else {
                continue;
            };
            let Some(ref_args) = bi.ref_call_args.get(call) else {
                continue;
            };
            for &ref_arg in ref_args {
                let mut roots = Vec::new();
                ctx.element_roots(ref_arg, &mut roots);
                if roots.is_empty() {
                    continue;
                }
                if roots.iter().any(|root| !root.key.starts_with("Arena<"))
                    && let Some(edge) = edge
                    && let Some(witness) =
                        self.summary(edge.callee)
                            .0
                            .iter()
                            .find_map(|(effect, &at)| {
                                let Retained(p) = *effect else { return None };
                                edge.args
                                    .get(p as usize)
                                    .and_then(Option::as_ref)
                                    .filter(|arg| arg.expr == ref_arg)
                                    .map(|_| at)
                            })
                {
                    errors.push(CompilerError::new_with_label(
                        body.expr_spans[ref_arg],
                        format!("{REJECTION_PREFIX} to `{}`: the reference is retained by a coroutine; pass the container and index instead", callee_name(&ctx, *call)),
                    ).with_label(witness, "the coroutine retains its parameters until resume".to_string()));
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
        effects: &EffectsAt,
        call: Idx<Expr>,
        ref_arg: Idx<Expr>,
    ) -> Effect {
        let mut effect = effects[&Expression(call)].clone();
        let taken = ctx.body.expr_spans[ref_arg];
        for arg in ctx.call_args(call) {
            let span = ctx.body.expr_spans[arg];
            if span.offset_range().0 <= taken.offset_range().0 {
                continue;
            }
            evaluated(ctx.body, arg, |node| {
                let point = match node {
                    HirChild::Expr(id) => {
                        ctx.assign_write(id, &mut effect);
                        Expression(id)
                    }
                    HirChild::Statement(stmt) => match ctx.body.statement_arena[stmt] {
                        Statement::For { iter, .. } => IteratorNext(iter),
                        _ => return,
                    },
                    HirChild::Pattern(_) => return,
                };
                if let Some(nested) = effects.get(&point) {
                    effect.union(nested);
                }
            });
        }
        effect
    }
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
    let arena = roots.iter().any(|root| root.key.starts_with("Arena<"));
    let remedy = if arena {
        "borrow again after the invalidating call, or copy the value and write it back after the call"
    } else {
        "copy the element out and write it back after the call, or keep data that is referenced across calls in an Arena"
    };
    let mut invalidating = roots.iter().filter_map(|root| {
        effect
            .invalidating(root)
            .map(|(effect, witness)| (root, effect, witness))
    });
    let first = invalidating.next()?;
    let (root, cause, witness) = std::iter::once(first)
        .chain(invalidating)
        .find(|(_, effect, _)| matches!(effect, Unknown | EffectOn::Host | HostParam(_)))
        .unwrap_or(first);
    let (prefix, reason, label) = match cause {
        Unknown | EffectOn::Host | HostParam(_) => (
            if arena {
                ARENA_REJECTION
            } else {
                REJECTION_PREFIX
            },
            "the call runs a closure or foreign function whose effect on the collection is unknown"
                .to_string(),
            "the call may run unknown code here".to_string(),
        ),
        target => {
            let key = shown(match target {
                Element(key) => key,
                _ => &root.key,
            });
            (
                if root.key.starts_with("Arena<") {
                    ARENA_REJECTION
                } else {
                    REJECTION_PREFIX
                },
                format!(
                    "the call may invalidate a collection of `{key}` while the reference is live"
                ),
                format!("a collection of `{key}` may change structure here"),
            )
        }
    };
    Some(
        CompilerError::new_with_label(at, format!("{prefix} to `{callee}`: {reason}; {remedy}"))
            .with_label(witness, label),
    )
}

fn annotation_errors(module: &Module) -> Vec<(BodyOwner, CompilerError)> {
    if is_stdlib_module_file(module.file.as_str()) {
        return Vec::new();
    }
    let mut errors = Vec::new();
    for (fid, function) in module.functions.iter() {
        for annotation in function
            .annotations
            .iter()
            .filter(|a| a.is_builtin(RELOCATES_ANNOTATION))
        {
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
            errors.retain(|e| {
                !e.message.starts_with(REJECTION_PREFIX)
                    && !e.message.starts_with(ARENA_REJECTION)
                    && e.message != ANNOTATION_MESSAGE
            });
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
        for &(mid, owner) in &program.sites {
            for error in program.rejections(mid, owner) {
                found.push((mid, owner, error));
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
