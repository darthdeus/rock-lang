//! **The backend ABI of one emitted body** (plan S 13, cut C+D step A\*).
//!
//! Every question about how a function's values cross the wire -- which params
//! splat to leaves, what the return carries, how many lanes a JS result
//! publishes, what an `inline(lua)` body's params are spelled -- is answered
//! here, for one [`Instance`](crate::instance::Instance) under one execution
//! target, by one [`BackendFnAbiPlan`].
//!
//! The policy itself ([`fn_scalar_abi_spec`]) is PRIVATE to this module. That
//! is the whole point of the module boundary: a definition's parameter list and
//! the call that targets it used to reach the policy independently, so they
//! agreed only by being asked the same question in two places. They cannot
//! diverge if there is no second place to ask from.
//!
//! The execution target is part of the key rather than a formality.
//! [`ScalarCtx`] carries `values_first_class`, `total_specialization` and the
//! codegen backend, and the derived facts are split across targets too --
//! `result_lanes` is a JS transport fact luajit never reads, and
//! `lua_param_names` spells a foreign body's params. One physical plan shared
//! across targets would be answering with another target's ABI.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bone_semantic::hir::FnBackend;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::Span;
use ustr::Ustr;

use super::{
    nobox_declared_shape_spec, nobox_param_slot_spec, sig_param_slot_spec, sig_value_shape_spec,
    mono_concrete_param_slot, mono_tuple_ret_shape,
    mono_nobox_value_shape, mono_value_shape, slot_param_names,
    tuple_ret_shape, RetSlot, ScalarCtx, SpecSubst, TupleRetShape, ValueShape,
};
use crate::concrete::ConcreteBodyPlan;
use crate::mir::FuncRef;

/// An interned physical representation.  Its numeric identity is meaningful
/// only through the build-scoped arena carried by [`ScalarCtx`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepId(u32);

impl RepId {
    pub fn raw(self) -> u32 {
        self.0
    }
}

/// The use site is part of a representation key: a return tuple's element is
/// not allowed to silently become a function parameter merely because its
/// payload has the same leaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RepUse {
    Parameter,
    Return,
    TupleElement,
    Boundary,
}

#[derive(Clone, Debug)]
pub struct RepUseFacts {
    pub tuple_spelled: bool,
    pub span: Option<Span>,
}

impl Default for RepUseFacts {
    fn default() -> Self {
        Self { tuple_spelled: false, span: None }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RepGuarantees {
    pub nobox: bool,
}

#[derive(Clone, Debug)]
pub struct RepError {
    pub span: Option<Span>,
    pub message: String,
}

impl std::fmt::Display for RepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RepError {}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RepLayout {
    Opaque,
    Leaves(ValueShape),
    Tuple { slots: Vec<RepId>, nullable: bool },
    Foreign { reason: Ustr },
}

/// One concrete physical answer in the target/use context supplied by the
/// planner.  Backends may inspect it, but they do not derive another answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepPlan {
    id: RepId,
    target: FnBackend,
    use_context: RepUse,
    layout: RepLayout,
}

impl RepPlan {
    pub fn id(&self) -> RepId {
        self.id
    }

    pub fn target(&self) -> FnBackend {
        self.target
    }

    pub fn use_context(&self) -> RepUse {
        self.use_context
    }

    pub fn layout(&self) -> &RepLayout {
        &self.layout
    }

    pub fn lanes(&self, arena: &RepArena) -> usize {
        match &self.layout {
            RepLayout::Opaque | RepLayout::Foreign { .. } => 1,
            RepLayout::Leaves(shape) => shape.leaf_count(),
            RepLayout::Tuple { slots, nullable } => {
                slots.iter().map(|slot| arena.get(*slot).lanes(arena)).sum::<usize>()
                    + usize::from(*nullable)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionRepPlan {
    params: Vec<RepId>,
    result: Option<RepId>,
    return_lanes: usize,
    scalar_abi: bool,
}

impl FunctionRepPlan {
    pub fn params(&self) -> &[RepId] {
        &self.params
    }

    pub fn result(&self) -> Option<RepId> {
        self.result
    }

    pub fn publisher_result(&self) -> Option<RepId> {
        self.result
    }

    pub fn return_lanes(&self) -> usize {
        self.return_lanes
    }

    pub fn uses_scalar_abi(&self) -> bool {
        self.scalar_abi
    }
}

#[derive(Debug, Default)]
struct RepArenaInner {
    plans: Vec<RepPlan>,
    by_key: HashMap<(FnBackend, RepUse, RepLayout), RepId>,
}

/// The one build-scoped physical representation arena.  `ScalarCtx` owns the
/// handle and every ABI plan references the same arena, so equal answers are
/// equal identities rather than merely equal cloned shapes.
#[derive(Debug, Default)]
pub struct RepArena {
    inner: Mutex<RepArenaInner>,
}

impl RepArena {
    fn intern(&self, target: FnBackend, use_context: RepUse, layout: RepLayout) -> RepId {
        let mut inner = self.inner.lock().expect("RepArena lock poisoned");
        let key = (target, use_context, layout.clone());
        if let Some(id) = inner.by_key.get(&key) {
            return *id;
        }
        let id = RepId(inner.plans.len() as u32);
        inner.plans.push(RepPlan { id, target, use_context, layout });
        inner.by_key.insert(key, id);
        id
    }

    pub fn get(&self, id: RepId) -> RepPlan {
        self.inner
            .lock()
            .expect("RepArena lock poisoned")
            .plans
            .get(id.0 as usize)
            .cloned()
            .expect("RepId must belong to its build arena")
    }

    pub(crate) fn ret_slot(&self, id: RepId) -> RetSlot {
        match self.get(id).layout().clone() {
            RepLayout::Opaque | RepLayout::Foreign { .. } => RetSlot::Opaque,
            RepLayout::Leaves(shape) => RetSlot::Leaves(shape),
            RepLayout::Tuple { slots, nullable } => RetSlot::Nested(TupleRetShape {
                slots: slots.into_iter().map(|slot| self.ret_slot(slot)).collect(),
                nullable,
            }),
        }
    }

    pub(crate) fn value_shape(&self, id: RepId) -> Option<ValueShape> {
        match self.get(id).layout() {
            RepLayout::Leaves(shape) => Some(shape.clone()),
            _ => None,
        }
    }

    pub(crate) fn tuple_shape(&self, id: RepId) -> Option<TupleRetShape> {
        match self.get(id).layout().clone() {
            RepLayout::Tuple { slots, nullable } => Some(TupleRetShape {
                slots: slots.into_iter().map(|slot| self.ret_slot(slot)).collect(),
                nullable,
            }),
            _ => None,
        }
    }

    /// Total physical planning for one concrete logical type. `Opaque` is an
    /// explicit representation, not an absent answer; only a guarantee that
    /// forbids that representation can produce an error.
    pub fn plan(
        &self,
        ws: &Workspace,
        target: FnBackend,
        ty: &crate::monoty::MonoTy,
        use_context: RepUse,
        facts: &RepUseFacts,
        guarantees: RepGuarantees,
    ) -> Result<RepId, RepError> {
        use crate::monoty::{MonoCtor, MonoTy};

        let reject = |message: String| RepError { span: facts.span, message };
        match ty {
            MonoTy::Nullable(inner) => match inner.as_ref() {
                MonoTy::Tuple(parts) => {
                    let slots = parts
                        .iter()
                        .map(|part| {
                            self.plan(
                                ws,
                                target,
                                part,
                                RepUse::TupleElement,
                                facts,
                                guarantees,
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let splats = slots.iter().any(|id| self.get(*id).lanes(self) > 1);
                    if splats && (matches!(use_context, RepUse::Return) || facts.tuple_spelled) {
                        return Ok(self.intern(
                            target,
                            use_context,
                            RepLayout::Tuple { slots, nullable: true },
                        ));
                    }
                    if guarantees.nobox {
                        return Err(reject("nullable tuple cannot cross a nobox boundary without a physical tuple plan".into()));
                    }
                    Ok(self.intern(target, use_context, RepLayout::Opaque))
                }
                _ => {
                    let shape = if guarantees.nobox {
                        crate::lower::mono_nobox_value_shape(ws, ty)
                    } else {
                        crate::lower::mono_value_shape(ws, ty)
                    };
                    if let Some(shape) = shape {
                        return Ok(self.intern(target, use_context, RepLayout::Leaves(shape)));
                    }
                    if guarantees.nobox {
                        return Err(reject(format!(
                            "{} has no concrete nobox representation",
                            ty.render()
                        )));
                    }
                    Ok(self.intern(target, use_context, RepLayout::Opaque))
                }
            },
            MonoTy::Tuple(parts) => {
                let slots = parts
                    .iter()
                    .map(|part| {
                        self.plan(
                            ws,
                            target,
                            part,
                            RepUse::TupleElement,
                            facts,
                            guarantees,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let splats = slots.iter().any(|id| self.get(*id).lanes(self) > 1);
                if matches!(use_context, RepUse::Return) || (facts.tuple_spelled && splats) {
                    return Ok(self.intern(
                        target,
                        use_context,
                        RepLayout::Tuple { slots, nullable: false },
                    ));
                }
                if guarantees.nobox && !facts.tuple_spelled {
                    return Ok(self.intern(target, use_context, RepLayout::Opaque));
                }
                Ok(self.intern(target, use_context, RepLayout::Opaque))
            }
            MonoTy::App { ctor: MonoCtor::Nominal(_), .. } => {
                let shape = if guarantees.nobox {
                    crate::lower::mono_nobox_value_shape(ws, ty)
                } else {
                    crate::lower::mono_value_shape(ws, ty)
                };
                if let Some(shape) = shape {
                    return Ok(self.intern(target, use_context, RepLayout::Leaves(shape)));
                }
                if guarantees.nobox {
                    return Ok(self.intern(target, use_context, RepLayout::Opaque));
                }
                Ok(self.intern(target, use_context, RepLayout::Opaque))
            }
            MonoTy::App { .. } => {
                if guarantees.nobox {
                    return Err(reject(format!("{} cannot cross a nobox boundary", ty.render())));
                }
                Ok(self.intern(target, use_context, RepLayout::Opaque))
            }
            MonoTy::Int
            | MonoTy::Uint
            | MonoTy::Float
            | MonoTy::Bool
            | MonoTy::String
            | MonoTy::Any
            | MonoTy::Unit
            | MonoTy::Never
            | MonoTy::Function { .. } => {
                Ok(self.intern(target, use_context, RepLayout::Opaque))
            }
        }
    }

    fn slot(&self, target: FnBackend, slot: &RetSlot) -> RepId {
        match slot {
            RetSlot::Opaque => self.intern(target, RepUse::TupleElement, RepLayout::Opaque),
            RetSlot::Leaves(shape) => self.intern(
                target,
                RepUse::TupleElement,
                RepLayout::Leaves(shape.clone()),
            ),
            RetSlot::Nested(shape) => self.tuple(target, RepUse::TupleElement, shape),
        }
    }

    fn tuple(&self, target: FnBackend, use_context: RepUse, shape: &TupleRetShape) -> RepId {
        let slots = shape.slots.iter().map(|slot| self.slot(target, slot)).collect();
        self.intern(target, use_context, RepLayout::Tuple { slots, nullable: shape.nullable })
    }

    pub fn function_value(
        &self,
        ws: &Workspace,
        target: FnBackend,
        ty: &crate::monoty::MonoTy,
    ) -> Result<FunctionRepPlan, RepError> {
        let (ty, _) = ty.peel_nullable();
        let crate::monoty::MonoTy::Function { params, ret } = ty else {
            return Err(RepError {
                span: None,
                message: "a function-value representation requires a function type".into(),
            });
        };
        let params = params
            .iter()
            .map(|param| {
                let tuple_spelled = mono_tuple_ret_shape(ws, param)
                    .is_some_and(|shape| shape.splats());
                self.plan(
                    ws,
                    target,
                    param,
                    RepUse::Parameter,
                    &RepUseFacts { tuple_spelled, span: None },
                    RepGuarantees { nobox: true },
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let result = self.plan(
            ws,
            target,
            ret,
            RepUse::Return,
            &RepUseFacts {
                tuple_spelled: mono_tuple_ret_shape(ws, ret).is_some(),
                span: None,
            },
            RepGuarantees { nobox: true },
        )?;
        let scalar_abi = params.iter().any(|id| !matches!(self.get(*id).layout(), RepLayout::Opaque))
            || matches!(self.get(result).layout(), RepLayout::Leaves(_));
        let return_lanes = self.get(result).lanes(self);
        Ok(FunctionRepPlan {
            params,
            result: Some(result),
            return_lanes,
            scalar_abi,
        })
    }

    fn function(
        &self,
        target: FnBackend,
        scalar: Option<&FnScalarAbi>,
        ret_tuple: Option<&TupleRetShape>,
        param_count: usize,
        has_return: bool,
    ) -> FunctionRepPlan {
        let params = match scalar {
            Some(abi) => abi
                .param_shapes
                .iter()
                .map(|slot| match slot {
                    RetSlot::Opaque => self.intern(target, RepUse::Parameter, RepLayout::Opaque),
                    RetSlot::Leaves(shape) => self.intern(
                        target,
                        RepUse::Parameter,
                        RepLayout::Leaves(shape.clone()),
                    ),
                    RetSlot::Nested(shape) => self.tuple(target, RepUse::Parameter, shape),
                })
                .collect(),
            None => (0..param_count)
                .map(|_| self.intern(target, RepUse::Parameter, RepLayout::Opaque))
                .collect(),
        };
        let result = if !has_return {
            None
        } else if let Some(shape) = ret_tuple {
            Some(self.tuple(target, RepUse::Return, shape))
        } else if let Some(shape) = scalar.and_then(|abi| abi.ret_shape.as_ref()) {
            Some(self.intern(target, RepUse::Return, RepLayout::Leaves(shape.clone())))
        } else {
            Some(self.intern(target, RepUse::Return, RepLayout::Opaque))
        };
        let return_lanes = result.map_or(0, |id| self.get(id).lanes(self));
        FunctionRepPlan { params, result, return_lanes, scalar_abi: scalar.is_some() }
    }

    pub(crate) fn function_value_legacy(
        &self,
        target: FnBackend,
        param_shapes: Vec<RetSlot>,
        ret_shape: Option<ValueShape>,
        ret_tuple: Option<&TupleRetShape>,
        param_count: usize,
    ) -> FunctionRepPlan {
        let scalar = (ret_shape.is_some() || param_shapes.iter().any(RetSlot::splats))
            .then_some(FnScalarAbi {
                param_shapes,
                ret_shape,
                nobox_only: true,
            });
        self.function(target, scalar.as_ref(), ret_tuple, param_count, true)
    }

    fn function_from_concrete(
        &self,
        ws: &Workspace,
        target: FnBackend,
        params: &[crate::monoty::MonoTy],
        result_ty: Option<&crate::monoty::MonoTy>,
        scalar: Option<&FnScalarAbi>,
        ret_tuple: Option<&TupleRetShape>,
    ) -> FunctionRepPlan {
        let params = if let Some(scalar) = scalar {
            params
                .iter()
                .enumerate()
                .map(|(index, ty)| {
                    let tuple_spelled = scalar
                        .param_shapes
                        .get(index)
                        .is_some_and(|slot| !matches!(slot, RetSlot::Opaque));
                    self.plan(
                        ws,
                        target,
                        ty,
                        RepUse::Parameter,
                        &RepUseFacts { tuple_spelled, span: None },
                        RepGuarantees { nobox: scalar.nobox_only },
                    )
                    .expect("concrete ABI planner must accept a validated parameter")
                })
                .collect()
        } else {
            params
                .iter()
                .map(|_| self.intern(target, RepUse::Parameter, RepLayout::Opaque))
                .collect()
        };
        let result = match (result_ty, ret_tuple, scalar.and_then(|abi| abi.ret_shape.as_ref())) {
            (Some(ty), Some(_), _) => Some(
                self.plan(
                    ws,
                    target,
                    ty,
                    RepUse::Return,
                    &RepUseFacts { tuple_spelled: true, span: None },
                    RepGuarantees::default(),
                )
                .expect("concrete ABI planner must accept a validated tuple return"),
            ),
            (Some(ty), None, Some(_)) => Some(
                self.plan(
                    ws,
                    target,
                    ty,
                    RepUse::Return,
                    &RepUseFacts::default(),
                    RepGuarantees { nobox: scalar.is_some_and(|abi| abi.nobox_only) },
                )
                .expect("concrete ABI planner must accept a validated value return"),
            ),
            (Some(_), None, None) => Some(self.intern(target, RepUse::Return, RepLayout::Opaque)),
            (None, _, _) => None,
        };
        let return_lanes = result.map_or(0, |id| self.get(id).lanes(self));
        FunctionRepPlan {
            params,
            result,
            return_lanes,
            scalar_abi: scalar.is_some(),
        }
    }
}

/// Whether a callee is a Rock body subject that the concrete registry must
/// contain. Foreign, compile-time expansion, and native-required intrinsic
/// declarations still have an ABI, but no body plan exists for them because
/// their backend or lowering arm supplies the body itself.
pub fn requires_concrete_body_plan(
    ws: &Workspace,
    inst: &crate::instance::Instance,
) -> bool {
    let f = &ws.module(inst.func.module).functions[inst.func.func];
    !f.is_extern
        && f.inline_body.is_none()
        && crate::intrinsics::expand_kind(f).is_none()
        && !(crate::intrinsics::is_intrinsic(f) && crate::intrinsics::is_native_required(f))
        && ws
            .module(inst.func.module)
            .bodies
            .contains_key(&bone_semantic::hir::BodyOwner::Function(inst.func.func))
}

/// The scalarized wire of one emitted body: what each declared param occupies,
/// and what the return carries. `None` for the whole plan means the boxed ABI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FnScalarAbi {
    pub(super) param_shapes: Vec<RetSlot>,
    pub(super) ret_shape: Option<ValueShape>,
    pub(super) nobox_only: bool,
}

/// The one ABI policy. PRIVATE: every consumer reads a [`BackendFnAbiPlan`].
///
/// Address-taken Rock bodies narrow to the `nobox` shapes an indirect caller can
/// derive from the fn type; direct-only bodies may scalarize every eligible value.
fn fn_scalar_abi_spec(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
    subst: &SpecSubst,
) -> Option<FnScalarAbi> {
    if scx.values_first_class {
        return None;
    }
    let f = &ws.module(fref.module).functions[fref.func];
    if f.is_extern {
        return None;
    }
    let has_scalar_abi = f.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi");
    let nobox_only = if has_scalar_abi {
        false
    } else {
        let name_is_fixed = f.annotations.iter().any(|a| match a.name.name.as_str() {
            "nomangle" => subst.is_empty(),
            "test" => true,
            _ => false,
        });
        if f.inline_body.is_some() || f.name.as_str() == "main" || name_is_fixed {
            return None;
        }
        scx.taken_by_ref.contains(&f.name)
    };
    if nobox_only {
        return fn_value_nobox_abi(ws, fref, subst);
    }
    let param_shapes: Vec<RetSlot> =
        f.param_decls.iter().map(|pd| sig_param_slot_spec(ws, subst, pd.ty)).collect();
    let ret_shape = f.return_ty.and_then(|ty| sig_value_shape_spec(ws, subst, ty));
    (ret_shape.is_some() || param_shapes.iter().any(RetSlot::splats))
        .then_some(FnScalarAbi { param_shapes, ret_shape, nobox_only })
}

/// The ABI an INDIRECT caller derives from a fn TYPE alone -- the `nobox` shapes
/// and nothing else.
///
/// A different question from [`fn_scalar_abi_spec`], which answers what the
/// DEFINITION emits, so it stays its own named oracle rather than a mode of the
/// plan: the two are compared against each other, and a comparison needs two
/// sides.
pub(super) fn fn_value_nobox_abi(
    ws: &Workspace,
    fref: FuncRef,
    subst: &SpecSubst,
) -> Option<FnScalarAbi> {
    let f = &ws.module(fref.module).functions[fref.func];
    let param_shapes: Vec<RetSlot> = f
        .param_decls
        .iter()
        .map(|pd| nobox_param_slot_spec(ws, subst, pd.ty))
        .collect();
    let ret_shape = f.return_ty.and_then(|ty| nobox_declared_shape_spec(ws, subst, ty));
    (ret_shape.is_some() || param_shapes.iter().any(RetSlot::splats))
        .then_some(FnScalarAbi { param_shapes, ret_shape, nobox_only: true })
}

/// **The backend ABI of one emitted body**, under one execution target.
///
/// Built from a declaration and the substitution its [`AbiSubject`] resolves
/// to. The substitution is what makes a per-body answer different from a
/// per-declaration one rather than merely more precise: `T` is one boxed slot
/// on the declaration and two lanes at `T = (bool, float)`.
///
/// The derived spellings are METHODS, not fields, so the ordinary lowering path
/// -- which needs only the wire shapes -- never pays for the leaf-name strings
/// that exist for foreign bodies.
#[derive(Clone, Debug)]
pub struct BackendFnAbiPlan {
    pub subject: crate::instance::Instance,
    /// The codegen backend this plan was built for. Part of the key: the same
    /// body has a different ABI under `values_first_class` (GLSL) than under
    /// luajit, and `result_lanes` is a JS transport fact.
    pub target: FnBackend,
    /// `None` when this body keeps the boxed ABI.
    scalar: Option<FnScalarAbi>,
    /// The declared return's tuple shape under this body's substitution. Read
    /// by the definition and by every call, which each computed it separately.
    ret_tuple: Option<TupleRetShape>,
    /// Whether the declaration returns anything at all -- the one result-wire
    /// fact no shape can carry, since both shapes answer `None` for a plain
    /// scalar return and for no return.
    has_return: bool,
    /// The physical function signature. Every call, entry binder, return
    /// lowerer, and publisher keeps these identities rather than rebuilding a
    /// shape from the declaration.
    representation: FunctionRepPlan,
    arena: Arc<RepArena>,
}

impl PartialEq for BackendFnAbiPlan {
    fn eq(&self, other: &Self) -> bool {
        self.subject == other.subject
            && self.target == other.target
            && self.scalar == other.scalar
            && self.ret_tuple == other.ret_tuple
            && self.has_return == other.has_return
            && self.representation == other.representation
            && Arc::ptr_eq(&self.arena, &other.arena)
    }
}

impl Eq for BackendFnAbiPlan {}

fn fn_scalar_abi_concrete(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
) -> Option<FnScalarAbi> {
    if scx.values_first_class {
        return None;
    }
    let inst = plan.function_instance()?;
    let f = &ws.module(inst.func.module).functions[inst.func.func];
    if f.is_extern {
        return None;
    }
    let has_scalar_abi = f.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi");
    let nobox_only = if has_scalar_abi {
        false
    } else {
        let name_is_fixed = f.annotations.iter().any(|a| match a.name.name.as_str() {
            "nomangle" => inst.args.is_empty(),
            "test" => true,
            _ => false,
        });
        if f.inline_body.is_some() || f.name.as_str() == "main" || name_is_fixed {
            return None;
        }
        scx.taken_by_ref.contains(&f.name)
    };
    let params = plan
        .signature
        .params
        .iter()
        .map(|id| scx.interner().get(*id))
        .collect::<Vec<_>>();
    let param_shapes: Vec<RetSlot> = params
        .iter()
        .enumerate()
        .map(|(index, ty)| {
            let tuple_spelled = *plan
                .signature
                .param_tuple_spelled
                .get(index)
                .expect("concrete signature is missing parameter ABI provenance");
            mono_concrete_param_slot(ws, ty, tuple_spelled, nobox_only)
        })
        .collect();
    let ret_shape = plan
        .signature
        .result
        .map(|id| scx.interner().get(id))
        .and_then(|ty| {
            if nobox_only {
                mono_nobox_value_shape(ws, &ty)
            } else {
                mono_value_shape(ws, &ty)
            }
        });
    (ret_shape.is_some() || param_shapes.iter().any(RetSlot::splats))
        .then_some(FnScalarAbi { param_shapes, ret_shape, nobox_only })
}

impl BackendFnAbiPlan {
    /// **THE constructor: one plan per (execution target, concrete body).**
    ///
    /// Every definition and every call that targets it reaches this with the
    /// SAME [`Instance`], which is what makes the caller's argument splat and
    /// the callee's parameter list one derivation rather than two.
    ///
    /// [`Instance`]: crate::instance::Instance
    pub fn for_instance(
        ws: &Workspace,
        scx: &ScalarCtx,
        inst: &crate::instance::Instance,
    ) -> Self {
        let subst = inst.subst(ws, scx.interner());
        Self::assemble(ws, scx, inst.clone(), &subst)
    }

    /// Build the ABI from the graph-owned concrete signature. No declaration
    /// substitution is reconstructed on this path.
    pub fn for_concrete_plan(
        ws: &Workspace,
        scx: &ScalarCtx,
        plan: &ConcreteBodyPlan,
    ) -> Self {
        let inst = plan
            .function_instance()
            .expect("a function ABI plan needs an ordinary concrete owner");
        let scalar = fn_scalar_abi_concrete(ws, scx, plan);
        let ret_tuple = plan
            .signature
            .result
            .map(|id| scx.interner().get(id))
            .and_then(|ty| mono_tuple_ret_shape(ws, &ty));
        let params = plan
            .signature
            .params
            .iter()
            .map(|id| scx.interner().get(*id))
            .collect::<Vec<_>>();
        let result_ty = plan.signature.result.map(|id| scx.interner().get(id));
        let arena = scx.rep_arena();
        let representation = arena.function_from_concrete(
            ws,
            scx.target,
            &params,
            result_ty.as_ref(),
            scalar.as_ref(),
            ret_tuple.as_ref(),
        );
        Self {
            subject: inst.clone(),
            target: scx.target,
            scalar,
            ret_tuple,
            has_return: plan.signature.result.is_some(),
            representation,
            arena,
        }
    }

    /// [`Self::for_instance`] for a caller that already resolved the
    /// substitution. A body's lowering and its call sites each hold both, so
    /// re-deriving one from the other would be work with no second opinion in
    /// it.
    ///
    /// The instance would otherwise be DECORATIVE here -- the ABI comes from
    /// `subst`, so one naming another instantiation would ride along unnoticed.
    /// The debug assertion is what makes the two a checked agreement rather
    /// than a convention; it is off in release, where the compile path pays
    /// nothing.
    pub(super) fn for_instance_with_subst(
        ws: &Workspace,
        scx: &ScalarCtx,
        inst: &crate::instance::Instance,
        subst: &SpecSubst,
    ) -> Self {
        debug_assert_eq!(
            &inst.subst(ws, scx.interner()),
            subst,
            "a body's instance and the substitution it lowers under must be one instantiation",
        );
        Self::assemble(ws, scx, inst.clone(), subst)
    }

    /// The plan of a declaration's own NON-GENERIC instance.
    ///
    /// Not "the shared body's plan": a declaration with generics has no body of
    /// its own to plan for, and asking this about one is a caller bug rather
    /// than a different answer.
    pub fn for_declaration(ws: &Workspace, scx: &ScalarCtx, decl: FuncRef) -> Self {
        Self::for_instance(ws, scx, &crate::instance::Instance::base(decl))
    }

    pub fn for_function_value(
        ws: &Workspace,
        scx: &ScalarCtx,
        ty: &crate::monoty::MonoTy,
    ) -> Result<FunctionRepPlan, RepError> {
        scx.rep_arena().function_value(ws, scx.target, ty)
    }

    fn assemble(
        ws: &Workspace,
        scx: &ScalarCtx,
        subject: crate::instance::Instance,
        subst: &SpecSubst,
    ) -> Self {
        let decl = subject.func;
        let f = &ws.module(decl.module).functions[decl.func];
        let scalar = fn_scalar_abi_spec(ws, scx, decl, subst);
        let ret_tuple = f.return_ty.and_then(|ty| tuple_ret_shape(ws, subst, ty));
        let arena = scx.rep_arena();
        let representation = arena.function(
            scx.target,
            scalar.as_ref(),
            ret_tuple.as_ref(),
            f.param_decls.len(),
            f.return_ty.is_some(),
        );
        Self {
            subject,
            target: scx.target,
            scalar,
            ret_tuple,
            has_return: f.return_ty.is_some(),
            representation,
            arena,
        }
    }

    pub fn decl(&self) -> FuncRef {
        self.subject.func
    }

    /// The scalarized wire, or `None` for the boxed ABI.
    pub(super) fn scalar(&self) -> Option<&FnScalarAbi> {
        self.scalar.as_ref()
    }

    pub fn representation(&self) -> &FunctionRepPlan {
        &self.representation
    }

    pub fn rep_plan(&self, id: RepId) -> RepPlan {
        self.arena.get(id)
    }

    /// Per declared param, what it occupies on the wire. Empty when this body
    /// keeps the boxed ABI, which is what the lowering path already treats as
    /// "no splatting".
    pub(super) fn param_shapes(&self) -> Vec<RetSlot> {
        self.scalar.as_ref().map(|a| a.param_shapes.clone()).unwrap_or_default()
    }

    /// The per-declared-param wire slots, or `None` under the boxed ABI --
    /// distinct from [`Self::param_shapes`], which flattens the distinction
    /// away for the lowering path that treats both as "no splatting".
    pub(super) fn param_leaf_shapes(&self) -> Option<&[RetSlot]> {
        self.scalar.as_ref().map(|a| a.param_shapes.as_slice())
    }

    pub(super) fn ret_shape(&self) -> Option<&ValueShape> {
        self.scalar.as_ref().and_then(|a| a.ret_shape.as_ref())
    }

    /// The declared return's tuple shape under this body's substitution.
    pub(crate) fn ret_tuple(&self) -> Option<&TupleRetShape> {
        self.ret_tuple.as_ref()
    }

    /// Physical values on the result wire: 0 with no return, 1 for an ordinary
    /// one, more for the two shapes that splat -- a tuple return (each element
    /// its own slot, a `nobox` element into its leaves) and a bare `nobox`
    /// value return under the scalar ABI.
    ///
    /// **A JS transport fact.** Plan S 13b: leaf 0 rides the ordinary JS
    /// `return`, lanes 1..N ride the module-scope scratch bank
    /// (`docs/boundary-contracts.md` 1.2). Luajit has native multi-return and
    /// consults nothing here; the artifact view still records the width for
    /// every artifact, because it is a property of the wire rather than of the
    /// backend that happens to need it spelled out.
    pub fn result_lanes(&self) -> usize {
        self.representation.return_lanes()
    }

    /// Depth-first leaf names for a scalar-ABI FOREIGN body (`@scalar_abi`):
    /// each value param expands to `<base>__<leaf>`, the names the hand-written
    /// body references; other params keep their name. `None` when this body
    /// does not use the scalar ABI, and the caller emits the plain list. The
    /// leaf ORDER matches `repr_to_leaves`, so def params line up with the
    /// callsite splat positionally.
    pub fn lua_param_names(&self, ws: &Workspace) -> Option<Vec<String>> {
        let abi = self.scalar.as_ref()?;
        let f = &ws.module(self.decl().module).functions[self.decl().func];
        let mut names = Vec::new();
        for (pd, slot) in f.param_decls.iter().zip(abi.param_shapes.iter()) {
            slot_param_names(pd.name.as_str(), slot, &mut names);
        }
        Some(names)
    }

    /// Per-DECLARED-param leaf width (a value-struct param's flattened leaf
    /// count, 1 otherwise). Per declared param rather than per slot so a
    /// defaulted value-struct param counts as its whole leaf group.
    pub fn param_leaf_widths(&self) -> Option<Vec<usize>> {
        self.scalar.as_ref().map(|a| a.param_shapes.iter().map(RetSlot::slot_count).collect())
    }
}
