//! The graph resolves inference and substitutions before building these facts;
//! lowering never receives the substitution that produced them.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bone_core::hir::{BindingInfo, Expr};
use bone_core::prelude::ModuleId;
use comfy_la_arena::{ArenaMap, Idx};
use rustc_hash::{FxHashMap, FxHashSet};
use ustr::Ustr;

use crate::bound::ResolvedBoundTree;
use crate::instance::Instance;
use crate::monoty::MonoTyId;
use crate::site::{Site, SiteDomain};

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ConcreteOwner {
    Function(Instance),
    Global { module: ModuleId, index: u32 },
    Decorator { module: ModuleId, index: u32 },
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct ConcreteTypeTables {
    pub expr_types: ArenaMap<Idx<Expr>, MonoTyId>,
    pub binding_types: ArenaMap<Idx<BindingInfo>, MonoTyId>,
    /// Deliberately untyped slots: a planned untyped operation, not an implicit
    /// boxed value or a source-type fallback.
    pub untyped_exprs: FxHashSet<Idx<Expr>>,
    pub untyped_bindings: FxHashSet<Idx<BindingInfo>>,
    pub ty_types: FxHashMap<bone_core::prelude::TyId, MonoTyId>,
    /// The field declaration lives outside the body's inference arena, so this
    /// cannot be recovered from `ty_types`.
    pub struct_field_types: ArenaMap<Idx<Expr>, FxHashMap<Ustr, MonoTyId>>,
}

impl ConcreteTypeTables {
    pub(crate) fn expr_type(&self, expr: Idx<Expr>) -> Option<MonoTyId> {
        self.expr_types.get(expr).copied()
    }

    pub(crate) fn binding_type(&self, binding: Idx<BindingInfo>) -> Option<MonoTyId> {
        self.binding_types.get(binding).copied()
    }

    pub fn ty_type(&self, ty: bone_core::prelude::TyId) -> Option<MonoTyId> {
        self.ty_types.get(&ty).copied()
    }

    pub fn is_untyped_expr(&self, expr: Idx<Expr>) -> bool {
        self.untyped_exprs.contains(&expr)
    }

    pub(crate) fn is_untyped_binding(&self, binding: Idx<BindingInfo>) -> bool {
        self.untyped_bindings.contains(&binding)
    }

    pub(crate) fn struct_field_type(&self, literal: Idx<Expr>, field: Ustr) -> Option<MonoTyId> {
        self.struct_field_types.get(literal)?.get(&field).copied()
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct ConcreteSignature {
    pub params: Vec<MonoTyId>,
    /// A concrete tuple bound to a generic parameter stays one boxed boundary slot.
    pub param_tuple_spelled: Vec<bool>,
    pub result: Option<MonoTyId>,
}

pub type ConcreteSignatureHandle = Arc<ConcreteSignature>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConcreteCallee {
    pub instance: Instance,
    pub signature: ConcreteSignatureHandle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConcreteBodyPlan {
    pub owner: ConcreteOwner,
    /// Empty for global and decorator plans.
    pub code_identity: Option<crate::code_identity::CodeIdentity>,
    pub types: ConcreteTypeTables,
    /// Struct-default domains, keyed because each has a distinct HIR arena.
    pub foreign_domains: BTreeMap<SiteDomain, ConcreteTypeTables>,
    pub signature: ConcreteSignatureHandle,
    pub callees: BTreeMap<Site, ConcreteCallee>,
    /// Visited but unresolved, distinct from absent, so lowering reports a planned
    /// failure at the site.
    pub failed_sites: BTreeMap<Site, &'static str>,
    pub bound_tree: ResolvedBoundTree,
}

/// Ordered for deterministic iteration; replacing one body only swaps its handle.
pub type ConcretePlans = BTreeMap<Instance, Arc<ConcreteBodyPlan>>;
pub type ConcreteAlwaysPlans = FxHashMap<ConcreteOwner, ConcreteBodyPlan>;

/// Keeps each selected body's handle, so the publisher can replace its own entry
/// without mutating a consumer's selection.
pub(crate) fn select_concrete_plans<'a>(
    plans: &ConcretePlans,
    instances: impl IntoIterator<Item = &'a Instance>,
) -> ConcretePlans {
    instances
        .into_iter()
        .filter_map(|instance| {
            plans
                .get(instance)
                .map(|plan| (instance.clone(), Arc::clone(plan)))
        })
        .collect()
}

/// Includes resolved Rock callees the target graph classifies as inline/native
/// operators.
pub fn select_concrete_plan_closure<'a>(
    plans: &ConcretePlans,
    roots: impl IntoIterator<Item = &'a Instance>,
) -> ConcretePlans {
    let mut selected = BTreeSet::new();
    let mut queue: Vec<_> = roots
        .into_iter()
        .filter(|instance| selected.insert((*instance).clone()))
        .cloned()
        .collect();
    while let Some(instance) = queue.pop() {
        let Some(plan) = plans.get(&instance) else {
            continue;
        };
        for callee in plan.callees.values() {
            if selected.insert(callee.instance.clone()) {
                queue.push(callee.instance.clone());
            }
        }
    }
    select_concrete_plans(plans, selected.iter())
}

impl ConcreteBodyPlan {
    pub fn function_instance(&self) -> Option<&Instance> {
        match &self.owner {
            ConcreteOwner::Function(instance) => Some(instance),
            ConcreteOwner::Global { .. } | ConcreteOwner::Decorator { .. } => None,
        }
    }

    pub(crate) fn domain_types(&self, domain: &SiteDomain) -> Option<&ConcreteTypeTables> {
        if matches!(domain, SiteDomain::Own) {
            Some(&self.types)
        } else {
            self.foreign_domains.get(domain)
        }
    }
}
