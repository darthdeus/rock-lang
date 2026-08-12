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

use bone_semantic::hir::{FnBackend, Function};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::Span;
use ustr::Ustr;

use super::{slot_param_names, value_storage_for_nominal, RetSlot, ScalarCtx, SpecSubst, TupleRetShape, ValueShape, ValueStorage};
use crate::concrete::{ConcreteBodyPlan, ConcreteSignature};
use crate::mir::FuncRef;

/// The one declaration-level classification shared by the instance graph,
/// artifact projection, and representation-boundary validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BoundaryClass {
    Extern,
    InlineBody,
    Intrinsic,
    ErasedRuntimeEntry,
    Invalid,
}

impl BoundaryClass {
    pub fn name(self) -> &'static str {
        match self {
            Self::Extern => "extern",
            Self::InlineBody => "inline-body",
            Self::Intrinsic => "intrinsic",
            Self::ErasedRuntimeEntry => "erased-runtime-entry",
            Self::Invalid => "invalid",
        }
    }
}

/// Classify only declaration-owned boundaries. Target capability and erased
/// runtime membership are supplied by the graph/artifact owner; keeping those
/// inputs out of this function prevents `@inline` from becoming an identity or
/// representation decision.
pub fn declared_boundary_class(f: &Function) -> Option<BoundaryClass> {
    if f.is_extern {
        return Some(BoundaryClass::Extern);
    }
    if crate::intrinsics::expand_kind(f).is_some() {
        return Some(BoundaryClass::Intrinsic);
    }
    if f.annotations.iter().any(|a| a.name.name.as_str() == "intrinsic") {
        let native_required = crate::intrinsics::is_native_required(f);
        let has_rock_body = f.inline_body.is_none() && !f.is_extern;
        return (native_required || !has_rock_body).then_some(BoundaryClass::Intrinsic);
    }
    f.inline_body.is_some().then_some(BoundaryClass::InlineBody)
}

/// Whether a declaration is an opaque boundary for scalarized value transport.
/// `@scalar_abi` is a transport contract for inline text; it does not create an
/// instance or change the declaration's boundary class.
pub fn boundary_requires_boxed_representation(f: &Function) -> bool {
    match declared_boundary_class(f) {
        Some(BoundaryClass::Extern) => true,
        Some(BoundaryClass::InlineBody) => {
            !f.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi")
        }
        _ => false,
    }
}

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
    Value,
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
    /// Function-value and mixed scalar ABIs may carry reference types in an
    /// explicit opaque slot while still flattening eligible value leaves.
    pub allow_opaque: bool,
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
    storage: Option<ValueStorage>,
    column_eligible: bool,
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

    pub fn value_storage(&self) -> Option<ValueStorage> {
        self.storage
    }

    pub fn column_eligible(&self) -> bool {
        self.column_eligible
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

    fn splats(&self, arena: &RepArena) -> bool {
        match &self.layout {
            RepLayout::Leaves(_) => true,
            RepLayout::Tuple { slots, .. } => slots.iter().any(|slot| arena.get(*slot).splats(arena)),
            RepLayout::Opaque | RepLayout::Foreign { .. } => false,
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

/// The physical representation of an always-emitted global initializer.
/// Globals have no callable `Instance`, so their result plan is carried on the
/// lowered initializer itself rather than being smuggled through a function ABI.
#[derive(Clone, Debug)]
pub struct GlobalPlan {
    representation: RepId,
    arena: Arc<RepArena>,
}

impl PartialEq for GlobalPlan {
    fn eq(&self, other: &Self) -> bool {
        self.representation == other.representation && Arc::ptr_eq(&self.arena, &other.arena)
    }
}

impl Eq for GlobalPlan {}

impl GlobalPlan {
    pub fn for_declared_type(
        ws: &Workspace,
        scx: &ScalarCtx,
        ty: bone_semantic::prelude::TyId,
    ) -> Result<Self, RepError> {
        let ty = crate::monoty::convert_decl(ws, ty, &SpecSubst::default()).map_err(|reason| RepError {
            span: None,
            message: format!("global type cannot be planned: {reason:?}"),
        })?;
        Self::for_mono_type(ws, scx, ty)
    }

    fn for_mono_type(ws: &Workspace, scx: &ScalarCtx, ty: crate::monoty::MonoTy) -> Result<Self, RepError> {
        let arena = scx.rep_arena();
        let representation = if scx.values_first_class {
            arena.intern(scx.target, RepUse::Value, RepLayout::Opaque)
        } else {
            arena.plan(
                ws,
                scx.target,
                &ty,
                RepUse::Value,
                &RepUseFacts::default(),
                RepGuarantees { nobox: true, allow_opaque: true },
            )?
        };
        Ok(Self { representation, arena })
    }

    pub(crate) fn for_concrete_result(
        ws: &Workspace,
        scx: &ScalarCtx,
        result: Option<crate::monoty::MonoTy>,
    ) -> Result<Self, RepError> {
        let ty = result.ok_or_else(|| RepError {
            span: None,
            message: "a global initializer must have a concrete result type".into(),
        })?;
        Self::for_mono_type(ws, scx, ty)
    }

    pub fn representation(&self) -> RepId {
        self.representation
    }

    pub fn use_context(&self) -> RepUse {
        self.arena.get(self.representation).use_context()
    }

    pub(crate) fn value_shape(&self) -> Option<ValueShape> {
        self.arena.value_shape(self.representation)
    }

    pub fn leaf_names(&self, base: Ustr) -> Option<Vec<Ustr>> {
        let shape = self.value_shape()?;
        Some(
            (0..shape.leaf_count())
                .map(|leaf| crate::lower::nobox_global_leaf_name(base, leaf))
                .collect(),
        )
    }

    pub fn lanes(&self) -> usize {
        self.arena.get(self.representation).lanes(&self.arena)
    }
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
    by_key: HashMap<(FnBackend, RepUse, RepLayout, Option<ValueStorage>, bool), RepId>,
}

/// The one build-scoped physical representation arena.  `ScalarCtx` owns the
/// handle and every ABI plan references the same arena, so equal answers are
/// equal identities rather than merely equal cloned shapes.
#[derive(Debug, Default)]
pub struct RepArena {
    inner: Mutex<RepArenaInner>,
}

fn mono_value_storage(
    ws: &Workspace,
    target: FnBackend,
    ty: &crate::monoty::MonoTy,
) -> Option<ValueStorage> {
    let (payload, _) = ty.peel_nullable();
    let crate::monoty::MonoTy::App {
        ctor: crate::monoty::MonoCtor::Nominal(name), ..
    } = payload
    else {
        return None;
    };
    value_storage_for_nominal(ws, target, *name)
}

impl RepArena {
    fn intern(&self, target: FnBackend, use_context: RepUse, layout: RepLayout) -> RepId {
        self.intern_with_storage(target, use_context, layout, None)
    }

    fn intern_with_storage(
        &self,
        target: FnBackend,
        use_context: RepUse,
        layout: RepLayout,
        storage: Option<ValueStorage>,
    ) -> RepId {
        self.intern_with_facts(target, use_context, layout, storage, false)
    }

    fn intern_with_facts(
        &self,
        target: FnBackend,
        use_context: RepUse,
        layout: RepLayout,
        storage: Option<ValueStorage>,
        column_eligible: bool,
    ) -> RepId {
        let mut inner = self.inner.lock().expect("RepArena lock poisoned");
        let key = (target, use_context, layout.clone(), storage, column_eligible);
        if let Some(id) = inner.by_key.get(&key) {
            return *id;
        }
        let id = RepId(inner.plans.len() as u32);
        inner.plans.push(RepPlan {
            id,
            target,
            use_context,
            layout,
            storage,
            column_eligible,
        });
        inner.by_key.insert(key, id);
        id
    }

    pub(crate) fn column_eligible(&self, id: RepId) -> bool {
        self.get(id).column_eligible()
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

    pub(crate) fn leaf_addresses(
        &self,
        ws: &Workspace,
        id: RepId,
    ) -> Option<Vec<crate::lower::LeafAddr>> {
        if !self.column_eligible(id) {
            return None;
        }
        let shape = self.value_shape(id)?;
        crate::lower::leaf_addr_plan(ws, shape.name).ok()
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
        let tuple_element_shape = matches!(use_context, RepUse::TupleElement);
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
                    if splats
                        && (matches!(use_context, RepUse::Return | RepUse::TupleElement)
                            || facts.tuple_spelled)
                    {
                        return Ok(self.intern(
                            target,
                            use_context,
                            RepLayout::Tuple { slots, nullable: true },
                        ));
                    }
                    if guarantees.nobox && !guarantees.allow_opaque {
                        return Err(reject("nullable tuple cannot cross a nobox boundary without a physical tuple plan".into()));
                    }
                    Ok(self.intern(target, use_context, RepLayout::Opaque))
                }
                _ => {
                    let shape = if guarantees.nobox || tuple_element_shape {
                        crate::lower::mono_nobox_value_shape(ws, ty)
                    } else {
                        crate::lower::mono_value_shape(ws, ty)
                    };
                    if let Some(shape) = shape {
                        let storage = mono_value_storage(ws, target, ty);
                        let column_eligible = crate::lower::is_ffi_clean_value_struct(ws, shape.name);
                        return Ok(self.intern_with_facts(
                            target,
                            use_context,
                            RepLayout::Leaves(shape),
                            storage,
                            column_eligible,
                        ));
                    }
                    if guarantees.nobox && !guarantees.allow_opaque {
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
                if matches!(use_context, RepUse::Return | RepUse::TupleElement)
                    || (facts.tuple_spelled && splats)
                {
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
                let shape = if guarantees.nobox || tuple_element_shape {
                    crate::lower::mono_nobox_value_shape(ws, ty)
                } else {
                    crate::lower::mono_value_shape(ws, ty)
                };
                if let Some(shape) = shape {
                    let storage = mono_value_storage(ws, target, ty);
                    let column_eligible = crate::lower::is_ffi_clean_value_struct(ws, shape.name);
                    return Ok(self.intern_with_facts(
                        target,
                        use_context,
                        RepLayout::Leaves(shape),
                        storage,
                        column_eligible,
                    ));
                }
                if guarantees.nobox && !guarantees.allow_opaque {
                    return Err(reject(format!(
                        "{} has no concrete nobox representation",
                        ty.render()
                    )));
                }
                Ok(self.intern(target, use_context, RepLayout::Opaque))
            }
            MonoTy::App { .. } => {
                if guarantees.nobox && !guarantees.allow_opaque {
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

    pub fn function_value(
        &self,
        ws: &Workspace,
        target: FnBackend,
        ty: &crate::monoty::MonoTy,
    ) -> Result<FunctionRepPlan, RepError> {
        self.function_value_with_mode(ws, target, ty, true)
    }

    pub fn function_value_with_mode(
        &self,
        ws: &Workspace,
        target: FnBackend,
        ty: &crate::monoty::MonoTy,
        nobox_only: bool,
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
                let tuple_spelled = matches!(param.peel_nullable().0, crate::monoty::MonoTy::Tuple(_))
                    && self
                        .plan(
                            ws,
                            target,
                            param,
                            RepUse::Return,
                            &RepUseFacts::default(),
                            RepGuarantees { nobox: true, allow_opaque: true },
                        )
                        .map(|id| self.get(id).splats(self))
                        .unwrap_or(false);
                self.plan(
                    ws,
                    target,
                    param,
                    RepUse::Parameter,
                    &RepUseFacts { tuple_spelled, span: None },
                    RepGuarantees { nobox: nobox_only, allow_opaque: true },
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let result = self.plan(
            ws,
            target,
            ret,
            RepUse::Return,
            &RepUseFacts {
                tuple_spelled: matches!(ret.peel_nullable().0, crate::monoty::MonoTy::Tuple(_)),
                span: None,
            },
            RepGuarantees { nobox: nobox_only, allow_opaque: true },
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

    fn function_from_declaration(
        &self,
        ws: &Workspace,
        target: FnBackend,
        f: &Function,
        subst: &SpecSubst,
        mode: Option<AbiMode>,
    ) -> Result<FunctionRepPlan, RepError> {
        let mut params = Vec::with_capacity(f.param_decls.len());
        for param in &f.param_decls {
            let ty = declared_mono_type(ws, param.ty, subst)?;
            let id = match (mode, ty) {
                (Some(mode), Some(ty)) => self.plan(
                    ws,
                    target,
                    &ty,
                    RepUse::Parameter,
                    &RepUseFacts {
                        tuple_spelled: declared_tuple_spelled(ws, param.ty),
                        span: None,
                    },
                    RepGuarantees { nobox: mode.nobox_only, allow_opaque: true },
                )?,
                _ => self.intern(target, RepUse::Parameter, RepLayout::Opaque),
            };
            params.push(id);
        }

        let result = f.return_ty.map(|return_ty| {
            let ty = declared_mono_type(ws, return_ty, subst)?;
            let tuple_spelled = declared_tuple_spelled(ws, return_ty);
            let id = if let Some(ty) = ty {
                let is_tuple = matches!(ty, crate::monoty::MonoTy::Tuple(_));
                if let Some(mode) = mode {
                    self.plan(
                        ws,
                        target,
                        &ty,
                        RepUse::Return,
                        &RepUseFacts { tuple_spelled: tuple_spelled || is_tuple, span: None },
                        RepGuarantees { nobox: mode.nobox_only, allow_opaque: true },
                    )?
                } else if is_tuple {
                    self.plan(
                        ws,
                        target,
                        &ty,
                        RepUse::Return,
                        &RepUseFacts { tuple_spelled: true, span: None },
                        RepGuarantees::default(),
                    )?
                } else {
                    self.intern(target, RepUse::Return, RepLayout::Opaque)
                }
            } else {
                self.intern(target, RepUse::Return, RepLayout::Opaque)
            };
            Ok(id)
        }).transpose()?;
        let scalar_abi = mode.is_some()
            && (params
                .iter()
                .any(|id| matches!(self.get(*id).layout(), RepLayout::Leaves(_) | RepLayout::Tuple { .. }))
                || result.is_some_and(|id| matches!(self.get(id).layout(), RepLayout::Leaves(_))));
        let return_lanes = result.map_or(0, |id| self.get(id).lanes(self));
        Ok(FunctionRepPlan { params, result, return_lanes, scalar_abi })
    }

    pub(crate) fn boxed_function(
        &self,
        target: FnBackend,
        param_count: usize,
    ) -> FunctionRepPlan {
        let params = (0..param_count)
            .map(|_| self.intern(target, RepUse::Parameter, RepLayout::Opaque))
            .collect();
        let result = self.intern(target, RepUse::Return, RepLayout::Opaque);
        FunctionRepPlan { params, result: Some(result), return_lanes: 1, scalar_abi: false }
    }

    fn function_from_concrete(
        &self,
        ws: &Workspace,
        target: FnBackend,
        params: &[crate::monoty::MonoTy],
        result_ty: Option<&crate::monoty::MonoTy>,
        param_tuple_spelled: &[bool],
        mode: Option<ConcreteAbiMode>,
    ) -> FunctionRepPlan {
        let params: Vec<RepId> = if let Some(mode) = mode {
            params
                .iter()
                .enumerate()
                .map(|(index, ty)| {
                    let tuple_spelled = *param_tuple_spelled
                        .get(index)
                        .expect("concrete signature is missing parameter ABI provenance");
                    self.plan(
                        ws,
                        target,
                        ty,
                        RepUse::Parameter,
                        &RepUseFacts { tuple_spelled, span: None },
                        RepGuarantees {
                            nobox: mode.nobox_only,
                            allow_opaque: true,
                        },
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
        let result = result_ty.map(|ty| {
            let tuple_spelled = match ty {
                crate::monoty::MonoTy::Tuple(_) => true,
                crate::monoty::MonoTy::Nullable(inner) => {
                    matches!(inner.as_ref(), crate::monoty::MonoTy::Tuple(_))
                }
                _ => false,
            };
            if mode.is_some() || tuple_spelled {
                self.plan(
                    ws,
                    target,
                    ty,
                    RepUse::Return,
                    &RepUseFacts { tuple_spelled, span: None },
                    RepGuarantees {
                        nobox: mode.is_some_and(|mode| mode.nobox_only),
                        allow_opaque: true,
                    },
                )
                .expect("concrete ABI planner must accept a validated result")
            } else {
                self.intern(target, RepUse::Return, RepLayout::Opaque)
            }
        });
        let return_lanes = result.map_or(0, |id| self.get(id).lanes(self));
        let scalar_abi = mode.is_some()
            && (params
                .iter()
                .any(|id| !matches!(self.get(*id).layout(), RepLayout::Opaque))
                || result.is_some_and(|id| matches!(self.get(id).layout(), RepLayout::Leaves(_))));
        FunctionRepPlan {
            params,
            result,
            return_lanes,
            scalar_abi,
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

#[derive(Clone, Copy, Debug)]
struct AbiMode {
    nobox_only: bool,
}

fn declared_tuple_spelled(ws: &Workspace, ty: bone_semantic::prelude::TyId) -> bool {
    match super::static_ty(ws, ty) {
        Some(bone_semantic::hir::TypeExpr::Tuple { .. }) => true,
        Some(bone_semantic::hir::TypeExpr::Nullable(inner)) => matches!(
            super::static_ty(ws, ty.with_idx(inner)),
            Some(bone_semantic::hir::TypeExpr::Tuple { .. })
        ),
        _ => false,
    }
}

fn declared_mono_type(
    ws: &Workspace,
    ty: bone_semantic::prelude::TyId,
    subst: &SpecSubst,
) -> Result<Option<crate::monoty::MonoTy>, RepError> {
    match crate::monoty::convert_decl(ws, ty, subst) {
        Ok(ty) => Ok(Some(ty)),
        Err(crate::monoty::MonoTyReason::UnboundGeneric(_)) => Ok(None),
        Err(reason) => Err(RepError {
            span: None,
            message: format!("declaration type cannot be planned: {reason:?}"),
        }),
    }
}

/// The one ABI policy. PRIVATE: every consumer reads a [`BackendFnAbiPlan`].
///
/// Address-taken Rock bodies narrow to the `nobox` shapes an indirect caller can
/// derive from the fn type; direct-only bodies may scalarize every eligible value.
fn abi_mode_spec(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
    subst: &SpecSubst,
) -> Option<AbiMode> {
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
    Some(AbiMode { nobox_only })
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
            && self.representation == other.representation
            && Arc::ptr_eq(&self.arena, &other.arena)
    }
}

impl Eq for BackendFnAbiPlan {}

fn concrete_abi_mode_for_instance(
    ws: &Workspace,
    scx: &ScalarCtx,
    inst: &crate::instance::Instance,
) -> Option<ConcreteAbiMode> {
    if scx.values_first_class {
        return None;
    }
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
    Some(ConcreteAbiMode { nobox_only })
}

#[derive(Clone, Copy, Debug)]
struct ConcreteAbiMode {
    nobox_only: bool,
}

impl BackendFnAbiPlan {
    /// ABI for an explicit opaque boundary instance.
    ///
    /// A boundary has no `ConcreteBodyPlan`, but its target-specific transport
    /// still belongs to the same RepPlan arena. Keeping this entry named for the
    /// boundary makes it impossible to mistake declaration substitution for a
    /// Rock-body planning path.
    pub fn for_boundary_instance(
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
        Self::for_concrete_signature(ws, scx, inst, &plan.signature)
    }

    /// Build an ABI from a concrete signature owned by a target-selected
    /// declaration. This covers backend siblings whose body is selected by the
    /// artifact but whose logical signature was recorded on the canonical graph
    /// instance.
    pub fn for_concrete_signature(
        ws: &Workspace,
        scx: &ScalarCtx,
        inst: &crate::instance::Instance,
        signature: &ConcreteSignature,
    ) -> Self {
        let mode = concrete_abi_mode_for_instance(ws, scx, inst);
        let params = signature
            .params
            .iter()
            .map(|id| scx.interner().get(*id))
            .collect::<Vec<_>>();
        let result_ty = signature.result.map(|id| scx.interner().get(id));
        let arena = scx.rep_arena();
        let representation = arena.function_from_concrete(
            ws,
            scx.target,
            &params,
            result_ty.as_ref(),
            &signature.param_tuple_spelled,
            mode,
        );
        Self {
            subject: inst.clone(),
            target: scx.target,
            representation,
            arena,
        }
    }

    /// ABI for a declaration-owned opaque boundary with no instance arguments.
    pub fn for_boundary_declaration(ws: &Workspace, scx: &ScalarCtx, decl: FuncRef) -> Self {
        Self::for_boundary_instance(ws, scx, &crate::instance::Instance::base(decl))
    }

    pub fn for_function_value(
        ws: &Workspace,
        scx: &ScalarCtx,
        ty: &crate::monoty::MonoTy,
    ) -> Result<FunctionRepPlan, RepError> {
        scx.rep_arena().function_value(ws, scx.target, ty)
    }

    pub fn for_function_value_with_mode(
        ws: &Workspace,
        scx: &ScalarCtx,
        ty: &crate::monoty::MonoTy,
        nobox_only: bool,
    ) -> Result<FunctionRepPlan, RepError> {
        scx.rep_arena().function_value_with_mode(ws, scx.target, ty, nobox_only)
    }

    fn assemble(
        ws: &Workspace,
        scx: &ScalarCtx,
        subject: crate::instance::Instance,
        subst: &SpecSubst,
    ) -> Self {
        let decl = subject.func;
        let f = &ws.module(decl.module).functions[decl.func];
        let arena = scx.rep_arena();
        let mode = abi_mode_spec(ws, scx, decl, subst);
        let representation = arena
            .function_from_declaration(ws, scx.target, f, subst, mode)
            .expect("declaration ABI planning must accept a checked signature");
        Self {
            subject,
            target: scx.target,
            representation,
            arena,
        }
    }

    pub fn decl(&self) -> FuncRef {
        self.subject.func
    }

    /// The scalarized wire, or `None` for the boxed ABI.
    pub(super) fn uses_scalar_abi(&self) -> bool {
        self.representation.uses_scalar_abi()
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
        if !self.uses_scalar_abi() {
            return Vec::new();
        }
        self.representation
            .params()
            .iter()
            .map(|id| self.arena.ret_slot(*id))
            .collect()
    }

    /// The per-declared-param wire slots, or `None` under the boxed ABI --
    /// distinct from [`Self::param_shapes`], which flattens the distinction
    /// away for the lowering path that treats both as "no splatting".
    pub(super) fn ret_shape(&self) -> Option<ValueShape> {
        self.representation.result().and_then(|id| self.arena.value_shape(id))
    }

    /// The declared return's tuple shape under this body's substitution.
    pub(crate) fn ret_tuple(&self) -> Option<TupleRetShape> {
        self.representation.result().and_then(|id| self.arena.tuple_shape(id))
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
        if !self.uses_scalar_abi() {
            return None;
        }
        let shapes = self.param_shapes();
        let f = &ws.module(self.decl().module).functions[self.decl().func];
        let mut names = Vec::new();
        for (pd, slot) in f.param_decls.iter().zip(shapes.iter()) {
            slot_param_names(pd.name.as_str(), slot, &mut names);
        }
        Some(names)
    }

    /// Per-DECLARED-param leaf width (a value-struct param's flattened leaf
    /// count, 1 otherwise). Per declared param rather than per slot so a
    /// defaulted value-struct param counts as its whole leaf group.
    pub fn param_leaf_widths(&self) -> Option<Vec<usize>> {
        self.uses_scalar_abi()
            .then(|| self.param_shapes().iter().map(RetSlot::slot_count).collect())
    }
}
