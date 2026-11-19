use super::*;
use crate::hir::{FamilyTier, unary_operator_entry};
use bone_core::modules::DefKind;
use std::borrow::Cow;

/// Hidden members are scored only to distinguish inaccessible fits from no fit.
#[derive(Default)]
pub(crate) struct Callees {
    pub(crate) visible: FnList,
    pub(crate) hidden: FnList,
}

pub(crate) fn resolve_callee(gamma: &Gamma, family: &CallFamily) -> Callees {
    let name = family.key();
    let _span = tracing::info_span!("resolve_callee", name = %name).entered();

    match *family {
        CallFamily::Resolved(key) => member_overloads(gamma, key),
        CallFamily::Merged { file, .. } => {
            let members = merged_members(gamma, file, name).unwrap_or_else(|| {
                panic!("merged family `{name}` of `{file}` was not recorded at lowering")
            });
            let mut out = Callees::default();
            for &member in members {
                let callees = member_overloads(gamma, member.key);
                out.visible.extend(callees.visible);
                out.hidden.extend(callees.hidden);
            }
            out
        }
    }
}

fn merged_members<'w>(
    gamma: &Gamma<'w>,
    file: Ustr,
    name: Ustr,
) -> Option<&'w [hir::FamilyMember]> {
    let ws = gamma.workspace;
    ws.module(ws.module_ids.get(&file).copied()?)
        .call_families
        .get(&name)
        .map(Vec::as_slice)
}

fn member_overloads(gamma: &Gamma, key: Ustr) -> Callees {
    gamma.dep_acc.borrow_mut().insert(key);
    let ws = gamma.workspace;
    let Some(fns) = ws.functions_by_name.get(&key) else {
        return Callees::default();
    };
    let site = gamma.access_site();
    let mut out = Callees {
        visible: FnList::with_capacity(fns.len()),
        hidden: FnList::default(),
    };
    for &(mid, fid) in fns {
        if ws.callable_member_admits(key, mid, fid, site) {
            out.visible.push((mid, fid));
        } else {
            out.hidden.push((mid, fid));
        }
    }
    out
}

/// The nearest lookup tier among the members of a merged v1 family that
/// contribute the overload `(mid, fid)`; `None` outside a merged
/// family, where no tier orders the candidates.
fn candidate_tier(
    gamma: &Gamma,
    members: &[hir::FamilyMember],
    candidate: (ModuleId, Idx<Function>),
) -> Option<FamilyTier> {
    members
        .iter()
        .filter(|member| {
            gamma
                .workspace
                .functions_by_name
                .get(&member.key)
                .is_some_and(|fns| fns.contains(&candidate))
        })
        .map(|member| member.tier)
        .min()
}

/// Lookup tier breaks equal-cost ties across members, so a module can override an import.
fn keep_nearest_tier(
    gamma: &Gamma,
    members: &[hir::FamilyMember],
    tied: &mut SmallVec<[(ModuleId, Idx<Function>); 4]>,
) {
    let Some(tiers) = tied
        .iter()
        .map(|&candidate| candidate_tier(gamma, members, candidate))
        .collect::<Option<SmallVec<[FamilyTier; 4]>>>()
    else {
        return;
    };
    let nearest = tiers.iter().min().copied();
    let mut tiers = tiers.into_iter();
    tied.retain(|_| tiers.next() == nearest);
}

fn ty_has_generic_hole(gamma: &Gamma, ty: TyId) -> bool {
    gamma
        .workspace
        .type_any(Some(&gamma.out), ty, &mut |node| match node {
            TypeExpr::Infer(var, _) => gamma.out.substitutions.get(var).is_none(),
            TypeExpr::Generic(..) => true,
            _ => false,
        })
}

/// Grade specificity before unification binds variables. A bare hole costs 2,
/// a type containing a hole costs 1, and a concrete type costs 0.
fn param_genericity(gamma: &mut Gamma, param: TyId, span: Span) -> usize {
    let p = resolve(gamma, param, span);
    if matches!(*gamma.get(p), TypeExpr::Infer(..) | TypeExpr::Generic(..)) {
        return 2;
    }
    usize::from(ty_has_generic_hole(gamma, p))
}

pub(crate) fn fit_cost(gamma: &mut Gamma, arg: TyId, param: TyId, span: Span) -> Option<usize> {
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
        // Mirrors `coerce`'s nullable target: fit the inner types, so an `int`
        // argument fits a `float?` param in selection as it does in binding.
        (&from, &TypeExpr::Nullable(pi)) => {
            let from_inner = match from {
                TypeExpr::Nullable(fi) => a.with_idx(fi),
                _ => a,
            };
            fit_cost(gamma, from_inner, p.with_idx(pi), span)
        }
        (&from, &to) => {
            let cost = match to {
                TypeExpr::App {
                    ctor: TypeCtor::Nominal(name),
                    ..
                } if gamma.workspace.interface_names.contains(&name) => 3,
                TypeExpr::App {
                    ctor: TypeCtor::Nominal(name),
                    ..
                // Score full field references as either capability, as coercion does.
                } if bone_core::modules::is_intrinsic_key_in(
                    name,
                    &["ReadFieldRef", "WriteFieldRef"],
                ) && matches!(from, TypeExpr::App { ctor: TypeCtor::Nominal(source), .. }
                            if bone_core::modules::is_intrinsic_key(source, "FieldRef")) =>
                {
                    1
                }
                _ if frame_ref_target(gamma, p).is_some() => 1 + penalty,
                _ => return None,
            };
            let snap = gamma.out.checkpoint();
            if gamma.trial(|g| coerce(g, a, p, span).is_ok()) {
                return Some(cost);
            }
            gamma.out.rollback_to(snap);
            None
        }
    }
}

/// Only a tie is deferrable: an unpinned variable already fits any candidate.
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
    format!(
        "{}({}){}",
        bone_core::modules::display_name(f.name.as_str()),
        params,
        ret
    )
}

/// Prefer portable, then Lua, so the native emitter usually keeps the representative.
fn backend_rank(b: skull_parser::FnBackend) -> usize {
    match b {
        skull_parser::FnBackend::Any => 0,
        skull_parser::FnBackend::Lua => 1,
        skull_parser::FnBackend::Js => 2,
    }
}

/// Where a tied candidate comes from, when a tie spans unrelated definitions
/// (a merged family): the DefKey, or the compiler-owned defining file.
fn candidate_origin(gamma: &Gamma, mid: ModuleId, fid: Idx<Function>) -> String {
    let f = &gamma.workspace.module(mid).functions[fid];
    if gamma.workspace.defs.contains_key(&f.name) {
        format!("v1 definition {}", f.name)
    } else {
        format!(
            "compiler-owned definition in {}",
            gamma.workspace.module(mid).file
        )
    }
}

/// Keep same-backend duplicates so ambiguity is still diagnosed.
pub(crate) fn collapse_backend_variants<'c>(
    gamma: &Gamma,
    candidates: &'c [(ModuleId, Idx<Function>)],
) -> Cow<'c, [(ModuleId, Idx<Function>)]> {
    if candidates.len() <= 1 {
        return Cow::Borrowed(candidates);
    }
    let backend_of =
        |(mid, fid): (ModuleId, Idx<Function>)| gamma.workspace.module(mid).functions[fid].backend;
    let first_backend = backend_of(candidates[0]);
    if candidates
        .iter()
        .all(|&candidate| backend_of(candidate) == first_backend)
    {
        return Cow::Borrowed(candidates);
    }
    let mut groups: SmallVec<[((Ustr, Ustr), SmallVec<[_; 2]>); 4]> = SmallVec::new();
    for &(mid, fid) in candidates {
        let key = gamma.workspace.backend_variant_key(mid, fid);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, group)) => group.push((mid, fid)),
            None => groups.push((key, smallvec::smallvec![(mid, fid)])),
        }
    }
    let mut out = Vec::with_capacity(candidates.len());
    for (_, group) in groups {
        let mut by_backend = [None; 3];
        if group.iter().all(|&candidate| {
            let rank = backend_rank(backend_of(candidate));
            by_backend[rank].replace(candidate).is_none()
        }) {
            out.extend(by_backend.into_iter().flatten().next());
        } else {
            out.extend(group);
        }
    }
    Cow::Owned(out)
}

pub(crate) fn select_overload(
    c: BodyCtx,
    gamma: &mut Gamma,
    family: CallFamily,
    callees: &Callees,
    recv_ty: Option<TyId>,
    args: &[Idx<Expr>],
    arg_ty: &FxHashMap<Idx<Expr>, TyId>,
    arg_tys_msg: &[TyId],
    generic_args: IdxRange<TypeExpr>,
    name: Ustr,
    span: Span,
) -> Result<(ModuleId, Idx<Function>), SelectError> {
    let collapsed = collapse_backend_variants(gamma, &callees.visible);
    let candidates: &[(ModuleId, Idx<Function>)] = &collapsed;

    let score = |gamma: &mut Gamma, (mid, fid)| {
        score_candidate(
            c,
            gamma,
            mid,
            fid,
            recv_ty,
            args,
            arg_ty,
            generic_args,
            span,
        )
    };
    let mut best_cost: Option<usize> = None;
    let mut best_set: SmallVec<[(ModuleId, Idx<Function>); 4]> = SmallVec::new();
    // When no candidate fits, report a bound failure from an otherwise fitting candidate.
    let mut bound_fail: Option<CompilerError> = None;

    for &candidate in candidates {
        match score(gamma, candidate) {
            Some(Ok(cost)) => {
                if best_cost.is_none_or(|bc| cost < bc) {
                    best_cost = Some(cost);
                    best_set.clear();
                }
                if best_cost == Some(cost) {
                    best_set.push(candidate);
                }
            }
            Some(Err(error)) if bound_fail.is_none() => bound_fail = Some(error),
            _ => {}
        }
    }

    if best_set.len() > 1
        && let CallFamily::Merged { file, name } = family
        && !args
            .iter()
            .any(|arg| ty_has_generic_hole(gamma, arg_ty[arg]))
        && recv_ty.is_none_or(|ty| !ty_has_generic_hole(gamma, ty))
        && let Some(members) = merged_members(gamma, file, name)
    {
        keep_nearest_tier(gamma, members, &mut best_set);
    }

    match best_set.len() {
        1 => Ok(best_set[0]),
        n if n > 1 => {
            let fn_name = |&(mid, fid): &(ModuleId, Idx<Function>)| {
                gamma.workspace.module(mid).functions[fid].name
            };
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
                    arg_tys_msg.iter().map(|t| gamma.render(*t)).join(", ")
                ),
            )))
        }

        _ => {
            let args_msg = arg_tys_msg.iter().map(|t| gamma.render(*t)).join(", ");
            let fits =
                |gamma: &mut Gamma, candidate| matches!(score(gamma, candidate), Some(Ok(_)));
            let hidden_fit = callees
                .hidden
                .iter()
                .any(|&candidate| fits(gamma, candidate));
            if !hidden_fit && bound_fail.is_none() && recv_ty.is_none() && generic_args.is_empty() {
                if let Some((symbol, true)) = bone_core::hir::operator_entry(
                    name.rsplit("::").next().unwrap_or(name.as_str()),
                ) {
                    let actuals: SmallVec<[TyId; 2]> = args
                        .iter()
                        .map(|arg| resolve(gamma, arg_ty[arg], span))
                        .collect();
                    // Exact shapes only: widening or generic guesses must not
                    // hide competing overloads behind a nullable explanation.
                    let mut near_misses = candidates.iter().filter_map(|&(mid, fid)| {
                        let f = &gamma.workspace.module(mid).functions[fid];
                        if !f.generics.is_empty() || f.param_decls.len() != actuals.len() {
                            return None;
                        }
                        let mut nullable_args = SmallVec::<[Idx<Expr>; 2]>::new();
                        for ((&actual, param), &arg) in actuals.iter().zip(&f.param_decls).zip(args)
                        {
                            let expected = resolve(gamma, param.ty, span);
                            if types_equal(gamma, actual, expected) {
                                continue;
                            }
                            let TypeExpr::Nullable(inner) = *gamma.get(actual) else {
                                return None;
                            };
                            if !types_equal(gamma, actual.with_idx(inner), expected) {
                                return None;
                            }
                            nullable_args.push(arg);
                        }
                        if nullable_args.is_empty() {
                            None
                        } else {
                            Some((mid, fid, nullable_args))
                        }
                    });
                    if let (Some((mid, fid, nullable_args)), None) =
                        (near_misses.next(), near_misses.next())
                    {
                        let expected = gamma.workspace.module(mid).functions[fid]
                            .param_decls
                            .iter()
                            .map(|p| gamma.workspace.render(None, p.ty))
                            .join(", ");
                        let mut error = CompilerError::new_with_label(
                            c.body.expr_spans[nullable_args[0]],
                            format!(
                                "operator `{symbol}` expects operands ({expected}), got ({args_msg})"
                            ),
                        );
                        error.labels[0].message = "nullable operand".to_string();
                        for &arg in &nullable_args[1..] {
                            error = error
                                .with_label(c.body.expr_spans[arg], "nullable operand".to_string());
                        }
                        return Err(SelectError::NoFit(error));
                    }
                }
            }
            let home_hint = if hidden_fit || bound_fail.is_some() {
                None
            } else {
                home_module_hint(c, gamma, recv_ty, args, arg_ty, name, span, fits)
            };
            let overloads = candidates
                .iter()
                .map(|&(mid, fid)| format!("  {}", render_candidate_sig(gamma, mid, fid)))
                .join("\n");
            let name = bone_core::modules::display_name(name.as_str());
            Err(SelectError::NoFit(CompilerError::new_with_label(
                span,
                if hidden_fit {
                    format!(
                        "No accessible overload for `{name}` with argument types ({args_msg}); the matching overload is not accessible\navailable overloads:\n{overloads}"
                    )
                } else {
                    match bound_fail {
                        Some(be) => format!(
                            "No overload for `{name}` with argument types ({args_msg}); the `Iterator`-bounded candidate was rejected: {}\navailable overloads:\n{overloads}",
                            be.message
                        ),
                        None => format!(
                            "No overload for `{name}` with argument types ({args_msg})\navailable overloads:\n{overloads}{}",
                            home_hint.unwrap_or_default()
                        ),
                    }
                },
            )))
        }
    }
}

/// A `help:` line for a call no overload in scope fits, when the module that
/// declares the first argument's nominal type has an accessible function of
/// the same name that would fit. Importing a type does not import its
/// functions, so this is the common near-miss after `use m::Type;`.
fn home_module_hint(
    c: BodyCtx,
    gamma: &mut Gamma,
    recv_ty: Option<TyId>,
    args: &[Idx<Expr>],
    arg_ty: &FxHashMap<Idx<Expr>, TyId>,
    name: Ustr,
    span: Span,
    fits: impl Fn(&mut Gamma, (ModuleId, Idx<Function>)) -> bool,
) -> Option<String> {
    let ws = gamma.workspace;
    let first = recv_ty.or_else(|| args.first().map(|a| arg_ty[a]))?;
    let first = resolve(gamma, first, span);
    let TypeExpr::App {
        ctor: TypeCtor::Nominal(type_key),
        ..
    } = *gamma.get(first)
    else {
        return None;
    };
    let meta = ws.defs.get(&type_key)?;
    let owner = ws.ns.package(meta.package)?;
    let bare = name.as_str().rsplit("::").next().unwrap_or(name.as_str());
    // `__`-prefixed names are operator dispatch families (`a + b` ->
    // `__op_add`); their import is `use m::operator(+)`, not this spelling.
    if bare.starts_with("__") {
        return None;
    }
    let key = bone_core::modules::def_key(&owner.self_name, &meta.module, bare);
    let site = gamma.access_site();
    let from_pkg = site.package?;
    // The path the call site writes: relative below its own module, rooted at
    // the package (or the dependency alias naming it) otherwise.
    let mut path: Vec<&str> = if from_pkg == meta.package {
        match meta.module.strip_prefix(site.module_path) {
            Some(rest) => rest.iter().map(|s| s.as_str()).collect(),
            None => std::iter::once(owner.self_name.as_str())
                .chain(meta.module.iter().map(|s| s.as_str()))
                .collect(),
        }
    } else {
        let root = ws
            .ns
            .package(from_pkg)
            .and_then(|pkg| pkg.aliases.iter().find(|(_, id)| **id == meta.package))
            .map_or(owner.self_name, |(alias, _)| *alias);
        std::iter::once(root.as_str())
            .chain(meta.module.iter().map(|s| s.as_str()))
            .collect()
    };
    if path.is_empty() {
        return None;
    }
    path.push(bare);
    let path = path.join("::");
    if !ws
        .functions_by_name
        .get(&key)?
        .iter()
        .copied()
        .filter(|&(mid, fid)| ws.callable_member_admits(key, mid, fid, site))
        .any(|candidate| fits(gamma, candidate))
    {
        return None;
    }
    let receiver = match (recv_ty, args.first().map(|&a| &c.body.expr_arena[a])) {
        (None, Some(Expr::Var(b))) => c.body.binding_arena[*b].name.as_str(),
        _ => "...",
    };
    let rest = if (recv_ty.is_none() && args.len() > 1) || (recv_ty.is_some() && !args.is_empty()) {
        ", ..."
    } else {
        ""
    };
    Some(format!(
        "\nhelp: `{path}` takes `{}` but is not imported here; add `use {path};` or call `{path}({receiver}{rest})`",
        gamma.render(first)
    ))
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
        check_expr(c, gamma, *fvalue, fty, vspan)?;
        if struct_key
            .and_then(|name| resolve_struct(gamma, name))
            .is_some_and(|sid| gamma.workspace.module(sid.module).structs[sid.def].is_value)
        {
            check_value_int_field_literal(c, gamma, *fvalue, fty)?;
        }
    }

    for field in fields.iter().filter(|field| !seen.contains(&field.name)) {
        if base_covers_all {
            if let Some(key) = struct_key {
                let site = gamma.access_site();
                field_access(gamma, key, field.name, site, FieldAccessNeed::Read)
                    .map_err(|e| e.to_compiler_error(span))?;
                field_access(gamma, key, field.name, site, FieldAccessNeed::Write)
                    .map_err(|e| e.to_compiler_error(span))?;
            }
        } else if !field.has_default {
            comp_error!(span, "Missing field `{}` in {} literal", field.name, what);
        }
    }

    Ok(())
}

fn score_candidate(
    c: BodyCtx,
    gamma: &mut Gamma,
    mid: ModuleId,
    fid: Idx<Function>,
    recv_ty: Option<TyId>,
    args: &[Idx<Expr>],
    arg_ty: &FxHashMap<Idx<Expr>, TyId>,
    generic_args: IdxRange<TypeExpr>,
    span: Span,
) -> Option<Result<usize, CompilerError>> {
    // Discard trial types unless spread inference recorded them in the body.
    let snap = gamma.out.checkpoint();
    let function = &gamma.workspace.module(mid).functions[fid];
    let cost = (|| {
        let (cost, subs) = check_call_arguments(
            c,
            gamma,
            function,
            function.name,
            recv_ty,
            args,
            arg_ty,
            generic_args,
            None,
            span,
        )
        .ok()?;
        if function.has_iterator_bounds() {
            let mut bounds = BoundTree::default();
            if let Err(error) = gamma.trial(|g| {
                check_iterator_bounds(c, g, function, &subs, &mut bounds, false, 0, span)
            }) {
                return Some(Err(error));
            }
        }
        Some(Ok(cost))
    })();
    if args.iter().any(|arg| c.body.spread_args.contains(arg)) {
        gamma.out.rollback_to(snap);
    } else {
        gamma.out.discard_since(snap);
    }
    cost
}

fn check_call_arguments(
    c: BodyCtx,
    gamma: &mut Gamma,
    function: &Function,
    name: Ustr,
    receiver: Option<TyId>,
    args: &[Idx<Expr>],
    inferred: &FxHashMap<Idx<Expr>, TyId>,
    generic_args: IdxRange<TypeExpr>,
    call: Option<Idx<Expr>>,
    span: Span,
) -> Result<(usize, FxHashMap<VarId, TyId>), CompilerError> {
    let scoring = call.is_none();
    let rejected = || CompilerError::silent(span);
    let has_bounds = function.has_iterator_bounds();
    let bounded_vars = bounded_generic_vars(gamma, function);
    let subs = instantiate_call_generics(gamma, function, generic_args, span)?;
    let mut bounds = BoundTree::default();
    let mut refs = Vec::new();
    let mut cost = 0;
    let params = &function.param_decls;
    let offset = usize::from(receiver.is_some());
    if offset > params.len() {
        return Err(if scoring {
            rejected()
        } else {
            CompilerError::new_with_label(
                span,
                format!("method `{}` has no receiver parameter", name),
            )
        });
    }

    if let Some(receiver) = receiver {
        let declared = params[0].ty;
        let expected = if scoring && subs.is_empty() {
            declared
        } else {
            substitute(gamma, declared, &subs, span)?
        };
        if scoring {
            cost += fit_cost_with_iter_coercion(
                c,
                gamma,
                receiver,
                expected,
                declared_is_bounded_generic(gamma, declared, &bounded_vars),
                span,
            )
            .ok_or_else(rejected)?;
        } else {
            coerce(gamma, receiver, expected, span)?;
            reject_redundant_unwrap(gamma, function, name, 0, receiver, span)?;
        }
    }

    let slots: Vec<Option<SlotFill>> = if !scoring && function.is_variadic {
        let fixed = (params.len() - 1).saturating_sub(offset);
        if args.len() < fixed {
            return Err(CompilerError::new_with_label(
                span,
                format!(
                    "`{}` expects at least {} argument(s), got {}",
                    name,
                    fixed,
                    args.len()
                ),
            ));
        }
        args.iter()
            .map(|&arg| Some(SlotFill::Direct(arg)))
            .collect()
    } else {
        reorder_call_args(
            c,
            gamma,
            args,
            &params[offset..],
            function.num_required.saturating_sub(offset),
            !scoring,
            span,
        )?
    };

    for (index, fill) in slots.into_iter().enumerate() {
        if !scoring && fill.is_none() {
            continue;
        }
        let index = (offset + index).min(params.len() - 1);
        let declared = params[index].ty;
        if !scoring && has_bounds {
            check_iterator_bounds(c, gamma, function, &subs, &mut bounds, false, 0, span)?;
        }
        let expected = if scoring && subs.is_empty() {
            declared
        } else {
            substitute(gamma, declared, &subs, span)?
        };
        let Some(fill) = fill else {
            cost += 1;
            continue;
        };
        if scoring {
            let actual = match fill {
                SlotFill::Direct(arg) => *inferred.get(&arg).ok_or_else(rejected)?,
                SlotFill::SpreadField(_, ty) => ty,
            };
            let resolved = resolve(gamma, expected, span);
            if matches!(fill, SlotFill::SpreadField(..))
                && frame_ref_target(gamma, resolved).is_some()
            {
                return Err(rejected());
            }
            let contextual_uint = matches!(fill, SlotFill::Direct(arg)
                if matches!(c.body.expr_arena[arg], Expr::Int(value)
                    if (0..=u32::MAX as i64).contains(&value))
                && matches!(gamma.get(resolved), TypeExpr::Builtin(BuiltinTy::Uint)));
            cost += if contextual_uint {
                1
            } else {
                fit_cost_with_iter_coercion(
                    c,
                    gamma,
                    actual,
                    expected,
                    declared_is_bounded_generic(gamma, declared, &bounded_vars),
                    span,
                )
                .ok_or_else(rejected)?
            };
            continue;
        }

        let is_ref = frame_ref_target(gamma, expected).is_some();
        match fill {
            SlotFill::SpreadField(arg, actual) => {
                if is_ref {
                    return Err(CompilerError::new_with_label(
                        c.body.expr_spans[arg],
                        "a `ref` argument must be passed directly; call spreads supply copied fields".to_string(),
                    ));
                }
                coerce(gamma, actual, expected, c.body.expr_spans[arg])?;
            }
            SlotFill::Direct(arg) => {
                let arg_span = c.body.expr_spans[arg];
                if is_ref {
                    refs.push(arg);
                }
                let bounded = declared_is_bounded_generic(gamma, declared, &bounded_vars)
                    && !matches!(c.body.expr_arena[arg], Expr::Lambda { .. });
                let iter_result = !bounded
                    && matches!(*gamma.get(declared), TypeExpr::Function { ret, .. }
                        if declared_is_bounded_generic(gamma, declared.with_idx(ret), &bounded_vars));
                if iter_result && let Expr::Lambda { body, .. } = c.body.expr_arena[arg] {
                    gamma.iter_result_lambda = Some(body);
                }
                if bounded {
                    let actual = match inferred.get(&arg) {
                        Some(&ty) => ty,
                        None => {
                            let ty = infer_expr(c, gamma, arg)?;
                            resolve(gamma, ty, span)
                        }
                    };
                    let implicit = implicit_iter_target(c, gamma, actual, span);
                    let actual = implicit.as_ref().map_or(actual, |(_, ty)| *ty);
                    coerce(gamma, actual, expected, arg_span)?;
                    if let Some((witness, _)) = implicit {
                        gamma.out.implicit_iter_args.insert(arg, witness);
                    }
                } else {
                    match inferred.get(&arg) {
                        Some(&actual) if !is_ref => coerce(gamma, actual, expected, arg_span)?,
                        _ => check_expr(c, gamma, arg, expected, arg_span)?,
                    }
                }
                gamma.iter_result_lambda = None;
                if iter_result && !matches!(c.body.expr_arena[arg], Expr::Lambda { .. }) {
                    reject_iterable_fn_value_result(c, gamma, arg, span)?;
                }
                if let Some(&actual) = gamma.out.expr_types.get(arg) {
                    reject_redundant_unwrap(gamma, function, name, index, actual, arg_span)?;
                }
            }
        }
    }
    if let Some(call) = call {
        if !refs.is_empty() {
            gamma.out.ref_call_args.insert(call, refs);
        }
        if has_bounds {
            check_iterator_bounds(c, gamma, function, &subs, &mut bounds, true, 0, span)?;
            gamma.out.bound_call_trees.insert(call, bounds);
        }
    }
    Ok((cost, subs))
}

fn instantiate_call_generics(
    gamma: &mut Gamma,
    function: &Function,
    generic_args: IdxRange<TypeExpr>,
    span: Span,
) -> Result<FxHashMap<VarId, TyId>, CompilerError> {
    if generic_args.is_empty() {
        return gamma
            .instantiate_generics(&function.generics, span)
            .map(|(subs, _)| subs);
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
    for (&generic, explicit) in function.generics.iter().zip(generic_args) {
        let TypeExpr::Generic(var_id, _) = *gamma.get(generic) else {
            return Err(CompilerError::new_with_label(
                span,
                format!(
                    "function `{}` has a malformed generic declaration",
                    function.name
                ),
            ));
        };
        let ty = TyId::Local(explicit);
        subs.insert(var_id, ty);
    }
    Ok(subs)
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

#[derive(Clone, Copy)]
enum SlotFill {
    Direct(Idx<Expr>),
    /// The param slot index encodes which spread field; skull-mir re-derives
    /// its name.
    SpreadField(Idx<Expr>, TyId),
}

fn map_nominal_spread_fields<T>(
    gamma: &mut Gamma,
    ty: TyId,
    name: Ustr,
    params: IdxRange<TypeExpr>,
    span: Span,
    missing_struct: impl FnOnce(Ustr) -> CompilerError,
    mut map_field: impl FnMut(Ustr, TyId) -> T,
) -> Result<Vec<T>, CompilerError> {
    let sid = resolve_struct(gamma, name).ok_or_else(|| missing_struct(name))?;
    let def = &gamma.workspace.module(sid.module).structs[sid.def];
    let subs = generic_arg_subs(gamma, &def.generics, ty, params);
    def.fields
        .iter()
        .map(|field| {
            let ty = substitute(gamma, field.ty, &subs, span)?;
            Ok(map_field(field.name, ty))
        })
        .collect()
}

/// Call spreads require statically known nominal struct fields.
fn spread_struct_fields(
    gamma: &mut Gamma,
    op_ty: TyId,
    span: Span,
) -> Result<Vec<(Ustr, TyId)>, CompilerError> {
    let r = resolve(gamma, op_ty, span);

    match *gamma.get(r) {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } => map_nominal_spread_fields(
            gamma,
            r,
            name,
            params,
            span,
            |name| {
                CompilerError::new_with_label(
                    span,
                    format!("cannot spread `{}`: not a struct", name),
                )
            },
            |name, ty| (name, ty),
        ),

        _ => comp_error!(
            span,
            "cannot spread `{}` into call arguments: expected a nominal struct",
            gamma.render(r)
        ),
    }
}

/// A rejection by `reorder_call_args`: its message is built only when `report`
/// is set, since overload scoring discards the rejections of the candidates it
/// tries.
macro_rules! slot_fail {
    ($report:expr, $span:expr, $($arg:tt)*) => {{
        if !$report {
            return Err(CompilerError::silent($span));
        }

        comp_error!($span, $($arg)*)
    }}
}

fn reorder_call_args(
    c: BodyCtx,
    gamma: &mut Gamma,
    args: &[Idx<Expr>],
    param_decls: &[bone_core::hir::ParamDecl],
    num_required: usize,
    report: bool,
    span: Span,
) -> Result<Vec<Option<SlotFill>>, CompilerError> {
    let mut slots: Vec<Option<SlotFill>> = vec![None; param_decls.len()];
    let mut pos = 0usize;

    for &arg in args {
        if c.body.spread_args.contains(&arg) {
            let op_ty = infer_expr(c, gamma, arg)?;
            let aspan = c.body.expr_spans[arg];
            for (fname, fty) in spread_struct_fields(gamma, op_ty, aspan)? {
                let Some(pi) = param_decls.iter().position(|p| p.name == fname) else {
                    slot_fail!(
                        report,
                        span,
                        "spread field `{}` names no parameter of the callee",
                        fname
                    )
                };
                match slots[pi] {
                    Some(SlotFill::SpreadField(..)) => {
                        slot_fail!(
                            report,
                            span,
                            "two spreads both supply parameter `{}`",
                            fname
                        )
                    }
                    Some(SlotFill::Direct(_)) => slot_fail!(
                        report,
                        span,
                        "parameter `{}` is given before the spread that also supplies it",
                        fname
                    ),
                    None => slots[pi] = Some(SlotFill::SpreadField(arg, fty)),
                }
            }
            continue;
        }

        let name = c.body.arg_names.get(&arg);
        let index = if let Some(&name) = name {
            let Some(index) = param_decls.iter().position(|param| param.name == name) else {
                slot_fail!(report, span, "unknown parameter `{}`", name);
            };
            index
        } else {
            if pos >= param_decls.len() {
                slot_fail!(
                    report,
                    span,
                    "Too many positional arguments, expected at most {}",
                    param_decls.len()
                );
            }
            pos += 1;
            pos - 1
        };
        // An explicit named argument can override a preceding spread field.
        if slots[index].is_some_and(|fill| name.is_none() || matches!(fill, SlotFill::Direct(_))) {
            slot_fail!(
                report,
                span,
                "Multiple values provided for parameter `{}`",
                param_decls[index].name
            );
        }
        slots[index] = Some(SlotFill::Direct(arg));
    }

    for i in 0..num_required {
        if slots[i].is_none() {
            slot_fail!(
                report,
                span,
                "Missing required argument for parameter `{}`",
                param_decls[i].name
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
        } => map_nominal_spread_fields(
            gamma,
            r,
            name,
            params,
            span,
            |name| CompilerError::new_with_label(span, format!("cannot spread `{}`", name)),
            |_, ty| ty,
        ),

        _ => comp_error!(
            span,
            "Cannot spread type {}, expected a tuple, array, or nominal struct",
            gamma.render(r)
        ),
    }
}

pub(crate) fn call_parts(
    body: &Body,
    expr: Idx<Expr>,
) -> (CallFamily, IdxRange<Expr>, IdxRange<TypeExpr>) {
    match body.expr_arena[expr] {
        Expr::Call {
            family,
            args,
            generic_args,
            ..
        } => (family, args, generic_args),
        Expr::SafeNavCall { family, args, .. } => (family, args, IdxRange::new_empty()),
        Expr::Not(inner, family) | Expr::UnaryMinus(inner, family) => (
            family,
            IdxRange::new_inclusive(inner..=inner),
            IdxRange::new_empty(),
        ),
        _ => unreachable!("named call expression"),
    }
}

pub(crate) fn resolve_named_call(
    gamma: &mut Gamma,
    c: BodyCtx,
    call_expr: Idx<Expr>,
    recv_override: Option<TyId>,
) -> Result<TyId, CompilerError> {
    let (family, args, _) = call_parts(c.body, call_expr);
    let span = c.body.expr_spans[call_expr];
    let name = family.key();
    let res = resolve_named_call_inner(gamma, c, call_expr, recv_override);

    let Err(e) = res else { return res };
    let operands = recv_override
        .into_iter()
        .chain(
            args.into_iter()
                .filter_map(|arg| gamma.out.expr_types.get(arg).copied()),
        )
        .collect::<SmallVec<[TyId; 4]>>();
    for ty in operands {
        let ty = resolve(gamma, ty, span);
        if matches!(gamma.get(ty), TypeExpr::Builtin(BuiltinTy::Error)) {
            return Ok(ty);
        }
    }
    if gamma.quiet {
        return Err(e);
    }

    // Suggest a field read only for failures at the call or receiver, not later arguments.
    let (recv_ty, recv_span) = match recv_override {
        Some(r) => (resolve(gamma, r, span), None),
        None => {
            let Some(recv) = method_receiver(c.body, args) else {
                return Err(e);
            };
            match infer_expr(c, gamma, recv) {
                Ok(t) => (resolve(gamma, t, span), Some(c.body.expr_spans[recv])),
                Err(_) => return Err(e),
            }
        }
    };

    let err_span = e.labels.first().map(|l| l.span);
    let about_receiver = err_span == Some(span) || (recv_span.is_some() && err_span == recv_span);
    if about_receiver {
        let name = written_method_name(c, recv_override, args, call_expr).unwrap_or(name);
        if let Ok(fty) = struct_field_ty(gamma, recv_ty, name, span) {
            let fty = resolve(gamma, fty, span);
            if !matches!(gamma.get(fty), TypeExpr::Function { .. }) {
                let recv_str = gamma.render(recv_ty);
                let fty_str = gamma.render(fty);
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

fn method_receiver(body: &Body, args: IdxRange<Expr>) -> Option<Idx<Expr>> {
    (!args.is_empty() && body.method_call_recvs.contains(&args.start())).then(|| args.start())
}

fn written_method_name(
    c: BodyCtx,
    recv_override: Option<TyId>,
    args: IdxRange<Expr>,
    call_expr: Idx<Expr>,
) -> Option<Ustr> {
    let receiver = match (recv_override, &c.body.expr_arena[call_expr]) {
        (Some(_), Expr::SafeNavCall { receiver, .. }) => *receiver,
        (None, _) => method_receiver(c.body, args)?,
        _ => return None,
    };
    c.body.method_names.get(&receiver).copied()
}

fn unary_operator_error(
    gamma: &mut Gamma,
    name: Ustr,
    operand: Option<TyId>,
    span: Span,
) -> CompilerError {
    let symbol = unary_operator_entry(name.as_str()).expect("unary operator name");
    let operand = operand
        .map(|t| {
            let t = resolve(gamma, t, span);
            gamma.render(t)
        })
        .unwrap_or_else(|| "<no operand>".to_string());
    CompilerError::new_with_label(
        span,
        format!("operator `{symbol}` is not defined for `{operand}`"),
    )
}

fn resolve_named_call_inner(
    gamma: &mut Gamma,
    c: BodyCtx,
    call_expr: Idx<Expr>,
    recv_override: Option<TyId>,
) -> Result<TyId, CompilerError> {
    let (family, args, generic_args) = call_parts(c.body, call_expr);
    let span = c.body.expr_spans[call_expr];
    let name = family.key();
    let _span = tracing::info_span!("named_call", name = %name).entered();
    let mut callees = resolve_callee(gamma, &family);
    // Empty merged families are scoped misses, not workspace-wide lookup requests.
    if !generic_args.is_empty() {
        callees.visible.retain(|(mid, fid)| {
            gamma.workspace.module(*mid).functions[*fid].generics.len() == generic_args.len()
        });
    }

    // A callable field wins over a free function with the same name.
    let method_recv: Option<TyId> = match recv_override {
        Some(r) => Some(resolve(gamma, r, span)),
        None => method_receiver(c.body, args)
            .map(|recv| infer_expr(c, gamma, recv).map(|ty| resolve(gamma, ty, span)))
            .transpose()?,
    };

    if let Some(recv_ty) = method_recv {
        let field = written_method_name(c, recv_override, args, call_expr).unwrap_or(name);
        if let Ok(field_ty) = struct_field_ty(gamma, recv_ty, field, span) {
            let field_ty = resolve(gamma, field_ty, span);

            if matches!(gamma.get(field_ty), TypeExpr::Function { .. }) {
                let mut call_args = args;
                if recv_override.is_none() {
                    call_args.next();
                }

                gamma.out.fn_field_calls.insert(call_expr, field);
                gamma.out.fn_field_call_types.insert(call_expr, field_ty);
                return call_fn_value(c, gamma, field_ty, call_args, span);
            }
        }
    }

    // BoundTree supplies the concrete next at specialization. iter(x) on a bound is identity.
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

    let unary = unary_operator_entry(name.as_str()).is_some();
    if callees.visible.is_empty() {
        let unbound_scoped_family = match family {
            CallFamily::Merged { file, name } => {
                merged_members(gamma, file, name).is_some_and(<[_]>::is_empty)
            }
            CallFamily::Resolved(_) => false,
        };
        if recv_override.is_none() && !unbound_scoped_family {
            if let Some(gty) = resolve_global(gamma, name) {
                let gty = resolve(gamma, gty, span);

                if matches!(gamma.get(gty), TypeExpr::Function { .. }) {
                    return call_fn_value(c, gamma, gty, args, span);
                }
            }
            if let Some(NewtypeId { module: ntm, def }) = resolve_newtype(gamma, name) {
                let nt = &gamma.workspace.module(ntm).newtypes[def];
                let (subs, fresh) = gamma.instantiate_generics(&nt.generics, span)?;

                if args.len() != 1 {
                    comp_error!(
                        span,
                        "newtype `{}` constructor expects exactly 1 argument, got {}",
                        name,
                        args.len()
                    );
                }

                let arg = args.start();
                let aspan = c.body.expr_spans[arg];
                let aty = infer_expr(c, gamma, arg)?;
                let w_sub = substitute(gamma, nt.wrapped, &subs, aspan)?;
                coerce(gamma, aty, w_sub, aspan)?;

                let arg_tys: Vec<TyId> = fresh.iter().map(|&f| resolve(gamma, f, span)).collect();
                let result = gamma.nominal_app(name, &arg_tys, span);

                return Ok(resolve(gamma, result, span));
            }
        }

        if unary {
            let operand = args
                .into_iter()
                .next()
                .map(|a| infer_expr(c, gamma, a))
                .transpose()?;
            return Err(unary_operator_error(gamma, name, operand, span));
        }

        if unbound_scoped_family && method_recv.is_none() {
            return Err(CompilerError::new_with_label(
                span,
                format!("`{name}` did not resolve to any definition"),
            ));
        }

        // A nearer value binding shadows callable names in lower lookup tiers.
        let meta = gamma.workspace.defs.get(&name);
        let shown = meta.map(|m| m.name).unwrap_or(name);
        let message = match meta.map(|m| m.kind) {
            Some(
                kind @ (DefKind::Global | DefKind::Type | DefKind::Decorator | DefKind::Module),
            ) => {
                let what = match kind {
                    DefKind::Global => "global",
                    DefKind::Type => "type",
                    DefKind::Decorator => "decorator",
                    _ => "module",
                };
                format!(
                    "Function `{shown}` doesn't exist -- `{shown}` binds to {what} `{name}` here"
                )
            }
            _ => format!("Function `{shown}` doesn't exist"),
        };
        return Err(CompilerError::new_with_label(span, message));
    }

    let mut preinferred: FxHashMap<Idx<Expr>, TyId> = FxHashMap::default();
    let arg_list = args.into_iter().collect_vec();

    // A hidden member may be the one the arguments match, so only a family
    // with nothing hidden may skip scoring.
    let single = callees.visible.len() == 1 && callees.hidden.is_empty() && !unary;
    let (callee_mid, callee_fn) = if single {
        callees.visible[0]
    } else {
        let mut arg_tys: Vec<TyId> = Vec::new();
        let mut arg_ty: FxHashMap<Idx<Expr>, TyId> = FxHashMap::default();

        if let Some(r) = recv_override {
            arg_tys.push(resolve(gamma, r, span));
        }

        for &a in &arg_list {
            // Field selectors and lambdas need the winning parameter type.
            let t = if matches!(c.body.expr_arena[a], Expr::Lambda { .. }) {
                gamma.fresh_callsite_var(*Q_LAM, span)
            } else if matches!(c.body.expr_arena[a], Expr::SiblingRef(_)) {
                gamma.fresh_callsite_var(*Q_FIELDREF, span)
            } else {
                let t = infer_expr(c, gamma, a)?;
                let t = resolve(gamma, t, span);
                preinferred.insert(a, t);
                t
            };

            arg_tys.push(t);
            arg_ty.insert(a, t);
        }

        match select_overload(
            c,
            gamma,
            family,
            &callees,
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
                if matches!(&e, SelectError::NoFit(_)) && unary {
                    return Err(unary_operator_error(
                        gamma,
                        name,
                        arg_tys.first().copied(),
                        span,
                    ));
                }

                // Retry a tie once the lambda parameter is pinned, before giving an operator error.
                if let SelectError::Tied(_) = e
                    && !gamma.quiet
                    && let Some((v, origin)) = deferrable_operand(gamma, &arg_tys, span)
                {
                    let result = defer_result_var(gamma, origin, span);
                    // Treat a deferred callee as impure until resolved.
                    gamma.invalidate_mutable_refinements();
                    queue_pending(
                        gamma,
                        v,
                        Pending {
                            kind: PendingKind::Call {
                                call_expr,
                                recv_override,
                                arg_ty,
                                arg_tys,
                                preinferred,
                            },
                            origin,
                            span,
                            result,
                        },
                    );
                    return Ok(result);
                }

                if name
                    .rsplit("::")
                    .next()
                    .unwrap_or(name.as_str())
                    .starts_with("__op_")
                {
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
    let (_, subs) = check_call_arguments(
        c,
        gamma,
        function,
        name,
        recv_override,
        arg_list,
        preinferred,
        generic_args,
        Some(call_expr),
        span,
    )?;

    // Record type arguments after checking the actual arguments has pinned them.
    if !function.generics.is_empty() {
        let mut bindings = Vec::with_capacity(function.generics.len());
        for &g in function.generics.iter() {
            let inst = substitute(gamma, g, &subs, span)?;
            bindings.push(resolve(gamma, inst, span));
        }
        gamma.out.call_generic_bindings.insert(call_expr, bindings);
    }

    let ret_ty = function
        .return_ty
        .unwrap_or_else(|| gamma.workspace.builtins.unit());

    // The call's effects occur after receiver and argument evaluation.
    if !fn_has_anno(function, "pure") {
        gamma.invalidate_mutable_refinements();
    }

    let ret_ty = substitute(gamma, ret_ty, &subs, span)?;

    Ok(resolve(gamma, ret_ty, span))
}

/// A fn value has no result expression to wrap in `iter`, so only a lambda's
/// iterable result coerces at a bounded result position. Say so, rather than
/// report that the Vec is not an iterator.
fn reject_iterable_fn_value_result(
    c: BodyCtx,
    gamma: &mut Gamma,
    arg: Idx<Expr>,
    span: Span,
) -> Result<(), CompilerError> {
    let Some(aty) = gamma.out.expr_types.get(arg).copied() else {
        return Ok(());
    };
    let aty = resolve(gamma, aty, span);
    let TypeExpr::Function { ret, .. } = *gamma.get(aty) else {
        return Ok(());
    };
    let ret = aty.with_idx(ret);
    let snap = gamma.out.checkpoint();
    let iterable = implicit_iter_target(c, gamma, ret, span).is_some();
    gamma.out.rollback_to(snap);
    if iterable {
        let rendered = gamma.render(ret);
        comp_error!(
            c.body.expr_spans[arg],
            "this callback returns `{}`, not an iterator; only a lambda's result is coerced -- pass `x => f(x)` or return `.iter()`",
            rendered
        );
    }
    Ok(())
}

/// An `@unwraps` parameter strips the nullable off its argument, so passing a
/// value already known to be non-null is redundant -- rejected like `x ?? d`.
fn reject_redundant_unwrap(
    gamma: &mut Gamma,
    function: &Function,
    name: Ustr,
    param_index: usize,
    arg_ty: TyId,
    span: Span,
) -> Result<(), CompilerError> {
    let unwraps = function
        .param_decls
        .get(param_index)
        .is_some_and(|p| p.annotations.iter().any(|a| a.is_builtin("unwraps")));
    if unwraps && redundant_optional_on(gamma, arg_ty, span) {
        comp_error!(
            span,
            "argument of `{}` is not nullable -- the `{}` is redundant",
            name,
            name
        );
    }
    Ok(())
}

pub(crate) fn call_fn_value(
    c: BodyCtx,
    gamma: &mut Gamma,
    fn_ty: TyId,
    args: IdxRange<Expr>,
    span: Span,
) -> Result<TyId, CompilerError> {
    let fn_ty = resolve(gamma, fn_ty, span);

    if matches!(gamma.get(fn_ty), TypeExpr::Builtin(BuiltinTy::Error)) {
        return Ok(fn_ty);
    }

    let (params, ret) = match *gamma.get(fn_ty) {
        TypeExpr::Function { params, ret } => (params, ret),
        _ => comp_error!(
            span,
            "Attempted to call a non-function type {}",
            gamma.render(fn_ty)
        ),
    };

    let param_tys: Vec<TyId> = params.into_iter().map(|p| fn_ty.with_idx(p)).collect();
    let what = format!("call to value of type {}", gamma.render(fn_ty));
    check_args(c, gamma, &param_tys, args, param_tys.len(), span, &what)?;

    // A function value can mutate state through a capture.
    gamma.invalidate_mutable_refinements();

    Ok(resolve(gamma, fn_ty.with_idx(ret), span))
}
