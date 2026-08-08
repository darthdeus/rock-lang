//! **The backend ABI of one emitted body** (plan S 13, cut C+D step A\*).
//!
//! Every question about how a function's values cross the wire -- which params
//! splat to leaves, what the return carries, how many lanes a JS result
//! publishes, what an `inline(lua)` body's params are spelled -- is answered
//! here, for one [`EmittedBodyId`] under one execution target, by one
//! [`BackendFnAbiPlan`].
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
    slot_param_names, spec_subst_from_key, tuple_ret_shape, RetSlot, ScalarCtx, SpecKey, SpecSubst,
    TupleRetShape, ValueShape,
};
use crate::mir::FuncRef;

/// **Which definition is being emitted.**
///
/// Pre-cutover, emission writes one shared body per declaration plus a
/// specialized body per instantiation the lowerer decided to serve
/// (`emit_specialized_callees` on luajit, `spec_queue` on JS). Those two
/// populations are the emitted bodies, and each has its own ABI: `ident<T>` is
/// one boxed slot on the declaration and two float leaves at `T = Vec2`.
///
/// This is not yet [`crate::monoty::MonoSubst`]-keyed identity, and cut C+D
/// step E1 is where it becomes `Instance` and both variants go.
///
/// **The stated obstacle is spent.** It was that a declined specialization
/// still records an instance, so enumerating instances would emit every
/// declined body as well -- but E1's `compute_spec_from_bindings` no longer
/// declines: "EVERY complete key specializes. There is no shared generic body
/// to decline to any more." The two populations have already converged, and the
/// key and the substitution are one fact
/// (`tests/spec_key_subst_roundtrip.rs`). What remains is mechanical: the
/// chunk cache, symbols, callers and both ABI plans move together, across all
/// three re-emit paths.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum EmittedBodyId {
    /// The declaration's own body: non-generic, or the shared generic snapshot.
    Shared(FuncRef),
    /// A specialized body, keyed by the instantiation the lowerer serves it for.
    Specialized(FuncRef, SpecKey),
}

impl EmittedBodyId {
    /// The body a `key` names. An all-`None` key is `Shared`, which is the same
    /// rule `mangle::spec_fn_name` states by declining to mint a name for one:
    /// nothing is reified, so there is no second definition to target.
    pub fn new(decl: FuncRef, key: SpecKey) -> Self {
        if key.iter().all(Option::is_none) {
            Self::Shared(decl)
        } else {
            Self::Specialized(decl, key)
        }
    }

    pub fn decl(&self) -> FuncRef {
        match self {
            Self::Shared(fr) | Self::Specialized(fr, _) => *fr,
        }
    }

    pub fn key(&self) -> Option<&SpecKey> {
        match self {
            Self::Shared(_) => None,
            Self::Specialized(_, k) => Some(k),
        }
    }

    /// The substitution this body lowers under.
    ///
    /// BOTH directions are total for a body emission writes -- `spec_subst_from_key`
    /// and `spec_key_from_subst` round-trip, over every shape a key takes,
    /// tuples included (`tests/spec_key_subst_roundtrip.rs`, mutation-verified
    /// on the positional half). The declaration supplies the ORDER, which is the
    /// only thing a substitution lacks.
    ///
    /// The old claim that a key holds `None` for a tuple was stale from D-2,
    /// which gave `MonoTy` a `Tuple` constructor; and since E1 a PARTIAL key
    /// names no body at all, so the other reason for keeping the two apart
    /// describes nothing emission emits.
    pub fn subst(&self, ws: &Workspace) -> SpecSubst {
        match self {
            Self::Shared(_) => SpecSubst::default(),
            Self::Specialized(fr, key) => spec_subst_from_key(ws, *fr, key),
        }
    }
}

/// What a [`BackendFnAbiPlan`] answers for. Two populations, named, because
/// they are two populations until E1: emission writes bodies, and the mono
/// graph reaches logical instances, and a declined specialization is an
/// instance with no emitted body of its own.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AbiSubject {
    /// A definition emission writes.
    Emitted(EmittedBodyId),
    /// A logical instance of the mono graph (`skull::artifact`), which has no
    /// emitted body of its own before E1.
    Instance(FuncRef),
}

impl AbiSubject {
    pub fn decl(&self) -> FuncRef {
        match self {
            Self::Emitted(id) => id.decl(),
            Self::Instance(fr) => *fr,
        }
    }
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
    pub subject: AbiSubject,
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

impl BackendFnAbiPlan {
    /// THE emission-side constructor: one plan per (execution target, emitted body).
    pub fn for_body(ws: &Workspace, scx: &ScalarCtx, body: EmittedBodyId) -> Self {
        let subst = body.subst(ws);
        Self::assemble(ws, scx, AbiSubject::Emitted(body), &subst)
    }

    /// [`Self::for_body`] for a caller that already resolved the substitution.
    /// A body's lowering and its call sites each hold both, so re-deriving one
    /// from the other would be work with no second opinion in it.
    ///
    /// The id would otherwise be DECORATIVE here -- the ABI comes from `subst`,
    /// so an id naming another instantiation would ride along unnoticed. The
    /// debug assertion is what makes the two a checked agreement rather than a
    /// convention; it is off in release, where the compile path pays nothing.
    pub(super) fn for_subject(
        ws: &Workspace,
        scx: &ScalarCtx,
        subject: AbiSubject,
        subst: &SpecSubst,
    ) -> Self {
        if let AbiSubject::Emitted(body) = &subject {
            debug_assert_eq!(
                &body.subst(ws),
                subst,
                "an emitted body's id and the substitution it lowers under must be one instantiation",
            );
        }
        Self::assemble(ws, scx, subject, subst)
    }

    /// The shared body's plan -- the declaration's own emitted definition.
    pub fn for_declaration(ws: &Workspace, scx: &ScalarCtx, decl: FuncRef) -> Self {
        Self::for_body(ws, scx, EmittedBodyId::Shared(decl))
    }

    /// The LOGICAL-instance route (`skull::artifact`), keyed by a mono
    /// instance's own substitution. A different population from the emitted
    /// bodies above until E1 collapses the two.
    pub fn for_instance(
        ws: &Workspace,
        scx: &ScalarCtx,
        decl: FuncRef,
        subst: &SpecSubst,
    ) -> Self {
        Self::assemble(ws, scx, AbiSubject::Instance(decl), subst)
    }

    fn assemble(
        ws: &Workspace,
        scx: &ScalarCtx,
        subject: AbiSubject,
        subst: &SpecSubst,
    ) -> Self {
        let decl = subject.decl();
        let f = &ws.module(decl.module).functions[decl.func];
        let scalar = fn_scalar_abi_spec(ws, scx, decl, subst);
        let ret_tuple = f.return_ty.and_then(|ty| tuple_ret_shape(ws, subst, ty));
        Self { subject, target: scx.target, scalar, ret_tuple, has_return: f.return_ty.is_some() }
    }

    pub fn decl(&self) -> FuncRef {
        self.subject.decl()
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
