//! The graph resolves inference and substitutions before building these facts;
//! lowering never receives the substitution that produced them.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bone_core::hir::{BindingInfo, Body, BodyOwner, Expr, TypeExpr};
use bone_core::prelude::{ModuleId, TyId};
use bone_core::workspace::{BodyInference, Workspace};
use comfy_la_arena::{ArenaMap, Idx};
use rustc_hash::FxHashMap;
use ustr::Ustr;

use crate::bound::ResolvedBoundTree;
use crate::instance::Instance;
use crate::monoty::{MonoCtor, MonoInterner, MonoSubst, MonoTy, MonoTyId, MonoTyReason, TyEnv};
use crate::site::{Site, SiteDomain};

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ConcreteOwner {
    Function(Instance),
    Global { module: ModuleId, index: u32 },
    Decorator { module: ModuleId, index: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TypeRow {
    Concrete(MonoTyId),
    Untyped,
    Unresolved,
}

impl TypeRow {
    fn concrete(self) -> Option<MonoTyId> {
        match self {
            Self::Concrete(id) => Some(id),
            Self::Untyped | Self::Unresolved => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TypeSource {
    owner: (ModuleId, BodyOwner),
    generation: u64,
    arena: u64,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct ConcreteTypeTables {
    source: Option<TypeSource>,
    missing_required: Option<TyId>,
    expr_types: ArenaMap<Idx<Expr>, TypeRow>,
    binding_types: ArenaMap<Idx<BindingInfo>, TypeRow>,
    ty_types: FxHashMap<TyId, TypeRow>,
    struct_field_types: ArenaMap<Idx<Expr>, FxHashMap<Ustr, MonoTyId>>,
}

impl ConcreteTypeTables {
    pub fn publish(
        ws: &Workspace,
        arena: &MonoInterner,
        owner: (ModuleId, BodyOwner),
        (inf, body): (&BodyInference, &Body),
        subst: &MonoSubst,
        declared_types: impl IntoIterator<Item = TyId>,
    ) -> Result<Self, MonoTyReason> {
        let mut memo = FxHashMap::default();
        let mut convert = |ty| {
            memo.entry(ty)
                .or_insert_with(|| match arena.convert(ws, inf, ty, subst) {
                    Ok(id) => Ok(TypeRow::Concrete(id)),
                    Err(MonoTyReason::UnresolvedInfer(_)) => Ok(TypeRow::Unresolved),
                    Err(reason) => Err(reason),
                })
                .clone()
        };
        let expr_types = site_types(
            body.expr_arena.iter().map(|(expr, _)| expr),
            &inf.expr_types,
            &mut convert,
        )?;
        let binding_types = site_types(
            body.binding_arena.iter().map(|(binding, _)| binding),
            &inf.binding_types,
            &mut convert,
        )?;
        let inferred = || {
            inf.expr_types
                .iter()
                .map(|(expr, ty)| (*ty, expr_types[expr].concrete().is_some()))
                .chain(
                    inf.binding_types
                        .iter()
                        .map(|(binding, ty)| (*ty, binding_types[binding].concrete().is_some())),
                )
        };
        let mandatory = inf
            .fn_field_call_types
            .values()
            .copied()
            .chain(declared_types)
            .chain(
                body.binding_arena
                    .iter()
                    .filter_map(|(_, binding)| binding.declared_ty),
            )
            .chain(body.expr_arena.iter().filter_map(|(_, expr)| match expr {
                Expr::As { target, .. } => Some(*target),
                _ => None,
            }));
        let mut ty_types = FxHashMap::default();
        let mut missing_required = None;
        let mut visit = |ty, required| {
            visit_type(ws, inf, ty, &mut ty_types, &mut |ty| {
                let row = convert(ty)?;
                if required && row == TypeRow::Unresolved {
                    missing_required.get_or_insert(ty);
                }
                Ok(row)
            })
        };
        for ty in inferred()
            .filter_map(|(ty, known)| known.then_some(ty))
            .chain(mandatory)
        {
            visit(ty, true)?;
        }
        for (ty, known) in inferred() {
            if !known {
                visit(ty, false)?;
            }
        }
        let struct_field_types = concrete_struct_field_types(ws, arena, body, &expr_types)?;
        Ok(Self {
            source: Some(TypeSource {
                owner,
                generation: inf.body_generation,
                arena: arena.generation(),
            }),
            missing_required,
            expr_types,
            binding_types,
            ty_types,
            struct_field_types,
        })
    }

    pub(crate) fn check_source(
        &self,
        arena: &MonoInterner,
        owner: (ModuleId, BodyOwner),
        body: &Body,
        label: &str,
    ) -> Result<(), String> {
        let source = self
            .source
            .ok_or_else(|| format!("{label} is missing concrete type tables"))?;
        if source.arena != arena.generation()
            || source.owner != owner
            || source.generation != body.generation
        {
            return Err(format!(
                "{label} has stale concrete type tables for {owner:?}"
            ));
        }
        if let Some(ty) = self.missing_required {
            return Err(format!("{label} is missing concrete type node {ty:?}"));
        }
        Ok(())
    }

    pub(crate) fn expr_type(&self, expr: Idx<Expr>) -> Option<MonoTyId> {
        self.expr_types.get(expr)?.concrete()
    }

    pub fn binding_type(&self, binding: Idx<BindingInfo>) -> Option<MonoTyId> {
        self.binding_types.get(binding)?.concrete()
    }

    pub fn ty_type(&self, ty: TyId) -> Option<MonoTyId> {
        self.ty_types.get(&ty)?.concrete()
    }

    pub fn is_untyped_expr(&self, expr: Idx<Expr>) -> bool {
        matches!(
            self.expr_types.get(expr),
            Some(TypeRow::Untyped | TypeRow::Unresolved)
        )
    }

    pub(crate) fn struct_field_type(&self, literal: Idx<Expr>, field: Ustr) -> Option<MonoTyId> {
        self.struct_field_types.get(literal)?.get(&field).copied()
    }
}

fn site_types<T>(
    sites: impl Iterator<Item = Idx<T>>,
    inferred: &ArenaMap<Idx<T>, TyId>,
    convert: &mut impl FnMut(TyId) -> Result<TypeRow, MonoTyReason>,
) -> Result<ArenaMap<Idx<T>, TypeRow>, MonoTyReason> {
    sites
        .map(|site| {
            let row = match inferred.get(site) {
                Some(&ty) => convert(ty)?,
                None => TypeRow::Untyped,
            };
            Ok((site, row))
        })
        .collect()
}

fn visit_type(
    ws: &Workspace,
    inf: &BodyInference,
    ty: TyId,
    rows: &mut FxHashMap<TyId, TypeRow>,
    convert: &mut impl FnMut(TyId) -> Result<TypeRow, MonoTyReason>,
) -> Result<(), MonoTyReason> {
    use std::collections::hash_map::Entry;
    let Entry::Vacant(row) = rows.entry(ty) else {
        return Ok(());
    };
    row.insert(convert(ty)?);
    let node = match ty {
        TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
        TyId::Local(idx) => inf.ty_arena[idx],
    };
    if let TypeExpr::Infer(var, _) = node {
        if let Some(next) = inf.substitutions.get(var) {
            visit_type(ws, inf, next, rows, convert)?;
        }
    } else if let Some(shape) = node.shape() {
        for child in shape.params.chain(shape.tail) {
            visit_type(ws, inf, ty.with_idx(child), rows, convert)?;
        }
    }
    Ok(())
}

fn concrete_struct_field_types(
    ws: &Workspace,
    arena: &MonoInterner,
    body: &Body,
    expr_types: &ArenaMap<Idx<Expr>, TypeRow>,
) -> Result<ArenaMap<Idx<Expr>, FxHashMap<Ustr, MonoTyId>>, MonoTyReason> {
    let mut out = ArenaMap::default();
    for (expr, node) in body.expr_arena.iter() {
        let Expr::StructLiteral { name, .. } = node else {
            continue;
        };
        let sid = ws.nominals.struct_id(*name).ok_or(MonoTyReason::NoNode)?;
        let def = &ws.modules[sid.module.index()].structs[sid.def];
        let params = match expr_types.get(expr).and_then(|row| row.concrete()) {
            Some(ty_id) => {
                let concrete = arena.get_ref(ty_id);
                let (payload, _) = concrete.peel_nullable();
                let MonoTy::App {
                    ctor: MonoCtor::Nominal(actual),
                    params,
                } = payload
                else {
                    return Err(MonoTyReason::NoNode);
                };
                if *actual != *name || def.generics.len() != params.len() {
                    return Err(MonoTyReason::NoNode);
                }
                params.to_vec()
            }
            None if def.generics.is_empty() => Vec::new(),
            None => return Err(MonoTyReason::NoNode),
        };
        let mut subst = MonoSubst::default();
        for (generic, param) in def.generics.iter().zip(params) {
            let TypeExpr::Generic(var, _) = TyEnv::Decl.decl_node(ws, *generic)? else {
                return Err(MonoTyReason::NoNode);
            };
            subst.insert(var, param.clone());
        }
        let fields = def
            .fields
            .iter()
            .map(|field| {
                arena
                    .convert_decl(ws, field.ty, &subst)
                    .map(|ty| (field.name, ty))
            })
            .collect::<Result<FxHashMap<_, _>, _>>()?;
        out.insert(expr, fields);
    }
    Ok(out)
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
