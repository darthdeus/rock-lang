//! Compilation storage shared by lowering and inference, plus the inference result data.

use crate::hir::*;
use crate::prelude::*;

/// Record [`Workspace::module_timings`]; off unless a host reports source costs.
pub static MODULE_TIMINGS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Generic parameters read by lowering before nominal definitions are available.
#[derive(Clone, Debug)]
pub struct TypeGenerics {
    pub owner: ModuleId,
    pub arity: usize,
    /// Best-effort trailing defaults; authoritative validation is in bone-lower.
    pub defaults: Vec<hir::GenericDefault>,
}

#[derive(Clone, Debug)]
pub struct Workspace {
    pub modules: Vec<Module>,
    pub module_ids: FxHashMap<Ustr, ModuleId>,

    pub builtins: Builtins,
    pub inferences: Vec<Option<InferenceResult>>,

    pub struct_by_name: FxHashMap<Ustr, StructId>,
    /// Set when an embed field is registered. A stale true only costs a check.
    pub has_embed_fields: bool,
    pub enums_by_name: FxHashMap<Ustr, EnumId>,
    pub newtypes_by_name: FxHashMap<Ustr, NewtypeId>,
    /// Keys are canonical DefKeys for user items and bare names for compiler-owned items.
    /// The first defining module owns an entry; re-lowering refreshes its own.
    pub type_generics: FxHashMap<Ustr, TypeGenerics>,
    pub interface_names: FxHashSet<Ustr>,
    pub globals_by_name: FxHashMap<Ustr, TyId>,
    /// Names of `const` globals; a whole-global assignment to one is rejected.
    pub const_globals: FxHashSet<Ustr>,

    pub functions_by_name: FxHashMap<Ustr, FnList>,
    /// Declared parameter types with a `\u{1}` separator. Keep indexed when
    /// replacing a function signature in place.
    pub param_signatures: Vec<ArenaMap<Idx<Function>, Ustr>>,

    /// Workspace-wide `deco` names, collected before lowering so annotations accept `@<deco>`
    /// cross-module. Insert-only.
    pub decorator_names: FxHashSet<Ustr>,

    pub phase_timings: Vec<(&'static str, Duration)>,
    /// Per-module time of the front-end passes that run module by module,
    /// recorded only while [`MODULE_TIMINGS`] is set.
    pub module_timings: Vec<(ModuleId, &'static str, Duration)>,

    /// Counts re-inferred bodies since the last session update reset. Parallel
    /// checks share this counter.
    pub reinferred_body_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Per-function memory effects (`crate::effects`), computed on first use
    /// from the current inferences and dropped when one is replaced.
    pub vec_effects: std::sync::OnceLock<crate::effects::EffectTable>,
    /// Counts even failed body lowerings, which have no inference keys.
    pub relowered_body_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,

    /// Package namespace scope, including implicit standalone packages.
    pub ns: crate::modules::NsTree,
    /// Per-file v1 scope facts: package, logical module path, and the
    /// fragment's own imports. Keyed by file id (`Module.file`).
    pub fragment_scopes: FxHashMap<Ustr, crate::modules::FragmentScope>,
    /// Canonical definition metadata keyed by DefKey. Compiler-owned
    /// definitions can keep bare-name keys without metadata.
    pub defs: FxHashMap<Ustr, crate::modules::DefMeta>,

    /// Host inputs stamped after checking for expansion during code generation.
    pub comptime_inputs: ComptimeInputs,

    /// Enforce identity storage in files outside any package instead of counting them as unmigrated
    /// unpackaged programs.
    pub identity_storage_enforce_unpackaged: bool,

    /// Skip recording scope probes and navigation spans when no session retains HIR.
    pub skip_session_facts: bool,
}

/// See [`Workspace::comptime_inputs`].
#[derive(Clone, Debug, Default)]
pub struct ComptimeInputs {
    /// Explicit compiler-derived inputs, resolved after checking. Kept after
    /// resolution so retained builds can recompute them from updated sources.
    pub source_fingerprints: FxHashMap<Ustr, SourceFingerprint>,
    /// Native files and compiled-byte hashes used by resolved fingerprints.
    /// Retained with the values so watcher seeding and rollback use the same snapshot.
    pub source_fingerprint_files: FxHashMap<Ustr, u64>,
    pub vars: FxHashMap<Ustr, ComptimeVarValue>,
    /// Declared metadata whose value stays out of artifact identity; each name is validated before
    /// its value hash is excluded.
    pub volatile_vars: FxHashSet<Ustr>,
    /// Declared build channel for package metadata; empty in synthetic workspaces.
    pub channel: String,
    /// Fallback root for `embed_string(path)` when the calling module has no file on disk;
    /// otherwise the path resolves against the nearest stick.toml above the calling file.
    pub embed_root: Option<std::path::PathBuf>,
    /// Where `embed_string` reads from: the filesystem natively, or the
    /// files a host without one supplied.
    pub embed_files: crate::comptime_files::EmbedFiles,
}

#[derive(Clone, Debug)]
pub struct SourceFingerprint {
    pub secret: bool,
    pub roots: Vec<Ustr>,
    /// Project-relative support files; `stdlib/` names the selected snapshot.
    pub files: Vec<String>,
    /// Other recorded inputs that identify decoder/runtime implementations.
    pub vars: Vec<Ustr>,
}

#[derive(Clone, Debug)]
pub struct ComptimeVarValue {
    pub value: String,
    /// Hashed into cache keys and allowed in the program payload, but kept out of logs,
    /// diagnostics, manifests and cache metadata.
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
            f32: bm.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::F32)),
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
            enums_by_name: Default::default(),
            newtypes_by_name: Default::default(),
            type_generics: Default::default(),
            interface_names: Default::default(),
            globals_by_name: Default::default(),
            const_globals: Default::default(),
            functions_by_name: Default::default(),
            param_signatures: Default::default(),
            decorator_names: Default::default(),

            phase_timings: Default::default(),
            module_timings: Default::default(),

            reinferred_body_count: Default::default(),
            vec_effects: Default::default(),
            relowered_body_count: Default::default(),
            comptime_inputs: Default::default(),
            ns: Default::default(),
            fragment_scopes: Default::default(),
            defs: Default::default(),
            identity_storage_enforce_unpackaged: false,
            skip_session_facts: false,
        };

        ws.module_ids.insert(ustr("$builtin"), ModuleId::builtin());
        ws.modules.push(bm);
        ws.param_signatures.push(Default::default());

        ws
    }

    pub fn module(&self, mid: ModuleId) -> &Module {
        &self.modules[mid.index()]
    }

    pub fn module_mut(&mut self, mid: ModuleId) -> &mut Module {
        &mut self.modules[mid.index()]
    }

    /// `file` as generated code spells it: relative to its package when the
    /// module is known and has one, otherwise unchanged.
    pub fn logical_file(&self, file: Ustr) -> Ustr {
        self.module_ids
            .get(&file)
            .and_then(|id| self.modules.get(id.index()))
            .map(|m| m.logical_file)
            .filter(|logical| !logical.is_empty())
            .unwrap_or(file)
    }

    /// The per-declaration visibility records for a v1 callable family.
    /// Compiler-owned callable keys have no namespace member table
    /// and return `None`, which means their existing visibility remains public.
    pub fn callable_family_members(&self, key: Ustr) -> Option<&[crate::modules::CallableMember]> {
        self.callable_members_of(self.defs.get(&key)?)
    }

    fn callable_members_of(
        &self,
        meta: &crate::modules::DefMeta,
    ) -> Option<&[crate::modules::CallableMember]> {
        if meta.kind != crate::modules::DefKind::Callable {
            return None;
        }
        let node = self.ns.package(meta.package)?.node(meta.node)?;
        node.callable_members.get(&meta.name).map(Vec::as_slice)
    }

    /// Match a callable to its family member by source file and overload ordinal.
    fn member_visibility<'a>(
        &self,
        members: &'a [crate::modules::CallableMember],
        key: Ustr,
        mid: ModuleId,
        fid: Idx<Function>,
    ) -> Option<&'a crate::modules::NsVisibility> {
        let module = self.module(mid);
        let ordinal = module
            .functions_by_name
            .get(&key)?
            .iter()
            .position(|(_, candidate)| *candidate == fid)?;
        members
            .iter()
            .find(|member| member.file == module.file && member.ordinal == ordinal)
            .map(|member| &member.vis)
    }

    /// Whether a lowered callable member is admitted at an access site.
    /// Callable keys without v1 member metadata are compiler-owned
    /// families and retain their existing public behavior.
    pub fn callable_member_admits(
        &self,
        key: Ustr,
        mid: ModuleId,
        fid: Idx<Function>,
        site: crate::modules::AccessSite<'_>,
    ) -> bool {
        let Some(meta) = self.defs.get(&key) else {
            return true;
        };
        let vis = match self.callable_members_of(meta) {
            Some(members) => {
                let Some(vis) = self.member_visibility(members, key, mid, fid) else {
                    return false;
                };
                vis
            }
            None => &meta.vis,
        };
        match site.package {
            Some(from_pkg) => {
                self.ns
                    .can_access(vis, meta.package, &meta.module, from_pkg, site.module_path)
            }
            None => *vis == crate::modules::NsVisibility::Pub,
        }
    }

    /// Whether a lowered callable member is public in its defining namespace.
    pub fn callable_member_is_public(&self, key: Ustr, mid: ModuleId, fid: Idx<Function>) -> bool {
        match self.callable_family_members(key) {
            Some(members) => self
                .member_visibility(members, key, mid, fid)
                .is_some_and(|vis| *vis == crate::modules::NsVisibility::Pub),
            None => self
                .defs
                .get(&key)
                .is_some_and(|meta| meta.vis == crate::modules::NsVisibility::Pub),
        }
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
        self.vec_effects = Default::default();
    }

    /// The memory effects of every function, over the current inferences.
    pub fn effects(&self) -> &crate::effects::EffectTable {
        self.vec_effects
            .get_or_init(|| crate::effects::EffectTable::compute(self))
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
        let id = self.place_module(module);
        let signatures = self.render_param_signatures(id);
        self.store_param_signatures(id, signatures);
        id
    }

    /// [`Self::insert_module`] for every module of a pass at once.
    pub fn insert_modules(&mut self, modules: impl IntoIterator<Item = Module>) -> Vec<ModuleId> {
        let mut modules = modules.into_iter();
        let mut ids = Vec::with_capacity(modules.size_hint().0);
        while let Some(module) = modules.next() {
            // Modules are large; growing one push at a time copies every placed module
            // at each doubling.
            if module.module_id().index() >= self.modules.len()
                && self.modules.len() == self.modules.capacity()
            {
                self.modules.reserve(1 + modules.size_hint().0);
            }
            ids.push(self.place_module(module));
        }
        let signatures: Vec<_> = ids
            .par_iter()
            .map(|&id| self.render_param_signatures(id))
            .collect();
        for (&id, signatures) in ids.iter().zip(signatures) {
            self.store_param_signatures(id, signatures);
        }
        ids
    }

    fn place_module(&mut self, module: Module) -> ModuleId {
        let id = module.module_id();
        let idx = id.index();
        if idx < self.modules.len() {
            self.modules[idx] = module;
        } else {
            debug_assert_eq!(
                idx,
                self.modules.len(),
                "non-sequential module id leaves a gap"
            );
            self.modules.push(module);
        }
        id
    }

    fn render_param_signatures(&self, id: ModuleId) -> ArenaMap<Idx<Function>, Ustr> {
        self.module(id)
            .functions
            .iter()
            .map(|(fid, _)| (fid, self.render_param_signature(id, fid)))
            .collect()
    }

    fn store_param_signatures(&mut self, id: ModuleId, signatures: ArenaMap<Idx<Function>, Ustr>) {
        let idx = id.index();
        if idx < self.param_signatures.len() {
            self.param_signatures[idx] = signatures;
        } else {
            self.param_signatures.push(signatures);
        }
    }

    /// Re-index one function whose signature was replaced in place.
    pub fn index_param_signature(&mut self, mid: ModuleId, fid: Idx<Function>) {
        let signature = self.render_param_signature(mid, fid);
        self.param_signatures[mid.index()].insert(fid, signature);
    }

    fn render_param_signature(&self, mid: ModuleId, fid: Idx<Function>) -> Ustr {
        let f = &self.module(mid).functions[fid];
        Ustr::from(
            &f.param_decls
                .iter()
                .map(|p| self.render(None, p.ty))
                .join("\u{1}"),
        )
    }

    /// Group backend variants by callable name and declared parameter types.
    pub fn backend_variant_key(&self, mid: ModuleId, fid: Idx<Function>) -> (Ustr, Ustr) {
        (
            self.module(mid).functions[fid].name,
            self.param_signatures[mid.index()][fid],
        )
    }

    pub fn log_time(&mut self, label: &'static str, timer: Instant) {
        self.phase_timings.push((label, timer.elapsed()));
    }

    pub fn builtin_module(&self) -> &Module {
        self.module(ModuleId::builtin())
    }

    /// Whether eviction must keep `mid`: only the builtin module, whose types builtin IDs point
    /// into.
    pub fn is_permanent_module(mid: ModuleId) -> bool {
        mid.is_builtin()
    }

    /// Every module compiled from source, in id order: all but the builtin one.
    pub fn source_modules(&self) -> impl Iterator<Item = &Module> {
        self.modules.iter().filter(|m| !m.id.is_builtin())
    }

    /// The source modules outside a session's `warm` stdlib set; an empty set selects every source
    /// module.
    pub fn user_modules<'a>(
        &'a self,
        warm: &'a FxHashSet<ModuleId>,
    ) -> impl Iterator<Item = &'a Module> {
        self.source_modules().filter(move |m| !warm.contains(&m.id))
    }

    /// Whether `mid` is a user module given the session's `warm` set.
    pub fn is_user_module(mid: ModuleId, warm: &FxHashSet<ModuleId>) -> bool {
        !mid.is_builtin() && !warm.contains(&mid)
    }

    /// Whether `name` is a nominal identity type: a `struct`, not a `value`.
    pub fn is_identity_struct(&self, name: Ustr) -> bool {
        self.struct_by_name
            .get(&name)
            .is_some_and(|sid| !self.module(sid.module).structs[sid.def].is_value)
    }

    pub(crate) fn type_expr(&self, body: Option<&BodyInference>, ty: TyId) -> Option<TypeExpr> {
        Some(match ty {
            TyId::Static(mid, idx) => self.module(mid).ty_arena[idx],
            TyId::Local(idx) => body?.ty_arena[idx],
        })
    }

    /// The type behind `ty`, following solved inference variables, with the id
    /// whose arena its child indices point into (`id.with_idx(child)`). `None`
    /// for a `Local` type without its body.
    pub fn resolve_ty(
        &self,
        body: Option<&BodyInference>,
        mut ty: TyId,
    ) -> Option<(TyId, TypeExpr)> {
        loop {
            let expr = self.type_expr(body, ty)?;
            if let TypeExpr::Infer(var, _) = expr
                && let Some(next) = body.and_then(|b| b.substitutions.get(var))
            {
                ty = next;
            } else {
                return Some((ty, expr));
            }
        }
    }

    /// Whether `hit` holds for some node of `ty`'s tree, visiting a solved
    /// inference variable and then its solution. A `Local` type without its
    /// body has no nodes.
    pub fn type_any(
        &self,
        body: Option<&BodyInference>,
        ty: TyId,
        hit: &mut impl FnMut(TypeExpr) -> bool,
    ) -> bool {
        let Some(expr) = self.type_expr(body, ty) else {
            return false;
        };
        if hit(expr) {
            return true;
        }
        let any = |children: IdxRange<TypeExpr>, hit: &mut _| {
            children
                .into_iter()
                .any(|child| self.type_any(body, ty.with_idx(child), hit))
        };
        match expr {
            TypeExpr::Infer(var, _) => body
                .and_then(|b| b.substitutions.get(var))
                .is_some_and(|next| self.type_any(body, next, hit)),
            TypeExpr::Builtin(_) | TypeExpr::Generic(..) => false,
            TypeExpr::Nullable(inner) => self.type_any(body, ty.with_idx(inner), hit),
            TypeExpr::App { params, .. } | TypeExpr::Tuple { elements: params } => any(params, hit),
            TypeExpr::Function { params, ret } => {
                any(params, hit) || self.type_any(body, ty.with_idx(ret), hit)
            }
        }
    }

    /// Whether `ty` holds a frame reference anywhere in its tree.
    pub fn type_contains_ref(&self, body: Option<&BodyInference>, ty: TyId) -> bool {
        self.type_any(body, ty, &mut |node| {
            matches!(
                node,
                TypeExpr::App {
                    ctor: TypeCtor::Ref,
                    ..
                }
            )
        })
    }

    pub fn render(&self, body: Option<&BodyInference>, ty: TyId) -> String {
        let (ty, expr) = self
            .resolve_ty(body, ty)
            .expect("Local type needs its BodyInference");
        let render_list = |types: IdxRange<TypeExpr>| {
            types
                .map(|idx| self.render(body, ty.with_idx(idx)))
                .collect_vec()
                .join(", ")
        };

        match expr {
            TypeExpr::Builtin(b) => match b {
                BuiltinTy::Int => "int".into(),
                BuiltinTy::Uint => "uint".into(),
                BuiltinTy::Float => "float".into(),
                BuiltinTy::F32 => "f32".into(),
                BuiltinTy::Bool => "bool".into(),
                BuiltinTy::String => "string".into(),
                BuiltinTy::Unit => "()".into(),
                BuiltinTy::Any => "any".into(),
                BuiltinTy::Never => "never".into(),
                BuiltinTy::Error => "<error>".into(),
            },

            TypeExpr::Generic(_, name) | TypeExpr::Infer(_, name) => name.to_string(),

            TypeExpr::Nullable(inner) => format!("{}?", self.render(body, ty.with_idx(inner))),

            TypeExpr::App { ctor, params } => {
                let params_str = render_list(params);

                match ctor {
                    hir::TypeCtor::Array => format!("[{}]", params_str),
                    hir::TypeCtor::Map => {
                        format!("Map<{}>", params_str)
                    }
                    hir::TypeCtor::Ref => {
                        format!("ref {}", params_str)
                    }

                    hir::TypeCtor::Nominal(name) => {
                        let name = crate::modules::display_name(name.as_str());
                        if params.is_empty() {
                            name.to_string()
                        } else {
                            format!("{}<{}>", name, params_str)
                        }
                    }
                }
            }

            TypeExpr::Function { params, ret } => format!(
                "fn({}) -> {}",
                render_list(params),
                self.render(body, ty.with_idx(ret))
            ),
            TypeExpr::Tuple { elements } => format!("({})", render_list(elements)),
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

/// How a bounded generic's `next` resolves at one instantiation site, for skull's inliner to
/// substitute the placeholder bound-calls in the spliced callee.
#[derive(Clone, Debug, PartialEq)]
pub enum BoundNext {
    /// The resolved `next` overload plus the tree for its own bounded generics, since iterator
    /// chains nest.
    Concrete {
        func: (ModuleId, Idx<Function>),
        sub: BoundTree,
        /// Synthetic calls have no expression key in `call_generic_bindings`.
        next_generics: FxHashMap<VarId, TyId>,
    },
    /// The enclosing function's own bounded generic, resolved through the caller's placeholder when
    /// the caller is inlined at a concrete root.
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

/// The iterator protocol witness and its caller-arena type substitution.
#[derive(Clone, Debug, PartialEq)]
pub struct IterWitness {
    pub func: (ModuleId, Idx<Function>),
    pub generics: FxHashMap<VarId, TyId>,
}

/// Iterator witnesses. `None` for `iter` means the input is already an iterator.
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
    /// The matched type of a record pattern whose written head does not identify an enum.
    pub record_pattern_types: FxHashMap<Idx<Pattern>, TyId>,
    pub binding_types: ArenaMap<Idx<BindingInfo>, TyId>,
    /// The actual arguments that filled `ref T` parameters at a resolved call.
    /// MIR uses this to preserve the argument place instead of copying it.
    pub ref_call_args: FxHashMap<Idx<Expr>, Vec<Idx<Expr>>>,
    pub resolved_overload_calls: FxHashMap<Idx<Expr>, (ModuleId, Idx<Function>)>,
    /// Instantiated generics in declaration order. Resolve them through
    /// `substitutions` before use.
    pub call_generic_bindings: FxHashMap<Idx<Expr>, Vec<TyId>>,
    /// Iterator protocol witnesses, keyed by the loop's iter expression.
    pub for_iter_protocol: FxHashMap<Idx<Expr>, ForIterInfo>,
    /// `next(x)` calls where `x: I` for a bound `I: Iterator<E>`, mapped to the generic's index in
    /// the enclosing function. Skull lowers them as placeholder callees resolved at inline time.
    pub bound_calls: FxHashMap<Idx<Expr>, usize>,
    /// `iter(x)` where `x: I`: identity, lowered to the argument itself.
    pub bound_identity_calls: FxHashSet<Idx<Expr>>,
    /// Method-syntax calls through the receiver's fn-typed field `f` rather than a function.
    pub fn_field_calls: FxHashMap<Idx<Expr>, Ustr>,
    /// The resolved function type of each call above, for skull's function-value call ABI.
    pub fn_field_call_types: FxHashMap<Idx<Expr>, TyId>,
    /// Calls of bounded functions: the conformance tree used when the callee is inlined at the
    /// call.
    pub bound_call_trees: FxHashMap<Idx<Expr>, BoundTree>,
    /// Arguments implicitly coerced from iterable to iterator at an `Iterator`-bounded parameter,
    /// with the resolved `iter` witness skull wraps them in.
    pub implicit_iter_args: FxHashMap<Idx<Expr>, IterWitness>,
    /// Inferred variables for the declared generics in a struct default body.
    pub struct_default_generic_slots: Vec<TyId>,

    pub ty_arena: Arena<TypeExpr>,
    pub ty_spans: ArenaMap<Idx<TypeExpr>, Span>,

    pub substitutions: Substitutions,

    pub trail: Vec<(VarId, Option<TyId>)>,

    /// Every coercion accepted into an `any` target, with its source type; read by the
    /// identity-storage body check.
    pub any_erasures: Vec<(Span, TyId)>,

    pub deps: FxHashSet<Ustr>,
}

/// What [`BodyInference::rollback_to`] restores.
#[derive(Copy, Clone, Debug)]
pub struct BodyCheckpoint {
    trail: usize,
    any_erasures: usize,
    types: usize,
}

impl BodyInference {
    /// Release spare capacity in tables a stored result keeps until emission.
    pub fn shrink_to_fit(&mut self) {
        self.expr_types.shrink_to_fit();
        self.record_pattern_types.shrink_to_fit();
        self.binding_types.shrink_to_fit();
        self.ref_call_args.shrink_to_fit();
        self.resolved_overload_calls.shrink_to_fit();
        self.call_generic_bindings.shrink_to_fit();
        self.for_iter_protocol.shrink_to_fit();
        self.bound_calls.shrink_to_fit();
        self.bound_identity_calls.shrink_to_fit();
        self.fn_field_calls.shrink_to_fit();
        self.fn_field_call_types.shrink_to_fit();
        self.bound_call_trees.shrink_to_fit();
        self.implicit_iter_args.shrink_to_fit();
        self.ty_arena.shrink_to_fit();
        self.ty_spans.shrink_to_fit();
        comfy_la_arena::exact_fit(&mut self.substitutions.0);
        self.deps.shrink_to_fit();
    }

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

    pub fn checkpoint(&self) -> BodyCheckpoint {
        BodyCheckpoint {
            trail: self.trail.len(),
            any_erasures: self.any_erasures.len(),
            types: self.ty_arena.len(),
        }
    }

    pub fn rollback_to(&mut self, cp: BodyCheckpoint) {
        while self.trail.len() > cp.trail {
            let (v, prev) = self.trail.pop().unwrap();
            self.substitutions.restore(v, prev);
        }
        self.any_erasures.truncate(cp.any_erasures);
    }

    /// [`Self::rollback_to`], also dropping every type allocated since `cp`. Only sound when
    /// nothing outside the substitution trail still names one of those types.
    pub fn discard_since(&mut self, cp: BodyCheckpoint) {
        self.rollback_to(cp);
        self.ty_arena.truncate(cp.types);
        self.ty_spans.truncate(cp.types);
    }

    /// The erasures recorded since `cp`, for a caller whose rollback undoes a
    /// classification's bindings but keeps its erasure.
    pub fn any_erasures_since(&self, cp: BodyCheckpoint) -> Vec<(Span, TyId)> {
        self.any_erasures[cp.any_erasures..].to_vec()
    }
}

#[derive(Clone, Debug, Default)]
pub struct InferenceResult {
    pub per_fn: FxHashMap<BodyOwner, BodyInference>,
    pub errors: FxHashMap<BodyOwner, Vec<CompilerError>>,
}
