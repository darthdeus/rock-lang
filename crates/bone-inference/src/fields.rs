use super::*;

pub(crate) fn resolve_struct(gamma: &Gamma, name: Ustr) -> Option<StructId> {
    gamma.dep_acc.borrow_mut().insert(name);
    gamma.workspace.struct_by_name.get(&name).copied()
}

pub(crate) fn struct_arg_subs(
    gamma: &Gamma,
    def: &StructDef,
    app_ty: TyId,
    params: IdxRange<TypeExpr>,
) -> FxHashMap<VarId, TyId> {
    let mut subs = FxHashMap::default();

    for (g, arg) in def.generics.iter().zip(params) {
        if let TypeExpr::Generic(var_id, _) = gamma.get(*g) {
            subs.insert(*var_id, app_ty.with_idx(arg));
        }
    }

    subs
}

pub(crate) fn resolve_enum(gamma: &Gamma, name: Ustr) -> Option<EnumId> {
    gamma.dep_acc.borrow_mut().insert(name);
    gamma.workspace.enums_by_name.get(&name).copied()
}

/// Every struct in the workspace defining `field` -- the reverse field
/// inference scan for an unresolved base (unique match pins the struct;
/// several matches are ambiguous, deferrable on a lambda-param base).
pub(crate) fn structs_with_field(gamma: &Gamma, field: Ustr) -> Vec<StructId> {
    let mut matches: Vec<StructId> = Vec::new();

    for m in gamma.workspace.modules.iter() {
        let mid = m.module_id();

        for (def, sdef) in m.structs.iter() {
            if sdef.fields.iter().any(|f| f.name == field) {
                matches.push(StructId { module: mid, def });
            }
        }
    }

    matches
}

// Resolve the type of `field` on a nominal struct, with generic subst.
// `base_ty` must be resolved and peeled nullable for safe nav.
pub(crate) fn struct_field_ty(
    gamma: &mut Gamma,
    base_ty: TyId,
    field: Ustr,
    span: Span,
) -> Result<TyId, CompilerError> {
    let (struct_name, params) = match *gamma.get(base_ty) {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } => {
            gamma.dep_acc.borrow_mut().insert(name);
            (name, params)
        }

        TypeExpr::App {
            ctor: TypeCtor::Array,
            params,
        } => (*VEC, params),

        TypeExpr::Infer(..) => {
            let matches = structs_with_field(gamma, field);

            let sid = match matches.as_slice() {
                [one] => *one,
                [] => comp_error!(span, "no struct has a field `{}`", field),
                _ => comp_error!(
                    span,
                    "ambiguous field `{}`: multiple structs define it; annotate the parameter",
                    field
                ),
            };

            let def = gamma.workspace.module(sid.module).structs[sid.def].clone();
            let (subs, fresh) = gamma.instantiate_generics(&def.generics, span)?;

            let struct_ty = gamma.nominal_app(def.name, &fresh, span);
            unify(gamma, base_ty, struct_ty, span)?;

            // `sid` was selected because it has this field (scan above), so the
            // re-find succeeds -- but return an error rather than unwrap, so a
            // future change to the scan degrades to a diagnostic, never a panic.
            let fty = def
                .fields
                .iter()
                .find(|f| f.name == field)
                .ok_or_else(|| {
                    CompilerError::new_with_label(
                        span,
                        format!("struct `{}` has no field `{}`", def.name, field),
                    )
                })?
                .ty;
            return substitute(gamma, fty, &subs, span);
        }

        _ => comp_error!(
            span,
            "cannot access field `{}` on non-struct type {}",
            field,
            gamma.workspace.render(Some(&gamma.out), base_ty)
        ),
    };

    let sid = resolve_struct(gamma, struct_name).ok_or_else(|| {
        CompilerError::new_with_label(span, format!("unknown struct `{}`", struct_name))
    })?;

    let def = &gamma.workspace.module(sid.module).structs[sid.def];
    let subs = struct_arg_subs(gamma, def, base_ty, params);
    let field_ty = def.fields.iter().find(|f| f.name == field).map(|f| f.ty);

    let Some(field_ty) = field_ty else {
        if let Some(res) = vec_swizzle_ty(gamma, struct_name, field, span) {
            return res;
        }
        return Err(CompilerError::new_with_label(
            span,
            format!("struct `{}` has no field `{}`", struct_name, field),
        ));
    };

    substitute(gamma, field_ty, &subs, span)
}

/// GLSL-style swizzle read on the float vec structs (docs/vec-swizzles-plan.md):
/// one alphabet (`xyzw` | `rgba`), every component within the receiver's arity,
/// at most 4 components. Result: len 1 -> float, len N -> VecN. Reached only on
/// a real-field miss, so len-1 `x/y/z/w` never arrive here (`r/g/b/a` do).
/// `None` = not swizzle-shaped for this receiver (fall through to the plain
/// missing-field error); `Some(Err)` = swizzle-shaped but invalid.
pub(crate) fn vec_swizzle_ty(
    gamma: &mut Gamma,
    struct_name: Ustr,
    field: Ustr,
    span: Span,
) -> Option<Result<TyId, CompilerError>> {
    let arity = match struct_name.as_str() {
        "Vec2" => 2,
        "Vec3" => 3,
        "Vec4" => 4,
        _ => return None,
    };
    let s = field.as_str();
    if s.is_empty() || !s.chars().all(|ch| "xyzwrgba".contains(ch)) {
        return None;
    }
    if s.len() > 4 {
        return Some(Err(CompilerError::new_with_label(
            span,
            format!("swizzle `{s}` has more than 4 components"),
        )));
    }
    let xyzw: Option<Vec<usize>> = s.chars().map(|ch| "xyzw".find(ch)).collect();
    let rgba: Option<Vec<usize>> = s.chars().map(|ch| "rgba".find(ch)).collect();
    let idxs = match xyzw.or(rgba) {
        Some(idxs) => idxs,
        None => {
            return Some(Err(CompilerError::new_with_label(
                span,
                format!("swizzle `{s}` mixes the `xyzw` and `rgba` alphabets"),
            )))
        }
    };
    if let Some(pos) = idxs.iter().position(|&i| i >= arity) {
        let ch = s.chars().nth(pos).expect("swizzle component position in range");
        return Some(Err(CompilerError::new_with_label(
            span,
            format!("`{struct_name}` has no `{ch}` component (swizzle `{s}`)"),
        )));
    }
    let ty = match idxs.len() {
        1 => gamma.workspace.builtins.float(),
        n => gamma.nominal_app(Ustr::from(&format!("Vec{n}")), &[], span),
    };
    Some(Ok(ty))
}

/// Reject `v.yx = ...` / `v.yx += ...`: a swizzle types as a read, so without
/// this pre-check an assignment target would infer as a plain field and emit a
/// broken store. Real fields (incl. len-1 `x/y/z/w`) pass through; invalid
/// swizzle shapes fall through to the read-path diagnostics.
pub(crate) fn reject_swizzle_write(
    c: BodyCtx,
    gamma: &mut Gamma,
    base: Idx<Expr>,
    field: Ustr,
    span: Span,
) -> Result<(), CompilerError> {
    let base_ty = infer_expr(c, gamma, base)?;
    let base_ty = resolve(gamma, base_ty, span);
    let TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } = *gamma.get(base_ty) else {
        return Ok(());
    };
    if vec_swizzle_components(name.as_str(), field.as_str()).is_none() {
        return Ok(());
    }
    let is_real_field = resolve_struct(gamma, name).is_some_and(|sid| {
        gamma.workspace.module(sid.module).structs[sid.def]
            .fields
            .iter()
            .any(|f| f.name == field)
    });
    if !is_real_field {
        comp_error!(span, "cannot assign to swizzle `{}`: swizzles are read-only", field);
    }
    Ok(())
}

/// The component indices of a VALID swizzle read on `struct_name` -- the
/// arity/alphabet rules of `vec_swizzle_ty` with the errors collapsed to None.
/// Callers (assignment rejection, MIR expansion) key off "would this read as a
/// swizzle"; invalid shapes surface through the read-path diagnostics instead.
pub fn vec_swizzle_components(struct_name: &str, field: &str) -> Option<Vec<usize>> {
    let arity = match struct_name {
        "Vec2" => 2,
        "Vec3" => 3,
        "Vec4" => 4,
        _ => return None,
    };
    if field.is_empty() || field.len() > 4 {
        return None;
    }
    let xyzw: Option<Vec<usize>> = field.chars().map(|ch| "xyzw".find(ch)).collect();
    let rgba: Option<Vec<usize>> = field.chars().map(|ch| "rgba".find(ch)).collect();
    let idxs = xyzw.or(rgba)?;
    idxs.iter().all(|&i| i < arity).then_some(idxs)
}

pub(crate) fn tuple_or_newtype_index_ty(
    gamma: &mut Gamma,
    tuple_ty: TyId,
    index: usize,
    span: Span,
) -> Result<TyId, CompilerError> {
    let ty = match *gamma.get(tuple_ty) {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } if gamma.workspace.newtypes_by_name.contains_key(&name) => {
            let NewtypeId { module: ntm, def } = resolve_newtype(gamma, name).unwrap();
            let nt = gamma.workspace.module(ntm).newtypes[def].clone();

            let mut subs = FxHashMap::default();

            for (g, arg) in nt.generics.iter().zip(params) {
                if let TypeExpr::Generic(var_id, _) = gamma.get(*g) {
                    subs.insert(*var_id, tuple_ty.with_idx(arg));
                }
            }

            if index != 0 {
                comp_error!(
                    span,
                    "newtype `{}` has no field .{} (newtypes wrap one value, indexed .0)",
                    name,
                    index
                );
            }

            substitute(gamma, nt.wrapped, &subs, span)?
        }

        TypeExpr::Tuple { elements } => {
            let elem = elements.into_iter().nth(index).ok_or_else(|| {
                CompilerError::new_with_label(
                    span,
                    format!(
                        "tuple index {} out of bounds (len {})",
                        index,
                        elements.len()
                    ),
                )
            })?;

            let elem_ty = tuple_ty.with_idx(elem);
            resolve(gamma, elem_ty, span)
        }

        _ => comp_error!(
            span,
            "cannot index a non-tuple type {}",
            gamma.workspace.render(Some(&gamma.out), tuple_ty),
        ),
    };

    Ok(ty)
}

pub(crate) fn resolve_global(gamma: &Gamma, name: Ustr) -> Option<TyId> {
    gamma.dep_acc.borrow_mut().insert(name);
    gamma.workspace.globals_by_name.get(&name).copied()
}

pub(crate) fn resolve_newtype(gamma: &Gamma, name: Ustr) -> Option<NewtypeId> {
    gamma.dep_acc.borrow_mut().insert(name);
    gamma.workspace.newtypes_by_name.get(&name).copied()
}
