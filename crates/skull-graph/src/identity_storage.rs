//! Identity storage only an instance's substitution shows. `fn stash<T>(x: T) { let v = [x] }`
//! stores nothing bone can see; `stash<Unit>` stores an identity struct in a
//! Vec. The body walk is bone's own ([`identity_body_sites_with`]), reading
//! each slot's type under the instance's substitution, and the sites bone
//! already reports for the unsubstituted body are left to bone.

use bone_core::hir::{Body, Function};
use bone_core::identity_storage::{
    identity_body_sites, identity_body_sites_with, identity_violation_in, BodySlotTypes,
    IdentityNode, IdentityPosition, IdentityStorageKind, IdentityStorageSite,
    IdentityStorageViolation, IdentityTypes,
};
use bone_core::prelude::TyId;
use bone_core::workspace::{BodyInference, Workspace};
use rustc_hash::FxHashSet;
use skull_mir::monoty::{MonoCtor, MonoSubst, MonoTy};

#[derive(Copy, Clone)]
struct MonoTypes<'a>(std::marker::PhantomData<&'a MonoTy>);

impl<'a> IdentityTypes for MonoTypes<'a> {
    type Ty = &'a MonoTy;
    type Children = std::slice::Iter<'a, MonoTy>;
    fn node(
        self,
        ty: &'a MonoTy,
    ) -> Option<IdentityNode<&'a MonoTy, std::slice::Iter<'a, MonoTy>>> {
        Some(match ty {
            MonoTy::Nullable(inner) => IdentityNode::Nullable(inner),
            MonoTy::Tuple(parts) => IdentityNode::Tuple(parts.iter()),
            MonoTy::App {
                ctor: MonoCtor::Array | MonoCtor::Map,
                params,
            } => IdentityNode::Collection(params.iter()),
            MonoTy::App {
                ctor: MonoCtor::Nominal(name),
                params,
            } => IdentityNode::Nominal(*name, params.iter()),
            _ => IdentityNode::Opaque,
        })
    }
}

/// Whether storing a value of `ty` stores an identity. A type argument for
/// which this is false cannot make any position of the body store one, so it
/// is the cheap gate before a body is re-read under its substitution.
pub fn stores_identity(ws: &Workspace, ty: &MonoTy) -> bool {
    identity_violation_in(
        ws,
        MonoTypes(std::marker::PhantomData),
        ty,
        IdentityPosition::Stored,
    )
    .is_some()
}

struct SubstitutedSlotTypes<'a> {
    ws: &'a Workspace,
    bi: &'a BodyInference,
    subst: &'a MonoSubst,
}

impl SubstitutedSlotTypes<'_> {
    // A slot that does not convert is not judged here: the concrete build
    // reports it as a `FreeType` or plan failure.
    fn check(&self, ty: TyId, position: IdentityPosition) -> Option<IdentityStorageViolation> {
        let mono = skull_mir::monoty::convert(self.ws, self.bi, ty, self.subst).ok()?;
        identity_violation_in(
            self.ws,
            MonoTypes(std::marker::PhantomData),
            &mono,
            position,
        )
    }
}

impl BodySlotTypes for SubstitutedSlotTypes<'_> {
    fn local(&self, ty: TyId) -> Option<IdentityStorageViolation> {
        self.check(ty, IdentityPosition::Borrow)
    }
    fn stored(&self, ty: TyId) -> Option<IdentityStorageViolation> {
        self.check(ty, IdentityPosition::Stored)
    }
    fn row(&self, ty: TyId) -> Option<IdentityStorageViolation> {
        self.check(ty, IdentityPosition::Row)
    }
}

/// The identity-storage sites of one instance's body that its substitution
/// adds to what bone reports for the declared body.
pub(crate) fn substitution_only_sites(
    ws: &Workspace,
    function: &Function,
    body: &Body,
    bi: &BodyInference,
    subst: &MonoSubst,
) -> Vec<IdentityStorageSite> {
    if !subst.values().any(|ty| stores_identity(ws, ty)) {
        return Vec::new();
    }
    let types = SubstitutedSlotTypes { ws, bi, subst };
    let substituted = identity_body_sites_with(ws, function, body, bi, &types);
    if substituted.is_empty() {
        return substituted;
    }
    let key = |site: &IdentityStorageSite| (site.kind, site.span.file, site.span.offset_range);
    let mut skip: FxHashSet<_> = identity_body_sites(ws, function, body, bi)
        .iter()
        .map(key)
        .collect();
    // A parameter holds what its caller already stored, and that storage is
    // reported where it was made. A `co fn` parameter is the frame's own.
    skip.extend(body.param_bindings.into_iter().map(|binding| {
        let span = body.binding_spans[binding];
        (
            IdentityStorageKind::LocalStorage,
            span.file,
            span.offset_range,
        )
    }));
    substituted
        .into_iter()
        .filter(|site| !skip.contains(&key(site)))
        .collect()
}
