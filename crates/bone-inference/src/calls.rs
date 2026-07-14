use super::*;

pub(crate) fn resolve_callee(gamma: &Gamma, name: Ustr) -> FnList {
    let _span = tracing::info_span!("resolve_callee", name = %name).entered();

    gamma.dep_acc.borrow_mut().insert(name);

    gamma
        .workspace
        .functions_by_name
        .get(&name)
        .cloned()
        .unwrap_or_default()

    // gamma
    //     .workspace
    //     .modules
    //     .iter()
    //     .flat_map(|(mid, m)| {
    //         m.functions
    //             .iter()
    //             .filter(|(_, f)| f.name == name)
    //             .map(|(idx, _)| (*mid, idx))
    //     })
    //     .collect()
}

pub(crate) fn fit_cost(gamma: &mut Gamma, arg: TyId, param: TyId, span: Span) -> Option<usize> {
    let snap = gamma.out.checkpoint();

    if gamma.trial(|g| unify(g, arg, param, span).is_ok()) {
        return Some(0);
    }

    gamma.out.rollback_to(snap);

    let a = resolve(gamma, arg, span);
    let p = resolve(gamma, param, span);

    match (gamma.get(a), gamma.get(p)) {
        (TypeExpr::Builtin(BuiltinTy::Int), TypeExpr::Builtin(BuiltinTy::Float)) => Some(1),
        (_, TypeExpr::Builtin(BuiltinTy::Any)) => Some(2),
        _ => {
            let p_is_iface = match *gamma.get(p) {
                TypeExpr::App {
                    ctor: TypeCtor::Nominal(n),
                    ..
                } => gamma.workspace.interface_names.contains(&n),
                _ => false,
            };

            if p_is_iface {
                let snap = gamma.out.checkpoint();
                if gamma.trial(|g| coerce(g, a, p, span).is_ok()) {
                    return Some(3);
                }

                gamma.out.rollback_to(snap);
            }

            None
        }
    }
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
) -> Result<(ModuleId, Idx<Function>), CompilerError> {
    let mut best: Option<(usize, (ModuleId, Idx<Function>))> = None;
    let mut tied = false;
    // The bound-conformance failure of an otherwise-fitting `Iterator`-bounded
    // candidate (params fit, bounds did not). When EVERY candidate rejects,
    // this is the near-miss the user needs to see -- the generic "No overload"
    // headline alone hides e.g. "EnumIter's `next` yields multiple values".
    let mut bound_fail: Option<CompilerError> = None;

    for &(mid, fid) in candidates {
        if let Some(cost) =
            score_candidate(c, gamma, mid, fid, recv_ty, args, arg_ty, span, &mut bound_fail)
        {
            match best {
                None => {
                    best = Some((cost, (mid, fid)));
                    tied = false;
                }
                Some((bc, _)) if cost < bc => {
                    best = Some((cost, (mid, fid)));
                    tied = false;
                }
                Some((bc, _)) if cost == bc => {
                    tied = true;
                }
                _ => {}
            }
        }
    }

    match best {
        Some((_, pick)) if !tied => Ok(pick),
        Some(_) => comp_error!(
            span,
            "Ambiguous overload for `{name}` with argument types ({})",
            arg_tys_msg
                .iter()
                .map(|t| gamma.workspace.render(Some(&gamma.out), *t))
                .join(", ")
        ),

        None => {
            let args_msg = arg_tys_msg
                .iter()
                .map(|t| gamma.workspace.render(Some(&gamma.out), *t))
                .join(", ");
            match bound_fail {
                Some(be) => comp_error!(
                    span,
                    "No overload for `{name}` with argument types ({args_msg}); the `Iterator`-bounded candidate was rejected: {}",
                    be.message
                ),
                None => comp_error!(
                    span,
                    "No overload for `{name}` with argument types ({args_msg})"
                ),
            }
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

        seen.push(*fname);

        let fty = substitute(gamma, fdef.ty, subs, vspan)?;
        check_expr(c, gamma, *fvalue, fty, vspan)?
    }

    if !base_covers_all {
        for f in fields {
            if !seen.contains(&f.name) && !f.has_default {
                comp_error!(span, "Missing field `{}` in {} literal", f.name, what);
            }
        }
    }

    Ok(())
}

pub(crate) fn score_candidate(
    c: BodyCtx,
    gamma: &mut Gamma,
    mid: ModuleId,
    fid: Idx<Function>,
    recv_ty: Option<TyId>,
    args: &[Idx<Expr>],
    arg_ty: &FxHashMap<Idx<Expr>, TyId>,
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

    let subs = match gamma.instantiate_generics(&f.generics, span) {
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
                match fit_cost_with_iter_coercion(c, gamma, aty, pty, db, span) {
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
        // Bounds that fit only structurally still rank below an exact concrete
        // overload -- prefer the specialized fn when both match.
        total += 1;
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
    name: Ustr,
    recv_override: Option<TyId>,
    args: IdxRange<Expr>,
    call_expr: Idx<Expr>,
    span: Span,
) -> Result<TyId, CompilerError> {
    let res = resolve_named_call_inner(gamma, c, name, recv_override, args, call_expr, span);

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
    name: Ustr,
    recv_override: Option<TyId>,
    args: IdxRange<Expr>,
    call_expr: Idx<Expr>,
    span: Span,
) -> Result<TyId, CompilerError> {
    let _span = tracing::info_span!("named_call", name = %name).entered();
    let candidates = resolve_callee(gamma, name);

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

        comp_error!(span, "Function `{}` doesn't exist", name);
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
            let t = if matches!(c.body.expr_arena[a], Expr::Lambda { .. }) {
                gamma.fresh_callsite_var(*Q_LAM, span)
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

        match select_overload(
            c,
            gamma,
            &candidates,
            recv_override,
            &arg_list,
            &arg_ty,
            &arg_tys,
            name,
            span,
        ) {
            Ok(pick) => pick,

            Err(e) => {
                if name.as_str().starts_with("__op_") {
                    if let Some(&first) = arg_tys.first() {
                        if arg_tys.iter().all(|&t| {
                            matches!(*gamma.get(t), TypeExpr::Generic(..) | TypeExpr::Infer(..))
                        }) {
                            return Ok(first);
                        }
                    }
                }

                return Err(e);
            }
        }
    };

    gamma
        .out
        .record_resolved_call(call_expr, callee_mid, callee_fn, span)?;

    let ws = gamma.workspace;
    let function = &ws.module(callee_mid).functions[callee_fn];
    let (subs, _arg_tys) = gamma.instantiate_generics(&function.generics, span)?;

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

    let arg_list: Vec<Idx<Expr>> = args.into_iter().collect();

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
                        param_tys[offset + 1]
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
                    if let Some((iter_fn, it_ty)) = implicit_iter_target(c, gamma, aty, span)
                    {
                        coerce(gamma, it_ty, pty, c.body.expr_spans[arg])?;
                        gamma.out.implicit_iter_args.insert(arg, iter_fn);
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
