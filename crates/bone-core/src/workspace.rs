//! Compilation storage + inference RESULT data (no algorithm).
//! `Workspace` is the shared store both lowering and inference build on;
//! the result structs are pure data the inference algorithm populates.

use crate::hir::*;
use crate::prelude::*;

#[derive(Clone, Debug)]
pub struct Workspace {
    pub modules: Vec<Module>,
    pub module_ids: FxHashMap<Ustr, ModuleId>,

    pub builtins: Builtins,
    pub inferences: Vec<Option<InferenceResult>>,

    pub struct_by_name: FxHashMap<Ustr, StructId>,
    /// Any struct anywhere declares an `embed` field. Set (never cleared) while
    /// registering struct fields, so the per-body projection-only check can bail
    /// in O(1) on the overwhelmingly common workspace that uses no `embed` at
    /// all. Monotonic on purpose: staying true after the last embed field is
    /// deleted costs a check, whereas a stale false would skip enforcement.
    pub has_embed_fields: bool,
    /// Any value type is declared `nobox`. Monotonic for the same incremental
    /// safety reason as `has_embed_fields`.
    pub has_nobox_types: bool,
    pub enums_by_name: FxHashMap<Ustr, EnumId>,
    pub newtypes_by_name: FxHashMap<Ustr, NewtypeId>,
    /// Declared generic-parameter count for every nominal type (struct / enum /
    /// newtype) across ALL modules, keyed by name. Built from the raw AST in
    /// `lower_modules` BEFORE type lowering, so the bare-generic arity check in
    /// `lower_type_expr_in` works cross-module (during the initial parallel
    /// lowering the other modules aren't in `ws.modules` yet -- this map is).
    /// Names are unique workspace-wide (duplicate-type-name is rejected).
    pub type_generic_arity: FxHashMap<Ustr, usize>,
    /// Trailing defaulted generic params per nominal, keyed by name (the
    /// `type_generic_arity` pattern: built from the raw AST in `lower_modules`
    /// BEFORE lowering, insert-not-clear for incremental). `defaults.len() <=
    /// arity`; a use site supplying `arity - defaults.len()..=arity` args pads
    /// the missing tail from here, fill-from-right. Best-effort: only cleanly
    /// classifiable trailing defaults are stored -- the authoritative
    /// validation (with spans) is bone-lower's `validate_generic_defaults`.
    pub type_generic_defaults: FxHashMap<Ustr, Vec<hir::GenericDefault>>,
    pub interface_names: FxHashSet<Ustr>,
    pub globals_by_name: FxHashMap<Ustr, TyId>,
    /// Names of `const` globals (read-only). A whole-global assignment to one of
    /// these is rejected in inference; mutable `global`s are absent.
    pub const_globals: FxHashSet<Ustr>,

    pub functions_by_name: FxHashMap<Ustr, FnList>,

    /// Workspace-wide `deco` declaration names, collected from the raw ASTs in
    /// `lower_modules` BEFORE lowering (the `type_generic_arity` pattern) so
    /// annotation validation can accept `@<deco>` cross-module. Insert-only:
    /// the incremental single-module re-lower augments rather than wipes.
    pub decorator_names: FxHashSet<Ustr>,

    pub phase_timings: Vec<(&'static str, Duration)>,

    /// Count of function bodies actually re-inferred (check_function run, NOT
    /// reused) since the last reset. `check_module_incremental` bumps it on the
    /// non-reuse branch; the incremental `Session` resets it per `update_file`,
    /// so a test/profiler can read EXACTLY how many bodies re-inferred -- a
    /// deterministic signal for reuse, replacing flaky phase-timing ratios. Arc
    /// keeps `Workspace: Clone` and lets the parallel dependent re-check (which
    /// holds `&Workspace`) share one counter across threads.
    pub reinferred_body_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Count of function bodies LOWERED since the last reset: every function
    /// a full or partial module lowering builds, including one whose body
    /// fails to lower, and the one function the body fast path re-lowers.
    /// The lowering twin of `reinferred_body_count`, reset by the session per
    /// update; `UpdateResult.relowered` is derived from inference keys, so a
    /// body that re-lowers into a lowering error is only visible here.
    pub relowered_body_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,

    /// Modules-v1 namespace scope (plan M2). Empty for legacy-only
    /// workspaces — every legacy path behaves exactly as before when `ns`
    /// has no packages and `fragment_scopes` is empty.
    pub ns: crate::modules::NsTree,
    /// Per-file v1 scope facts: package, logical module path, and the
    /// fragment's own imports. Keyed by file id (`Module.file`).
    pub fragment_scopes: FxHashMap<Ustr, crate::modules::FragmentScope>,
    /// Canonical definition metadata keyed by DefKey (plan 2.1). Legacy
    /// definitions keep their bare-name keys and gain no entry.
    pub defs: FxHashMap<Ustr, crate::modules::DefMeta>,
    /// Per-legacy-file compatibility views over the public root exports of
    /// participating v1 packages. Built once by the semantic pass; lowering
    /// only reads the finished view.
    pub legacy_projections: FxHashMap<Ustr, crate::modules::LegacyProjection>,
    /// Contextual item-level `use` bindings in legacy files. These are
    /// additive and do not change legacy `x :: T` ascription parsing.
    pub legacy_imports: FxHashMap<Ustr, crate::modules::LegacyImports>,

    /// Resolved compile-time inputs (docs/comptime-plan.md tier 1), set by the
    /// host once per session build before emission. bone itself never reads them
    /// -- values don't affect types -- but they live here because every
    /// consumer (skull's `build_var`/`embed_string` expansion at MIR lowering,
    /// on all backends) already holds a `&Workspace` and nothing else with
    /// that reach exists. Empty in LSP/typecheck-only contexts, where no
    /// expansion runs.
    pub comptime_inputs: ComptimeInputs,
}

/// See [`Workspace::comptime_inputs`].
#[derive(Clone, Debug, Default)]
pub struct ComptimeInputs {
    pub vars: FxHashMap<Ustr, ComptimeVarValue>,
    /// Declared metadata whose value is intentionally outside executable
    /// Artifact identity (for example a presentation build clock). The
    /// compiler validates every name before excluding its value hash.
    pub volatile_vars: FxHashSet<Ustr>,
    /// Effective declared build channel for package metadata. Empty means a
    /// synthetic/type-only workspace did not come through CLI resolution.
    pub channel: String,
    /// Source-language mode of the root package ("legacy" or "v1", plan M0).
    /// Part of the Artifact fingerprint: the same sources under different
    /// language modes are different builds.
    pub modules_mode: String,
    /// FALLBACK root for `embed_string(path)`, used only when the calling
    /// module has no file on disk to resolve from (a synthetic module).
    /// Normally the path resolves against the nearest stick.toml at or above
    /// the CALLING file. None and no calling file = every embed rejects.
    pub embed_root: Option<std::path::PathBuf>,
}

#[derive(Clone, Debug)]
pub struct ComptimeVarValue {
    pub value: String,
    /// Metadata hygiene, not confidentiality (docs/comptime-plan.md): hashed
    /// into cache keys, ALLOWED in the compiled program payload, and kept out
    /// of logs, diagnostics, manifests and cache metadata.
    pub secret: bool,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace {
    pub fn new() -> Self {
        use hir::BuiltinTy;

        let mut bm = Module {
            id: ModuleId::builtin(),
            file: Ustr::from("$builtin"),
            ..Default::default()
        };

        let builtins = Builtins {
            int: bm.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Int)),
            uint: bm.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Uint)),
            float: bm.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Float)),
            bool: bm.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Bool)),
            string: bm.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::String)),
            unit: bm.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Unit)),
            any: bm.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Any)),
            never: bm.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Never)),
            error: bm.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Error)),
        };

        let mut ws = Self {
            modules: Default::default(),
            module_ids: Default::default(),

            builtins,
            inferences: Default::default(),

            struct_by_name: Default::default(),
            has_embed_fields: false,
            has_nobox_types: false,
            enums_by_name: Default::default(),
            newtypes_by_name: Default::default(),
            type_generic_arity: Default::default(),
            type_generic_defaults: Default::default(),
            interface_names: Default::default(),
            globals_by_name: Default::default(),
            const_globals: Default::default(),
            functions_by_name: Default::default(),
            decorator_names: Default::default(),

            phase_timings: Default::default(),

            reinferred_body_count: Default::default(),
            relowered_body_count: Default::default(),
            comptime_inputs: Default::default(),
            ns: Default::default(),
            fragment_scopes: Default::default(),
            defs: Default::default(),
            legacy_projections: Default::default(),
            legacy_imports: Default::default(),
        };

        ws.module_ids.insert(ustr("$builtin"), ModuleId::builtin());
        ws.modules.push(bm);

        ws
    }

    pub fn module(&self, mid: ModuleId) -> &Module {
        &self.modules[mid.index()]
    }

    pub fn module_mut(&mut self, mid: ModuleId) -> &mut Module {
        &mut self.modules[mid.index()]
    }

    /// The per-declaration visibility records for a v1 callable family.
    /// Legacy and compiler-owned callable keys have no namespace member table
    /// and return `None`, which means their existing visibility remains public.
    pub fn callable_family_members(&self, key: Ustr) -> Option<&[crate::modules::CallableMember]> {
        let meta = self.defs.get(&key)?;
        if meta.kind != crate::modules::DefKind::Callable {
            return None;
        }
        let package = self.ns.package(meta.package)?;
        let node = package.node(package.key_of(&meta.module))?;
        node.callable_members.get(&meta.name).map(Vec::as_slice)
    }

    /// The declared visibility of one lowered callable member. The source
    /// namespace table remains authoritative, so a visibility-only edit does
    /// not require a second visibility field on HIR functions.
    pub fn callable_member_visibility(
        &self,
        key: Ustr,
        mid: ModuleId,
        fid: Idx<Function>,
    ) -> Option<&crate::modules::NsVisibility> {
        let function = &self.module(mid).functions[fid];
        self.callable_family_members(key)?
            .iter()
            .find(|member| member.file == self.module(mid).file && member.span == function.span)
            .map(|member| &member.vis)
    }

    /// Whether a lowered callable member is admitted at an access site.
    /// Callable keys without v1 member metadata are legacy or compiler-owned
    /// families and retain their existing public behavior.
    pub fn callable_member_admits(
        &self,
        key: Ustr,
        mid: ModuleId,
        fid: Idx<Function>,
        site: crate::modules::AccessSite<'_>,
    ) -> bool {
        let Some(meta) = self.defs.get(&key) else { return true };
        let Some(from_pkg) = site.package else { return true };
        let vis = self.callable_member_visibility(key, mid, fid).unwrap_or(&meta.vis);
        self.ns.can_access(vis, meta.package, &meta.module, from_pkg, site.module_path)
    }

    /// Whether a lowered callable member is public in its defining namespace.
    pub fn callable_member_is_public(&self, key: Ustr, mid: ModuleId, fid: Idx<Function>) -> bool {
        self.callable_member_visibility(key, mid, fid)
            .map_or_else(
                || {
                    self.defs
                        .get(&key)
                        .is_some_and(|meta| meta.vis == crate::modules::NsVisibility::Pub)
                },
                |vis| *vis == crate::modules::NsVisibility::Pub,
            )
    }

    pub fn inference(&self, mid: ModuleId) -> Option<&InferenceResult> {
        self.inferences.get(mid.index()).and_then(|o| o.as_ref())
    }

    pub fn set_inference(&mut self, mid: ModuleId, inf: InferenceResult) {
        let idx = mid.index();

        if idx >= self.inferences.len() {
            self.inferences.resize_with(idx + 1, || None);
        }

        self.inferences[idx] = Some(inf);
    }

    pub fn inferences_iter(&self) -> impl Iterator<Item = (ModuleId, &InferenceResult)> {
        self.inferences
            .iter()
            .enumerate()
            .filter_map(|(i, o)| o.as_ref().map(|inf| (ModuleId::new(i as u32), inf)))
    }

    pub fn intern_module(&mut self, file: Ustr) -> ModuleId {
        if let Some(&id) = self.module_ids.get(&file) {
            return id;
        }

        let id = ModuleId::new(self.module_ids.len() as u32);
        self.module_ids.insert(file, id);
        id
    }

    pub fn insert_module(&mut self, module: Module) -> ModuleId {
        let id = module.module_id();

        let idx = id.index();

        if idx < self.modules.len() {
            self.modules[idx] = module;
        } else {
            debug_assert_eq!(
                idx,
                self.modules.len(),
                "insert_module: non-sequential module id leaves a gap"
            );
            self.modules.push(module);
        }

        id
    }

    pub fn log_time(&mut self, label: &'static str, timer: Instant) {
        self.phase_timings.push((label, timer.elapsed()));
    }

    pub fn builtin_module(&self) -> &Module {
        self.module(ModuleId::builtin())
    }

    pub fn render(&self, body: Option<&BodyInference>, ty: TyId) -> String {
        let te = match ty {
            TyId::Static(mid, idx) => self.module(mid).ty_arena[idx],
            TyId::Local(idx) => body.expect("Local type needs its BodyInference").ty_arena[idx],
        };

        match te {
            TypeExpr::Builtin(b) => match b {
                BuiltinTy::Int => "int".into(),
                BuiltinTy::Uint => "uint".into(),
                BuiltinTy::Float => "float".into(),
                BuiltinTy::Bool => "bool".into(),
                BuiltinTy::String => "string".into(),
                BuiltinTy::Unit => "()".into(),
                BuiltinTy::Any => "any".into(),
                BuiltinTy::Never => "never".into(),
                BuiltinTy::Error => "<error>".into(),
            },

            TypeExpr::Generic(_, name) => name.to_string(),
            TypeExpr::Infer(var_id, name) => match body.and_then(|b| b.substitutions.get(var_id)) {
                Some(sub) => self.render(body, sub),
                None => name.to_string(),
            },

            TypeExpr::Nullable(inner) => format!("{}?", self.render(body, ty.with_idx(inner))),

            TypeExpr::App { ctor, params } => {
                let params_str = params
                    .map(|x| self.render(body, ty.with_idx(x)))
                    .collect_vec()
                    .join(", ");

                match ctor {
                    hir::TypeCtor::Array => format!("[{}]", params_str),
                    hir::TypeCtor::Map => {
                        format!("Map<{}>", params_str)
                    }
                    hir::TypeCtor::Ref => {
                        format!("ref {}", params_str)
                    }

                    hir::TypeCtor::Nominal(name) => {
                        if params.is_empty() {
                            name.to_string()
                        } else {
                            format!("{}<{}>", name, params_str)
                        }
                    }
                }
            }

            TypeExpr::Function { params, ret } => {
                let params_str = params
                    .map(|x| self.render(body, ty.with_idx(x)))
                    .collect_vec()
                    .join(", ");

                format!(
                    "fn({}) -> {}",
                    params_str,
                    self.render(body, ty.with_idx(ret))
                )
            }

            TypeExpr::Tuple { elements } => {
                format!(
                    "({})",
                    elements
                        .map(|x| self.render(body, ty.with_idx(x)))
                        .collect_vec()
                        .join(", ")
                )
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Substitutions(Vec<Option<TyId>>);

impl Substitutions {
    pub fn get(&self, v: VarId) -> Option<TyId> {
        self.0.get(v.0 as usize).copied().flatten()
    }

    pub fn bind(&mut self, v: VarId, t: TyId) -> Option<TyId> {
        let i = v.0 as usize;

        if i >= self.0.len() {
            self.0.resize(i + 1, None);
        }

        self.0[i].replace(t)
    }

    pub fn restore(&mut self, v: VarId, prev: Option<TyId>) {
        self.0[v.0 as usize] = prev;
    }
}

/// How a bounded generic's `next` resolves at one instantiation site. Built at
/// mono roots (a `for` over a concrete chain type, or a call of a bounded fn)
/// during conformance checking; skull's inliner consumes it to substitute the
/// placeholder bound-calls inside the spliced callee body.
#[derive(Clone, Debug, PartialEq)]
pub enum BoundNext {
    /// Concrete instantiation: the resolved `next` overload, plus the tree for
    /// ITS bounded generics (chains nest: `next(MapIter<VecIter<..>, ..>)`
    /// resolves to MapIter's `next`, whose inner bound resolves to VecIter's).
    Concrete {
        func: (ModuleId, Idx<Function>),
        sub: BoundTree,
        /// `func`'s own generic instantiation at this site, callee `VarId` ->
        /// caller-arena `TyId`. A `for`-protocol `next` call is SYNTHESIZED (it
        /// has no call expr), so it can never be keyed in
        /// `call_generic_bindings` -- this is the only record of what its
        /// generics bound to, and skull needs it to specialize the callee body
        /// (`VecIter<Vec2>.next` reading flat element storage rather than the
        /// shared generic snapshot's boxed one).
        next_generics: FxHashMap<VarId, TyId>,
    },
    /// The instantiation was the ENCLOSING fn's own bounded generic (bound
    /// propagation): resolves via the caller's placeholder for that generic
    /// index when the caller is itself inlined at a concrete root.
    CallerGeneric(usize),
}

/// Per bounded-generic index of a resolved callee: how its `next` bound-calls
/// resolve. Empty for unbounded callees.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BoundTree {
    pub next: FxHashMap<usize, BoundNext>,
}

impl BoundTree {
    pub fn is_empty(&self) -> bool {
        self.next.is_empty()
    }
}

/// A synthesized `iter` call: the resolved fn plus its own generic
/// instantiation, callee `VarId` -> caller-arena `TyId`.
///
/// Symmetric to [`BoundNext::Concrete`]'s `next_generics`, and for the same
/// reason: a synthesized call has no call expr, so it can never be keyed in
/// `call_generic_bindings`, and this is the only record of what its generics
/// bound to. Empty for a non-generic `iter`. Resolution computes this
/// substitution to type the call; retaining it recomputes nothing.
#[derive(Clone, Debug, PartialEq)]
pub struct IterWitness {
    pub func: (ModuleId, Idx<Function>),
    pub generics: FxHashMap<VarId, TyId>,
}

/// `for x in coll` resolution through the iterator protocol: the optional
/// `iter` witness (`None` = identity, the collection IS the iterator -- bare
/// combinator chains and bounded generics iterate as themselves) and how
/// `next` resolves (concrete fn + bound tree, or the enclosing fn's bounded
/// generic).
#[derive(Clone, Debug, PartialEq)]
pub struct ForIterInfo {
    pub iter: Option<IterWitness>,
    pub next: BoundNext,
}

#[derive(Clone, Debug, Default)]
pub struct BodyInference {
    /// Body.generation at inference time - trusted only while they match.
    pub body_generation: u64,

    pub expr_types: ArenaMap<Idx<Expr>, TyId>,
    pub binding_types: ArenaMap<Idx<BindingInfo>, TyId>,
    pub resolved_overload_calls: FxHashMap<Idx<Expr>, (ModuleId, Idx<Function>)>,
    /// Calls to GENERIC fns: the winning candidate's instantiated generics in
    /// declaration order, keyed like `resolved_overload_calls`. Entries may
    /// still be inference vars at record time -- readers resolve through
    /// `substitutions` like `expr_types`. skull's inliner reads this to
    /// specialize value-instantiated `@inline` splices (docs/flatvec-plan.md).
    pub call_generic_bindings: FxHashMap<Idx<Expr>, Vec<TyId>>,
    /// `for x in <coll>` over a user type: the resolved iter/next protocol for
    /// the desugar, keyed by the For's iter `Idx<Expr>`. Separate from
    /// `resolved_overload_calls` so it can't collide with the collection
    /// expr's own call resolution (e.g. `for x in foo()`). skull's `lower_for`
    /// reads it to emit the desugared loop.
    pub for_iter_protocol: FxHashMap<Idx<Expr>, ForIterInfo>,
    /// In a body with `I: Iterator<E>` bounds: `next(x)` calls where `x: I`.
    /// Keyed by the call expr; value = the generic's index into the enclosing
    /// fn's `generics`. skull lowers these as placeholder callees resolved at
    /// inline time.
    pub bound_calls: FxHashMap<Idx<Expr>, usize>,
    /// Same, for `iter(x)` where `x: I` -- identity (types as `I`, lowers to
    /// the argument itself, no call).
    pub bound_identity_calls: FxHashSet<Idx<Expr>>,
    /// Calls of bounded fns: the conformance-resolution tree built at the call
    /// site, keyed by the call expr. Tells skull how to substitute the
    /// placeholder bound-calls inside that callee when it is inlined here.
    pub bound_call_trees: FxHashMap<Idx<Expr>, BoundTree>,
    /// Args (incl. UFCS receivers) implicitly coerced iterable->iterator at an
    /// `Iterator`-bounded param: arg expr -> the resolved `iter` witness. skull
    /// wraps the lowered arg value in this call (the call-argument analog of
    /// `for_iter_protocol`'s `iter`).
    pub implicit_iter_args: FxHashMap<Idx<Expr>, IterWitness>,
    /// `StructDefaults` bodies only: the fresh inference var each of the
    /// struct's DECLARED generics was instantiated to, in declaration order.
    /// Empty for every other body owner.
    ///
    /// Checking a field default replaces the declared `Generic(T)` with a fresh
    /// var and coerces the initializer against the substituted field type, so
    /// the default's own generic joins that var's class. Without this the map
    /// is local to the check, and a consumer holding `Box<int>` cannot say
    /// which var is `Box`'s generic 0 -- so the default lowers to the SHARED
    /// generic body and names no instance. skull's `with_struct_default` binds
    /// these to the literal's concrete arguments.
    ///
    /// Entries may still be unbound vars at record time; readers resolve
    /// through `substitutions`, like `expr_types`.
    pub struct_default_generic_slots: Vec<TyId>,

    pub ty_arena: Arena<TypeExpr>,
    pub ty_spans: ArenaMap<Idx<TypeExpr>, Span>,

    pub substitutions: Substitutions,

    pub trail: Vec<(VarId, Option<TyId>)>,

    pub deps: FxHashSet<Ustr>,
}

impl BodyInference {
    pub fn record_resolved_call(
        &mut self,
        call_expr: Idx<Expr>,
        callee_mid: ModuleId,
        callee_fn: Idx<Function>,
        error_span: Span,
    ) -> Result<(), CompilerError> {
        if let Some(previous) = self
            .resolved_overload_calls
            .insert(call_expr, (callee_mid, callee_fn))
        {
            comp_error!(
                error_span,
                "Recording resolved overload twice, was {:?}, now {:?}",
                previous,
                (callee_mid, callee_fn)
            );
        }

        Ok(())
    }

    pub fn bind_var(&mut self, v: VarId, t: TyId) {
        let prev = self.substitutions.bind(v, t);
        self.trail.push((v, prev));
    }

    pub fn checkpoint(&self) -> usize {
        self.trail.len()
    }

    pub fn rollback_to(&mut self, cp: usize) {
        while self.trail.len() > cp {
            let (v, prev) = self.trail.pop().unwrap();
            self.substitutions.restore(v, prev);
        }
    }
}


#[derive(Clone, Debug, Default)]
pub struct InferenceResult {
    pub per_fn: FxHashMap<BodyOwner, BodyInference>,
    pub errors: FxHashMap<BodyOwner, Vec<CompilerError>>,
}

impl InferenceResult {
    // TODO: probably want to remove this?
    pub fn body_at(&self, module: &Module, offset: usize) -> Option<&BodyInference> {
        self.per_fn.get(&module.body_at(offset)?)
    }
}
