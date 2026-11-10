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
use rustc_hash::{FxHashMap, FxHashSet};
use skull_mir::monoty::{MonoCtor, MonoSubst, MonoTy};
use std::cell::RefCell;

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

/// Verdicts already reached, by position and type. A verdict depends on the
/// type and the workspace's declarations only, and the bodies one thread
/// walks ask about the same few types.
#[derive(Default)]
pub(crate) struct IdentityVerdicts([FxHashMap<MonoTy, Option<IdentityStorageViolation>>; 3]);

impl IdentityVerdicts {
    fn of(
        &mut self,
        ws: &Workspace,
        ty: &MonoTy,
        position: IdentityPosition,
    ) -> Option<IdentityStorageViolation> {
        let known = &mut self.0[match position {
            IdentityPosition::Stored => 0,
            IdentityPosition::Borrow => 1,
            IdentityPosition::Row => 2,
        }];
        if let Some(verdict) = known.get(ty) {
            return *verdict;
        }
        let verdict = identity_violation_in(ws, MonoTypes(std::marker::PhantomData), ty, position);
        known.insert(ty.clone(), verdict);
        verdict
    }
}

struct SubstitutedSlotTypes<'a> {
    ws: &'a Workspace,
    bi: &'a BodyInference,
    subst: &'a MonoSubst,
    verdicts: &'a RefCell<IdentityVerdicts>,
}

impl SubstitutedSlotTypes<'_> {
    // A slot that does not convert is not judged here: the concrete build
    // reports it as a `FreeType` or plan failure.
    fn check(&self, ty: TyId, position: IdentityPosition) -> Option<IdentityStorageViolation> {
        let mono = skull_mir::monoty::convert(self.ws, self.bi, ty, self.subst).ok()?;
        self.verdicts.borrow_mut().of(self.ws, &mono, position)
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
    verdicts: &RefCell<IdentityVerdicts>,
) -> Vec<IdentityStorageSite> {
    let stores = |ty: &MonoTy| {
        verdicts
            .borrow_mut()
            .of(ws, ty, IdentityPosition::Stored)
            .is_some()
    };
    if !subst.values().any(stores) {
        return Vec::new();
    }
    let types = SubstitutedSlotTypes {
        ws,
        bi,
        subst,
        verdicts,
    };
    let mut sites = identity_body_sites_with(ws, function, body, bi, &types);
    if sites.is_empty() {
        return sites;
    }
    let key = |site: &IdentityStorageSite| (site.kind, site.span.file, site.span.offset_range());
    // A parameter holds what its caller already stored, and that storage is
    // reported where it was made. A `co fn` parameter is the frame's own.
    let params: FxHashSet<_> = body
        .param_bindings
        .into_iter()
        .map(|binding| {
            let span = body.binding_spans[binding];
            (
                IdentityStorageKind::LocalStorage,
                span.file,
                span.offset_range(),
            )
        })
        .collect();
    sites.retain(|site| !params.contains(&key(site)));
    // Most bodies store an identity only through a parameter, so the
    // declared body is read for the few that have a site left.
    if sites.is_empty() {
        return sites;
    }
    let declared: FxHashSet<_> = identity_body_sites(ws, function, body, bi)
        .iter()
        .map(key)
        .collect();
    sites.retain(|site| !declared.contains(&key(site)));
    sites
}
