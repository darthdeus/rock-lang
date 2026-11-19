//! Source ownership of a closed named type's declaration dependencies.
//!
//! This is a conservative source projection, not a runtime layout hash. The
//! host retains responsibility for hashing source contents and decoder code.

use crate::hir::{TypeCtor, TypeExpr, VariantPayload};
use crate::workspace::Workspace;
use std::collections::{BTreeMap, HashSet};
use ustr::Ustr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeSource {
    pub name: Ustr,
    pub file: Ustr,
}

/// Return declaration owners in canonical-name order. Roots must be closed
/// named types: a bare generic declaration cannot identify its caller's type
/// arguments. Application arguments and declaration fields are both visited,
/// so no concrete instantiation or inference arena mutation is necessary.
pub fn declaration_sources(ws: &Workspace, roots: &[Ustr]) -> Result<Vec<TypeSource>, String> {
    for root in roots {
        if ws.type_generics.get(root).map_or(0, |entry| entry.arity) != 0 {
            return Err(format!(
                "source provenance requires a closed named root: {root}"
            ));
        }
    }
    let mut names = roots.to_vec();
    let mut types = Vec::new();
    let mut seen_types = HashSet::new();
    let mut owners = BTreeMap::new();
    while !names.is_empty() || !types.is_empty() {
        if let Some(name) = names.pop() {
            if owners.contains_key(&name) {
                continue;
            }
            let module = if let Some(id) = ws.struct_by_name.get(&name) {
                let module = ws.module(id.module);
                types.extend(
                    module.structs[id.def]
                        .fields
                        .iter()
                        .map(|field| field.storage_ty),
                );
                module
            } else if let Some(id) = ws.enums_by_name.get(&name) {
                let module = ws.module(id.module);
                for variant in &module.enums[id.def].variants {
                    match &variant.payload {
                        VariantPayload::Unit => {}
                        VariantPayload::Tuple(fields) => types.extend(fields.iter().copied()),
                        VariantPayload::Record(fields) => {
                            types.extend(fields.iter().map(|field| field.storage_ty))
                        }
                    }
                }
                module
            } else if let Some(id) = ws.newtypes_by_name.get(&name) {
                let module = ws.module(id.module);
                types.push(module.newtypes[id.def].wrapped);
                module
            } else {
                return Err(format!("source provenance cannot find declaration {name}"));
            };
            owners.insert(
                name,
                TypeSource {
                    name,
                    file: module.file,
                },
            );
            continue;
        }
        let ty = types.pop().expect("pending type or declaration");
        if !seen_types.insert(ty) {
            continue;
        }
        let Some(expr) = ws.type_expr(None, ty) else {
            return Err("source provenance encountered a body-local type".to_string());
        };
        match expr {
            TypeExpr::Builtin(_) | TypeExpr::Generic(_, _) => {}
            TypeExpr::Infer(_, _) => {
                return Err("source provenance encountered an unresolved type".to_string());
            }
            TypeExpr::Nullable(inner) => types.push(ty.with_idx(inner)),
            TypeExpr::Tuple { elements } => {
                types.extend(elements.into_iter().map(|idx| ty.with_idx(idx)))
            }
            TypeExpr::Function { params, ret } => {
                types.extend(params.into_iter().map(|idx| ty.with_idx(idx)));
                types.push(ty.with_idx(ret));
            }
            TypeExpr::App { ctor, params } => {
                types.extend(params.into_iter().map(|idx| ty.with_idx(idx)));
                match ctor {
                    TypeCtor::Nominal(name) => names.push(name),
                    TypeCtor::Array => {
                        let name = Ustr::from("Vec");
                        if ws.struct_by_name.contains_key(&name) {
                            names.push(name);
                        }
                    }
                    // These containers have no source declaration. Their
                    // implementation belongs to the host's runtime/ABI inputs.
                    TypeCtor::Map | TypeCtor::Ref => {}
                }
            }
        }
    }
    Ok(owners.into_values().collect())
}
