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
    pub enums_by_name: FxHashMap<Ustr, EnumId>,
    pub newtypes_by_name: FxHashMap<Ustr, NewtypeId>,
    /// Declared generic-parameter count for every nominal type (struct / enum /
    /// newtype) across ALL modules, keyed by name. Built from the raw AST in
    /// `lower_modules` BEFORE type lowering, so the bare-generic arity check in
    /// `lower_type_expr_in` works cross-module (during the initial parallel
    /// lowering the other modules aren't in `ws.modules` yet -- this map is).
    /// Names are unique workspace-wide (duplicate-type-name is rejected).
    pub type_generic_arity: FxHashMap<Ustr, usize>,
    pub interface_names: FxHashSet<Ustr>,
    pub globals_by_name: FxHashMap<Ustr, TyId>,
    /// Names of `const` globals (read-only). A whole-global assignment to one of
    /// these is rejected in inference; mutable `global`s are absent.
    pub const_globals: FxHashSet<Ustr>,

    pub functions_by_name: FxHashMap<Ustr, FnList>,

    pub phase_timings: Vec<(&'static str, Duration)>,

    /// Count of function bodies actually re-inferred (check_function run, NOT
    /// reused) since the last reset. `check_module_incremental` bumps it on the
    /// non-reuse branch; the incremental `Session` resets it per `update_file`,
    /// so a test/profiler can read EXACTLY how many bodies re-inferred -- a
    /// deterministic signal for reuse, replacing flaky phase-timing ratios. Arc
    /// keeps `Workspace: Clone` and lets the parallel dependent re-check (which
    /// holds `&Workspace`) share one counter across threads.
    pub reinferred_body_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
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
            enums_by_name: Default::default(),
            newtypes_by_name: Default::default(),
            type_generic_arity: Default::default(),
            interface_names: Default::default(),
            globals_by_name: Default::default(),
            const_globals: Default::default(),
            functions_by_name: Default::default(),

            phase_timings: Default::default(),

            reinferred_body_count: Default::default(),
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

#[derive(Clone, Debug, Default)]
pub struct BodyInference {
    /// Body.generation at inference time - trusted only while they match.
    pub body_generation: u64,

    pub expr_types: ArenaMap<Idx<Expr>, TyId>,
    pub binding_types: ArenaMap<Idx<BindingInfo>, TyId>,
    pub resolved_overload_calls: FxHashMap<Idx<Expr>, (ModuleId, Idx<Function>)>,

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
