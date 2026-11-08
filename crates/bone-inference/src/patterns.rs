use super::*;

/// A match arm whose whole pattern is a bare `Name` is ambiguous: skull reads it
/// as a nullary-variant tag test when the name is a variant of the scrutinee
/// enum, and as an irrefutable binding otherwise -- so a typo'd variant silently
/// becomes a catch-all. The parser already settles the same ambiguity by
/// capitalization (`Color::Red` vs `value :: int`, `peek_is_uppercase_ident`),
/// so an uppercase bare name against an enum scrutinee must be a real variant.
///
/// `Some(v)` means the arm tag-tests `v` (it covers a variant and catches
/// nothing else); `None` means a genuine binding.
pub(crate) fn unqualified_arm_variant(
    c: BodyCtx,
    gamma: &mut Gamma,
    pat: Idx<Pattern>,
    scrut_ty: TyId,
    span: Span,
) -> Result<Option<Ustr>, CompilerError> {
    let Pattern::Bind(b) = c.body.pattern_arena[pat] else {
        return Ok(None);
    };

    let name = c.body.binding_arena[b].name;
    if !name
        .as_str()
        .starts_with(|ch: char| ch.is_ascii_uppercase())
    {
        return Ok(None);
    }

    let scrut_ty = resolve(gamma, scrut_ty, span);
    let TypeExpr::App {
        ctor: TypeCtor::Nominal(ename),
        ..
    } = *gamma.get(scrut_ty)
    else {
        return Ok(None);
    };

    let Some(eid) = resolve_enum(gamma, ename) else {
        return Ok(None);
    };

    let def = &gamma.workspace.module(eid.module).enums[eid.def];
    if def.variants.iter().any(|v| v.name == name) {
        return Ok(Some(name));
    }

    let known = def
        .variants
        .iter()
        .map(|v| v.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    Err(CompilerError::new_with_label(
        c.body.binding_spans.get(b).copied().unwrap_or(span),
        format!(
            "enum `{ename}` has no variant `{name}` (an uppercase match-arm name \
             is read as a variant, not a binding). Known variants: {known}"
        ),
    ))
}

pub(crate) fn check_pattern(
    c: BodyCtx,
    gamma: &mut Gamma,
    pat: Idx<Pattern>,
    expected: TyId,
    span: Span,
) -> Result<(), CompilerError> {
    let er = resolve(gamma, expected, span);

    if matches!(gamma.get(er), TypeExpr::Builtin(BuiltinTy::Error)) {
        return bind_pattern_leaves_to(c, gamma, pat, er, span);
    }

    match c.body.pattern_arena[pat] {
        Pattern::Wildcard => Ok(()),
        Pattern::Bind(b) => gamma.assign_named_type(b, expected),

        Pattern::Literal(lit) => {
            let lit_ty = infer_expr(c, gamma, lit)?;
            coerce(gamma, lit_ty, expected, span)
        }

        Pattern::Range { start, end, .. } => {
            for bound in [start, end].into_iter().flatten() {
                let bty = infer_expr(c, gamma, bound)?;
                coerce(gamma, bty, expected, span)?;
            }

            Ok(())
        }

        Pattern::EnumVariant {
            enum_name,
            variant,
            sub,
            ..
        } => {
            let expected = resolve(gamma, expected, span);
            let (scrut_name, params) = match *gamma.get(expected) {
                TypeExpr::App {
                    ctor: TypeCtor::Nominal(scrut_name),
                    params,
                } => (scrut_name, params),
                _ => comp_error!(
                    span,
                    "Expected a nominal type in pattern, got {}",
                    gamma.workspace.render(Some(&gamma.out), expected)
                ),
            };

            let ename = enum_name.unwrap_or(scrut_name);

            let eid = resolve_enum(gamma, ename).ok_or_else(|| {
                CompilerError::new_with_label(span, format!("unknown enum `{}`", ename))
            })?;

            // TODO: maybe don't need to clone
            let def = gamma.workspace.module(eid.module).enums[eid.def].clone();

            let mut subs = FxHashMap::default();
            for (g, arg) in def.generics.iter().zip(params) {
                if let TypeExpr::Generic(var_id, _) = gamma.get(*g) {
                    subs.insert(*var_id, expected.with_idx(arg));
                }
            }

            let vdef = def
                .variants
                .iter()
                .find(|v| v.name == variant)
                .cloned()
                .ok_or_else(|| {
                    CompilerError::new_with_label(
                        span,
                        format!("enum `{}` has no variant `{}`", ename, variant),
                    )
                })?;

            match vdef.payload {
                VariantPayload::Unit => {
                    if sub.len() != 0 {
                        comp_error!(
                            span,
                            "variant `{}` has no payload, but pattern has subpattern",
                            variant
                        );
                    }
                }

                VariantPayload::Tuple(tys) => {
                    if sub.len() != tys.len() {
                        comp_error!(
                            span,
                            "variant `{}` expected {} subpatterns, but got {}",
                            variant,
                            tys.len(),
                            sub.len()
                        );
                    }

                    for (subpat, ty) in sub.zip(tys.iter()) {
                        let pty = substitute(gamma, *ty, &subs, span)?;
                        check_pattern(c, gamma, subpat, pty, span)?;
                    }
                }

                VariantPayload::Record(_) => comp_error!(
                    span,
                    "Record variants not supported in patterns yet (variant `{}`)",
                    variant
                ),
            }

            Ok(())
        }

        Pattern::Record {
            enum_name,
            name,
            fields,
            ..
        } => {
            let expected = resolve(gamma, expected, span);

            if let TypeExpr::App {
                ctor: TypeCtor::Nominal(scrut),
                params,
            } = *gamma.get(expected)
            {
                let ename = enum_name.unwrap_or(scrut);

                if let Some(eid) = resolve_enum(gamma, ename) {
                    // TODO: another clone we might not need?
                    let def = gamma.workspace.module(eid.module).enums[eid.def].clone();

                    let mut subs = FxHashMap::default();
                    for (g, arg) in def.generics.iter().zip(params) {
                        if let TypeExpr::Generic(var_id, _) = gamma.get(*g) {
                            subs.insert(*var_id, expected.with_idx(arg));
                        }
                    }

                    let vname = name.ok_or_else(|| {
                        CompilerError::new_with_label(
                            span,
                            "record-variant pattern requires a variant name".to_string(),
                        )
                    })?;

                    let vdef = def
                        .variants
                        .iter()
                        .find(|v| v.name == vname)
                        .cloned()
                        .ok_or_else(|| {
                            CompilerError::new_with_label(
                                span,
                                format!("enum `{}` has no variant `{}`", ename, vname),
                            )
                        })?;

                    let rec = match vdef.payload {
                        VariantPayload::Record(fs) => fs,
                        _ => comp_error!(span, "variant `{}` is not a record variant", vname),
                    };

                    for f in fields {
                        let pf = c.body.record_pat_field_arena[f];
                        let fdef = rec.iter().find(|sf| sf.name == pf.name).ok_or_else(|| {
                            CompilerError::new_with_label(
                                span,
                                format!("record variant `{}` has no field `{}`", vname, pf.name),
                            )
                        })?;

                        let fty = substitute(gamma, fdef.ty, &subs, span)?;
                        check_pattern(c, gamma, pf.pat, fty, span)?;
                    }

                    return Ok(());
                }
            }

            for f in fields {
                let field = c.body.record_pat_field_arena[f];
                let fty = struct_field_ty(gamma, expected, field.name, span)?;
                check_pattern(c, gamma, field.pat, fty, span)?;
            }

            Ok(())
        }

        Pattern::Or { alternatives } => {
            for alt in alternatives {
                if pattern_binds(c, alt) {
                    comp_error!(
                        span,
                        "or-patterns cannot bind variables, use `_` for payload"
                    );
                }

                check_pattern(c, gamma, alt, expected, span)?;
            }

            Ok(())
        }

        Pattern::Tuple { elements } => {
            let expected = resolve(gamma, expected, span);

            let tuple_elems = match *gamma.get(expected) {
                TypeExpr::Tuple { elements } => elements,

                _ => comp_error!(
                    span,
                    "expected a tuple type in pattern, got {}",
                    gamma.render(expected)
                ),
            };

            if elements.len() != tuple_elems.len() {
                comp_error!(
                    span,
                    "tuple pattern expected {} elements, but got {}",
                    tuple_elems.len(),
                    elements.len()
                );
            }

            for (subpat, elem) in elements.zip(tuple_elems) {
                let elem_ty = resolve(gamma, expected.with_idx(elem), span);
                check_pattern(c, gamma, subpat, elem_ty, span)?;
            }

            Ok(())
        }
    }
}

pub(crate) fn destructure_pattern(
    gamma: &mut Gamma,
    pat: &BindingPattern,
    ty: TyId,
    span: Span,
) -> Result<(), CompilerError> {
    // A poisoned scrutinee has no tuple/nominal shape.
    // Bind every slot to `error` so a failed RHS doesn't force a shape error
    // or leave vars unbound
    let r = resolve(gamma, ty, span);
    if matches!(gamma.get(r), TypeExpr::Builtin(BuiltinTy::Error)) {
        return match pat {
            BindingPattern::Leaf(b) => gamma.assign_named_type(*b, r),
            BindingPattern::Tuple(subs) => {
                for sub in subs {
                    destructure_pattern(gamma, sub, r, span)?;
                }

                Ok(())
            }
        };
    }

    match pat {
        BindingPattern::Leaf(b) => gamma.assign_named_type(*b, ty),

        BindingPattern::Tuple(subs) => {
            let resolved = resolve(gamma, ty, span);

            match *gamma.get(resolved) {
                TypeExpr::Tuple { elements } => {
                    let es: Vec<_> = elements.into_iter().collect();
                    if es.len() != subs.len() {
                        comp_error!(
                            span,
                            "tuple binding count mismatch: expected {}, got {}",
                            subs.len(),
                            es.len()
                        );
                    }

                    for (sub, e) in subs.iter().zip(es) {
                        let et = resolve(gamma, resolved.with_idx(e), span);
                        // recurse
                        destructure_pattern(gamma, sub, et, span)?;
                    }

                    Ok(())
                }

                _ => comp_error!(
                    span,
                    "expected tuple type, got {}",
                    gamma.workspace.render(Some(&gamma.out), resolved)
                ),
            }
        }
    }
}

pub(crate) fn pattern_binds(c: BodyCtx, pat: Idx<Pattern>) -> bool {
    match c.body.pattern_arena[pat] {
        Pattern::Bind(_) => true,
        Pattern::EnumVariant { sub, .. } => sub.into_iter().any(|p| pattern_binds(c, p)),
        Pattern::Record { fields, .. } => fields
            .into_iter()
            .any(|f| pattern_binds(c, c.body.record_pat_field_arena[f].pat)),

        Pattern::Or { alternatives } => alternatives.into_iter().any(|p| pattern_binds(c, p)),
        Pattern::Tuple { elements } => elements.into_iter().any(|p| pattern_binds(c, p)),

        _ => false,
    }
}

pub(crate) fn bind_pattern_leaves_to(
    c: BodyCtx,
    gamma: &mut Gamma,
    pat: Idx<Pattern>,
    ty: TyId,
    _span: Span,
) -> Result<(), CompilerError> {
    match c.body.pattern_arena[pat] {
        Pattern::Bind(b) => gamma.assign_named_type(b, ty)?,

        Pattern::EnumVariant { sub, .. } => {
            for p in sub {
                bind_pattern_leaves_to(c, gamma, p, ty, _span)?;
            }
        }

        Pattern::Record { fields, .. } => {
            for f in fields {
                bind_pattern_leaves_to(c, gamma, c.body.record_pat_field_arena[f].pat, ty, _span)?;
            }
        }

        Pattern::Tuple { elements } => {
            for p in elements {
                bind_pattern_leaves_to(c, gamma, p, ty, _span)?;
            }
        }

        Pattern::Or { alternatives } => {
            for p in alternatives {
                bind_pattern_leaves_to(c, gamma, p, ty, _span)?;
            }
        }

        Pattern::Wildcard | Pattern::Literal(..) | Pattern::Range { .. } => {}
    }

    Ok(())
}
