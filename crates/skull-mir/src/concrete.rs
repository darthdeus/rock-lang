//! Concrete, instance-owned facts handed to MIR lowering.
//!
//! The graph resolves inference and substitutions before it constructs these
//! values. Lowering may inspect the facts, but it does not receive the
//! substitution that produced them.

use std::collections::BTreeMap;
use std::sync::Arc;

use bone_semantic::hir::{BindingInfo, Expr};
use bone_semantic::prelude::ModuleId;
use comfy_la_arena::{ArenaMap, Idx};
use rustc_hash::{FxHashMap, FxHashSet};
use ustr::Ustr;

use crate::bound::ResolvedBoundTree;
use crate::instance::Instance;
use crate::monoty::MonoTyId;
use crate::site::{Site, SiteDomain};

/// The owner of one concrete body contribution.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ConcreteOwner {
    /// An ordinary Rock function at one concrete instance.
    Function(Instance),
    /// An always-emitted global initializer.
    Global { module: ModuleId, index: u32 },
    /// An always-emitted decorator body.
    Decorator { module: ModuleId, index: u32 },
}

/// Concrete types for one expression/binding arena.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct ConcreteTypeTables {
    pub expr_types: ArenaMap<Idx<Expr>, MonoTyId>,
    pub binding_types: ArenaMap<Idx<BindingInfo>, MonoTyId>,
    /// Slots that inference deliberately left without a concrete answer.
    /// Their absence is a planned untyped operation, not an implicit boxed
    /// value or a source-type fallback.
    pub untyped_exprs: FxHashSet<Idx<Expr>>,
    pub untyped_bindings: FxHashSet<Idx<BindingInfo>>,
    /// Inference TyIds are the bridge for declaration-shaped helpers that are
    /// being migrated. The expression and binding maps remain the primary
    /// ownership; this index prevents a second substitution lookup while a
    /// query starts from one of those concrete slots.
    pub ty_types: FxHashMap<bone_semantic::prelude::TyId, MonoTyId>,
    /// Declared field types for each concrete value-struct literal. The field
    /// declaration lives outside the body's inference arena, so its concrete
    /// type cannot be recovered from `ty_types` alone.
    pub struct_field_types: ArenaMap<Idx<Expr>, FxHashMap<Ustr, MonoTyId>>,
}

impl ConcreteTypeTables {
    pub fn expr_type(&self, expr: Idx<Expr>) -> Option<MonoTyId> {
        self.expr_types.get(expr).copied()
    }

    pub fn binding_type(&self, binding: Idx<BindingInfo>) -> Option<MonoTyId> {
        self.binding_types.get(binding).copied()
    }

    pub fn ty_type(&self, ty: bone_semantic::prelude::TyId) -> Option<MonoTyId> {
        self.ty_types.get(&ty).copied()
    }

    pub fn is_untyped_expr(&self, expr: Idx<Expr>) -> bool {
        self.untyped_exprs.contains(&expr)
    }

    pub fn is_untyped_binding(&self, binding: Idx<BindingInfo>) -> bool {
        self.untyped_bindings.contains(&binding)
    }

    pub fn struct_field_type(&self, literal: Idx<Expr>, field: Ustr) -> Option<MonoTyId> {
        self.struct_field_types.get(literal)?.get(&field).copied()
    }
}

/// Concrete parameter/result signature for one lowered subject.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct ConcreteSignature {
    pub params: Vec<MonoTyId>,
    /// Whether each declared parameter was written as a tuple (including a
    /// nullable tuple). A concrete tuple bound to a generic parameter remains
    /// one boxed boundary slot; its reified children do not change that source
    /// ABI choice.
    pub param_tuple_spelled: Vec<bool>,
    pub result: Option<MonoTyId>,
}

pub type ConcreteSignatureHandle = Arc<ConcreteSignature>;

/// All concrete facts MIR lowering needs for one body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConcreteBodyPlan {
    pub owner: ConcreteOwner,
    /// The logical identity the concrete lowering belongs to. Function plans
    /// carry their mono instance; global/decorator plans leave this empty.
    pub code_identity: Option<crate::code_identity::CodeIdentity>,
    /// The owner's own expression and binding arena.
    pub types: ConcreteTypeTables,
    /// Struct-default domains entered while lowering this owner. The key is
    /// part of the plan because each domain has a distinct HIR arena.
    pub foreign_domains: BTreeMap<SiteDomain, ConcreteTypeTables>,
    pub signature: ConcreteSignatureHandle,
    /// Every resolved site is owned by its concrete callee instance.
    pub callees: BTreeMap<Site, Instance>,
    /// Concrete signatures for the same resolved sites, including boundaries
    /// whose body is supplied by the target rather than lowered here.
    pub callee_signatures: BTreeMap<Site, ConcreteSignatureHandle>,
    /// Sites the graph visited but could not resolve. Keeping these distinct
    /// from absent entries lets lowering report a planned failure at the site.
    pub failed_sites: BTreeMap<Site, &'static str>,
    pub bound_tree: ResolvedBoundTree,
}

/// Stable per-instance plan handles. The table is still ordered for
/// deterministic iteration, while replacing one body only swaps its handle.
pub type ConcretePlans = BTreeMap<Instance, Arc<ConcreteBodyPlan>>;
pub type ConcreteAlwaysPlans = FxHashMap<ConcreteOwner, ConcreteBodyPlan>;

impl ConcreteBodyPlan {
    pub fn function_instance(&self) -> Option<&Instance> {
        match &self.owner {
            ConcreteOwner::Function(instance) => Some(instance),
            ConcreteOwner::Global { .. } | ConcreteOwner::Decorator { .. } => None,
        }
    }

    pub fn domain_types(&self, domain: &SiteDomain) -> Option<&ConcreteTypeTables> {
        if matches!(domain, SiteDomain::Own) {
            Some(&self.types)
        } else {
            self.foreign_domains.get(domain)
        }
    }
}
