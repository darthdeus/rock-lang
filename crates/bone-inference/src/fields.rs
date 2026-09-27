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
            StructFieldError::MissingField { struct_name, field, span } => {
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

// Resolve the type of `field` on a nominal struct, with generic subst.
// `base_ty` must be resolved and peeled nullable for safe nav.
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

        TypeExpr::Infer(..) => {
            let matches = structs_with_field(gamma, field);

            let sid = match matches.as_slice() {
                [one] => *one,
                [] => {
                    return Err(StructFieldError::MissingField {
                        struct_name: None,
                        field,
                        span,
                    })
                }
                _ => {
                    return Err(StructFieldError::Other(CompilerError::new_with_label(
                        span,
                        format!(
                            "ambiguous field `{}`: multiple structs define it; annotate the parameter",
                            field
                        ),
                    )))
                }
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
                .ok_or_else(|| StructFieldError::MissingField {
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
                    gamma.workspace.render(Some(&gamma.out), base_ty)
                ),
            )))
        }
    };

    let sid = resolve_struct(gamma, struct_name).ok_or_else(|| {
        CompilerError::new_with_label(span, format!("unknown struct `{}`", struct_name))
    })?;

    let def = &gamma.workspace.module(sid.module).structs[sid.def];
    let subs = struct_arg_subs(gamma, def, base_ty, params);
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
    let arity = if bone_core::modules::is_intrinsic_key(struct_name, "Vec2") {
        2
    } else if bone_core::modules::is_intrinsic_key(struct_name, "Vec3") {
        3
    } else if bone_core::modules::is_intrinsic_key(struct_name, "Vec4") {
        4
    } else {
        return None;
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

/// Whether `ty` resolves to a `value` struct (C-layout / GC-tier both).
fn ty_is_value_struct(gamma: &Gamma, ty: TyId) -> Option<Ustr> {
    let TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } = *gamma.get(ty) else {
        return None;
    };
    resolve_struct(gamma, name)
        .is_some_and(|sid| gamma.workspace.module(sid.module).structs[sid.def].is_value)
        .then_some(name)
}

/// Whether a value-struct field write whose base is `base` targets real MUTABLE
/// STORAGE (legal place write) rather than a value LOCAL / rvalue copy (rejected).
/// A value struct reached through a mutable struct field, a container element
/// (`Index`), or a global lives in mutable storage and its leaves are writable in
/// place; one held directly as a value-typed local (`Var`) -- or a chain of value
/// projections off one -- or produced by a call/literal is an immutable copy.
/// docs/value-semantics-axes.md Mutability (place vs rvalue).
fn value_field_write_is_place(
    c: BodyCtx,
    gamma: &mut Gamma,
    base: Idx<Expr>,
    span: Span,
) -> Result<bool, CompilerError> {
    let mut cur = base;
    loop {
        match c.body.expr_arena[cur] {
            // Element of a container, or a global: mutable storage.
            Expr::Index { .. } | Expr::Global(_) => return Ok(true),
            Expr::FieldAccess { base: parent, .. } => {
                let pty = infer_expr(c, gamma, parent)?;
                let pty = resolve(gamma, pty, span);
                // A value field of a MUTABLE (non-value) struct lives in that
                // struct's storage -- but only if the struct ITSELF is reachable
                // storage. `mk().p.x = 5` writes through a temporary that no one
                // else can observe, and sharing (a hoisted constant, a returned
                // interior box) makes that write land on storage the caller never
                // named. Keep descending either way; the chain bottoms out at a
                // Var / Global / Index.
                if ty_is_value_struct(gamma, pty).is_none() {
                    return Ok(expr_is_reachable_storage(c, parent));
                }
                cur = parent;
            }
            // A bare value local, a call result, a literal: an immutable copy.
            _ => return Ok(false),
        }
    }
}

/// Whether `e` names storage a second reference could reach -- a variable or a
/// global, or a projection/element of one. A call result, a literal, or any other
/// temporary is reachable only through the expression that made it, so a write
/// into it is unobservable and rejects.
///
/// skull's `is_mutable_place` (`skull-mir/src/lower.rs`) is the read-side
/// sibling: same temporary/place verdict, but it stops at the first
/// container/global instead of walking to the root. Keep the temporary verdict
/// in sync -- a write bone accepts that skull thinks cannot be reached is an
/// aliasing bug.
fn expr_is_reachable_storage(c: BodyCtx, e: Idx<Expr>) -> bool {
    match c.body.expr_arena[e] {
        Expr::Var(_) | Expr::Global(_) => true,
        Expr::FieldAccess { base, .. } | Expr::Index { base, .. } | Expr::SafeNav { base, .. } => {
            expr_is_reachable_storage(c, base)
        }
        _ => false,
    }
}

/// Reject a write to a `value` struct's field UNLESS it targets a mutable place.
/// Value structs are immutable *as values* (docs/value-types-design.md 1): a write
/// into a value LOCAL / rvalue (`let p = Pt{}; p.x = 5`) is a lost write on a copy
/// and rejects. But a value that LIVES in mutable storage -- a field of a mutable
/// struct (`mob.pos.x`), a container element (`buf[i].pos.x`), a global place
/// (`CAST_ENV.value = x`) -- is writable in place (the mutable-place model,
/// docs/value-semantics-axes.md; the brief's Phase B place/rvalue classification).
/// Fires from the assignment / compound-assign / inc-dec target when its BASE is a
/// value-struct projection.
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
        "cannot assign to a field of `value` struct `{}`: it is an immutable local/temporary. Replace it wholesale (`p = {} {{ .. }}`), or mutate in place only through a mutable field / element / global.",
        name,
        name
    );
}

/// The component indices of a VALID swizzle read on `struct_name` -- the
/// arity/alphabet rules of `vec_swizzle_ty` with the errors collapsed to None.
/// Callers (assignment rejection, MIR expansion) key off "would this read as a
/// swizzle"; invalid shapes surface through the read-path diagnostics instead.
pub fn vec_swizzle_components(struct_name: &str, field: &str) -> Option<Vec<usize>> {
    let arity = if bone_core::modules::is_intrinsic_key(Ustr::from(struct_name), "Vec2") {
        2
    } else if bone_core::modules::is_intrinsic_key(Ustr::from(struct_name), "Vec3") {
        3
    } else if bone_core::modules::is_intrinsic_key(Ustr::from(struct_name), "Vec4") {
        4
    } else {
        return None;
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
        #[allow(dead_code)]
        vis: NsVisibility,
        need: FieldAccessNeed,
    },
}

impl AccessDenied {
    pub fn to_compiler_error(&self, span: Span) -> CompilerError {
        match self {
            AccessDenied::NoSuchField { struct_name, field } => {
                CompilerError::new_with_label(
                    span,
                    format!("struct `{}` has no field `{}`", struct_name, field),
                )
            }
            AccessDenied::Privilege { struct_name, field, vis: _, need } => {
                let msg = match need {
                    FieldAccessNeed::Read => {
                        format!("field `{}` on struct `{}` is not accessible from this module", field, struct_name)
                    }
                    FieldAccessNeed::Write => {
                        format!("field `{}` on struct `{}` is not writable from this module", field, struct_name)
                    }
                };
                CompilerError::new_with_label(span, msg)
            }
        }
    }
}

/// Single access predicate (plan 2.3; spec 10.2).
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
        // Legacy-owned definitions keep their bare names as keys and have no entry
        // in `workspace.defs`. Legacy fields are public read/write.
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
            // Legacy callers targeting v1 definitions get the same checks (spec 3.3).
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
    let base_ty = match *gamma.get(base_ty) {
        TypeExpr::App {
            ctor: TypeCtor::Ref,
            params,
        } => {
            let mut params = params;
            if let Some(inner) = params.next() {
                base_ty.with_idx(inner)
            } else {
                base_ty
            }
        }
        _ => base_ty,
    };
    if let TypeExpr::App {
        ctor: TypeCtor::Nominal(struct_key),
        ..
    } = *gamma.get(base_ty) {
        let site = gamma.access_site();
        field_access(gamma, struct_key, field, site, FieldAccessNeed::Write)
            .map_err(|e| e.to_compiler_error(span))?;
    }

    // Value-place and embed writeback: if `base_ty` is a value struct, or if `base` is an `embed`
    // field on an enclosing struct, writing to this field writes into `base`'s storage.
    // That enclosing field must also be writable (spec 10.3)!
    if let Expr::FieldAccess { base: parent, field: parent_field } = c.body.expr_arena[base] {
        let is_value = ty_is_value_struct(gamma, base_ty).is_some();
        let mut is_embed = false;
        let parent_ty = infer_expr(c, gamma, parent)?;
        let mut p_ty = resolve(gamma, parent_ty, span);
        if let TypeExpr::Nullable(inner) = *gamma.get(p_ty) {
            p_ty = resolve(gamma, p_ty.with_idx(inner), span);
        }
        if let TypeExpr::App { ctor: TypeCtor::Nominal(p_name), .. } = *gamma.get(p_ty) {
            if let Some(sid) = resolve_struct(gamma, p_name) {
                is_embed = gamma.workspace.module(sid.module).structs[sid.def]
                    .fields
                    .iter()
                    .any(|f| f.name == parent_field && f.is_embed);
            }
        }
        if is_value || is_embed {
            check_field_write_access(c, gamma, parent, parent_field, span)?;
        }
    }

    Ok(())
}
