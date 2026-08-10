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

use bone_semantic::hir::FnBackend;
use bone_semantic::inference::Workspace;

use super::{
    nobox_declared_shape_spec, nobox_param_slot_spec, sig_param_slot_spec, sig_value_shape_spec,
    mono_ret_slot, mono_sig_param_slot, mono_tuple_ret_shape,
    mono_value_shape, slot_param_names,
    tuple_ret_shape, RetSlot, ScalarCtx, SpecSubst, TupleRetShape, ValueShape,
};
use crate::concrete::ConcreteBodyPlan;
use crate::mir::FuncRef;

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
        .then_some(FnScalarAbi { param_shapes, ret_shape })
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
        .then_some(FnScalarAbi { param_shapes, ret_shape })
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
}

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
    let param_shapes: Vec<RetSlot> = if nobox_only {
        params.iter().map(|ty| mono_ret_slot(ws, ty)).collect()
    } else {
        params.iter().map(|ty| mono_sig_param_slot(ws, ty)).collect()
    };
    let ret_shape = plan
        .signature
        .result
        .map(|id| scx.interner().get(id))
        .and_then(|ty| mono_value_shape(ws, &ty));
    (ret_shape.is_some() || param_shapes.iter().any(RetSlot::splats))
        .then_some(FnScalarAbi { param_shapes, ret_shape })
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
        Self {
            subject: inst.clone(),
            target: scx.target,
            scalar,
            ret_tuple,
            has_return: plan.signature.result.is_some(),
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
        Self { subject, target: scx.target, scalar, ret_tuple, has_return: f.return_ty.is_some() }
    }

    pub fn decl(&self) -> FuncRef {
        self.subject.func
    }

    /// The scalarized wire, or `None` for the boxed ABI.
    pub(super) fn scalar(&self) -> Option<&FnScalarAbi> {
        self.scalar.as_ref()
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
        if !self.has_return {
            return 0;
        }
        match &self.ret_tuple {
            Some(shape) => shape.slot_count(),
            None => match self.ret_shape() {
                Some(vs) => vs.leaf_count(),
                None => 1,
            },
        }
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
