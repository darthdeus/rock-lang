use super::*;

pub(crate) fn resolve_struct(gamma: &Gamma, name: Ustr) -> Option<StructId> {
    gamma.dep_acc.borrow_mut().insert(name);
    gamma.workspace.struct_by_name.get(&name).copied()
}

pub(crate) fn generic_arg_subs(
    gamma: &Gamma,
    generics: &[TyId],
    app_ty: TyId,
    params: IdxRange<TypeExpr>,
) -> FxHashMap<VarId, TyId> {
    let mut subs = FxHashMap::default();

    for (g, arg) in generics.iter().zip(params) {
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

/// Keeps interface conformance decisions independent of diagnostic wording.
#[derive(Debug)]
pub(crate) enum StructFieldError {
    MissingField {
        struct_name: Option<Ustr>,
        field: Ustr,
        span: Span,
    },
    AccessDenied {
        denied: AccessDenied,
        span: Span,
    },
    Other(CompilerError),
}

impl From<CompilerError> for StructFieldError {
    fn from(error: CompilerError) -> Self {
        Self::Other(error)
    }
}

impl From<StructFieldError> for CompilerError {
    fn from(error: StructFieldError) -> Self {
        match error {
            StructFieldError::MissingField {
                struct_name,
                field,
                span,
            } => {
                let message = match struct_name {
                    Some(struct_name) => format!("struct `{struct_name}` has no field `{field}`"),
                    None => format!("no struct has a field `{field}`"),
                };
                CompilerError::new_with_label(span, message)
            }
            StructFieldError::AccessDenied { denied, span } => denied.to_compiler_error(span),
            StructFieldError::Other(error) => error,
        }
    }
}

pub(crate) fn struct_field_ty(
    gamma: &mut Gamma,
    base_ty: TyId,
    field: Ustr,
    span: Span,
) -> Result<TyId, StructFieldError> {
    let base_ty = match *gamma.get(base_ty) {
        TypeExpr::App {
            ctor: TypeCtor::Ref,
            params,
        } => {
            let mut params = params;
            let Some(inner) = params.next() else {
                return Err(StructFieldError::Other(CompilerError::new_with_label(
                    span,
                    "malformed `ref` type has no target".to_string(),
                )));
            };
            base_ty.with_idx(inner)
        }
        _ => base_ty,
    };
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

        TypeExpr::Builtin(BuiltinTy::Error) => return Ok(base_ty),

        TypeExpr::Infer(..) => {
            let matches = structs_with_field(gamma, field);

            let sid = match matches.as_slice() {
                [one] => *one,
                [] => {
                    return Err(StructFieldError::MissingField {
                        struct_name: None,
                        field,
                        span,
                    });
                }
                _ => {
                    return Err(StructFieldError::Other(CompilerError::new_with_label(
                        span,
                        format!(
                            "ambiguous field `{}`: multiple structs define it; annotate the parameter",
                            field
                        ),
                    )));
                }
            };

            let def = &gamma.workspace.module(sid.module).structs[sid.def];
            let (subs, fresh) = gamma.instantiate_generics(&def.generics, span)?;

            let struct_ty = gamma.nominal_app(def.name, &fresh, span);
            unify(gamma, base_ty, struct_ty, span)?;

            let fty = def
                .fields
                .iter()
                .find(|f| f.name == field)
                .ok_or(StructFieldError::MissingField {
                    struct_name: Some(def.name),
                    field,
                    span,
                })?
                .ty;
            return substitute(gamma, fty, &subs, span).map_err(StructFieldError::from);
        }

        _ => {
            return Err(StructFieldError::Other(CompilerError::new_with_label(
                span,
                format!(
                    "cannot access field `{}` on non-struct type {}",
                    field,
                    gamma.render(base_ty)
                ),
            )));
        }
    };

    let sid = resolve_struct(gamma, struct_name).ok_or_else(|| {
        CompilerError::new_with_label(span, format!("unknown struct `{}`", struct_name))
    })?;

    let def = &gamma.workspace.module(sid.module).structs[sid.def];
    let subs = generic_arg_subs(gamma, &def.generics, base_ty, params);
    let field_ty = def.fields.iter().find(|f| f.name == field).map(|f| f.ty);

    let Some(field_ty) = field_ty else {
        if let Some(res) = vec_swizzle_ty(gamma, struct_name, field, span) {
            return res.map_err(StructFieldError::from);
        }
        return Err(StructFieldError::MissingField {
            struct_name: Some(struct_name),
            field,
            span,
        });
    };

    let site = gamma.access_site();
    field_access(gamma, struct_name, field, site, FieldAccessNeed::Read)
        .map_err(|denied| StructFieldError::AccessDenied { denied, span })?;

    substitute(gamma, field_ty, &subs, span).map_err(StructFieldError::from)
}

pub fn struct_fields_of(
    gamma: &mut Gamma,
    base_ty: TyId,
    span: Span,
) -> Option<(Ustr, Vec<(Ustr, TyId)>)> {
    let base_ty = resolve(gamma, base_ty, span);
    let (name, params) = match *gamma.get(base_ty) {
        TypeExpr::App {
            ctor: TypeCtor::Ref,
            mut params,
        } => return struct_fields_of(gamma, base_ty.with_idx(params.next()?), span),
        TypeExpr::Nullable(inner) => return struct_fields_of(gamma, base_ty.with_idx(inner), span),
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } => (name, params),
        TypeExpr::App {
            ctor: TypeCtor::Array,
            params,
        } => (*VEC, params),
        _ => return None,
    };
    let ws = gamma.workspace;
    let sid = resolve_struct(gamma, name)?;
    let def = &ws.module(sid.module).structs[sid.def];
    let subs = generic_arg_subs(gamma, &def.generics, base_ty, params);
    let fields = def
        .fields
        .iter()
        .map(|f| Some((f.name, substitute(gamma, f.ty, &subs, span).ok()?)))
        .collect::<Option<Vec<_>>>()?;
    Some((name, fields))
}

/// Type an `xyzw` or `rgba` swizzle on a float vector. `None` means this is not
/// a swizzle; `Some(Err)` means it is malformed or out of range.
pub(crate) fn vec_swizzle_ty(
    gamma: &mut Gamma,
    struct_name: Ustr,
    field: Ustr,
    span: Span,
) -> Option<Result<TyId, CompilerError>> {
    let arity = bone_core::modules::vec_swizzle_arity(struct_name)?;
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
            )));
        }
    };
    if let Some(pos) = idxs.iter().position(|&i| i >= arity) {
        let ch = s
            .chars()
            .nth(pos)
            .expect("swizzle component position in range");
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

/// Reject swizzle writes before the read-path type check can treat one as a field.
pub(crate) fn reject_swizzle_write(
    c: BodyCtx,
    gamma: &mut Gamma,
    base: Idx<Expr>,
    field: Ustr,
    span: Span,
) -> Result<(), CompilerError> {
    let base_ty = infer_expr(c, gamma, base)?;
    let base_ty = resolve(gamma, base_ty, span);
    let TypeExpr::App {
        ctor: TypeCtor::Nominal(name),
        ..
    } = *gamma.get(base_ty)
    else {
        return Ok(());
    };
    if bone_core::modules::vec_swizzle_components_of(name, field.as_str()).is_none() {
        return Ok(());
    }
    let is_real_field = resolve_struct(gamma, name).is_some_and(|sid| {
        gamma.workspace.module(sid.module).structs[sid.def]
            .fields
            .iter()
            .any(|f| f.name == field)
    });
    if !is_real_field {
        comp_error!(
            span,
            "cannot assign to swizzle `{}`: swizzles are read-only",
            field
        );
    }
    Ok(())
}

fn ty_is_value_struct(gamma: &Gamma, ty: TyId) -> Option<Ustr> {
    let TypeExpr::App {
        ctor: TypeCtor::Nominal(name),
        ..
    } = *gamma.get(ty)
    else {
        return None;
    };
    resolve_struct(gamma, name)
        .is_some_and(|sid| gamma.workspace.module(sid.module).structs[sid.def].is_value)
        .then_some(name)
}

fn value_field_write_is_place(
    c: BodyCtx,
    gamma: &mut Gamma,
    base: Idx<Expr>,
    span: Span,
) -> Result<bool, CompilerError> {
    let mut cur = base;
    loop {
        match c.body.expr_arena[cur] {
            Expr::Index { .. } | Expr::Global(_) => return Ok(true),
            Expr::FieldAccess { base: parent, .. } => {
                let pty = infer_expr(c, gamma, parent)?;
                let pty = resolve(gamma, pty, span);
                // A mutable struct field is writable only if its storage is
                // reachable; a temporary or shared interior value may not be.
                if ty_is_value_struct(gamma, pty).is_none() {
                    return Ok(expr_is_reachable_storage(c, parent));
                }
                cur = parent;
            }
            Expr::Var(_) => return Ok(true),
            _ => return Ok(false),
        }
    }
}

/// Whether `e` reaches storage through a place; keep temporary handling in sync
/// with lowering to avoid accepting unobservable writes.
fn expr_is_reachable_storage(c: BodyCtx, e: Idx<Expr>) -> bool {
    match c.body.expr_arena[e] {
        Expr::Var(_) | Expr::Global(_) => true,
        Expr::FieldAccess { base, .. } | Expr::Index { base, .. } | Expr::SafeNav { base, .. } => {
            expr_is_reachable_storage(c, base)
        }
        _ => false,
    }
}

pub(crate) fn reject_value_struct_field_write(
    c: BodyCtx,
    gamma: &mut Gamma,
    base: Idx<Expr>,
    span: Span,
) -> Result<(), CompilerError> {
    let base_ty = infer_expr(c, gamma, base)?;
    let base_ty = resolve(gamma, base_ty, span);
    let Some(name) = ty_is_value_struct(gamma, base_ty) else {
        return Ok(());
    };
    if value_field_write_is_place(c, gamma, base, span)? {
        return Ok(());
    }
    comp_error!(
        span,
        "cannot assign to a field of `value` struct `{}`: it is a temporary, so the write would be lost. Bind it to a local first, or write through a local, parameter, field, element or global.",
        name
    );
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
            let nt = &gamma.workspace.module(ntm).newtypes[def];

            let subs = generic_arg_subs(gamma, &nt.generics, tuple_ty, params);

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
            gamma.render(tuple_ty),
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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FieldAccessNeed {
    Read,
    Write,
}

#[derive(Clone, Debug)]
pub enum AccessDenied {
    NoSuchField {
        struct_name: Ustr,
        field: Ustr,
    },
    Privilege {
        struct_name: Ustr,
        field: Ustr,
        vis: NsVisibility,
        need: FieldAccessNeed,
    },
}

impl AccessDenied {
    pub fn to_compiler_error(&self, span: Span) -> CompilerError {
        match self {
            AccessDenied::NoSuchField { struct_name, field } => CompilerError::new_with_label(
                span,
                format!("struct `{}` has no field `{}`", struct_name, field),
            ),
            AccessDenied::Privilege {
                struct_name,
                field,
                vis: _,
                need,
            } => {
                let msg = match need {
                    FieldAccessNeed::Read => {
                        format!(
                            "field `{}` on struct `{}` is not accessible from this module",
                            field, struct_name
                        )
                    }
                    FieldAccessNeed::Write => {
                        format!(
                            "field `{}` on struct `{}` is not writable from this module",
                            field, struct_name
                        )
                    }
                };
                CompilerError::new_with_label(span, msg)
            }
        }
    }
}

pub fn field_access(
    gamma: &Gamma,
    struct_key: Ustr,
    field: Ustr,
    site: AccessSite,
    need: FieldAccessNeed,
) -> Result<(), AccessDenied> {
    let Some(sid) = resolve_struct(gamma, struct_key) else {
        return Ok(());
    };
    let Some(meta) = gamma.workspace.defs.get(&struct_key) else {
        // Compiler-owned definitions expose public fields.
        return Ok(());
    };

    let sdef = &gamma.workspace.module(sid.module).structs[sid.def];
    let Some(fdef) = sdef.fields.iter().find(|f| f.name == field) else {
        return Err(AccessDenied::NoSuchField {
            struct_name: meta.name,
            field,
        });
    };

    let vis = match need {
        FieldAccessNeed::Read => &fdef.read_vis,
        FieldAccessNeed::Write => &fdef.write_vis,
    };

    let allowed = match site.package {
        Some(caller_pkg) => gamma.workspace.ns.can_access(
            vis,
            meta.package,
            &meta.module,
            caller_pkg,
            site.module_path,
        ),
        None => {
            // Compiler-owned callers have no package visibility grants.
            matches!(vis, NsVisibility::Pub)
        }
    };

    if allowed {
        Ok(())
    } else {
        Err(AccessDenied::Privilege {
            struct_name: meta.name,
            field,
            vis: vis.clone(),
            need,
        })
    }
}

pub(crate) fn check_field_write_access(
    c: BodyCtx,
    gamma: &mut Gamma,
    base: Idx<Expr>,
    field: Ustr,
    span: Span,
) -> Result<(), CompilerError> {
    let base_ty = infer_expr(c, gamma, base)?;
    let base_ty = resolve(gamma, base_ty, span);
    let base_ty = peel_ref(gamma, base_ty);
    if let TypeExpr::App {
        ctor: TypeCtor::Nominal(struct_key),
        ..
    } = *gamma.get(base_ty)
    {
        let site = gamma.access_site();
        field_access(gamma, struct_key, field, site, FieldAccessNeed::Write)
            .map_err(|e| e.to_compiler_error(span))?;
    }

    // Writes through value or embedded fields also require writable access to
    // the enclosing field.
    if let Expr::FieldAccess {
        base: parent,
        field: parent_field,
    } = c.body.expr_arena[base]
    {
        let is_value = ty_is_value_struct(gamma, base_ty).is_some();
        let parent_ty = infer_expr(c, gamma, parent)?;
        let is_embed = crate::embed::embed_field_of(gamma, parent_ty, parent_field, span);
        if is_value || is_embed {
            check_field_write_access(c, gamma, parent, parent_field, span)?;
        }
    }

    Ok(())
}
