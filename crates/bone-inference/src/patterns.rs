use super::*;

/// Treat a bare uppercase name against an enum as a tag test, so a misspelled
/// variant cannot silently become a catch-all. `None` means a genuine binding.
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
        return bind_pattern_leaves_to(c, gamma, pat, er);
    }

    // Structural patterns test the unwrapped value; `null` remains its own case.
    if let TypeExpr::Nullable(inner) = *gamma.get(er)
        && matches!(
            c.body.pattern_arena[pat],
            Pattern::EnumVariant { .. }
                | Pattern::Record { .. }
                | Pattern::Newtype { .. }
                | Pattern::Tuple { .. }
        )
    {
        return check_pattern(c, gamma, pat, er.with_idx(inner), span);
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
                    gamma.render(expected)
                ),
            };

            let ename = enum_name.unwrap_or(scrut_name);

            let eid = resolve_enum(gamma, ename).ok_or_else(|| {
                CompilerError::new_with_label(span, format!("unknown enum `{}`", ename))
            })?;

            let def = &gamma.workspace.module(eid.module).enums[eid.def];

            let subs = generic_arg_subs(gamma, &def.generics, expected, params);

            let vdef = def
                .variants
                .iter()
                .find(|v| v.name == variant)
                .ok_or_else(|| {
                    CompilerError::new_with_label(
                        span,
                        format!("enum `{}` has no variant `{}`", ename, variant),
                    )
                })?;

            match &vdef.payload {
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
            if enum_name.is_none() {
                gamma.out.record_pattern_types.insert(pat, expected);
            }

            if let TypeExpr::App {
                ctor: TypeCtor::Nominal(scrut),
                params,
            } = *gamma.get(expected)
            {
                let ename = enum_name.unwrap_or(scrut);

                if let Some(eid) = resolve_enum(gamma, ename) {
                    let def = &gamma.workspace.module(eid.module).enums[eid.def];

                    let subs = generic_arg_subs(gamma, &def.generics, expected, params);

                    let vname = name.ok_or_else(|| {
                        CompilerError::new_with_label(
                            span,
                            "record-variant pattern requires a variant name".to_string(),
                        )
                    })?;

                    let vname = Ustr::from(vname.rsplit("::").next().unwrap_or(vname.as_str()));
                    let vdef = def
                        .variants
                        .iter()
                        .find(|v| v.name == vname)
                        .ok_or_else(|| {
                            CompilerError::new_with_label(
                                span,
                                format!("enum `{}` has no variant `{}`", ename, vname),
                            )
                        })?;

                    let rec = match &vdef.payload {
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

        Pattern::Newtype {
            name,
            name_span,
            sub,
        } => {
            let er = resolve(gamma, expected, span);

            let params = match *gamma.get(er) {
                TypeExpr::App {
                    ctor: TypeCtor::Nominal(n),
                    params,
                } if n == name => Some(params),
                // The scrutinee's nominal identity cannot be checked yet.
                TypeExpr::Infer(..) | TypeExpr::Generic(..) => None,
                _ => comp_error!(
                    name_span,
                    "cannot destructure this value with newtype pattern `{}` - the value is not a newtype `{}`",
                    name,
                    name
                ),
            };

            let NewtypeId { module: ntm, def } = resolve_newtype(gamma, name).ok_or_else(|| {
                CompilerError::new_with_label(name_span, format!("unknown newtype `{}`", name))
            })?;
            let nt = &gamma.workspace.module(ntm).newtypes[def];

            let subs = match params {
                Some(params) => generic_arg_subs(gamma, &nt.generics, er, params),
                None => FxHashMap::default(),
            };
            let wt = substitute(gamma, nt.wrapped, &subs, span)?;
            check_pattern(c, gamma, sub, wt, span)
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

pub(crate) fn pattern_binds(c: BodyCtx, pat: Idx<Pattern>) -> bool {
    !c.body.pattern_bindings(pat).is_empty()
}

pub(crate) fn bind_pattern_leaves_to(
    c: BodyCtx,
    gamma: &mut Gamma,
    pat: Idx<Pattern>,
    ty: TyId,
) -> Result<(), CompilerError> {
    for b in c.body.pattern_bindings(pat) {
        gamma.assign_named_type(b, ty)?;
    }
    Ok(())
}

/// Whether `pat` alone covers every value; coverage is conservative, so `false`
/// may mean the checker cannot prove it.
pub(crate) fn pattern_is_irrefutable(
    c: BodyCtx,
    gamma: &mut Gamma,
    pat: Idx<Pattern>,
    scrut_ty: TyId,
    tag: Option<Ustr>,
    span: Span,
) -> bool {
    let arms = [exhaustive::ArmPattern { pattern: pat, tag }];
    exhaustive::missing_match_patterns(c, gamma, scrut_ty, &arms, span).is_empty()
}

pub(crate) fn require_irrefutable(
    c: BodyCtx,
    gamma: &mut Gamma,
    pat: Idx<Pattern>,
    scrut_ty: TyId,
    tag: Option<Ustr>,
    span: Span,
    suggestion: &str,
) -> Result<(), CompilerError> {
    let arms = [exhaustive::ArmPattern { pattern: pat, tag }];
    let missing = exhaustive::missing_match_patterns(c, gamma, scrut_ty, &arms, span);
    if missing.is_empty() {
        return Ok(());
    }
    comp_error!(
        span,
        "this pattern can fail: `{}` is not matched; {}",
        missing.join("`, `"),
        suggestion
    )
}
