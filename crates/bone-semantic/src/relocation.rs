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
    BinOp, BindingInfo, BindingKind, Body, BuiltinTy, Callee, Expr, Function, Module, Pattern,
    Statement, TypeCtor, TypeExpr, VarId, VariantPayload,
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
type Region = (BodyKey, Option<Idx<Expr>>);

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
    escaped: FxHashMap<Idx<BindingInfo>, usize>,
    points: ArenaMap<Idx<Expr>, usize>,
}

fn binding_initializer(body: &Body, stmt: &Statement) -> Option<(Idx<BindingInfo>, Idx<Expr>)> {
    let Statement::Let { pattern, value, .. } = *stmt else {
        return None;
    };
    Some((body.pattern_single_bind(pattern)?, value))
}

fn binding_assignment(body: &Body, target: Idx<Expr>) -> Option<Idx<BindingInfo>> {
    match body.expr_arena[target] {
        Expr::Var(binding) => Some(binding),
        _ => None,
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
    callee: Region,
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
    Direct(EffectOn),
    HostAccess(CollectionOrigin),
}

type EffectsAt = FxHashMap<SuspensionPoint, Effect>;

type Operations = Vec<(SuspensionPoint, Operation)>;

impl Operation {
    fn edge(&self) -> Option<&Edge> {
        match self {
            Self::Call(edge) | Self::Implicit(edge) => edge.as_ref(),
            _ => None,
        }
    }
}

struct RegionEffects {
    members: Option<FxHashSet<Idx<Expr>>>,
    summary: Effect,
}

impl RegionEffects {
    fn operations<'a>(
        &'a self,
        ops: &'a [(SuspensionPoint, Operation)],
    ) -> impl Iterator<Item = &'a (SuspensionPoint, Operation)> {
        ops.iter().filter(move |(point, _)| {
            let (Expression(expr) | IteratorNext(expr)) = point;
            self.members
                .as_ref()
                .is_none_or(|members| members.contains(expr))
        })
    }
}

fn stdlib_cast(ws: &Workspace, (mid, fid): FnKey) -> bool {
    let module = ws.module(mid);
    is_stdlib_module_file(module.file.as_str())
        && matches!(module.functions[fid].name.as_str(), "cast" | "transmute")
}

#[derive(Default)]
struct HostRecords {
    types: FxHashSet<Ustr>,
    host_boundary: bool,
    escaping_calls: RefCell<FxHashMap<FnKey, bool>>,
}

impl HostRecords {
    fn collect(ws: &Workspace) -> Self {
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
                    .map(|(id, _)| (m.id, id))
            })
            .collect();
        let mut records = Self::default();
        if host_entry_points.is_empty() {
            return records;
        }
        records.host_boundary = true;
        let mut pending = Vec::new();
        for module in user_modules() {
            // Raw host code can reach the global directory by name.
            for global in module.globals.values() {
                records.add(ws, None, global.ty, &mut pending);
            }
            for (id, function) in module.functions.iter() {
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
                for &(_, ty) in &bi.any_erasures {
                    records.add(ws, Some(bi), ty, &mut pending);
                }
                for (&site, &target) in &bi.resolved_overload_calls {
                    if !host_entry_points.contains(&target) && !stdlib_cast(ws, target) {
                        continue;
                    }
                    let (receiver, args) = match body.expr_arena[site] {
                        Expr::Call { args, .. } => (None, args),
                        Expr::SafeNavCall { receiver, args, .. } => (Some(receiver), args),
                        _ => (None, IdxRange::new_empty()),
                    };
                    for expr in std::iter::once(site).chain(receiver).chain(args) {
                        if let Some(&ty) = bi.expr_types.get(expr) {
                            records.add(ws, Some(bi), ty, &mut pending);
                        }
                    }
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

    /// Reuse resolved calls to ask if an argument can reach a foreign call
    /// or an aggregate store. A failed search proves that every node it
    /// visited is closed, including nodes in recursive cycles.
    fn escapes(&self, ws: &Workspace, root: FnKey) -> bool {
        let mut cache = self.escaping_calls.borrow_mut();
        let mut seen = FxHashSet::default();
        let mut pending = vec![root];
        while let Some(key) = pending.pop() {
            if let Some(&escapes) = cache.get(&key) {
                if escapes {
                    cache.insert(root, true);
                    return true;
                }
            } else if seen.insert(key) {
                let module = ws.module(key.0);
                let function = &module.functions[key.1];
                let opaque = function.inline_body.is_some() || function.is_extern;
                let retains = !is_stdlib_module_file(module.file.as_str())
                    || (function.param_decls.len() > 1
                        && function.annotations.iter().any(|a| {
                            a.is_builtin(RELOCATES_ANNOTATION) || a.is_builtin("writes_elems")
                        }));
                if opaque && retains {
                    cache.insert(key, true);
                    cache.insert(root, true);
                    return true;
                }
                if let Some((_, body, bi)) = inferred_body(ws, key) {
                    if body.expr_arena.iter().any(|(id, expr)| match *expr {
                        Expr::Assign { target, value } => {
                            !matches!(body.expr_arena[target], Expr::Var(_))
                                && bi.expr_types.get(value).is_some_and(|&ty| {
                                    self.carries_collection(ws, bi, ty)
                                        || matches!(
                                            ws.resolve_ty(Some(bi), ty),
                                            Some((_, TypeExpr::Generic(..)))
                                        )
                                })
                        }
                        Expr::Call {
                            callee: Callee::Local(_) | Callee::Value(_),
                            ..
                        }
                        | Expr::SafeNavCall { .. } => !bi.resolved_overload_calls.contains_key(&id),
                        _ => false,
                    }) {
                        cache.insert(key, true);
                        cache.insert(root, true);
                        return true;
                    }
                    pending.extend(bi.resolved_overload_calls.values().copied());
                }
            }
        }
        cache.extend(seen.into_iter().map(|key| (key, false)));
        false
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
    values: FxHashMap<Idx<BindingInfo>, Vec<Idx<Expr>>>,
    arena_storage: bool,
    collection_sources: Option<CollectionSources>,
    host_records: Option<&'a HostRecords>,
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
        host_records: Option<&'a HostRecords>,
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
            values: FxHashMap::default(),
            root_cache: Default::default(),
            collection_sources: None,
            host_records,
            arena_storage: function.is_some_and(|f| {
                bone_core::modules::stdlib_file_name(f.span.file.as_str()) == Some("arena.rock")
            }),
            return_origins,
        };
        ctx.fresh_locals = ctx.collect_fresh_locals();
        if let Some(host_records) = host_records {
            ctx.collection_sources = Some(ctx.collect_collection_sources(host_records));
        }
        ctx
    }

    /// `let` bindings whose every value is a new collection.
    fn collect_fresh_locals(&mut self) -> FxHashSet<Idx<BindingInfo>> {
        let mut fresh = FxHashSet::default();
        let mut stale = FxHashSet::default();
        for (name, value) in self
            .body
            .statement_arena
            .values()
            .filter_map(|stmt| binding_initializer(self.body, stmt))
        {
            self.record_value(name, value);
            if self.body.binding_arena[name].kind == BindingKind::Let
                && self.is_new_collection(value)
            {
                fresh.insert(name);
            } else {
                stale.insert(name);
            }
        }
        for expr in self.body.expr_arena.values() {
            if let Expr::Assign { target, value } = *expr
                && let Some(binding) = binding_assignment(self.body, target)
            {
                if !self.is_new_collection(value) {
                    stale.insert(binding);
                }
                self.record_value(binding, value);
            }
        }
        fresh.retain(|b| !stale.contains(b));
        fresh
    }

    fn record_value(&mut self, binding: Idx<BindingInfo>, value: Idx<Expr>) {
        if self.bi.binding_types.get(binding).is_some_and(|&ty| {
            ref_target(self.ws, self.bi, ty).is_some()
                || self
                    .host_records
                    .is_some_and(|host| host.carries_collection(self.ws, self.bi, ty))
        }) {
            self.values.entry(binding).or_default().push(value);
        }
    }

    fn ref_values(&self, binding: Idx<BindingInfo>) -> Option<&Vec<Idx<Expr>>> {
        let values = self.values.get(&binding)?;
        // Contexts with host analysis also record non-reference aliases.
        if self.host_records.is_none()
            || self
                .bi
                .binding_types
                .get(binding)
                .is_some_and(|&ty| ref_target(self.ws, self.bi, ty).is_some())
        {
            Some(values)
        } else {
            None
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
            collection_origin: self.origin(expr, at),
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
                for &value in self.ref_values(binding).into_iter().flatten() {
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

    fn origin(&self, expr: Idx<Expr>, at: Idx<Expr>) -> CollectionOrigin {
        let (Some(host), Some(sources)) = (self.host_records, &self.collection_sources) else {
            return CollectionOrigin::Rock;
        };
        if !self.ty(expr).is_some_and(|ty| {
            collection_or_any(self.ws, Some(self.bi), ty) || host.may_callback(self.ws, self.bi, ty)
        }) {
            return CollectionOrigin::Rock;
        }
        self.collection_origin_inner(
            expr,
            sources,
            sources.points.get(at).copied().unwrap_or(usize::MAX),
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
                match self.values.get(&binding) {
                    Some(values) => values.iter().fold(param.unwrap_or(Rock), |origin, &value| {
                        origin.merge(self.collection_origin_inner(value, sources, at, seen))
                    }),
                    None => param.unwrap_or(Host),
                }
            }
            Expr::Array(_) | Expr::StructLiteral { .. } | Expr::Null => Rock,
            Expr::Call { .. } | Expr::SafeNavCall { .. } => {
                if self.is_new_collection(expr) {
                    return Rock;
                }
                if self
                    .resolved(expr)
                    .is_some_and(|key| stdlib_cast(self.ws, key))
                    && let Some(arg) = self.call_args(expr).next()
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
        let carries = |ty| host.carries_collection(self.ws, self.bi, ty);
        let mut sinks = Vec::new();
        let mut stores = FxHashMap::<_, Vec<_>>::default();
        for (site, expr) in self.body.expr_arena.iter() {
            match *expr {
                Expr::Assign { target, value }
                    if binding_assignment(self.body, target).is_none()
                        && self.ty(value).is_some_and(carries) =>
                {
                    if let Expr::Var(binding) = storage_root(self.body, target) {
                        stores.entry(binding).or_default().push((site, value));
                    } else {
                        sinks.push((site, value));
                    }
                }
                Expr::Call { .. } | Expr::SafeNavCall { .. } => {
                    let mut args = self
                        .call_args(site)
                        .filter(|&arg| self.ty(arg).is_some_and(carries))
                        .peekable();
                    if args.peek().is_some()
                        && self
                            .resolved(site)
                            .is_none_or(|key| host.escapes(self.ws, key))
                    {
                        sinks.extend(args.map(|arg| (site, arg)));
                    }
                }
                _ => {}
            }
        }
        if sinks.is_empty() {
            return sources;
        }
        sources.points = self.statement_points();
        let point = |site| sources.points.get(site).copied().unwrap_or_default();
        let mut pending: Vec<_> = sinks
            .into_iter()
            .map(|(site, value)| (value, point(site)))
            .collect();
        let mut seen = FxHashMap::default();
        while let Some((expr, at)) = pending.pop() {
            if seen.get(&expr).is_some_and(|&previous| previous <= at) {
                continue;
            }
            seen.insert(expr, at);
            match self.body.expr_arena[expr] {
                Expr::Var(binding) => {
                    let first = sources.escaped.entry(binding).or_insert(at);
                    *first = (*first).min(at);
                    pending.extend(
                        self.values
                            .get(&binding)
                            .into_iter()
                            .flatten()
                            .map(|&value| (value, at)),
                    );
                    pending.extend(
                        stores
                            .get(&binding)
                            .into_iter()
                            .flatten()
                            .map(|&(site, value)| (value, at.max(point(site)))),
                    );
                }
                Expr::Lambda { body, .. } => {
                    pending.push((body, at));
                    self.body
                        .walk_descendants(HirChild::Expr(body), &mut |child| {
                            if let HirChild::Expr(child) = child
                                && matches!(self.body.expr_arena[child], Expr::Var(_))
                            {
                                pending.push((child, at));
                            }
                            true
                        });
                }
                _ => self.body.for_each_child(HirChild::Expr(expr), |child| {
                    if let HirChild::Expr(child) = child {
                        pending.push((child, at));
                    }
                }),
            }
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
            walk_region(self.body, child, |node| {
                match node {
                    HirChild::Expr(e)
                        if !matches!(self.body.expr_arena[e], Expr::Lambda { .. }) =>
                    {
                        if points.get(e).is_none() {
                            points.insert(e, point + 1);
                        }
                    }
                    HirChild::Statement(s)
                        if matches!(self.body.statement_arena[s], Statement::Defer(_)) =>
                    {
                        return false;
                    }
                    _ => {}
                }
                true
            });
        }
        points
    }

    fn facts(self, host_records: &HostRecords) -> Operations {
        let mut operations = Vec::new();
        let host_access = |base| {
            self.ty(base)
                .is_some_and(|ty| host_records.may_callback(self.ws, self.bi, ty))
        };
        for (id, expr) in self.body.expr_arena.iter() {
            let call = match *expr {
                Expr::Call { .. } | Expr::SafeNavCall { .. } => Some(self.call_edge(id)),
                Expr::CompoundAssign { .. } if self.resolved(id).is_some() => {
                    Some(self.call_edge(id))
                }
                _ => None,
            };
            let origin = |base| self.origin(base, id);
            let host = match *expr {
                Expr::Call { .. } | Expr::SafeNavCall { .. } => self
                    .raw_table_receiver(id)
                    .map_or(CollectionOrigin::Rock, origin),
                Expr::Index { base, .. }
                    if matches!(
                        self.collection_ctor(base),
                        Some(TypeCtor::Array | TypeCtor::Map)
                    ) =>
                {
                    origin(base)
                }
                Expr::FieldAccess { base, field } | Expr::SafeNav { base, field }
                    if field.as_str() == "len"
                        && self.collection_ctor(base) == Some(TypeCtor::Array) =>
                {
                    origin(base)
                }
                Expr::FieldAccess { base, .. }
                | Expr::SafeNav { base, .. }
                | Expr::Spread(base)
                    if host_access(base) =>
                {
                    origin(base)
                }
                Expr::StructLiteral { bases, .. } => bases
                    .into_iter()
                    .filter(|&base| host_access(base))
                    .fold(CollectionOrigin::Rock, |seen, base| {
                        seen.merge(origin(base))
                    }),
                _ => CollectionOrigin::Rock,
            };
            if let Some(edge) = call {
                operations.push((Expression(id), Operation::Call(edge)));
            }
            if host != CollectionOrigin::Rock {
                operations.push((Expression(id), Operation::HostAccess(host)));
            }
            let pattern = match *expr {
                Expr::IfLet { pattern, value, .. }
                    if self.pattern_may_callback(host_records, pattern) =>
                {
                    Some(value)
                }
                Expr::Match {
                    scrutinee, arms, ..
                } if arms.into_iter().any(|arm| {
                    self.pattern_may_callback(host_records, self.body.match_arm_arena[arm].pattern)
                }) =>
                {
                    Some(scrutinee)
                }
                _ => None,
            };
            if let Some(value) = pattern {
                operations.push((
                    Expression(value),
                    Operation::HostAccess(CollectionOrigin::Host),
                ));
            }
            match *expr {
                Expr::Assign { .. } => {
                    if let Some(target) = self.assigned_collection(id) {
                        operations.push((Expression(id), Operation::Direct(target)));
                    }
                }
                Expr::StructLiteral { fields, bases, .. } if bases.is_empty() => operations.extend(
                    self.default_fields(id, fields)
                        .into_iter()
                        .map(|edge| (Expression(id), Operation::Implicit(Some(edge)))),
                ),
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
        operations
    }

    fn default_fields(
        &self,
        literal: Idx<Expr>,
        fields: IdxRange<crate::hir::StructLitField>,
    ) -> Vec<Edge> {
        self.field_defaults(literal, fields).unwrap_or_default()
    }

    fn field_defaults(
        &self,
        literal: Idx<Expr>,
        fields: IdxRange<crate::hir::StructLitField>,
    ) -> Option<Vec<Edge>> {
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
        let generics: Vec<_> = params
            .into_iter()
            .map(|param| self.key(ty.with_idx(param)))
            .collect();
        Some(
            defaults
                .field_defaults
                .iter()
                .filter(|(name, _)| {
                    !fields
                        .into_iter()
                        .any(|field| self.body.struct_lit_field_arena[field].name == **name)
                })
                .map(|(_, &root)| Edge {
                    callee: ((id.module, owner), Some(root)),
                    generics: generics.clone(),
                    args: Vec::new(),
                })
                .collect(),
        )
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
            callee: (function_body(callee), None),
            generics,
            // A missing receiver here is the iterator returned by a preceding iter call.
            args: receiver
                .map(|arg| vec![Some(self.arg(arg, arg))])
                .unwrap_or_default(),
        }
    }

    /// Map index assignment inserts; a Vec `len` write resizes.
    fn assigned_collection(&self, e: Idx<Expr>) -> Option<EffectOn> {
        let Expr::Assign { target, .. } = self.body.expr_arena[e] else {
            return None;
        };
        let base = match self.body.expr_arena[target] {
            Expr::Index { base, .. } if self.collection_ctor(base) == Some(TypeCtor::Map) => base,
            Expr::FieldAccess { base, field }
                if field.as_str() == "len"
                    && self.collection_ctor(base) == Some(TypeCtor::Array) =>
            {
                base
            }
            _ => return None,
        };
        let (fresh, param) = self.place_binding(base);
        Some(
            fresh
                .map(Local)
                .or_else(|| param.map(Param))
                .or_else(|| self.element_key(base).map(Element))
                .unwrap_or(Unknown),
        )
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
            callee: (function_body(callee), None),
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
            && self.ref_values(binding).is_some()
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
                for &value in self.ref_values(*binding).into_iter().flatten() {
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

fn map_edge(mapped: &mut Effect, edge: &Edge, callee: &Effect, at: Span) {
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
}

fn declared_effect(ws: &Workspace, (module, owner): BodyKey, has_body: bool) -> Effect {
    let mut effect = Effect::default();
    let BodyOwner::Function(id) = owner else {
        return effect;
    };
    let module = ws.module(module);
    let f = &module.functions[id];
    let trusted = is_stdlib_module_file(module.file.as_str());
    if has_body {
        if trusted
            && f.name.as_str() == "remove"
            && f.param_decls
                .first()
                .is_some_and(|p| arena_element(ws, None, p.ty).is_some())
        {
            effect.add(Param(0), f.span);
        }
    } else {
        if f.annotations
            .iter()
            .any(|a| a.is_builtin(RELOCATES_ANNOTATION))
        {
            effect.add(Param(0), f.span);
        } else if !trusted
            || matches!(
                f.name.as_str(),
                "resume" | "kill" | "fire_hot_reload" | "__rock_coroutine_kill"
            )
            || f.param_decls.iter().any(|p| {
                matches!(
                    ws.resolve_ty(None, p.ty),
                    Some((_, TypeExpr::Function { .. }))
                )
            })
        {
            effect.add(Unknown, f.span);
        }
        if trusted
            && f.inline_body.is_some()
            && f.param_decls.first().is_some_and(|param| {
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
            effect.add(HostParam(0), f.span);
        }
    }
    if f.is_coroutine {
        for (i, param) in f.param_decls.iter().enumerate() {
            if ws.type_contains_ref(None, param.ty) {
                effect.add(Retained(i as u16), f.span);
            }
        }
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

fn walk_region(body: &Body, root: HirChild, mut visit: impl FnMut(HirChild) -> bool) {
    let mut visit = |node| {
        visit(node)
            && match node {
                HirChild::Expr(e) => !matches!(body.expr_arena[e], Expr::Lambda { .. }),
                HirChild::Statement(s) => !matches!(body.statement_arena[s], Statement::Fn { .. }),
                HirChild::Pattern(_) => true,
            }
    };
    if visit(root) {
        body.walk_descendants(root, &mut visit);
    }
}

fn evaluated(body: &Body, root: Idx<Expr>, mut visit: impl FnMut(HirChild)) {
    walk_region(body, HirChild::Expr(root), |child| {
        visit(child);
        true
    });
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
            let ctx = BodyCtx::new(ws, Some(function), body, bi, self, None);
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
    facts: FxHashMap<BodyKey, Operations>,
    regions: FxHashMap<Region, RegionEffects>,
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
            regions: FxHashMap::default(),
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
            let ctx = BodyCtx::new(ws, function, body, bi, &program.return_origins, None);
            let mut roots = Vec::new();
            for &arg in bi.ref_call_args.values().flatten() {
                ctx.element_roots(arg, &mut roots);
            }
            for values in ctx.values.values() {
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
        let mut pending: Vec<_> = program.sites.iter().map(|&key| (key, None)).collect();
        if program
            .relevance
            .keys
            .iter()
            .any(|key| key.starts_with("Arena<"))
        {
            program.host_records = HostRecords::collect(ws);
        }
        let mut callers = FxHashMap::<Region, Vec<Region>>::default();
        while let Some(region @ (key, root)) = pending.pop() {
            if program.regions.contains_key(&region) {
                continue;
            }
            let inferred = inferred_site(ws, key);
            let mut effects = RegionEffects {
                members: None,
                summary: declared_effect(ws, key, inferred.is_some()),
            };
            if let Some((function, body, bi)) = inferred {
                let ops = program.facts.entry(key).or_insert_with(|| {
                    BodyCtx::new(
                        ws,
                        function,
                        body,
                        bi,
                        &program.return_origins,
                        program
                            .host_records
                            .host_boundary
                            .then_some(&program.host_records),
                    )
                    .facts(&program.host_records)
                });
                effects.members = root.map(|root| {
                    let mut members = FxHashSet::default();
                    evaluated(body, root, |child| {
                        if let HirChild::Expr(expr) = child {
                            members.insert(expr);
                        }
                    });
                    members
                });
                for (_, op) in effects.operations(ops) {
                    if let Some(edge) = op.edge() {
                        callers.entry(edge.callee).or_default().push(region);
                        pending.push(edge.callee);
                    }
                }
            }
            program.regions.insert(region, effects);
        }
        program.solve(&callers);
        program
    }

    fn apply(&self, effect: &mut Effect, body: &Body, point: SuspensionPoint, op: &Operation) {
        let (Expression(expr) | IteratorNext(expr)) = point;
        let at = body.expr_spans[expr];
        match op {
            Operation::Call(Some(edge)) | Operation::Implicit(Some(edge)) => {
                map_edge(effect, edge, &self.regions[&edge.callee].summary, at);
            }
            Operation::Call(None) | Operation::Implicit(None) => effect.add(Unknown, at),
            Operation::Direct(target) => effect.add(target.clone(), at),
            Operation::HostAccess(origin) => effect.add_host_access(*origin, at),
        }
    }

    /// Summaries only grow, and keys are bounded, so the worklist drains.
    fn solve(&mut self, callers: &FxHashMap<Region, Vec<Region>>) {
        let mut pending: Vec<_> = self
            .regions
            .keys()
            .copied()
            .filter(|key| self.facts.contains_key(&key.0))
            .collect();
        let mut queued: FxHashSet<_> = pending.iter().copied().collect();
        while let Some(region @ (key @ (module, owner), _)) = pending.pop() {
            queued.remove(&region);
            let body = &self.ws.module(module).bodies[&owner];
            let info = &self.regions[&region];
            let mut effect = declared_effect(self.ws, key, true);
            for &(point, ref op) in info.operations(&self.facts[&key]) {
                self.apply(&mut effect, body, point, op);
            }
            effect.0.retain(|target, _| !matches!(target, Local(_)));
            self.relevance.trim(&mut effect);
            if !info.summary.0.keys().eq(effect.0.keys()) {
                self.regions
                    .get_mut(&region)
                    .expect("queued region")
                    .summary = effect;
                for &caller in callers.get(&region).into_iter().flatten() {
                    if queued.insert(caller) {
                        pending.push(caller);
                    }
                }
            }
        }
    }

    fn rejections(&self, mid: ModuleId, owner: BodyOwner) -> Vec<CompilerError> {
        let Some((function, body, bi)) = inferred_site(self.ws, (mid, owner)) else {
            return Vec::new();
        };
        let ctx = BodyCtx::new(self.ws, function, body, bi, &self.return_origins, None);
        let ops = &self.facts[&(mid, owner)];
        let mut effects = EffectsAt::default();
        for &(point, ref op) in ops {
            self.apply(effects.entry(point).or_default(), body, point, op);
        }
        for effect in effects.values_mut() {
            self.relevance.trim(effect);
        }
        let mut errors = Vec::new();
        let mut borrowed = FxHashMap::default();
        let mut active_values = FxHashSet::default();
        for (&binding, values) in &ctx.values {
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
        for (point, operation) in ops {
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
                        self.regions[&edge.callee]
                            .summary
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
                    HirChild::Expr(id) => Expression(id),
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
