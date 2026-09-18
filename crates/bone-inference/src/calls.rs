use super::*;

pub(crate) fn resolve_callee(gamma: &Gamma, family: &CallFamily) -> FnList {
    let name = family.key();
    let _span = tracing::info_span!("resolve_callee", name = %name).entered();

    let CallFamily::Merged { file, .. } = *family else {
        gamma.dep_acc.borrow_mut().insert(name);
        return gamma.workspace.functions_by_name.get(&name).cloned().unwrap_or_default();
    };
    // A merged family: every member's overloads under its own key, as
    // lowering recorded them for the module the call was lowered in. Each
    // key is a dependency, so an overload added to any member re-infers this
    // caller.
    let ws = gamma.workspace;
    let keys = ws
        .module_ids
        .get(&file)
        .and_then(|mid| ws.module(*mid).call_families.get(&name))
        .unwrap_or_else(|| panic!("merged family `{name}` of `{file}` was not recorded at lowering"));
    let mut out = FnList::default();
    for &key in keys {
        gamma.dep_acc.borrow_mut().insert(key);
        if let Some(fns) = ws.functions_by_name.get(&key) {
            out.extend(fns.iter().copied());
        }
    }
    out
}

/// True if `ty` contains an unbound generic hole (an `Infer`/`Generic` node)
/// anywhere in its structure -- resolved so already-bound vars read through.
fn ty_has_generic_hole(gamma: &mut Gamma, ty: TyId, span: Span) -> bool {
    let r = resolve(gamma, ty, span);
    let node = *gamma.get(r);
    match node {
        TypeExpr::Infer(..) | TypeExpr::Generic(..) => true,
        TypeExpr::Nullable(inner) => ty_has_generic_hole(gamma, r.with_idx(inner), span),
        TypeExpr::App { params, .. } | TypeExpr::Tuple { elements: params } => params
            .into_iter()
            .any(|c| ty_has_generic_hole(gamma, r.with_idx(c), span)),
        TypeExpr::Function { params, ret } => {
            ty_has_generic_hole(gamma, r.with_idx(ret), span)
                || params
                    .into_iter()
                    .any(|c| ty_has_generic_hole(gamma, r.with_idx(c), span))
        }
        TypeExpr::Builtin(_) => false,
    }
}

/// Genericity penalty of a param, measured BEFORE unification binds its vars: a
/// bare hole (`T`) costs 2, a ctor/tuple/fn CONTAINING a hole (`[T]`, `Wrap<T>`,
/// `(T, int)`) costs 1, a fully concrete param costs 0. Grades overload
/// specificity -- `int` beats `[T]` beats `T` -- so a concrete overload always
/// outranks a catch-all that also fits. The `Iterator`-bounded case is one
/// instance (a bounded `it: I` is a bare hole -> 2), so bounds add no separate
/// cost.
fn param_genericity(gamma: &mut Gamma, param: TyId, span: Span) -> usize {
    let p = resolve(gamma, param, span);
    if matches!(*gamma.get(p), TypeExpr::Infer(..) | TypeExpr::Generic(..)) {
        return 2;
    }
    if ty_has_generic_hole(gamma, p, span) {
        1
    } else {
        0
    }
}

pub(crate) fn fit_cost(gamma: &mut Gamma, arg: TyId, param: TyId, span: Span) -> Option<usize> {
    // Measured before the trial: a successful unify binds the param's holes, so
    // its genericity has to be read here or it always looks concrete afterward.
    let penalty = param_genericity(gamma, param, span);
    let snap = gamma.out.checkpoint();

    if gamma.trial(|g| unify(g, arg, param, span).is_ok()) {
        return Some(penalty);
    }

    gamma.out.rollback_to(snap);

    let a = resolve(gamma, arg, span);
    let p = resolve(gamma, param, span);

    match (gamma.get(a), gamma.get(p)) {
        (
            TypeExpr::Builtin(BuiltinTy::Int | BuiltinTy::Uint),
            TypeExpr::Builtin(BuiltinTy::Float),
        ) => Some(1),
        (_, TypeExpr::Builtin(BuiltinTy::Any)) => Some(2),
        _ => {
            let p_is_iface = match *gamma.get(p) {
                TypeExpr::App {
                    ctor: TypeCtor::Nominal(n),
                    ..
                } => gamma.workspace.interface_names.contains(&n),
                _ => false,
            };

            // A full field ref carries both capabilities: `FieldRef<T, V>` fits a
            // `ReadFieldRef<T, V>` param (`field_get`) and a `WriteFieldRef<T, V>`
            // one (`field_set`) -- docs/field-refs-plan.md. Scoring has to know it
            // too -- otherwise the coercion only survives the single-candidate
            // bypass in `check_call`, and a second decl of the name (a backend
            // sibling, an unrelated overload) makes every
            // `field_get(o, f: FieldRef<..>)` a no-fit.
            let p_is_single_cap_from_fieldref = match (*gamma.get(a), *gamma.get(p)) {
                (
                    TypeExpr::App { ctor: TypeCtor::Nominal(an), .. },
                    TypeExpr::App { ctor: TypeCtor::Nominal(pn), .. },
                ) => {
                    bone_core::modules::is_intrinsic_key(an, "FieldRef")
                        && bone_core::modules::is_intrinsic_key_in(
                            pn,
                            &["ReadFieldRef", "WriteFieldRef"],
                        )
                }
                _ => false,
            };

            if p_is_iface || p_is_single_cap_from_fieldref {
                let snap = gamma.out.checkpoint();
                if gamma.trial(|g| coerce(g, a, p, span).is_ok()) {
                    return Some(if p_is_iface { 3 } else { 1 });
                }

                gamma.out.rollback_to(snap);
            }

            None
        }
    }
}

/// Why `select_overload` failed: a cost tie between fitting candidates
/// (deferrable when an operand is an unpinned lambda-param var) vs no
/// candidate fitting at all (never deferrable -- an unresolved var unifies
/// with anything, so a no-fit means some OTHER operand rejects everywhere).
#[derive(Debug)]
pub(crate) enum SelectError {
    Tied(CompilerError),
    NoFit(CompilerError),
}

impl From<SelectError> for CompilerError {
    fn from(e: SelectError) -> CompilerError {
        match e {
            SelectError::Tied(e) | SelectError::NoFit(e) => e,
        }
    }
}

/// One candidate's signature for the "No overload" error list: `name(p: T, ..) -> R`.
fn render_candidate_sig(gamma: &Gamma, mid: ModuleId, fid: Idx<Function>) -> String {
    let f = &gamma.workspace.module(mid).functions[fid];
    let params = f
        .param_decls
        .iter()
        .map(|p| format!("{}: {}", p.name, gamma.workspace.render(None, p.ty)))
        .join(", ");
    let ret = f
        .return_ty
        .map(|t| format!(" -> {}", gamma.workspace.render(None, t)))
        .unwrap_or_default();
    format!("{}({}){}", f.name, params, ret)
}

/// Backend-selection rank: a lower rank is the preferred representative when a
/// group of same-signature backend-variant decls is collapsed. `Any` (portable
/// Rock) wins, then `Lua` (native default), then `Js` -- so the luajit-servable
/// variant is what a backend-agnostic pass records, and codegen only has to look
/// for a sibling on the JS side (never the Lua side, which is the common path).
fn backend_rank(b: skull_parser::FnBackend) -> u8 {
    match b {
        skull_parser::FnBackend::Any => 0,
        skull_parser::FnBackend::Lua => 1,
        skull_parser::FnBackend::Js => 2,
    }
}

/// The signature key two decls must share to be backend variants of one another:
/// the family key plus param types (rendered, inference-free) -- the same key
/// `static_owner_tag`-class grouping uses and skull's `backend_variant_group`
/// applies. Return type is deliberately excluded (overload resolution keys on
/// params). The key matters for a merged legacy family, whose members are
/// unrelated definitions: identical signatures there are not backend siblings
/// (spec 8.3) but a tie.
fn signature_key(gamma: &Gamma, mid: ModuleId, fid: Idx<Function>) -> String {
    let f = &gamma.workspace.module(mid).functions[fid];
    std::iter::once(f.name.to_string())
        .chain(f.param_decls.iter().map(|p| gamma.workspace.render(None, p.ty)))
        .join("\u{1}")
}

/// Where a tied candidate comes from, when a tie spans unrelated definitions
/// (a merged legacy family): the v1 DefKey, or the legacy defining file.
fn candidate_origin(gamma: &Gamma, mid: ModuleId, fid: Idx<Function>) -> String {
    let f = &gamma.workspace.module(mid).functions[fid];
    if gamma.workspace.defs.contains_key(&f.name) {
        format!("v1 definition {}", f.name)
    } else {
        format!("legacy definition in {}", gamma.workspace.module(mid).file)
    }
}

/// See the call site in [`select_overload`]. Groups candidates by signature; a
/// group whose backends are all distinct collapses to its lowest-rank member; a
/// group with a repeated backend (a genuine duplicate) is kept whole so the tie
/// still surfaces as an "Ambiguous overload" error.
pub(crate) fn collapse_backend_variants(
    gamma: &Gamma,
    candidates: &[(ModuleId, Idx<Function>)],
) -> Vec<(ModuleId, Idx<Function>)> {
    if candidates.len() <= 1 {
        return candidates.to_vec();
    }
    // Fast path: a cross-backend collapse is only possible when candidates span
    // >= 2 distinct backends. If they all share one backend (the overwhelmingly
    // common case -- an all-`Any` portable overload set, or an all-`Lua`
    // inline(lua)/operator set), no group is ever "all distinct", so the result
    // equals the input. Skip the signature rendering entirely -- this keeps the
    // collapse off the hot overload path unless a backend variant is in play.
    let first_backend = gamma.workspace.module(candidates[0].0).functions[candidates[0].1].backend;
    if candidates
        .iter()
        .all(|&(mid, fid)| gamma.workspace.module(mid).functions[fid].backend == first_backend)
    {
        return candidates.to_vec();
    }
    // Preserve input order for determinism; group indices by signature key.
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, &(mid, fid)) in candidates.iter().enumerate() {
        let key = signature_key(gamma, mid, fid);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(i),
            None => groups.push((key, vec![i])),
        }
    }
    let mut out = Vec::with_capacity(candidates.len());
    for (_, idxs) in groups {
        if idxs.len() == 1 {
            out.push(candidates[idxs[0]]);
            continue;
        }
        let backend_of = |i: usize| gamma.workspace.module(candidates[i].0).functions[candidates[i].1].backend;
        let all_distinct = {
            let mut seen: Vec<skull_parser::FnBackend> = Vec::new();
            idxs.iter().all(|&i| {
                let b = backend_of(i);
                if seen.contains(&b) {
                    false
                } else {
                    seen.push(b);
                    true
                }
            })
        };
        if all_distinct {
            // Collapse to the lowest-rank representative (stable: first among ties,
            // but ranks are distinct here so there is no tie).
            let rep = *idxs
                .iter()
                .min_by_key(|&&i| backend_rank(backend_of(i)))
                .unwrap();
            out.push(candidates[rep]);
        } else {
            for &i in &idxs {
                out.push(candidates[i]);
            }
        }
    }
    out
}

pub(crate) fn select_overload(
    c: BodyCtx,
    gamma: &mut Gamma,
    candidates: &[(ModuleId, Idx<Function>)],
    recv_ty: Option<TyId>,
    args: &[Idx<Expr>],
    arg_ty: &FxHashMap<Idx<Expr>, TyId>,
    arg_tys_msg: &[TyId],
    name: Ustr,
    span: Span,
) -> Result<(ModuleId, Idx<Function>), SelectError> {
    select_overload_with_generics(
        c,
        gamma,
        candidates,
        recv_ty,
        args,
        arg_ty,
        arg_tys_msg,
        IdxRange::new_empty(),
        name,
        span,
    )
}

pub(crate) fn select_overload_with_generics(
    c: BodyCtx,
    gamma: &mut Gamma,
    candidates: &[(ModuleId, Idx<Function>)],
    recv_ty: Option<TyId>,
    args: &[Idx<Expr>],
    arg_ty: &FxHashMap<Idx<Expr>, TyId>,
    arg_tys_msg: &[TyId],
    generic_args: IdxRange<TypeExpr>,
    name: Ustr,
    span: Span,
) -> Result<(ModuleId, Idx<Function>), SelectError> {
    // Collapse backend-variant decls (same name, same signature, different
    // `backend`) to ONE representative BEFORE scoring, so a same-name pair like
    // `extern(ffi,..) fn f` + `extern(js) fn f` never scores as a tie. Each
    // codegen backend later selects its own variant. Genuine same-signature
    // same-backend duplicates are NOT collapsed -- they still tie -> "Ambiguous
    // overload", the error the plan requires them to keep. Different signatures
    // stay distinct overloads (normal overload resolution).
    let collapsed = collapse_backend_variants(gamma, candidates);
    let candidates: &[(ModuleId, Idx<Function>)] = &collapsed;

    let mut best_cost: Option<usize> = None;
    // Every candidate at `best_cost`, so a tie can NAME the tied signatures.
    let mut best_set: SmallVec<[(ModuleId, Idx<Function>); 4]> = SmallVec::new();
    // The bound-conformance failure of an otherwise-fitting `Iterator`-bounded
    // candidate (params fit, bounds did not). When EVERY candidate rejects,
    // this is the near-miss the user needs to see -- the generic "No overload"
    // headline alone hides e.g. "EnumIter's `next` yields multiple values".
    let mut bound_fail: Option<CompilerError> = None;

    for &(mid, fid) in candidates {
        if let Some(cost) = score_candidate_with_generics(
            c,
            gamma,
            mid,
            fid,
            recv_ty,
            args,
            arg_ty,
            generic_args,
            span,
            &mut bound_fail,
        )
        {
            match best_cost {
                None => {
                    best_cost = Some(cost);
                    best_set.clear();
                    best_set.push((mid, fid));
                }
                Some(bc) if cost < bc => {
                    best_cost = Some(cost);
                    best_set.clear();
                    best_set.push((mid, fid));
                }
                Some(bc) if cost == bc => {
                    best_set.push((mid, fid));
                }
                _ => {}
            }
        }
    }

    match best_set.len() {
        1 => Ok(best_set[0]),
        n if n > 1 => {
            let fn_name = |&(mid, fid): &(ModuleId, Idx<Function>)| gamma.workspace.module(mid).functions[fid].name;
            let spans_families = best_set.iter().any(|c| fn_name(c) != fn_name(&best_set[0]));
            let tied = best_set
                .iter()
                .map(|&(mid, fid)| {
                    let sig = render_candidate_sig(gamma, mid, fid);
                    if spans_families {
                        format!("  {sig} ({})", candidate_origin(gamma, mid, fid))
                    } else {
                        format!("  {sig}")
                    }
                })
                .join("\n");
            Err(SelectError::Tied(CompilerError::new_with_label(
                span,
                format!(
                    "Ambiguous overload for `{name}` with argument types ({})\ntied candidates (equally-ranked matches):\n{tied}",
                    arg_tys_msg
                        .iter()
                        .map(|t| gamma.workspace.render(Some(&gamma.out), *t))
                        .join(", ")
                ),
            )))
        }

        _ => {
            let args_msg = arg_tys_msg
                .iter()
                .map(|t| gamma.workspace.render(Some(&gamma.out), *t))
                .join(", ");
            let overloads = candidates
                .iter()
                .map(|&(mid, fid)| format!("  {}", render_candidate_sig(gamma, mid, fid)))
                .join("\n");
            Err(SelectError::NoFit(CompilerError::new_with_label(
                span,
                match bound_fail {
                    Some(be) => format!(
                        "No overload for `{name}` with argument types ({args_msg}); the `Iterator`-bounded candidate was rejected: {}\navailable overloads:\n{overloads}",
                        be.message
                    ),
                    None => format!(
                        "No overload for `{name}` with argument types ({args_msg})\navailable overloads:\n{overloads}"
                    ),
                },
            )))
        }
    }
}

pub(crate) fn check_positional_args(
    c: BodyCtx,
    gamma: &mut Gamma,
    declared: &[TyId],
    args: IdxRange<Expr>,
    num_required: usize,
    subs: &FxHashMap<VarId, TyId>,
    span: Span,
    what: &str,
) -> Result<(), CompilerError> {
    // if args.len() != declared.len() {
    //     comp_error!(
    //         span,
    //         "Expected {} arguments in {}, but got {}",
    //         declared.len(),
    //         what,
    //         args.len()
    //     );
    // }
    //
    // for (arg, dty) in args.zip(declared.iter()) {
    //     let aspan = c.body.expr_spans[arg];
    //     let pty = substitute(gamma, *dty, subs, span)?;
    //     let aty = infer_expr(c, gamma, arg)?;
    //
    //     coerce(gamma, aty, pty, aspan)?;
    // }

    let params: Vec<TyId> = declared
        .iter()
        .map(|d| substitute(gamma, *d, subs, span))
        .collect::<Result<_, _>>()?;

    check_args(c, gamma, &params, args, num_required, span, what)
}

pub(crate) fn check_named_fields(
    c: BodyCtx,
    gamma: &mut Gamma,
    struct_key: Option<Ustr>,
    fields: &[StructFieldDef],
    provided: &[(Ustr, Idx<Expr>)],
    subs: &FxHashMap<VarId, TyId>,
    span: Span,
    what: &str,
    // A functional-update base (`{ ..base }`) copies every unwritten field, so
    // missing-field errors are suppressed when a base is present.
    base_covers_all: bool,
) -> Result<(), CompilerError> {
    let mut seen: Vec<Ustr> = Vec::new();

    for (fname, fvalue) in provided {
        let vspan = c.body.expr_spans[*fvalue];

        let Some(fdef) = fields.iter().find(|f| f.name == *fname) else {
            comp_error!(vspan, "{} has no field named `{}`", what, fname);
        };

        if seen.contains(fname) {
            comp_error!(vspan, "Duplicate field `{}` in {} literal", fname, what);
        }

        if let Some(skey) = struct_key {
            let site = gamma.access_site();
            field_access(gamma, skey, *fname, site, FieldAccessNeed::Write)
                .map_err(|e| e.to_compiler_error(vspan))?;
        }

        seen.push(*fname);

        let fty = substitute(gamma, fdef.ty, subs, vspan)?;
        check_expr(c, gamma, *fvalue, fty, vspan)?
    }

    if base_covers_all {
        if let Some(skey) = struct_key {
            let site = gamma.access_site();
            for f in fields {
                if !seen.contains(&f.name) {
                    field_access(gamma, skey, f.name, site, FieldAccessNeed::Read)
                        .map_err(|e| e.to_compiler_error(span))?;
                    field_access(gamma, skey, f.name, site, FieldAccessNeed::Write)
                        .map_err(|e| e.to_compiler_error(span))?;
                }
            }
        }
    } else {
        for f in fields {
            if !seen.contains(&f.name) && !f.has_default {
                comp_error!(span, "Missing field `{}` in {} literal", f.name, what);
            }
        }
    }

    Ok(())
}

pub(crate) fn score_candidate_with_generics(
    c: BodyCtx,
    gamma: &mut Gamma,
    mid: ModuleId,
    fid: Idx<Function>,
    recv_ty: Option<TyId>,
    args: &[Idx<Expr>],
    arg_ty: &FxHashMap<Idx<Expr>, TyId>,
    generic_args: IdxRange<TypeExpr>,
    span: Span,
    bound_fail: &mut Option<CompilerError>,
) -> Option<usize> {
    let snap = gamma.out.checkpoint();

    let f = &gamma.workspace.module(mid).functions[fid];

    let mut declared: SmallVec<[TyId; 8]> = SmallVec::with_capacity(c.body.param_bindings.len());
    let mut pnames: SmallVec<[Ustr; 8]> = SmallVec::with_capacity(c.body.param_bindings.len());

    for p in f.param_decls.iter() {
        declared.push(p.ty);
        pnames.push(p.name);
    }

    let bounded_vars = bounded_generic_vars(gamma, f);

    let subs = match instantiate_call_generics(gamma, f, generic_args, span) {
        Ok((s, _)) => s,
        Err(_) => {
            gamma.out.rollback_to(snap);
            return None;
        }
    };

    let offset = if recv_ty.is_some() { 1 } else { 0 };
    if offset > declared.len() {
        gamma.out.rollback_to(snap);

        return None;
    }

    // if arg_tys.len() < f.num_required || arg_tys.len() > declared.len() {
    //     gamma.out.substitutions = outer;
    //
    //     return None;
    // }

    let mut total = 0usize;

    // Receiver (fills slot 0) - named args never name it
    if let Some(r) = recv_ty {
        let pty = if subs.is_empty() {
            declared[0]
        } else {
            match substitute(gamma, declared[0], &subs, span) {
                Ok(t) => t,
                Err(_) => {
                    gamma.out.rollback_to(snap);
                    return None;
                }
            }
        };

        let db = declared_is_bounded_generic(gamma, declared[0], &bounded_vars);
        match fit_cost_with_iter_coercion(c, gamma, r, pty, db, span) {
            Some(c) => total += c,
            None => {
                gamma.out.rollback_to(snap);
                return None;
            }
        }
    }

    // Map named + positional args onto THIS candidate's slots

    let slots = match reorder_call_args(
        c,
        gamma,
        args,
        &pnames[offset..],
        f.num_required.saturating_sub(offset),
        span,
    ) {
        Ok(s) => s,
        Err(_) => {
            gamma.out.rollback_to(snap);
            return None;
        }
    };

    for (i, slot) in slots.iter().enumerate() {
        let pty = if subs.is_empty() {
            declared[offset + i]
        } else {
            match substitute(gamma, declared[offset + i], &subs, span) {
                Ok(t) => t,
                Err(_) => {
                    gamma.out.rollback_to(snap);
                    return None;
                }
            }
        };

        match slot {
            Some(fill) => {
                let aty = match fill {
                    SlotFill::Direct(arg) => match arg_ty.get(arg) {
                        Some(&t) => t,
                        None => {
                            gamma.out.rollback_to(snap);
                            return None;
                        }
                    },
                    SlotFill::SpreadField(_, fty) => *fty,
                };

                let db =
                    declared_is_bounded_generic(gamma, declared[offset + i], &bounded_vars);
                let pty_r = resolve(gamma, pty, span);
                let contextual_uint = matches!(fill, SlotFill::Direct(arg)
                    if matches!(c.body.expr_arena[*arg], Expr::Int(value)
                        if (0..=u32::MAX as i64).contains(&value))
                    && matches!(gamma.get(pty_r), TypeExpr::Builtin(BuiltinTy::Uint)));
                match if contextual_uint {
                    Some(1)
                } else {
                    fit_cost_with_iter_coercion(c, gamma, aty, pty, db, span)
                } {
                    Some(c) => total += c,
                    None => {
                        gamma.out.rollback_to(snap);
                        return None;
                    }
                }
            }

            None => total += 1, // omitted default param
        }
    }

    // An `Iterator`-bounded candidate only fits if its bounds hold for this
    // instantiation: a bare `it: I` unifies with ANY arg (`[int]`, Arena, ..),
    // so without this check a bounded combinator would tie with -- or shadow --
    // the concrete overloads (`map([T], f)`, `map(Arena<T>, f)`).
    let f = &gamma.workspace.module(mid).functions[fid];
    if f.generic_bounds.iter().any(Option::is_some) {
        let mut tree = BoundTree::default();
        let mut done = FxHashSet::default();
        let mut berr: Option<CompilerError> = None;
        let conforms = gamma.trial(|g| {
            match check_iterator_bounds(c, g, f, &subs, &mut tree, &mut done, false, 0, span) {
                Ok(()) => true,
                Err(e) => {
                    berr = Some(e);
                    false
                }
            }
        });
        if !conforms {
            // Keep the FIRST bound failure for the all-rejected diagnostic.
            if bound_fail.is_none() {
                *bound_fail = berr;
            }
            gamma.out.rollback_to(snap);
            return None;
        }
        // A bounded `it: I` is a bare generic hole, so `param_genericity` already
        // ranked it below any concrete overload -- no separate bounds cost here.
    }

    // for (dty, &aty) in declared.iter().zip(arg_tys) {
    //     let pty = match substitute(gamma, *dty, &subs, span) {
    //         Ok(t) => t,
    //         Err(_) => {
    //             gamma.out.substitutions = outer;
    //             return None;
    //         }
    //     };
    //
    //     match fit_cost(gamma, aty, pty, span) {
    //         Some(c) => total += c,
    //         None => {
    //             gamma.out.substitutions = outer;
    //             return None;
    //         }
    //     }
    // }
    //
    // total += declared.len() - arg_tys.len();

    gamma.out.rollback_to(snap);
    Some(total)
}

fn instantiate_call_generics(
    gamma: &mut Gamma,
    function: &Function,
    generic_args: IdxRange<TypeExpr>,
    span: Span,
) -> Result<(FxHashMap<VarId, TyId>, Vec<TyId>), CompilerError> {
    if generic_args.is_empty() {
        return gamma.instantiate_generics(&function.generics, span);
    }
    if generic_args.len() != function.generics.len() {
        return Err(CompilerError::new_with_label(
            span,
            format!(
                "function `{}` expects {} explicit type argument(s), got {}",
                function.name,
                function.generics.len(),
                generic_args.len()
            ),
        ));
    }
    let mut subs = FxHashMap::default();
    let mut args = Vec::with_capacity(generic_args.len());
    for (&generic, explicit) in function.generics.iter().zip(generic_args) {
        let TypeExpr::Generic(var_id, _) = *gamma.get(generic) else {
            return Err(CompilerError::new_with_label(
                span,
                format!("function `{}` has a malformed generic declaration", function.name),
            ));
        };
        let ty = TyId::Local(explicit);
        subs.insert(var_id, ty);
        args.push(ty);
    }
    Ok((subs, args))
}

pub(crate) fn check_args(
    c: BodyCtx,
    gamma: &mut Gamma,
    params: &[TyId],
    args: IdxRange<Expr>,
    num_required: usize,
    span: Span,
    what: &str,
) -> Result<(), CompilerError> {
    if args.len() < num_required || args.len() > params.len() {
        comp_error!(
            span,
            "Expected {} to {} arguments in {}, but got {}",
            num_required,
            params.len(),
            what,
            args.len()
        );
    }

    for (arg, pty) in args.zip(params.iter()) {
        let aspan = c.body.expr_spans[arg];
        check_expr(c, gamma, arg, *pty, aspan)?;
    }

    Ok(())
}

/// How a callee parameter slot is filled after `..spread` expansion.
#[derive(Clone, Copy)]
pub(crate) enum SlotFill {
    /// A written positional or named argument.
    Direct(Idx<Expr>),
    /// A field pulled from a spread operand: (operand expr, field type). The
    /// param slot index encodes which field; skull-mir re-derives the name.
    SpreadField(Idx<Expr>, TyId),
}

/// The (name, type) of every field of a call-site spread operand. The operand
/// must be a nominal struct with statically known fields (rule: not tuple, not
/// array, not Map/any).
pub(crate) fn spread_struct_fields(
    gamma: &mut Gamma,
    op_ty: TyId,
    span: Span,
) -> Result<Vec<(Ustr, TyId)>, CompilerError> {
    let r = resolve(gamma, op_ty, span);

    match *gamma.get(r) {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } => {
            let sid = resolve_struct(gamma, name).ok_or_else(|| {
                CompilerError::new_with_label(span, format!("cannot spread `{}`: not a struct", name))
            })?;

            let def = gamma.workspace.module(sid.module).structs[sid.def].clone();
            let subs = struct_arg_subs(gamma, &def, r, params);
            def.fields
                .iter()
                .map(|f| Ok((f.name, substitute(gamma, f.ty, &subs, span)?)))
                .collect()
        }

        _ => comp_error!(
            span,
            "cannot spread `{}` into call arguments: expected a nominal struct",
            gamma.workspace.render(Some(&gamma.out), r)
        ),
    }
}

pub(crate) fn reorder_call_args(
    c: BodyCtx,
    gamma: &mut Gamma,
    args: &[Idx<Expr>],
    param_names: &[Ustr],
    num_required: usize,
    span: Span,
) -> Result<Vec<Option<SlotFill>>, CompilerError> {
    let mut slots: Vec<Option<SlotFill>> = vec![None; param_names.len()];
    let mut pos = 0usize;

    for &arg in args {
        // Struct spread `..op`: each field of the operand fills the param slot
        // of the same name (rules 2/4/5).
        if c.body.spread_args.contains(&arg) {
            let op_ty = infer_expr(c, gamma, arg)?;
            let aspan = c.body.expr_spans[arg];
            for (fname, fty) in spread_struct_fields(gamma, op_ty, aspan)? {
                let pi = param_names.iter().position(|p| *p == fname).ok_or_else(|| {
                    CompilerError::new_with_label(
                        span,
                        format!("spread field `{}` names no parameter of the callee", fname),
                    )
                })?;
                match slots[pi] {
                    // rule 5: two spreads supplying the same param.
                    Some(SlotFill::SpreadField(..)) => {
                        comp_error!(span, "two spreads both supply parameter `{}`", fname)
                    }
                    // rule 4: a named/positional arg written BEFORE the spread.
                    Some(SlotFill::Direct(_)) => comp_error!(
                        span,
                        "parameter `{}` is given before the spread that also supplies it",
                        fname
                    ),
                    None => slots[pi] = Some(SlotFill::SpreadField(arg, fty)),
                }
            }
            continue;
        }

        match c.body.arg_names.get(&arg) {
            // positional
            None => {
                if pos >= param_names.len() {
                    comp_error!(
                        span,
                        "Too many positional arguments, expected at most {}",
                        param_names.len()
                    );
                }

                if slots[pos].is_some() {
                    comp_error!(span, "Multiple values provided for parameter `{}`", param_names[pos]);
                }
                slots[pos] = Some(SlotFill::Direct(arg));
                pos += 1;
            }

            Some(nm) => {
                let pi = param_names.iter().position(|p| p == nm).ok_or_else(|| {
                    CompilerError::new_with_label(span, format!("unknown parameter `{}`", nm))
                })?;

                match slots[pi] {
                    // rule 3: a named arg AFTER a spread overrides that field.
                    Some(SlotFill::SpreadField(..)) | None => {
                        slots[pi] = Some(SlotFill::Direct(arg))
                    }
                    Some(SlotFill::Direct(_)) => {
                        comp_error!(span, "Multiple values provided for parameter `{}`", nm)
                    }
                }
            }
        }
    }

    for i in 0..num_required {
        if slots[i].is_none() {
            comp_error!(
                span,
                "Missing required argument for parameter `{}`",
                param_names[i]
            );
        }
    }

    Ok(slots)
}

pub(crate) fn spread_constituents(
    gamma: &mut Gamma,
    op_ty: TyId,
    span: Span,
) -> Result<Vec<TyId>, CompilerError> {
    let r = resolve(gamma, op_ty, span);

    match *gamma.get(r) {
        TypeExpr::Tuple { elements } => Ok(elements.into_iter().map(|e| r.with_idx(e)).collect()),
        TypeExpr::App {
            ctor: TypeCtor::Array,
            params,
        } => {
            let e = params.into_iter().next().ok_or_else(|| {
                CompilerError::new_with_label(
                    span,
                    "Array type missing element type parameter".to_string(),
                )
            })?;

            Ok(vec![r.with_idx(e)])
        }

        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } => {
            let sid = resolve_struct(gamma, name).ok_or_else(|| {
                CompilerError::new_with_label(span, format!("cannot spread `{}`", name))
            })?;

            let def = gamma.workspace.module(sid.module).structs[sid.def].clone();
            // mirror struct_field_ty; empty for generics
            let subs = struct_arg_subs(gamma, &def, r, params);
            def.fields
                .iter()
                .map(|f| substitute(gamma, f.ty, &subs, span))
                .collect()
        }

        _ => comp_error!(
            span,
            "Cannot spread type {}, expected a tuple, array, or nominal struct",
            gamma.workspace.render(Some(&gamma.out), r)
        ),
    }
}

pub(crate) fn resolve_named_call(
    gamma: &mut Gamma,
    c: BodyCtx,
    family: CallFamily,
    recv_override: Option<TyId>,
    args: IdxRange<Expr>,
    call_expr: Idx<Expr>,
    span: Span,
) -> Result<TyId, CompilerError> {
    resolve_named_call_with_generics(
        gamma,
        c,
        family,
        recv_override,
        args,
        IdxRange::new_empty(),
        call_expr,
        span,
    )
}

pub(crate) fn resolve_named_call_with_generics(
    gamma: &mut Gamma,
    c: BodyCtx,
    family: CallFamily,
    recv_override: Option<TyId>,
    args: IdxRange<Expr>,
    generic_args: IdxRange<TypeExpr>,
    call_expr: Idx<Expr>,
    span: Span,
) -> Result<TyId, CompilerError> {
    let name = family.key();
    let res = resolve_named_call_inner(
        gamma,
        c,
        family,
        recv_override,
        args,
        generic_args,
        call_expr,
        span,
    );

    let Err(e) = res else { return res };
    if gamma.quiet {
        return Err(e);
    }

    // `recv.name(...)` that failed to resolve: if `recv` has a field `name`
    // that isn't callable, the `()` is almost certainly a slip (a field read
    // written as a call). Gate on the failure being about the receiver or the
    // call itself, so a later-arg mismatch under a legit UFCS call keeps its
    // real error.
    let (recv_ty, recv_span) = match recv_override {
        Some(r) => (resolve(gamma, r, span), None),
        None if !args.is_empty() && c.body.method_call_recvs.contains(&args.start()) => {
            match infer_expr(c, gamma, args.start()) {
                Ok(t) => (resolve(gamma, t, span), Some(c.body.expr_spans[args.start()])),
                Err(_) => return Err(e),
            }
        }
        None => return Err(e),
    };

    let err_span = e.labels.first().map(|l| l.span);
    let about_receiver = err_span == Some(span) || (recv_span.is_some() && err_span == recv_span);
    if about_receiver {
        if let Ok(fty) = struct_field_ty(gamma, recv_ty, name, span) {
            let fty = resolve(gamma, fty, span);
            if !matches!(gamma.get(fty), TypeExpr::Function { .. }) {
                let recv_str = gamma.workspace.render(Some(&gamma.out), recv_ty);
                let fty_str = gamma.workspace.render(Some(&gamma.out), fty);
                return Err(CompilerError::new_with_label(
                    span,
                    format!(
                        "`{name}` is a field of `{recv_str}` (type `{fty_str}`), not a method -- remove the `()` to read the field"
                    ),
                ));
            }
        }
    }

    Err(e)
}

/// Generic-param indices of `fid` that its body compares with `==`/`!=` via a
/// directly-referenced binding of that generic type. The direct fn-eq ban is a
/// static-type check at the `==` site (bone-inference Eq arm); a `T`-typed
/// wrapper (`fn same<T>(a: T, b: T) { a == b }`) erases the fn type and slips
/// past it. Recording these indices lets the call site reject when such a `T`
/// resolves to a fn type (docs/closure-env-records-plan.md, KNOWN HOLE).
///
/// A pure syntactic read of the callee's already-lowered body -- no inference
/// state, so it is independent of the (parallel, cross-arena) inference order.
/// `VarId` is the global generic identity, comparable across arenas even though
/// `TyId`s are arena-scoped. Only DIRECT `T`-typed binding operands are caught;
/// an operand like `mk_t() == mk_t()` (no `T`-typed binding) is missed, pinned
/// as the any-cast bypass is. Transitive forwarding through a generic-to-generic
/// call is a separate hole (tests/errors/fn_identity_generic_transitive.rock.err).
pub(crate) fn eq_compared_generic_indices(
    ws: &Workspace,
    mid: ModuleId,
    fid: Idx<Function>,
) -> SmallVec<[usize; 2]> {
    let mut out: SmallVec<[usize; 2]> = SmallVec::new();
    let module = ws.module(mid);
    let function = &module.functions[fid];
    if function.generics.is_empty() {
        return out;
    }
    // inline(lua)/extern fns have no Rock body to scan -- trusted.
    let Some(body) = module.bodies.get(&BodyOwner::Function(fid)) else {
        return out;
    };

    let mut gen_index: SmallVec<[(VarId, usize); 4]> = SmallVec::new();
    for (i, &g) in function.generics.iter().enumerate() {
        if let TypeExpr::Generic(var_id, _) = peek_ty(ws, body, g) {
            gen_index.push((var_id, i));
        }
    }
    if gen_index.is_empty() {
        return out;
    }

    let operand_generic = |operand: Idx<Expr>| -> Option<usize> {
        let Expr::Var(bid) = body.expr_arena[operand] else {
            return None;
        };
        let decl = body.binding_arena[bid].declared_ty?;
        let TypeExpr::Generic(var_id, _) = peek_ty(ws, body, decl) else {
            return None;
        };
        gen_index.iter().find(|(v, _)| *v == var_id).map(|(_, idx)| *idx)
    };

    for e in body.expr_arena.values() {
        if let Expr::BinOp {
            op: hir::BinOp::Eq | hir::BinOp::NotEq,
            lhs,
            rhs,
        } = *e
        {
            for operand in [lhs, rhs] {
                if let Some(idx) = operand_generic(operand) {
                    if !out.contains(&idx) {
                        out.push(idx);
                    }
                }
            }
        }
    }
    out
}

pub(crate) fn resolve_named_call_inner(
    gamma: &mut Gamma,
    c: BodyCtx,
    family: CallFamily,
    recv_override: Option<TyId>,
    args: IdxRange<Expr>,
    generic_args: IdxRange<TypeExpr>,
    call_expr: Idx<Expr>,
    span: Span,
) -> Result<TyId, CompilerError> {
    let name = family.key();
    let _span = tracing::info_span!("named_call", name = %name).entered();
    let all_candidates = resolve_callee(gamma, &family);
    let candidates: Vec<_> = if generic_args.is_empty() {
        all_candidates.to_vec()
    } else {
        all_candidates
            .into_iter()
            .filter(|(mid, fid)| gamma.workspace.module(*mid).functions[*fid].generics.len() == generic_args.len())
            .collect()
    };

    // Method-syntax `recv.name(args)` / `recv?.name(args)`:
    // - a fn-typed FIELD on the receiver wins over free fns (UFCS is only a fallback)
    // - recv_override is Some exactly for SafeNavCall
    // - plain method path carries receiver as args[0]
    let method_recv: Option<TyId> = match recv_override {
        Some(r) => Some(resolve(gamma, r, span)),

        None if !args.is_empty() && c.body.method_call_recvs.contains(&args.start()) => {
            let recv_ty = infer_expr(c, gamma, args.start())?;
            Some(resolve(gamma, recv_ty, span))
        }

        None => None,
    };

    if let Some(recv_ty) = method_recv {
        if let Ok(field_ty) = struct_field_ty(gamma, recv_ty, name, span) {
            let field_ty = resolve(gamma, field_ty, span);

            if matches!(gamma.get(field_ty), TypeExpr::Function { .. }) {
                let call_args = if recv_override.is_some() {
                    // safe-nav: receiver isn't in args, pass them as is
                    args
                } else {
                    // plain method call ... drop the receiver
                    let r = args.make_range();

                    IdxRange::new_exclusive(
                        Idx::from_raw(RawIdx::from_u32(r.start + 1))
                            ..Idx::from_raw(RawIdx::from_u32(r.end)),
                    )
                };

                return call_fn_value(c, gamma, field_ty, call_args, span);
            }
        }
    }

    // Bound-calls: `next(x)` / `iter(x)` (either spelling incl. UFCS) where
    // `x: I` and the enclosing fn declares `I: Iterator<E>`. There is no
    // concrete callee at check time -- the body is checked ONCE against the
    // bound; skull lowers these as placeholders and the instantiation-time
    // BoundTree substitutes the concrete `next` per splice. `iter(x)` is
    // IDENTITY (types as `I`, lowers to the argument, no call).
    if !gamma.fn_bounds.is_empty()
        && recv_override.is_none()
        && args.len() == 1
        && matches!(name.as_str(), "next" | "iter")
    {
        let aty = infer_expr(c, gamma, args.start())?;
        let aty = resolve(gamma, aty, span);
        if let TypeExpr::Generic(var_id, _) = *gamma.get(aty) {
            if let Some((idx, elem)) = gamma.fn_bound_for(var_id) {
                if name.as_str() == "iter" {
                    gamma.out.bound_identity_calls.insert(call_expr);
                    return Ok(aty);
                }
                gamma.out.bound_calls.insert(call_expr, idx);
                let b = gamma.workspace.builtins.bool();
                return Ok(tuple_of(gamma, &[b, elem], span));
            }
        }
    }

    if candidates.is_empty() {
        if recv_override.is_none() {
            if let Some(gty) = resolve_global(gamma, name) {
                let gty = resolve(gamma, gty, span);

                if matches!(gamma.get(gty), TypeExpr::Function { .. }) {
                    return call_fn_value(c, gamma, gty, args, span);
                }
            }
        }

        if recv_override.is_none() {
            if let Some(NewtypeId { module: ntm, def }) = resolve_newtype(gamma, name) {
                let nt = gamma.workspace.module(ntm).newtypes[def].clone();
                let (subs, fresh) = gamma.instantiate_generics(&nt.generics, span)?;

                if args.len() != 1 {
                    comp_error!(
                        span,
                        "newtype `{}` constructor expects exactly 1 argument, got {}",
                        name,
                        args.len()
                    );
                }

                // TODO: do tests still pass without this?
                for arg in args.into_iter() {
                    let aspan = c.body.expr_spans[arg];
                    let aty = infer_expr(c, gamma, arg)?;
                    let w_sub = substitute(gamma, nt.wrapped, &subs, aspan)?;

                    coerce(gamma, aty, w_sub, aspan)?;
                }

                let arg_tys: Vec<TyId> = fresh.iter().map(|&f| resolve(gamma, f, span)).collect();
                let result = gamma.nominal_app(name, &arg_tys, span);

                return Ok(resolve(gamma, result, span));
            }
        }

        // Unary-operator desugars (`!x` -> `__not`, `-x` -> `__unm`) with an
        // empty overload set read as operator errors naming the operand type,
        // not as a missing internal function.
        let op_symbol = match name.as_str() {
            "__not" => Some("!"),
            "__unm" => Some("-"),
            _ => None,
        };
        if let Some(sym) = op_symbol {
            let operand = args
                .into_iter()
                .next()
                .map(|a| -> Result<String, CompilerError> {
                    let t = infer_expr(c, gamma, a)?;
                    let t = resolve(gamma, t, span);
                    Ok(gamma.workspace.render(Some(&gamma.out), t))
                })
                .transpose()?
                .unwrap_or_else(|| "<no operand>".to_string());
            comp_error!(
                span,
                "operator `{}` is not defined for `{}`",
                sym,
                operand
            );
        }

        // A v1 DefKey renders as its declared name (plan 2.1); legacy keys
        // ARE the name. When a losing tier held a candidate of the same
        // spelling, name it: spec 8.3 forbids the fallthrough, so the user
        // has to import it explicitly.
        let shown = gamma.workspace.defs.get(&name).map(|m| m.name).unwrap_or(name);
        if let Some(hidden) = family.hidden() {
            comp_error!(
                span,
                "Function `{}` doesn't exist -- `{}` binds to `{}` here, and a \
                 shadowed candidate `{}` needs an explicit import to be called",
                shown,
                shown,
                name,
                hidden
            );
        }
        comp_error!(span, "Function `{}` doesn't exist", shown);
    }

    // let (callee_mid, callee_fn) = resolve_callee(gamma, name).ok_or_else(|| {
    //     CompilerError::new_with_label(
    //         span,
    //         format!("Function `{}` doens't exist", name),
    //     )
    // })?;

    let mut preinferred: FxHashMap<Idx<Expr>, TyId> = FxHashMap::default();

    let (callee_mid, callee_fn) = if candidates.len() == 1 {
        candidates[0]
    } else {
        let mut arg_tys: Vec<TyId> = Vec::new();
        let mut arg_ty: FxHashMap<Idx<Expr>, TyId> = FxHashMap::default();
        let mut arg_list: Vec<Idx<Expr>> = Vec::new();

        if let Some(r) = recv_override {
            arg_tys.push(resolve(gamma, r, span));
        }

        for a in args {
            // A `.field` arg needs a FieldRef expected type to infer, so like a
            // lambda it gets a fresh var (fits any candidate, stops
            // discriminating) and stays out of `preinferred` -- so post-selection
            // it falls through to `check_expr` against the winner's param.
            let t = if matches!(c.body.expr_arena[a], Expr::Lambda { .. }) {
                gamma.fresh_callsite_var(*Q_LAM, span)
            } else if matches!(c.body.expr_arena[a], Expr::SiblingRef(_)) {
                gamma.fresh_callsite_var(*Q_FIELDREF, span)
            } else {
                let t = infer_expr(c, gamma, a)?;
                let t = resolve(gamma, t, span);
                preinferred.insert(a, t);
                t
                // Ok::<_, CompilerError>(t)
            };

            arg_tys.push(t);
            arg_ty.insert(a, t);
            arg_list.push(a);
        }

        match select_overload_with_generics(
            c,
            gamma,
            &candidates,
            recv_override,
            &arg_list,
            &arg_ty,
            &arg_tys,
            generic_args,
            name,
            span,
        ) {
            Ok(pick) => pick,

            Err(e) => {
                // Defer-to-use: a cost tie whose operand is an unpinned
                // lambda-param var re-resolves when a later use pins it
                // (defer.rs). Wins over the `__op_*` fallback below -- the
                // deferred path records a real callee once it commits.
                if let SelectError::Tied(_) = e {
                    if !gamma.quiet {
                        if let Some((v, origin)) = deferrable_operand(gamma, &arg_tys, span) {
                            let result = defer_result_var(gamma, origin, span);
                            // The callee's purity is unknown until it
                            // resolves; clear global refinements as any
                            // non-`@pure` call would.
                            gamma.refined_nonnull.retain(|(root, _)| !root.is_global());
                            queue_pending(
                                gamma,
                                v,
                                Pending::Call {
                                    family,
                                    call_expr,
                                    recv_override,
                                    args: arg_list,
                                    arg_ty,
                                    arg_tys,
                                    preinferred,
                                    origin,
                                    span,
                                    result,
                                },
                            );
                            return Ok(result);
                        }
                    }
                }

                if name.as_str().starts_with("__op_") {
                    if let Some(&first) = arg_tys.first() {
                        if arg_tys.iter().all(|&t| {
                            matches!(*gamma.get(t), TypeExpr::Generic(..) | TypeExpr::Infer(..))
                        }) {
                            return Ok(first);
                        }
                    }
                }

                return Err(e.into());
            }
        }
    };

    let arg_list: Vec<Idx<Expr>> = args.into_iter().collect();
    commit_selected_call(
        c,
        gamma,
        name,
        callee_mid,
        callee_fn,
        recv_override,
        &arg_list,
        &preinferred,
        generic_args,
        call_expr,
        span,
    )
}

/// The post-selection half of a named call: record the callee, instantiate
/// its generics, coerce/check every arg against the substituted params, run
/// bound conformance and the fn-eq ban, apply the callee's purity effect, and
/// return the substituted return type. Shared by the eager path above and the
/// pending-constraint retry (defer.rs).
pub(crate) fn commit_selected_call(
    c: BodyCtx,
    gamma: &mut Gamma,
    name: Ustr,
    callee_mid: ModuleId,
    callee_fn: Idx<Function>,
    recv_override: Option<TyId>,
    arg_list: &[Idx<Expr>],
    preinferred: &FxHashMap<Idx<Expr>, TyId>,
    generic_args: IdxRange<TypeExpr>,
    call_expr: Idx<Expr>,
    span: Span,
) -> Result<TyId, CompilerError> {
    gamma
        .out
        .record_resolved_call(call_expr, callee_mid, callee_fn, span)?;

    let ws = gamma.workspace;
    let function = &ws.module(callee_mid).functions[callee_fn];
    let (subs, _arg_tys) = instantiate_call_generics(gamma, function, generic_args, span)?;

    // Caller, for arg exprs
    let callee_module = &ws.module(callee_mid);
    let function = &callee_module.functions[callee_fn];

    // `Iterator`-bounded callee: conformance is (re)tried after every arg
    // unification -- resolving `next(C)` is also what binds the bound's elem
    // generic, and a LATER lambda arg (`map(it, x => ..)`) must see it bound.
    let has_bounds = function.generic_bounds.iter().any(Option::is_some);
    let mut bound_tree = BoundTree::default();
    let mut bounds_done: FxHashSet<usize> = FxHashSet::default();

    let mut param_tys = Vec::with_capacity(c.body.param_bindings.len());
    let mut param_names = Vec::with_capacity(c.body.param_bindings.len());

    for param in function.param_decls.iter() {
        param_tys.push(param.ty);
        param_names.push(param.name);
    }

    // let what = format!("function `{}`", name);
    // check_positional_args(
    //     module,
    //     gamma,
    //     &param_tys,
    //     args,
    //     function.num_required,
    //     &subs,
    //     span,
    //     &what,
    // )?;

    let offset = if let Some(r) = recv_override {
        if param_tys.is_empty() {
            comp_error!(span, "method `{}` has no receiver parameter", name);
        }

        let pty = substitute(gamma, param_tys[0], &subs, span)?;
        coerce(gamma, r, pty, span)?;

        1
    } else {
        0
    };

    let pairs: Vec<(SlotFill, TyId)> = if function.is_variadic {
        let velem = param_tys[param_tys.len() - 1];
        let nfixed = (param_tys.len() - 1).saturating_sub(offset);

        if arg_list.len() < nfixed {
            comp_error!(
                span,
                "`{}` expects at least {} argument(s), got {}",
                name,
                nfixed,
                arg_list.len()
            );
        }

        arg_list
            .iter()
            .enumerate()
            .map(|(i, &arg)| {
                (
                    SlotFill::Direct(arg),
                    if i < nfixed {
                        param_tys[offset + i]
                    } else {
                        velem
                    },
                )
            })
            .collect()
    } else {
        let slots = reorder_call_args(
            c,
            gamma,
            &arg_list,
            &param_names[offset..],
            function.num_required.saturating_sub(offset),
            span,
        )?;

        slots
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.map(|fill| (fill, param_tys[offset + i])))
            .collect()
    };

    let bounded_vars = if has_bounds {
        bounded_generic_vars(gamma, function)
    } else {
        SmallVec::new()
    };

    for (fill, pty_decl) in pairs {
        if has_bounds {
            check_iterator_bounds(
                c, gamma, function, &subs, &mut bound_tree, &mut bounds_done, false, 0, span,
            )?;
        }
        let pty = substitute(gamma, pty_decl, &subs, span)?;
        match fill {
            SlotFill::Direct(arg) => {
                // Implicit iterable->iterator coercion at a bounded param:
                // unify the param with `iter(arg)`'s result and record the
                // insertion for skull to emit. Lambdas can't be iterators and
                // need the expected type to infer, so they take the plain path.
                let mut coerced = false;
                if declared_is_bounded_generic(gamma, pty_decl, &bounded_vars)
                    && !matches!(c.body.expr_arena[arg], Expr::Lambda { .. })
                {
                    let aty = match preinferred.get(&arg) {
                        Some(&t) => t,
                        None => {
                            let t = infer_expr(c, gamma, arg)?;
                            resolve(gamma, t, span)
                        }
                    };
                    if let Some((witness, it_ty)) = implicit_iter_target(c, gamma, aty, span)
                    {
                        coerce(gamma, it_ty, pty, c.body.expr_spans[arg])?;
                        gamma.out.implicit_iter_args.insert(arg, witness);
                        coerced = true;
                    } else {
                        coerce(gamma, aty, pty, c.body.expr_spans[arg])?;
                        coerced = true;
                    }
                }
                if !coerced {
                    match preinferred.get(&arg) {
                        Some(&aty) => coerce(gamma, aty, pty, c.body.expr_spans[arg])?,
                        None => check_expr(c, gamma, arg, pty, c.body.expr_spans[arg])?,
                    }
                }
            }
            // A spread-supplied field: check the field's type against the param.
            SlotFill::SpreadField(op, fty) => {
                coerce(gamma, fty, pty, c.body.expr_spans[op])?
            }
        }
    }

    if has_bounds {
        check_iterator_bounds(
            c, gamma, function, &subs, &mut bound_tree, &mut bounds_done, true, 0, span,
        )?;
        gamma.out.bound_call_trees.insert(call_expr, bound_tree);
    }

    // Close the generic fn-eq bypass: a type param the callee `==`-compares must
    // not be instantiated with a fn type here. The direct ban lives at the `==`
    // site, but a `T`-typed wrapper erases the fn type past it; catch it once the
    // args have unified `subs` to the concrete types. fn identity is not
    // meaningful (docs/closure-env-records-plan.md).
    for idx in eq_compared_generic_indices(ws, callee_mid, callee_fn) {
        let inst = substitute(gamma, function.generics[idx], &subs, span)?;
        let inst = resolve(gamma, inst, span);
        let peeled = peel_nullable(gamma, inst, span);
        if matches!(*gamma.get(peeled), TypeExpr::Function { .. }) {
            comp_error!(
                span,
                "Cannot compare fn values: `{}` compares a generic parameter with `==`/`!=`, but it is instantiated with a fn type here -- fn identity is not meaningful (a capture-free lambda is one shared fn; a capturing lambda is a fresh record per creation). Restructure around explicit keys",
                function.name
            );
        }
    }

    // Persist the instantiation for skull's inliner call-site specialization
    // (docs/flatvec-plan.md). After arg unification, so `subs` points at the
    // concrete types; a still-unbound var stays a var and readers resolve
    // through `substitutions` like `expr_types`.
    if !function.generics.is_empty() {
        let mut bindings = Vec::with_capacity(function.generics.len());
        for &g in function.generics.iter() {
            let inst = substitute(gamma, g, &subs, span)?;
            bindings.push(resolve(gamma, inst, span));
        }
        gamma.out.call_generic_bindings.insert(call_expr, bindings);
    }

    // A non-`@pure` callee may reassign a global, so its call invalidates every
    // global refinement. Args + receiver above were checked while the
    // refinements still held (the call's effect is sequenced after them); a
    // `@pure` callee provably can't write globals (verify_pure_expr), so it
    // preserves them. Done here, after arg checking, so nested impure calls in
    // earlier args have already cleared in evaluation order.
    let callee_pure = fn_has_anno(
        &gamma.workspace.module(callee_mid).functions[callee_fn],
        "pure",
    );

    let ret_ty = function
        .return_ty
        .unwrap_or_else(|| gamma.workspace.builtins.unit());

    if !callee_pure {
        gamma.refined_nonnull.retain(|(root, _)| !root.is_global());
    }

    let ret_ty = substitute(gamma, ret_ty, &subs, span)?;

    Ok(resolve(gamma, ret_ty, span))
}

pub(crate) fn call_fn_value(
    c: BodyCtx,
    gamma: &mut Gamma,
    fn_ty: TyId,
    args: IdxRange<Expr>,
    span: Span,
) -> Result<TyId, CompilerError> {
    let fn_ty = resolve(gamma, fn_ty, span);

    let (params, ret) = match *gamma.get(fn_ty) {
        TypeExpr::Function { params, ret } => (params, ret),
        _ => comp_error!(
            span,
            "Attempted to call a non-function type {}",
            gamma.workspace.render(Some(&gamma.out), fn_ty)
        ),
    };

    let param_tys: Vec<TyId> = params.into_iter().map(|p| fn_ty.with_idx(p)).collect();
    let what = format!(
        "call to value of type {}",
        gamma.workspace.render(Some(&gamma.out), fn_ty)
    );
    check_args(c, gamma, &param_tys, args, param_tys.len(), span, &what)?;

    // A fn-value callee is opaque -- we can't prove it doesn't write globals, so
    // conservatively drop all global refinements (args above were checked while
    // they still held).
    gamma.refined_nonnull.retain(|(root, _)| !root.is_global());

    Ok(resolve(gamma, fn_ty.with_idx(ret), span))
}
