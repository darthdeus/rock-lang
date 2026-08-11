//! Target-neutral planning for compiler-owned shader artifacts.
//!
//! This crate stops at the logical artifact boundary. It lowers concrete shader
//! instances, walks their reachable bodies, classifies the fullscreen entry and
//! builds [`ShaderInterfacePlan`]. GLSL/WGSL policies only say which calls their
//! own emitters render inline; no target ABI, source text or backend crate enters
//! the plan.

pub mod structure;

use bone_semantic::hir::{AnnoValue, BodyOwner, BuiltinTy, Expr, Function, TyId, TypeCtor, TypeExpr};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{FnList, ModuleId, Span};
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use skull_mir::concrete::{ConcretePlans, ConcreteSignature};
use skull_mir::gpu_layout::{SamplerKind, WireKind, texture_slots};
use skull_mir::instance::Instance;
use skull_mir::lower::{ScalarCtx, SpecSubst, lower_instance_source};
use skull_mir::mir::{FuncRef, Inst, Lit, MirFunc};
use skull_mir::monoty::{MonoCtor, MonoTy};
use skull_mir::shader_ir::{
    ComputeAccess, ComputeAddressSpace, ComputeBuiltin, ComputeDispatchContract,
    ComputeInterfacePlan, ComputeResourceShape, ComputeValueKind, ComputeWorkgroupSize,
    FragmentOutput, LogicalComputeResource, LogicalSampler, LogicalTextureGroup,
    LogicalUniformGroup, ShaderIR, ShaderInstance, ShaderInterfacePlan, ShaderStage,
    ShaderStageIr, UniformRole,
};
use skull_parser::CompilerError;
use std::sync::Arc;
use ustr::Ustr;

/// The target-specific part of planning: whether a call is rendered inline by
/// the backend rather than emitted as a helper body. The planner never asks
/// how the call is spelled, so WGSL can plan without importing GLSL.
pub trait ShaderPlannerPolicy {
    fn inline_call(&self, function: &Function) -> bool;
    fn backend_name(&self) -> &'static str;

    fn validate_concrete_signature(
        &self,
        _interner: &skull_mir::monoty::MonoInterner,
        _signature: &ConcreteSignature,
        _span: Span,
    ) -> Result<(), CompilerError> {
        Ok(())
    }

    fn inline_instance(
        &self,
        _ws: &Workspace,
        function: &Function,
        _instance: &Instance,
        _interner: &skull_mir::monoty::MonoInterner,
    ) -> bool {
        self.inline_call(function)
    }

    fn lowered_native_operator(
        &self,
        _ws: &Workspace,
        _function: &Function,
        _instance: &Instance,
        _interner: &skull_mir::monoty::MonoInterner,
    ) -> bool {
        false
    }
}

/// The graph/view facts translated by the driver for one retained shader stage.
/// The neutral planner never discovers a second population from declarations.
#[derive(Clone)]
pub struct ConcreteStageInput {
    pub stage: ShaderStage,
    pub entry: Instance,
    pub retained: Vec<Instance>,
    pub helpers: Vec<Instance>,
    pub builtins: Vec<Instance>,
    pub order: Vec<Instance>,
    pub dependencies: Vec<FuncRef>,
    pub concrete_plans: Arc<ConcretePlans>,
}

/// A neutral fullscreen fragment artifact. The parameter indices are logical
/// entry metadata used by a renderer to drop resource parameters from its
/// generated helper signature; they contain no target type or binding answer.
#[derive(Clone)]
pub struct ShaderArtifactPlan {
    pub entry: ShaderInstance,
    pub ir: ShaderIR,
    pub interface: ShaderInterfacePlan,
    pub dependencies: Vec<FuncRef>,
    pub global_params: Vec<usize>,
    pub sampler_params: Vec<usize>,
}

fn err(span: Span, message: impl Into<String>) -> CompilerError {
    CompilerError::new_with_label(span, message.into())
}

/// Resolve a root name to one function, preferring the caller's module.
pub fn resolve_root(
    ws: &Workspace,
    user_mid: ModuleId,
    name: &str,
) -> Result<FuncRef, CompilerError> {
    let key = Ustr::from(name);
    let Some(list): Option<&FnList> = ws.functions_by_name.get(&key) else {
        return Err(CompilerError::new_no_location(format!(
            "shader root `{name}` not found"
        )));
    };
    let in_user: Vec<_> = list
        .iter()
        .filter(|(module, _)| *module == user_mid)
        .collect();
    let pick = if in_user.len() == 1 {
        *in_user[0]
    } else if list.len() == 1 {
        list[0]
    } else {
        return Err(CompilerError::new_no_location(format!(
            "shader root `{name}` is overloaded/ambiguous ({} definitions); shader roots must be uniquely named",
            list.len()
        )));
    };
    Ok(FuncRef {
        module: pick.0,
        func: pick.1,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShaderTy {
    Float,
    Int,
    Bool,
    Vec2,
    Vec3,
    Vec4,
    Bvec3,
    Mat4,
    Sampler(SamplerKind),
}

impl ShaderTy {
    fn is_sampler(self) -> bool {
        matches!(self, Self::Sampler(_))
    }

    fn name(self) -> &'static str {
        match self {
            Self::Float => "float",
            Self::Int => "int",
            Self::Bool => "bool",
            Self::Vec2 => "Vec2",
            Self::Vec3 => "Vec3",
            Self::Vec4 => "Vec4",
            Self::Bvec3 => "Bvec3",
            Self::Mat4 => "Mat4",
            Self::Sampler(SamplerKind::Float) => "Sampler2D",
            Self::Sampler(SamplerKind::Int) => "ISampler2D",
        }
    }
}

fn ty_expr(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(module, index) => Some(ws.module(module).ty_arena[index]),
        TyId::Local(_) => None,
    }
}

fn nominal_ty(name: &str) -> Option<ShaderTy> {
    match name {
        "Vec2" => Some(ShaderTy::Vec2),
        "Vec3" => Some(ShaderTy::Vec3),
        "Vec4" => Some(ShaderTy::Vec4),
        "Bvec3" => Some(ShaderTy::Bvec3),
        "Mat4" => Some(ShaderTy::Mat4),
        "Sampler2D" => Some(ShaderTy::Sampler(SamplerKind::Float)),
        "ISampler2D" => Some(ShaderTy::Sampler(SamplerKind::Int)),
        _ => None,
    }
}

fn mono_ty(ty: &MonoTy) -> Option<ShaderTy> {
    match ty {
        MonoTy::Float => Some(ShaderTy::Float),
        MonoTy::Int => Some(ShaderTy::Int),
        MonoTy::Bool => Some(ShaderTy::Bool),
        MonoTy::App {
            ctor: MonoCtor::Nominal(name),
            ..
        } => nominal_ty(name.as_str()),
        _ => None,
    }
}

fn shader_ty(
    ws: &Workspace,
    subst: &SpecSubst,
    ty: TyId,
    span: Span,
    what: &str,
) -> Result<ShaderTy, CompilerError> {
    let reject = || {
        err(
            span,
            format!(
                "shader artifact: {what} type is outside the admitted scalar/vector shader vocabulary"
            ),
        )
    };
    match ty_expr(ws, ty) {
        Some(TypeExpr::Builtin(BuiltinTy::Float)) => Ok(ShaderTy::Float),
        Some(TypeExpr::Builtin(BuiltinTy::Int)) => Ok(ShaderTy::Int),
        Some(TypeExpr::Builtin(BuiltinTy::Bool)) => Ok(ShaderTy::Bool),
        Some(TypeExpr::Generic(var, name)) => subst.get(&var).and_then(mono_ty).ok_or_else(|| {
            err(
                span,
                format!(
                    "shader artifact: {what} type `{name}` has no admitted concrete shader shape"
                ),
            )
        }),
        Some(TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        }) => nominal_ty(name.as_str()).ok_or_else(reject),
        _ => Err(reject()),
    }
}

fn check_tier0_signature_spec(
    ws: &Workspace,
    subst: &SpecSubst,
    function: &Function,
    policy: &dyn ShaderPlannerPolicy,
) -> Result<(), CompilerError> {
    for param in &function.param_decls {
        shader_ty(
            ws,
            subst,
            param.ty,
            function.name_span,
            &format!("parameter `{}`", param.name),
        )?;
    }
    if let Some(return_ty) = function.return_ty {
        if shader_ty(ws, subst, return_ty, function.name_span, "return")?.is_sampler() {
            return Err(err(
                function.name_span,
                format!(
                    "shader artifact: a sampler cannot be returned on {}",
                    policy.backend_name()
                ),
            ));
        }
    }
    Ok(())
}

const CONST_CHAIN_DEPTH: usize = 16;

fn const_lit(ws: &Workspace, name: Ustr) -> Option<Lit> {
    const_lit_depth(ws, name, 0)
}

fn const_lit_depth(ws: &Workspace, name: Ustr, depth: usize) -> Option<Lit> {
    if depth > CONST_CHAIN_DEPTH || !ws.const_globals.contains(&name) {
        return None;
    }
    let (module, global_id) = ws.modules.iter().find_map(|module| {
        module
            .globals
            .iter()
            .find(|(_, global)| global.name == name && global.is_const)
            .map(|(global_id, _)| (module, global_id))
    })?;
    let body = module.bodies.get(&BodyOwner::Global(global_id))?;
    fold_const_expr(ws, body, body.root?, depth)
}

fn fold_const_expr(
    ws: &Workspace,
    body: &bone_semantic::hir::Body,
    expr: Idx<Expr>,
    depth: usize,
) -> Option<Lit> {
    match body.expr_arena[expr] {
        Expr::Int(value) => Some(Lit::Int(value)),
        Expr::Float(value) => Some(Lit::Float(value.into_inner())),
        Expr::Bool(value) => Some(Lit::Bool(value)),
        Expr::UnaryMinus(inner) => match fold_const_expr(ws, body, inner, depth)? {
            Lit::Int(value) => Some(Lit::Int(-value)),
            Lit::Float(value) => Some(Lit::Float(-value)),
            _ => None,
        },
        Expr::Global(name) => const_lit_depth(ws, name, depth + 1),
        _ => None,
    }
}

fn inst_kind(inst: &Inst) -> &'static str {
    match inst {
        Inst::Global(_) => "global read",
        Inst::StringConcat(..)
        | Inst::ToString(_)
        | Inst::Format { .. }
        | Inst::FormatDyn { .. } => "string",
        Inst::BitAnd(..)
        | Inst::BitOr(..)
        | Inst::BitXor(..)
        | Inst::Shl(..)
        | Inst::Shr(..)
        | Inst::BitNot(_) => "bitwise op",
        Inst::CallMulti { .. }
        | Inst::CallValue { .. }
        | Inst::CallValueMulti { .. }
        | Inst::CallBoundMulti { .. } => "indirect/multi-value call",
        Inst::Lambda { .. } => "closure",
        Inst::MakeTuple(_) => "tuple",
        Inst::MakeVec(_) => "Vec",
        Inst::MakeStruct { .. } => "heap struct",
        Inst::MakeEnum { .. } | Inst::MakeEnumDyn { .. } => "enum",
        Inst::Index { .. }
        | Inst::Len { .. }
        | Inst::ArrayPush { .. }
        | Inst::StoreIndex { .. } => "container op",
        Inst::StoreField { .. } => "field mutation",
        Inst::StoreGlobal { .. } => "global write",
        Inst::CoroSetInner(_) | Inst::CoroHook { .. } | Inst::MapNext { .. } => {
            "coroutine/iteration"
        }
        _ => "unsupported instruction",
    }
}

fn profile_body(
    ws: &Workspace,
    function: &Function,
    body: &MirFunc,
    callees: &mut Vec<ShaderInstance>,
    policy: &dyn ShaderPlannerPolicy,
    concrete: bool,
    interner: &skull_mir::monoty::MonoInterner,
) -> Result<(), CompilerError> {
    for (instruction_id, instruction) in body.insts.iter() {
        match instruction {
            Inst::Call { func, .. } => {
                callees.push(callee_instance(
                    ws,
                    body,
                    instruction_id,
                    *func,
                    function.name_span,
                    policy,
                    concrete,
                    interner,
                )?);
            }
            Inst::Global(name) => {
                if const_lit(ws, *name).is_none() {
                    if ws.const_globals.contains(name) {
                        return Err(err(
                            function.name_span,
                            format!(
                                "shader profile: const `{name}` has an initializer that is not a constant expression"
                            ),
                        ));
                    }
                    return Err(err(
                        function.name_span,
                        format!(
                            "shader profile: `{}` uses a non-tier-0 construct (global read) that has no {} lowering",
                            function.name,
                            policy.backend_name()
                        ),
                    ));
                }
            }
            Inst::Const(Lit::Str(_)) => {
                return Err(err(
                    function.name_span,
                    format!(
                        "shader profile: `{}` uses a non-tier-0 construct (string literal) that has no {} lowering",
                        function.name,
                        policy.backend_name()
                    ),
                ));
            }
            Inst::Const(Lit::Nil) => {}
            Inst::Const(_)
            | Inst::Load(_)
            | Inst::IntAdd(..)
            | Inst::IntSub(..)
            | Inst::IntMul(..)
            | Inst::IntDiv(..)
            | Inst::IntMod(..)
            | Inst::FloatAdd(..)
            | Inst::FloatSub(..)
            | Inst::FloatMul(..)
            | Inst::FloatDiv(..)
            | Inst::FloatMod(..)
            | Inst::IntLt(..)
            | Inst::IntLeq(..)
            | Inst::IntGt(..)
            | Inst::IntGeq(..)
            | Inst::FloatLt(..)
            | Inst::FloatLeq(..)
            | Inst::FloatGt(..)
            | Inst::FloatGeq(..)
            | Inst::Eq(..)
            | Inst::NotEq(..)
            | Inst::Neg(_)
            | Inst::Not(_)
            | Inst::FloatToInt(_)
            | Inst::IntToFloat(_)
            | Inst::MakeValueStruct { .. }
            | Inst::MaterializeValueStruct { .. }
            | Inst::Field { .. }
            | Inst::Store { .. } => {}
            other => {
                return Err(err(
                    function.name_span,
                    format!(
                        "shader profile: `{}` uses a non-tier-0 construct ({}) that has no {} lowering",
                        function.name,
                        inst_kind(other),
                        policy.backend_name()
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn callee_instance(
    ws: &Workspace,
    body: &MirFunc,
    instruction_id: Idx<Inst>,
    function: FuncRef,
    span: Span,
    policy: &dyn ShaderPlannerPolicy,
    concrete: bool,
    interner: &skull_mir::monoty::MonoInterner,
) -> Result<ShaderInstance, CompilerError> {
    let callee = &ws.module(function.module).functions[function.func];
    let key = if concrete {
        body.callee_instances.get(&instruction_id).cloned()
    } else {
        body.spec_bindings.get(&instruction_id).cloned()
    };
    let instance = match key {
        Some(instance) => instance,
        None => {
            let base = Instance::base(function);
            if callee.generics.is_empty() || policy.inline_instance(ws, callee, &base, interner) {
                base
            } else {
                return Err(err(
                    span,
                    format!(
                        "shader profile: the call to generic `{}` has no recorded instantiation, so {} has no concrete shape to emit",
                        callee.name,
                        policy.backend_name()
                    ),
                ));
            }
        }
    };
    if policy.inline_instance(ws, callee, &instance, interner)
        || callee.generics.is_empty()
    {
        return Ok(ShaderInstance::new(instance));
    }
    if instance.args.is_empty() {
        return Err(err(
            span,
            format!(
                "shader profile: the call to generic `{}` has no recorded instantiation, so {} has no concrete shape to emit",
                callee.name,
                policy.backend_name()
            ),
        ));
    }
    if instance.args.len() != callee.generics.len() {
        return Err(err(
            span,
            format!(
                "shader profile: the call to generic `{}` recorded {} type argument(s) for {} declared generic(s)",
                callee.name,
                instance.args.len(),
                callee.generics.len()
            ),
        ));
    }
    Ok(ShaderInstance::new(instance))
}

struct Reach<'a> {
    ws: &'a Workspace,
    scx: &'a ScalarCtx,
    policy: &'a dyn ShaderPlannerPolicy,
    bodies: HashMap<ShaderInstance, MirFunc>,
    seeds: Vec<ShaderInstance>,
    entry: Option<ShaderInstance>,
}

impl<'a> Reach<'a> {
    fn walk(&mut self) -> Result<Vec<ShaderInstance>, CompilerError> {
        skull_mir::reach::walk(self)
    }
}

impl skull_mir::reach::ReachPolicy for Reach<'_> {
    type Node = ShaderInstance;

    fn roots(&mut self) -> Result<Vec<Self::Node>, CompilerError> {
        Ok(std::mem::take(&mut self.seeds))
    }

    fn initially_in_progress(&mut self) -> Vec<Self::Node> {
        self.entry.clone().into_iter().collect()
    }

    fn is_leaf(&mut self, instance: ShaderInstance) -> Result<bool, CompilerError> {
        let function = &self.ws.module(instance.fref().module).functions[instance.fref().func];
        if self.policy.inline_call(function) {
            return Ok(true);
        }
        if function.is_extern || function.inline_body.is_some() {
            return Err(err(
                function.name_span,
                format!(
                    "shader profile: `{}` has no emittable body and no {} intrinsic mapping",
                    function.name,
                    self.policy.backend_name()
                ),
            ));
        }
        Ok(false)
    }

    fn on_cycle(&mut self, instance: ShaderInstance) -> Result<(), CompilerError> {
        let function = &self.ws.module(instance.fref().module).functions[instance.fref().func];
        Err(err(
            function.name_span,
            format!(
                "shader profile: recursion through `{}` is not allowed on {}",
                function.name,
                self.policy.backend_name()
            ),
        ))
    }

    fn visit(&mut self, instance: ShaderInstance) -> Result<Vec<ShaderInstance>, CompilerError> {
        let function = &self.ws.module(instance.fref().module).functions[instance.fref().func];
        let subst = instance.subst(self.ws, self.scx.interner());
        check_tier0_signature_spec(self.ws, &subst, function, self.policy)?;
        let body = lower_instance_source(self.ws, self.scx, &instance.inst)
        .map_err(|error| {
            err(
                function.name_span,
                format!(
                    "shader profile: `{}` cannot lower for {}: {}",
                    function.name,
                    self.policy.backend_name(),
                    error.message
                ),
            )
        })?;
        let mut callees = Vec::new();
        profile_body(
            self.ws,
            function,
            &body,
            &mut callees,
            self.policy,
            false,
            self.scx.interner(),
        )?;
        self.bodies.insert(instance, body);
        Ok(callees)
    }
}

/// Plan one concrete stage's target-neutral MIR and reachable helpers.
fn plan_stage(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
    stage: ShaderStage,
    policy: &dyn ShaderPlannerPolicy,
) -> Result<ShaderStageIr, CompilerError> {
    let function = &ws.module(fref.module).functions[fref.func];
    let entry = ShaderInstance::plain(fref);
    let body = lower_instance_source(ws, scx, &entry.inst).map_err(
        |error| {
            err(
                function.name_span,
                format!(
                    "shader profile: `{}` cannot lower for {}: {}",
                    function.name,
                    policy.backend_name(),
                    error.message
                ),
            )
        },
    )?;
    let mut callees = Vec::new();
    profile_body(
        ws,
        function,
        &body,
        &mut callees,
        policy,
        false,
        scx.interner(),
    )?;
    let mut reach = Reach {
        ws,
        scx,
        policy,
        bodies: HashMap::default(),
        seeds: callees,
        entry: Some(entry.clone()),
    };
    let order = reach.walk()?;
    reject_stage_violations(
        ws,
        stage,
        reach.bodies.values().chain(std::iter::once(&body)),
        function.name_span,
    )?;
    Ok(ShaderStageIr {
        stage,
        entry,
        body,
        order,
        bodies: reach.bodies,
    })
}

fn checked_concrete_callees(
    ws: &Workspace,
    caller: &ShaderInstance,
    callees: Vec<ShaderInstance>,
    allowed: &HashSet<ShaderInstance>,
    policy: &dyn ShaderPlannerPolicy,
    interner: &skull_mir::monoty::MonoInterner,
    builtins: &mut HashSet<FuncRef>,
) -> Result<Vec<ShaderInstance>, CompilerError> {
    let caller_f = &ws.module(caller.fref().module).functions[caller.fref().func];
    let mut retained = Vec::new();
    for callee in callees {
        let f = &ws.module(callee.fref().module).functions[callee.fref().func];
        if policy.inline_instance(ws, f, &callee.inst, interner) {
            builtins.insert(callee.fref());
            continue;
        }
        if policy.lowered_native_operator(ws, f, &callee.inst, interner) {
            continue;
        }
        if f.is_extern || f.inline_body.is_some() {
            return Err(err(
                f.name_span,
                format!(
                    "shader profile: `{}` has no emittable body and no {} intrinsic mapping",
                    f.name,
                    policy.backend_name()
                ),
            ));
        }
        if !allowed.contains(&callee) {
            return Err(err(
                caller_f.name_span,
                format!(
                    "shader graph/view omitted the concrete callee `{}` reached by `{}`",
                    f.name, caller_f.name
                ),
            ));
        }
        retained.push(callee);
    }
    Ok(retained)
}

fn reject_concrete_cycles(
    ws: &Workspace,
    calls: &HashMap<ShaderInstance, Vec<ShaderInstance>>,
    policy: &dyn ShaderPlannerPolicy,
) -> Result<(), CompilerError> {
    fn visit(
        ws: &Workspace,
        calls: &HashMap<ShaderInstance, Vec<ShaderInstance>>,
        policy: &dyn ShaderPlannerPolicy,
        node: &ShaderInstance,
        active: &mut HashSet<ShaderInstance>,
        done: &mut HashSet<ShaderInstance>,
    ) -> Result<(), CompilerError> {
        if !active.insert(node.clone()) {
            let f = &ws.module(node.fref().module).functions[node.fref().func];
            return Err(err(
                f.name_span,
                format!(
                    "shader profile: recursion through `{}` is not allowed on {}",
                    f.name,
                    policy.backend_name()
                ),
            ));
        }
        if !done.insert(node.clone()) {
            active.remove(node);
            return Ok(());
        }
        for callee in calls.get(node).into_iter().flatten() {
            visit(ws, calls, policy, callee, active, done)?;
        }
        active.remove(node);
        Ok(())
    }

    let mut active = HashSet::default();
    let mut done = HashSet::default();
    for node in calls.keys() {
        visit(ws, calls, policy, node, &mut active, &mut done)?;
    }
    Ok(())
}

/// Lower and profile one graph-retained shader stage using its mandatory
/// concrete plans. The driver supplies the population; this crate supplies the
/// one target-neutral body/interface planner.
pub fn plan_stage_from_concrete(
    ws: &Workspace,
    scx: &ScalarCtx,
    input: &ConcreteStageInput,
    policy: &dyn ShaderPlannerPolicy,
) -> Result<ShaderStageIr, CompilerError> {
    let order_set: HashSet<Instance> = input.order.iter().cloned().collect();
    let helper_set: HashSet<Instance> = input.helpers.iter().cloned().collect();
    if order_set.len() != input.order.len()
        || helper_set.len() != input.helpers.len()
        || order_set != helper_set
        || input.order.iter().any(|instance| instance == &input.entry)
    {
        return Err(CompilerError::new_no_location(
            "shader graph/view helper order is not a unique retained helper population"
                .to_string(),
        ));
    }
    let mut expected_retained: HashSet<Instance> = input.order.iter().cloned().collect();
    expected_retained.insert(input.entry.clone());
    let retained_set: HashSet<Instance> = input.retained.iter().cloned().collect();
    if retained_set.len() != input.retained.len() || retained_set != expected_retained {
        return Err(CompilerError::new_no_location(
            "shader graph/view retained population is not entry plus helpers".to_string(),
        ));
    }

    let entry = ShaderInstance::new(input.entry.clone());
    let entry_function = &ws.module(entry.fref().module).functions[entry.fref().func];
    let entry_plan = input.concrete_plans.get(&input.entry).ok_or_else(|| {
        err(
            entry_function.name_span,
            format!("shader profile: no concrete plan for `{}`", entry_function.name),
        )
    })?;
    let entry_body = skull_mir::lower::lower_instance_with_plans(
        ws,
        scx,
        entry_plan,
        &input.concrete_plans,
    )
    .map_err(|e| {
        err(
            entry_function.name_span,
            format!(
                "shader profile: `{}` cannot lower for {}: {}",
                entry_function.name,
                policy.backend_name(),
                e.message
            ),
        )
    })?;
    let mut entry_callees = Vec::new();
    profile_body(
        ws,
        entry_function,
        &entry_body,
        &mut entry_callees,
        policy,
        true,
        scx.interner(),
    )?;

    let mut allowed: HashSet<ShaderInstance> = HashSet::default();
    allowed.insert(entry.clone());
    allowed.extend(input.order.iter().cloned().map(ShaderInstance::new));
    let mut discovered_builtins = HashSet::default();
    let entry_calls = checked_concrete_callees(
        ws,
        &entry,
        entry_callees,
        &allowed,
        policy,
        scx.interner(),
        &mut discovered_builtins,
    )?;
    let mut calls = HashMap::default();
    calls.insert(entry.clone(), entry_calls);
    let mut bodies = HashMap::default();
    for raw in &input.order {
        let helper = ShaderInstance::new(raw.clone());
        let function = &ws.module(helper.fref().module).functions[helper.fref().func];
        let plan = input.concrete_plans.get(raw).ok_or_else(|| {
            err(
                function.name_span,
                format!("shader profile: no concrete plan for `{}`", function.name),
            )
        })?;
        policy.validate_concrete_signature(scx.interner(), &plan.signature, function.name_span)?;
        let body = skull_mir::lower::lower_instance_with_plans(
            ws,
            scx,
            plan,
            &input.concrete_plans,
        )
        .map_err(|e| {
            err(
                function.name_span,
                format!(
                    "shader profile: `{}` cannot lower for {}: {}",
                    function.name,
                    policy.backend_name(),
                    e.message
                ),
            )
        })?;
        let mut callees = Vec::new();
        profile_body(
            ws,
            function,
            &body,
            &mut callees,
            policy,
            true,
            scx.interner(),
        )?;
        let checked = checked_concrete_callees(
            ws,
            &helper,
            callees,
            &allowed,
            policy,
            scx.interner(),
            &mut discovered_builtins,
        )?;
        calls.insert(helper.clone(), checked);
        bodies.insert(helper, body);
    }
    let expected_builtins: HashSet<FuncRef> = input.builtins.iter().map(|instance| instance.func).collect();
    if discovered_builtins != expected_builtins {
        return Err(CompilerError::new_no_location(
            "shader graph/view builtin declaration population differs from lowered calls"
                .to_string(),
        ));
    }
    reject_concrete_cycles(ws, &calls, policy)?;
    reject_stage_violations(
        ws,
        input.stage,
        bodies.values().chain(std::iter::once(&entry_body)),
        entry_function.name_span,
    )?;

    let order = input.order.iter().cloned().map(ShaderInstance::new).collect();
    let ir = ShaderStageIr {
        stage: input.stage,
        entry,
        body: entry_body,
        order,
        bodies,
    };
    if ir.declarations() != input.dependencies {
        return Err(CompilerError::new_no_location(
            "shader graph/view hot-reload dependencies differ from lowered order".to_string(),
        ));
    }
    Ok(ir)
}

fn reject_stage_violations<'a>(
    ws: &Workspace,
    stage: ShaderStage,
    bodies: impl Iterator<Item = &'a MirFunc>,
    span: Span,
) -> Result<(), CompilerError> {
    for body in bodies {
        for (_, instruction) in body.insts.iter() {
            let Inst::Call { func, .. } = instruction else {
                continue;
            };
            let name = ws.module(func.module).functions[func.func].name;
            match (stage, name.as_str()) {
                (ShaderStage::Vertex, "fwidth" | "frag_coord" | "discard") => {
                    return Err(err(
                        span,
                        format!(
                            "shader vertex: `{name}()` is a fragment-stage builtin, not available in the vertex stage"
                        ),
                    ));
                }
                (ShaderStage::Fragment, "vertex_id") => {
                    return Err(err(
                        span,
                        format!(
                            "shader fragment: `{name}()` is a vertex-stage builtin, not available in the fragment stage"
                        ),
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

pub struct EntryClass {
    pub global_params: Vec<usize>,
    pub sampler_params: Vec<usize>,
    pub samplers: Vec<(usize, Ustr, SamplerKind)>,
    pub texture_group: Option<Ustr>,
    pub uniform_group: Option<Ustr>,
}

fn has_anno(function: &Function, index: usize, name: &str) -> bool {
    function.param_decls[index]
        .annotations
        .iter()
        .any(|annotation| annotation.name.name.as_str() == name)
}

fn nominal_name(ws: &Workspace, ty: TyId) -> Option<Ustr> {
    match ty_expr(ws, ty)? {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } => Some(name),
        _ => None,
    }
}

fn shader_stage(function: &Function) -> Option<&'static str> {
    let annotation = function
        .annotations
        .iter()
        .find(|annotation| annotation.name.name.as_str() == "shader")?;
    match annotation.args.first().map(|argument| &argument.value) {
        Some(bone_semantic::hir::AnnoValue::Ident(identifier))
            if identifier.name.as_str() == "fragment" =>
        {
            Some("fragment")
        }
        Some(bone_semantic::hir::AnnoValue::Ident(identifier))
            if identifier.name.as_str() == "vertex" =>
        {
            Some("vertex")
        }
        Some(bone_semantic::hir::AnnoValue::Ident(identifier))
            if identifier.name.as_str() == "compute" =>
        {
            Some("compute")
        }
        _ => Some("<unknown>"),
    }
}

fn texture_group_samplers(
    ws: &Workspace,
    ty: TyId,
    param_index: usize,
    span: Span,
) -> Result<Vec<(usize, Ustr, SamplerKind)>, CompilerError> {
    let group = nominal_name(ws, ty).ok_or_else(|| {
        err(
            span,
            "shader entry: an @textures param must be a value struct",
        )
    })?;
    let slots = texture_slots(ws, group).map_err(|error| err(span, error.message))?;
    Ok(slots
        .slots
        .iter()
        .map(|slot| (param_index, slot.name, slot.kind))
        .collect())
}

fn reject_conflicting_texture_role(
    function: &Function,
    index: usize,
    span: Span,
) -> Result<(), CompilerError> {
    for role in ["attr", "instance", "uniform", "engine"] {
        if has_anno(function, index, role) {
            return Err(err(
                span,
                format!(
                    "shader entry: param `{}` has conflicting role markers (@textures and @{role})",
                    function.param_decls[index].name
                ),
            ));
        }
    }
    Ok(())
}

pub fn classify_entry(ws: &Workspace, function: &Function) -> Result<EntryClass, CompilerError> {
    let span = function.name_span;
    let mut uniform_param = None;
    let mut samplers = Vec::new();
    let mut tex_coord = None;
    let mut texture_params = Vec::new();
    let mut texture_group = None;
    let mut uniform_group = None;
    for (index, param) in function.param_decls.iter().enumerate() {
        if has_anno(function, index, "textures") {
            reject_conflicting_texture_role(function, index, span)?;
            if texture_group.is_some() {
                return Err(err(
                    span,
                    "shader entry: at most one @textures group param is allowed",
                ));
            }
            texture_group = nominal_name(ws, param.ty);
            samplers.extend(texture_group_samplers(ws, param.ty, index, span)?);
            texture_params.push(index);
            continue;
        }
        match shader_ty(ws, &SpecSubst::default(), param.ty, span, "parameter") {
            Ok(ShaderTy::Sampler(kind)) => samplers.push((index, param.name, kind)),
            Ok(ShaderTy::Vec2) => {
                if tex_coord.replace(index).is_some() {
                    return Err(err(
                        span,
                        "shader entry: exactly one Vec2 UV parameter is allowed",
                    ));
                }
            }
            Ok(other) => {
                return Err(err(
                    span,
                    format!(
                        "shader entry: param `{}` ({}) is not a fullscreen UV, sampler, or uniform struct",
                        param.name,
                        other.name()
                    ),
                ));
            }
            Err(error) => {
                let Some(group) = nominal_name(ws, param.ty) else {
                    return Err(error);
                };
                if !has_anno(function, index, "uniform") {
                    return Err(err(
                        span,
                        format!(
                            "shader entry: value-struct param `{}` needs @uniform",
                            param.name
                        ),
                    ));
                }
                if uniform_param.is_some() {
                    return Err(err(
                        span,
                        "shader entry: at most one uniform struct param is allowed",
                    ));
                }
                uniform_param = Some(index);
                uniform_group = Some(group);
            }
        }
    }
    if tex_coord.is_none() {
        return Err(err(span, "shader entry: a Vec2 UV parameter is required"));
    }
    if !matches!(
        function
            .return_ty
            .and_then(|ty| shader_ty(ws, &SpecSubst::default(), ty, span, "return").ok()),
        Some(ShaderTy::Vec4)
    ) {
        return Err(err(span, "shader entry: the return type must be Vec4"));
    }
    Ok(EntryClass {
        global_params: uniform_param.into_iter().chain(texture_params).collect(),
        sampler_params: samplers.iter().map(|(index, _, _)| *index).collect(),
        samplers,
        texture_group,
        uniform_group,
    })
}

#[derive(Clone)]
struct ComputeEntryClass {
    resources: Vec<LogicalComputeResource>,
    uniform_group: Option<Ustr>,
    workgroup_size: ComputeWorkgroupSize,
}

fn anno_span(function: &Function, index: usize, name: &str) -> Span {
    function.param_decls[index]
        .annotations
        .iter()
        .find(|annotation| annotation.name.name.as_str() == name)
        .map(|annotation| annotation.span)
        .unwrap_or(function.name_span)
}

fn compute_access(function: &Function, index: usize) -> Result<ComputeAccess, CompilerError> {
    let annotation = function.param_decls[index]
        .annotations
        .iter()
        .find(|annotation| annotation.name.name.as_str() == "storage")
        .ok_or_else(|| err(function.name_span, "compute StorageBuffer parameters require @storage(access)"))?;
    let [argument] = annotation.args.as_slice() else {
        return Err(err(annotation.span, "@storage requires exactly one access argument"));
    };
    let AnnoValue::Ident(access) = &argument.value else {
        return Err(err(annotation.span, "@storage access must be read, write, or read_write"));
    };
    match access.name.as_str() {
        "read" => Ok(ComputeAccess::Read),
        "write" => Ok(ComputeAccess::Write),
        "read_write" => Ok(ComputeAccess::ReadWrite),
        other => Err(err(
            annotation.span,
            format!("unsupported @storage access `{other}`; use read, write, or read_write"),
        )),
    }
}

fn compute_storage_element(
    ws: &Workspace,
    ty: TyId,
    span: Span,
) -> Result<ComputeValueKind, CompilerError> {
    let Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), params }) = ty_expr(ws, ty) else {
        return Err(err(span, "compute storage parameters must be StorageBuffer<int>"));
    };
    if name.as_str() != "StorageBuffer" {
        return Err(err(span, "compute storage parameters must be StorageBuffer<int>"));
    }
    let Some(element) = params.into_iter().next().map(|index| ty.with_idx(index)) else {
        return Err(err(span, "StorageBuffer is missing its element type"));
    };
    match ty_expr(ws, element) {
        Some(TypeExpr::Builtin(BuiltinTy::Int)) => Ok(ComputeValueKind::I32),
        Some(TypeExpr::Builtin(BuiltinTy::Float)) => Ok(ComputeValueKind::F32),
        Some(TypeExpr::Builtin(BuiltinTy::Bool)) => Ok(ComputeValueKind::Bool),
        _ => Err(err(
            span,
            "the first compute storage element type is Rock int (WGSL i32)",
        )),
    }
}

fn compute_workgroup(function: &Function) -> Result<ComputeWorkgroupSize, CompilerError> {
    let Some(annotation) = function
        .annotations
        .iter()
        .find(|annotation| annotation.name.name.as_str() == "workgroup")
    else {
        return Err(err(
            function.name_span,
            "@shader(compute) entries require @workgroup(x)",
        ));
    };
    let [argument] = annotation.args.as_slice() else {
        return Err(err(annotation.span, "@workgroup takes exactly one integer x size"));
    };
    let AnnoValue::Int(x) = argument.value else {
        return Err(err(annotation.span, "@workgroup takes exactly one integer x size"));
    };
    if !(1..=u32::MAX as i64).contains(&x) {
        return Err(err(annotation.span, "compute workgroup size must be non-zero and fit u32"));
    }
    Ok(ComputeWorkgroupSize { x: x as u32, y: 1, z: 1 })
}

fn classify_compute_entry(
    ws: &Workspace,
    function: &Function,
) -> Result<ComputeEntryClass, CompilerError> {
    let span = function.name_span;
    if shader_stage(function) != Some("compute") {
        return Err(err(
            span,
            format!("compute entry `{}` must carry @shader(compute)", function.name),
        ));
    }
    let workgroup_size = compute_workgroup(function)?;
    let mut resources = Vec::new();
    let mut uniform_group = None;
    for (index, param) in function.param_decls.iter().enumerate() {
        let storage = has_anno(function, index, "storage");
        let uniform = has_anno(function, index, "uniform");
        if storage && uniform {
            return Err(err(
                anno_span(function, index, "storage"),
                format!("compute parameter `{}` cannot be both @storage and @uniform", param.name),
            ));
        }
        if storage {
            let kind = compute_storage_element(ws, param.ty, anno_span(function, index, "storage"))?;
            let access = compute_access(function, index)?;
            resources.push(LogicalComputeResource {
                name: param.name,
                shape: ComputeResourceShape::Array(kind),
                address_space: ComputeAddressSpace::Storage,
                access,
            });
            continue;
        }
        if uniform {
            let Some(group) = nominal_name(ws, param.ty) else {
                return Err(err(
                    anno_span(function, index, "uniform"),
                    format!("compute uniform `{}` must be a named value struct", param.name),
                ));
            };
            if uniform_group.replace(group).is_some() {
                return Err(err(span, "compute entries admit exactly one @uniform parameter"));
            }
            continue;
        }
        return Err(err(
            function.name_span,
            format!(
                "compute parameter `{}` must be @storage(access) or @uniform",
                param.name
            ),
        ));
    }
    if resources.is_empty() {
        return Err(err(span, "compute entry must declare at least one @storage parameter"));
    }
    if function.return_ty.is_some() {
        return Err(err(span, "compute entries must return unit"));
    }
    Ok(ComputeEntryClass { resources, uniform_group, workgroup_size })
}

fn compute_builtin_calls(
    ws: &Workspace,
    stage: &ShaderStageIr,
    span: Span,
) -> Result<Vec<ComputeBuiltin>, CompilerError> {
    let mut names = HashSet::default();
    for body in stage.bodies.values().chain(std::iter::once(&stage.body)) {
        for (_, instruction) in body.insts.iter() {
            let Inst::Call { func, .. } = instruction else { continue };
            let name = ws.module(func.module).functions[func.func].name;
            match name.as_str() {
                "global_invocation_id" => {
                    names.insert(ComputeBuiltin::GlobalInvocationId);
                }
                "local_invocation_id" | "workgroup_id" | "local_invocation_index" | "num_workgroups" => {
                    return Err(err(
                        span,
                        format!("compute builtin `{name}()` is not supported in the first slice"),
                    ));
                }
                _ => {}
            }
        }
    }
    Ok([
        ComputeBuiltin::GlobalInvocationId,
        ComputeBuiltin::LocalInvocationId,
        ComputeBuiltin::WorkgroupId,
        ComputeBuiltin::LocalInvocationIndex,
        ComputeBuiltin::NumWorkgroups,
    ]
    .into_iter()
    .filter(|builtin| names.contains(builtin))
    .collect())
}

fn validate_compute_access(
    ws: &Workspace,
    stage: &ShaderStageIr,
    class: &ComputeEntryClass,
    entry_function: &Function,
    span: Span,
) -> Result<(), CompilerError> {
    let entry_params = &entry_function.param_decls;
    let entry_cells = stage.body.params.clone();
    let resource_access = |cell| {
        entry_cells.iter().position(|candidate| *candidate == cell).and_then(|index| {
            entry_params.get(index).and_then(|param| {
                class.resources.iter().find(|resource| resource.name == param.name).map(|resource| resource.access)
            })
        })
    };
    for (instance, body) in stage
        .bodies
        .iter()
        .map(|(instance, body)| (Some(instance), body))
        .chain(std::iter::once((None, &stage.body)))
    {
        for (_, instruction) in body.insts.iter() {
            let Inst::Call { func, args } = instruction else { continue };
            let name = ws.module(func.module).functions[func.func].name;
            if !matches!(name.as_str(), "storage_load" | "storage_store") {
                continue;
            }
            if instance.is_some() {
                return Err(err(
                    span,
                    "storage intrinsics in helpers are not supported until resource forwarding is explicit",
                ));
            }
            let Some(Inst::Load(cell)) = args.first().map(|arg| &stage.body.insts[*arg]) else {
                return Err(err(span, format!("{name} must receive a direct StorageBuffer parameter")));
            };
            let Some(access) = resource_access(*cell) else {
                return Err(err(span, format!("{name} received a resource absent from the compute interface")));
            };
            let allowed = match name.as_str() {
                "storage_load" => access != ComputeAccess::Write,
                "storage_store" => access != ComputeAccess::Read,
                _ => true,
            };
            if !allowed {
                return Err(err(
                    span,
                    format!("{name} is incompatible with the resource's {:?} access", access),
                ));
            }
        }
    }
    Ok(())
}

fn logical_members(
    ws: &Workspace,
    struct_name: Ustr,
    span: Span,
) -> Result<Vec<(Ustr, WireKind)>, CompilerError> {
    skull_mir::gpu_layout::uniform_group_members(ws, struct_name)
        .map_err(|error| err(span, error.message))
}

fn logical_uniform_group(
    ws: &Workspace,
    role: UniformRole,
    struct_name: Ustr,
    span: Span,
) -> Result<LogicalUniformGroup, CompilerError> {
    Ok(LogicalUniformGroup {
        role,
        struct_name,
        members: logical_members(ws, struct_name, span)?,
    })
}

fn logical_textures(
    ws: &Workspace,
    group: Option<Ustr>,
    has_bare_sampler: bool,
    span: Span,
) -> Result<Option<LogicalTextureGroup>, CompilerError> {
    let Some(group) = group else { return Ok(None) };
    if has_bare_sampler {
        return Err(err(
            span,
            format!("shader entry: `{group}` is a @textures group, so every sampler must join it"),
        ));
    }
    let slots = texture_slots(ws, group).map_err(|error| err(span, error.message))?;
    Ok(Some(LogicalTextureGroup {
        struct_name: Some(group),
        samplers: slots
            .slots
            .iter()
            .map(|slot| (slot.name, slot.kind))
            .collect(),
        stages: vec![ShaderStage::Fragment],
    }))
}

fn bare_sampler_list(
    samplers: &[(usize, Ustr, SamplerKind)],
    grouped_params: &[usize],
) -> Vec<LogicalSampler> {
    samplers
        .iter()
        .filter(|(index, _, _)| !grouped_params.contains(index))
        .map(|(_, name, kind)| LogicalSampler {
            name: *name,
            kind: *kind,
            stage: ShaderStage::Fragment,
        })
        .collect()
}

pub fn check_entry_collisions(
    interface: &ShaderInterfacePlan,
    span: Span,
) -> Result<(), CompilerError> {
    let mut seen: HashSet<Ustr> = [Ustr::from("TexCoord"), Ustr::from("FragColor")]
        .into_iter()
        .collect();
    let names = interface
        .uniform_groups
        .iter()
        .flat_map(|group| group.members.iter().map(|(name, _)| *name))
        .chain(
            interface
                .textures
                .iter()
                .flat_map(|group| group.samplers.iter().map(|(name, _)| *name)),
        )
        .chain(interface.bare_samplers.iter().map(|sampler| sampler.name));
    for name in names {
        if !seen.insert(name) {
            return Err(err(
                span,
                format!(
                    "shader entry: uniform name `{name}` would collide (fields, samplers, TexCoord and FragColor share the generated namespace)"
                ),
            ));
        }
    }
    Ok(())
}

/// Build the first target-neutral artifact shape: one compiler-owned fragment
/// entry plus the fullscreen vertex supplied by a physical backend.
pub fn plan_fullscreen_fragment_from_concrete(
    ws: &Workspace,
    scx: &ScalarCtx,
    input: &ConcreteStageInput,
    policy: &dyn ShaderPlannerPolicy,
) -> Result<ShaderArtifactPlan, CompilerError> {
    if input.stage != ShaderStage::Fragment {
        return Err(CompilerError::new_no_location(
            "shader artifact: fullscreen input must be a fragment stage".to_string(),
        ));
    }
    let function = &ws.module(input.entry.func.module).functions[input.entry.func.func];
    match shader_stage(function) {
        Some("fragment") => {}
        Some(stage) => {
            return Err(err(
                function.name_span,
                format!(
                    "shader entry: @shader({stage}) is not supported by the fullscreen artifact; only @shader(fragment) is admitted"
                ),
            ));
        }
        None => {
            return Err(err(
                function.name_span,
                format!(
                    "shader entry: `{}` has no @shader(fragment) annotation",
                    function.name
                ),
            ));
        }
    }
    let class = classify_entry(ws, function)?;
    let stage = plan_stage_from_concrete(ws, scx, input, policy)?;
    let interface = ShaderInterfacePlan {
        stages: vec![ShaderStage::Fragment],
        uniform_groups: class
            .uniform_group
            .map(|group| {
                logical_uniform_group(
                    ws,
                    UniformRole::Stage(ShaderStage::Fragment),
                    group,
                    function.name_span,
                )
            })
            .transpose()?
            .into_iter()
            .collect(),
        textures: logical_textures(
            ws,
            class.texture_group,
            class
                .samplers
                .iter()
                .any(|(index, _, _)| !class.global_params.contains(index)),
            function.name_span,
        )?,
        bare_samplers: bare_sampler_list(&class.samplers, &class.global_params),
        fragment_output: FragmentOutput::Single(WireKind::F32x4),
        ..Default::default()
    };
    check_entry_collisions(&interface, function.name_span)?;
    Ok(ShaderArtifactPlan {
        entry: stage.entry.clone(),
        ir: ShaderIR {
            interner: scx.interner_handle(),
            stages: vec![stage],
        },
        interface,
        dependencies: input.dependencies.clone(),
        global_params: class.global_params,
        sampler_params: class.sampler_params,
    })
}

pub fn plan_compute_from_concrete(
    ws: &Workspace,
    scx: &ScalarCtx,
    input: &ConcreteStageInput,
    policy: &dyn ShaderPlannerPolicy,
) -> Result<ShaderArtifactPlan, CompilerError> {
    if input.stage != ShaderStage::Compute {
        return Err(CompilerError::new_no_location(
            "compute artifact input must be a compute stage".to_string(),
        ));
    }
    let function = &ws.module(input.entry.func.module).functions[input.entry.func.func];
    let class = classify_compute_entry(ws, function)?;
    let stage = plan_stage_from_concrete(ws, scx, input, policy)?;
    validate_compute_access(ws, &stage, &class, function, function.name_span)?;
    let builtins = compute_builtin_calls(ws, &stage, function.name_span)?;
    let compute = ComputeInterfacePlan {
        resources: class.resources,
        builtins,
        workgroup_size: class.workgroup_size,
        dispatch: ComputeDispatchContract::Runtime { dimensions: 1 },
    };
    compute.validate().map_err(|message| err(function.name_span, message))?;
    let uniform_groups = class
        .uniform_group
        .map(|group| {
            logical_uniform_group(
                ws,
                UniformRole::Stage(ShaderStage::Compute),
                group,
                function.name_span,
            )
        })
        .transpose()?
        .into_iter()
        .collect();
    let interface = ShaderInterfacePlan {
        stages: vec![ShaderStage::Compute],
        uniform_groups,
        compute: Some(compute),
        fragment_output: FragmentOutput::None,
        ..Default::default()
    };
    Ok(ShaderArtifactPlan {
        entry: stage.entry.clone(),
        ir: ShaderIR {
            interner: scx.interner_handle(),
            stages: vec![stage],
        },
        interface,
        dependencies: input.dependencies.clone(),
        global_params: (0..function.param_decls.len()).collect(),
        sampler_params: Vec::new(),
    })
}

pub fn plan_fullscreen_fragment(
    ws: &Workspace,
    fref: FuncRef,
    policy: &dyn ShaderPlannerPolicy,
) -> Result<ShaderArtifactPlan, CompilerError> {
    let function = &ws.module(fref.module).functions[fref.func];
    match shader_stage(function) {
        Some("fragment") => {}
        Some(stage) => {
            return Err(err(
                function.name_span,
                format!(
                    "shader entry: @shader({stage}) is not supported by the fullscreen artifact; only @shader(fragment) is admitted"
                ),
            ));
        }
        None => {
            return Err(err(
                function.name_span,
                format!(
                    "shader entry: `{}` has no @shader(fragment) annotation",
                    function.name
                ),
            ));
        }
    }
    let class = classify_entry(ws, function)?;
    let scx = ScalarCtx::new_values_first_class(ws);
    let stage = plan_stage(ws, &scx, fref, ShaderStage::Fragment, policy)?;
    let interface = ShaderInterfacePlan {
        stages: vec![ShaderStage::Fragment],
        uniform_groups: class
            .uniform_group
            .map(|group| {
                logical_uniform_group(
                    ws,
                    UniformRole::Stage(ShaderStage::Fragment),
                    group,
                    function.name_span,
                )
            })
            .transpose()?
            .into_iter()
            .collect(),
        textures: logical_textures(
            ws,
            class.texture_group,
            class
                .samplers
                .iter()
                .any(|(index, _, _)| !class.global_params.contains(index)),
            function.name_span,
        )?,
        bare_samplers: bare_sampler_list(&class.samplers, &class.global_params),
        fragment_output: FragmentOutput::Single(WireKind::F32x4),
        ..Default::default()
    };
    check_entry_collisions(&interface, function.name_span)?;
    let dependencies = stage.declarations();
    Ok(ShaderArtifactPlan {
        entry: stage.entry.clone(),
        ir: ShaderIR {
            interner: scx.interner_handle(),
            stages: vec![stage],
        },
        interface,
        dependencies,
        global_params: class.global_params,
        sampler_params: class.sampler_params,
    })
}
