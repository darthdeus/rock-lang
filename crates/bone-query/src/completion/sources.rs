//! Candidates for contextual completion and whole-call synthesis.

use crate::modules_scope::{ResolvedNames, Spellings};
use crate::query::{LocalScope, VariantShape};
use crate::reachability::{MethodRef, ReachPath, ReachabilityIndex};
use bone_core::modules::FragmentScope;
use std::collections::HashMap;

use bone_core::type_equiv::TyRef;
use bone_incremental::session::Session;
use bone_semantic::prelude::{ModuleId, TyId, Workspace, is_field_ref_former, is_intrinsic_key};
use ustr::Ustr;

use crate::backend::{
    ExpectedArg, analyze_variant, bind_generic, expected_for_anchor, user_module_for_path,
};
use crate::completion::match_policy::admit;
use crate::query::clean_match;
use crate::rank_key::RankKey;
use crate::types::{self, ty_of};

use crate::cursor_context::Record;

use super::classify::{SlotAnchor, balance_delimiters, completion_munges};
use super::rank::*;
use super::select::*;

pub(crate) fn callee_param_names_in_scope(
    ws: &Workspace,
    callee: &str,
    frag: &bone_core::modules::FragmentScope,
) -> Vec<(Ustr, String)> {
    let name = Ustr::from(callee);
    let mut out: Vec<(Ustr, String)> = Vec::new();
    for (_, _, f) in crate::modules_scope::admitted_overloads(ws, frag, name) {
        for p in &f.param_decls {
            if out.iter().any(|(n, _)| *n == p.name) {
                continue;
            }
            out.push((p.name, ws.render(None, p.ty)));
        }
    }
    out
}

/// FieldRef is invariant in its value type.
pub(crate) fn fields_of_type(ws: &Workspace, t_name: Ustr, v: TyId) -> Vec<Ustr> {
    let mut seen: std::collections::HashSet<Ustr> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in ws.modules.iter() {
        let Some(idx) = m.nominals.struct_id(t_name) else {
            continue;
        };
        for fld in &m.structs[idx.def].fields {
            if !ws.types_equivalent(TyRef::of_static(fld.ty), TyRef::of_static(v)) {
                continue;
            }
            if seen.insert(fld.name) {
                out.push(fld.name);
            }
        }
    }
    out
}

fn receiver_owner_binding(
    ws: &Workspace,
    f: &bone_semantic::hir::Function,
    recv_struct: Option<Ustr>,
    recv_elem: Option<Ustr>,
) -> (Option<bone_semantic::hir::VarId>, Option<Ustr>) {
    use bone_semantic::hir::{TypeCtor, TypeExpr};
    let Some(p0) = f.param_decls.first() else {
        return (None, recv_struct);
    };
    match ty_of(ws, None, p0.ty) {
        TypeExpr::Generic(v, _) => (Some(v), recv_struct),
        TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            params,
        } if Some(n) == recv_struct && params.len() == 1 => {
            let arg_idx = params.into_iter().next().unwrap();
            match (ty_of(ws, None, p0.ty.with_idx(arg_idx)), recv_elem) {
                (TypeExpr::Generic(v, _), Some(e)) => (Some(v), Some(e)),
                _ => (None, recv_struct),
            }
        }
        _ => (None, recv_struct),
    }
}

fn fieldref_slot_owner(
    ws: &Workspace,
    f: &bone_semantic::hir::Function,
    pi: usize,
    recv_var: Option<bone_semantic::hir::VarId>,
    recv_owner: Option<Ustr>,
    sibling_owner: impl FnOnce(Ustr) -> Option<Ustr>,
) -> Option<(Ustr, TyId)> {
    use bone_semantic::hir::{TypeCtor, TypeExpr};
    let p = f.param_decls.get(pi)?;
    let (t_idx, v_idx) = match ty_of(ws, None, p.ty) {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            params,
        } if is_field_ref_former(n) && params.len() == 2 => {
            let mut it = params.into_iter();
            (it.next().unwrap(), it.next().unwrap())
        }
        _ => return None,
    };
    let v = p.ty.with_idx(v_idx);
    let owner = match ty_of(ws, None, p.ty.with_idx(t_idx)) {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            ..
        } => n,
        TypeExpr::Generic(tv, gname) => {
            if recv_var == Some(tv) {
                recv_owner?
            } else {
                sibling_owner(gname)?
            }
        }
        _ => return None,
    };
    Some((owner, v))
}

/// For UFCS, the receiver shifts the written argument indices by one.
fn sibling_owner_bind(
    ws: &Workspace,
    path: &str,
    call: &crate::cursor_context::CallArgument,
    recv_offset: usize,
    f: &bone_semantic::hir::Function,
    gname: Ustr,
) -> Option<Ustr> {
    let mid = user_module_for_path(ws, path)?;
    let inf = ws.inference(mid)?;
    for (j, &aoff) in call.preceding.iter().enumerate() {
        let Some(pty) = f.param_decls.get(j + recv_offset).map(|p| p.ty) else {
            continue;
        };
        let Some(at) = bone_semantic::query::type_at(ws, mid, inf, aoff) else {
            continue;
        };
        if let Some(bound) = bind_generic(ws, gname, None, pty, at.body, at.ty) {
            if let Some(sn) = types::nominal_name(ws, at.body, bound) {
                return Some(sn);
            }
        }
    }
    None
}

pub(crate) fn field_ref_candidates(
    ws: &Workspace,
    path: &str,
    call: &crate::cursor_context::CallArgument,
    prefix: &str,
    lead_dot: bool,
) -> Option<Vec<Cand>> {
    let name = Ustr::from(call.callee.as_str());
    let frag = crate::modules_scope::fragment(ws, Ustr::from(path))?;
    let (recv_struct, recv_elem) = call
        .receiver
        .as_ref()
        .and_then(|recv| {
            let mid = user_module_for_path(ws, path)?;
            let m = crate::query::members_of_receiver_expr(ws, mid, recv.end)?;
            Some((Some(m.owner), m.sole_arg))
        })
        .unwrap_or((None, None));
    let recv_offset = usize::from(call.receiver.is_some());

    let (fields, v) =
        crate::modules_scope::admitted_overloads(ws, frag, name).find_map(|(_, _, f)| {
            let (recv_var, recv_owner) = receiver_owner_binding(ws, f, recv_struct, recv_elem);
            let (owner, v) =
                fieldref_slot_owner(ws, f, call.index, recv_var, recv_owner, |gname| {
                    sibling_owner_bind(ws, path, call, recv_offset, f, gname)
                })?;
            let fields = fields_of_type(ws, owner, v);
            (!fields.is_empty()).then_some((fields, v))
        })?;
    let pfx = prefix.as_bytes();
    let ty = ws.render(None, v);
    Some(
        fields
            .into_iter()
            .filter(|name| admit(name.as_bytes(), pfx))
            .map(|name| {
                Cand::new(
                    arg_rank_key(0, 0, name.as_str(), pfx, 1, 0, 0),
                    CandName::Sym(name),
                    CandShape::FieldRef {
                        ty: ty.clone(),
                        add_dot: !lead_dot,
                    },
                )
            })
            .collect(),
    )
}

pub(crate) fn arg_matches_residual(q: &[u8], k: usize, arg: &str) -> bool {
    let mut qi = k.min(q.len());
    if qi < q.len() && q[qi] == b'(' {
        qi += 1;
    }
    for &c in arg.as_bytes() {
        if qi < q.len() && c.to_ascii_lowercase() == q[qi] {
            qi += 1;
        }
    }
    if qi < q.len() && q[qi] == b')' {
        qi += 1;
    }
    qi == q.len()
}

struct CtFn {
    member: MethodRef,
    first_param_ty: Ustr,
    multiparam: bool,
}

struct ScopedCtFn {
    index: usize,
    name: Ustr,
    name_lc: String,
}

struct CtArg {
    root: Option<Ustr>,
    text: String,
    payload: VariantShape,
    method: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct ValueRoot {
    pub name: Ustr,
    pub ty: Ustr,
}

impl ValueRoot {
    fn direct(self) -> CtArg {
        CtArg {
            root: Some(self.name),
            text: self.name.to_string(),
            payload: VariantShape::Unit,
            method: false,
        }
    }

    fn arguments<'a>(
        self,
        reach: &'a ReachabilityIndex,
        ws: &'a Workspace,
        frag: &'a FragmentScope,
        names: &'a Spellings,
    ) -> impl Iterator<Item = (Ustr, CtArg)> + 'a {
        reach
            .arg_paths_from(self.ty.as_str())
            .iter()
            .filter_map(move |path| {
                if path.is_method && path.takes_args
                    || !crate::reachability::path_admitted(ws, frag, path)
                {
                    return None;
                }
                let text = self.path_text(path, names)?;
                Some((
                    Ustr::from(&path.final_ty),
                    CtArg {
                        root: Some(self.name),
                        text: if path.is_method {
                            format!("{text}()")
                        } else {
                            text
                        },
                        payload: VariantShape::Unit,
                        method: path.via_call,
                    },
                ))
            })
    }

    pub(crate) fn path_text(self, path: &ReachPath, names: &Spellings) -> Option<String> {
        let suffix = crate::modules_scope::display_method_suffix_with_names(
            &path.suffix,
            &path.method_members,
            names,
        )?;
        Some(format!("{}.{suffix}", self.name))
    }
}

pub(crate) fn local_roots<'a>(
    ws: &'a Workspace,
    scope: &'a LocalScope<'_>,
) -> impl Iterator<Item = ValueRoot> + 'a {
    scope.bindings.iter().filter_map(|binding| {
        Some(ValueRoot {
            name: binding.name,
            ty: crate::reachability::type_key_in_body(ws, scope.inference, binding.ty?),
        })
    })
}

pub(crate) struct CallThroughIndex {
    fns: Vec<CtFn>,
    globals: Vec<ValueRoot>,
}

impl CallThroughIndex {
    pub(crate) fn build(ws: &Workspace) -> Self {
        let fns = ws
            .modules
            .iter()
            .flat_map(|m| {
                m.functions.iter().filter_map(|(fid, f)| {
                    let param = f.param_decls.first()?;
                    (!f.name.as_str().starts_with("__")).then(|| CtFn {
                        member: (f.name, m.id, fid),
                        first_param_ty: crate::reachability::type_key(ws, param.ty),
                        multiparam: f.param_decls.len() > 1,
                    })
                })
            })
            .collect();
        let globals = ws
            .modules
            .iter()
            .flat_map(|m| {
                m.global_by_name.iter().map(|(&name, &idx)| ValueRoot {
                    name,
                    ty: crate::reachability::type_key(ws, m.globals[idx].ty),
                })
            })
            .collect();
        Self { fns, globals }
    }

    pub(crate) fn globals<'a>(
        &'a self,
        names: &'a ResolvedNames,
    ) -> impl Iterator<Item = ValueRoot> + 'a {
        self.globals.iter().flat_map(|root| {
            names
                .globals
                .get(&root.name)
                .into_iter()
                .flatten()
                .map(|&name| ValueRoot { name, ty: root.ty })
        })
    }
}

pub(crate) struct ScopedCallThroughIndex {
    fns_by_first_char: HashMap<u8, Vec<ScopedCtFn>>,
    global_type_args: HashMap<Ustr, Vec<CtArg>>,
}

impl ScopedCallThroughIndex {
    pub(crate) fn build(
        ws: &Workspace,
        frag: &FragmentScope,
        names: &ResolvedNames,
        index: &CallThroughIndex,
        reach: &ReachabilityIndex,
    ) -> Self {
        let mut fns_by_first_char: HashMap<u8, Vec<ScopedCtFn>> = HashMap::new();
        let mut global_type_args: HashMap<Ustr, Vec<CtArg>> = HashMap::new();
        for (index, cf) in index.fns.iter().enumerate() {
            let (key, mid, fid) = cf.member;
            let Some(spellings) = names.callables.get(&key) else {
                continue;
            };
            if !crate::modules_scope::callable_member_admitted(ws, frag, key, mid, fid) {
                continue;
            }
            global_type_args.entry(cf.first_param_ty).or_default();
            for &name in spellings {
                let name_lc = name.as_str().to_lowercase();
                if let Some(&first) = name_lc.as_bytes().first() {
                    fns_by_first_char
                        .entry(first)
                        .or_default()
                        .push(ScopedCtFn {
                            index,
                            name,
                            name_lc,
                        });
                }
            }
        }
        for root in index.globals(names) {
            if let Some(args) = global_type_args.get_mut(&root.ty) {
                args.push(root.direct());
            }
        }
        for (&ty, args) in &mut global_type_args {
            if let Some(spellings) = names.types.get(&ty) {
                for (variant, shape) in crate::query::enum_variants_with_shape(ws, ty.as_str()) {
                    args.extend(spellings.iter().map(|&name| CtArg {
                        root: None,
                        text: format!("{name}::{variant}"),
                        payload: shape,
                        method: false,
                    }));
                }
            }
        }
        for root in index.globals(names) {
            for (ty, arg) in root.arguments(reach, ws, frag, &names.callables) {
                if let Some(args) = global_type_args.get_mut(&ty) {
                    args.push(arg);
                }
            }
        }
        Self {
            fns_by_first_char,
            global_type_args,
        }
    }
}

pub(crate) fn call_through_param_completions(
    ws: &Workspace,
    locals: &LocalScope<'_>,
    prefix: &str,
    index: &CallThroughIndex,
    frag: &FragmentScope,
    names: &ResolvedNames,
    scoped_index: &ScopedCallThroughIndex,
    reach: &crate::reachability::ReachabilityIndex,
    score_leads: bool,
    mut expected: Option<&mut ExpectedArg>,
    enclosing_fn: Option<&str>,
    sink: &mut Sink,
) {
    let q = prefix.as_bytes();
    if q.len() < 2 {
        // One char only completes the fn name's first letter.
        return;
    }
    let Some(scoped_fns) = scoped_index.fns_by_first_char.get(&q[0]) else {
        return;
    };
    let shadowed = |name| locals.get(name).is_some();
    let mut local_args: HashMap<Ustr, Vec<CtArg>> = HashMap::new();
    for root in local_roots(ws, locals) {
        local_args.entry(root.ty).or_default().push(root.direct());
        for (ty, arg) in root.arguments(reach, ws, frag, &names.callables) {
            local_args.entry(ty).or_default().push(arg);
        }
    }

    let mut match_memo: HashMap<_, Vec<(&CtArg, Vec<i32>, i32, bool)>> = HashMap::new();
    let mut prefix_scores = Vec::new();
    let mut cands = Vec::new();
    let mut committed = Vec::new();
    for (si, scoped) in scoped_fns.iter().enumerate() {
        let cf = &index.fns[scoped.index];
        if enclosing_fn == Some(cf.member.0.as_str()) || shadowed(scoped.name) {
            continue;
        }
        let k = prefix_subseq_len(q, &scoped.name_lc);
        if !clean_match(scoped.name_lc.as_bytes(), &q[..k.min(q.len())]) {
            continue;
        }
        let t = cf.first_param_ty;
        let local = local_args.get(&t).map_or(&[][..], Vec::as_slice);
        if k == q.len() {
            let mut confident = local.iter().filter(|a| !a.method);
            if let Some(a) = confident.next()
                && confident.next().is_none()
            {
                let score = fuzzy_score(scoped.name.as_bytes(), q) + 5000;
                committed.push((score, si, a, 0));
            }
            continue;
        }
        if k < q.len().div_ceil(2) && !local.iter().any(|a| arg_matches_residual(q, k, &a.text)) {
            continue;
        }
        let group = match_memo.entry((t, k)).or_insert_with(|| {
            let global = scoped_index
                .global_type_args
                .get(&t)
                .into_iter()
                .flatten()
                .filter(|arg| !arg.root.is_some_and(shadowed))
                .map(|arg| (arg, false));
            let local = local_args
                .get(&t)
                .into_iter()
                .flatten()
                .map(|arg| (arg, true));
            let mut v = Vec::new();
            for (a, local) in global.chain(local) {
                if arg_matches_residual(q, k, &a.text) {
                    let mut suffix = Vec::new();
                    fuzzy_suffix_scores(a.text.as_bytes(), q, &mut suffix);
                    v.push((a, suffix, arg_path_penalty(&a.text), local));
                }
            }
            v
        });
        fuzzy_prefix_scores(scoped_fns[si].name.as_bytes(), q, &mut prefix_scores);
        let mut scored: Vec<(i32, &CtArg, bool)> = Vec::with_capacity(group.len());
        for (a, suffix, penalty, local) in group.iter() {
            let score = combine_scores(&prefix_scores, suffix) - penalty;
            scored.push((score, a, *local));
        }
        let (mut methods, simple): (Vec<_>, Vec<_>) =
            scored.into_iter().partition(|(_, a, _)| a.method);
        if methods.len() > PER_FN_METHOD_CAP {
            methods.select_nth_unstable_by(PER_FN_METHOD_CAP, |x, y| {
                y.0.cmp(&x.0)
                    .then_with(|| ci_cmp(x.1.text.as_bytes(), y.1.text.as_bytes()))
            });
            methods.truncate(PER_FN_METHOD_CAP);
        }
        for (score, a, local) in simple.into_iter().chain(methods) {
            cands.push((score, si, a, if local { 1 } else { 4 }));
        }
    }
    cands.extend(committed);
    if cands.len() > CALL_THROUGH_CAP {
        cands.select_nth_unstable_by(CALL_THROUGH_CAP, |x, y| {
            y.0.cmp(&x.0)
                .then_with(|| ci_cmp(x.2.text.as_bytes(), y.2.text.as_bytes()))
        });
        cands.truncate(CALL_THROUGH_CAP);
    }
    let ct_start = sink.cands.len();
    for (score, si, a, type_rank) in cands {
        let scoped = &scoped_fns[si];
        let cf = &index.fns[scoped.index];
        let fname = scoped.name;
        let type_rank = match expected.as_deref_mut() {
            Some(exp) if type_rank < 4 => {
                if exp.accepts_fn_returning(ws, cf.member.0.as_str()) {
                    type_rank
                } else {
                    4
                }
            }
            _ => type_rank,
        };
        let call = format!("{fname}({})", a.text);
        if !sink.admit(&call) {
            continue;
        }
        let (ex, bd) = match_band_exact(&call, q, q);
        let key = if score_leads {
            RankKey {
                exact: ex,
                type_rank: if type_rank >= 4 { 4 } else { 0 },
                band: bd,
                kind: type_rank.min(3),
                ..Default::default()
            }
        } else {
            // A call synth is fn-shaped (kind 2), so a real local of the slot type beats it.
            RankKey {
                exact: ex,
                type_rank,
                band: bd,
                kind: 2,
                ..Default::default()
            }
        };
        let key = RankKey {
            method: 1,
            fine: RankKey::fine(score),
            len: RankKey::len_key(score, call.len()),
            ..key
        };
        sink.push(Cand::new(
            key,
            CandName::Owned(call),
            CandShape::CallThrough {
                fname,
                arg: a.text.clone(),
                payload: a.payload,
                multiparam: cf.multiparam,
            },
        ));
    }
    // The final stable sort preserves insertion order on full ties.
    sink.cands[ct_start..].sort_by(cand_cmp);
}

pub(crate) fn build_freq_index(ws: &Workspace) -> HashMap<Ustr, u32> {
    use bone_semantic::hir::{self, TypeCtor, TypeExpr};
    let mut counts: HashMap<Ustr, u32> = HashMap::new();
    for m in ws.modules.iter() {
        for te in m.live_type_exprs() {
            if let TypeExpr::App {
                ctor: TypeCtor::Nominal(n),
                ..
            } = te
            {
                *counts.entry(*n).or_insert(0) += 1;
            }
        }
        for body in m.bodies.values() {
            for te in body.lowered_tys.values() {
                if let TypeExpr::App {
                    ctor: TypeCtor::Nominal(n),
                    ..
                } = te
                {
                    *counts.entry(*n).or_insert(0) += 1;
                }
            }
            for (_, expr) in body.expr_arena.iter() {
                let n = match expr {
                    hir::Expr::Call {
                        callee: hir::Callee::Unresolved { name, .. },
                        ..
                    } => *name,
                    hir::Expr::SafeNavCall { method, .. } => *method,
                    hir::Expr::Global(n) => *n,
                    hir::Expr::StructLiteral { name, .. } => *name,
                    hir::Expr::EnumVariant { enum_name, .. } => *enum_name,
                    _ => continue,
                };
                *counts.entry(n).or_insert(0) += 1;
            }
        }
    }
    counts
}

pub(crate) fn fn_is_coroutine_in_scope(
    ws: &Workspace,
    name: &str,
    frag: &bone_core::modules::FragmentScope,
) -> bool {
    let n = Ustr::from(name);
    crate::modules_scope::admitted_overloads(ws, frag, n).any(|(_, _, f)| f.is_coroutine)
}

/// The host's `Wait` yield type is an enum, so this probes structs and enums.
pub(crate) fn nominal_exists(
    ws: &Workspace,
    frag: &bone_core::modules::FragmentScope,
    name: &str,
) -> bool {
    let Some(key) = crate::modules_scope::resolve_type_key(ws, frag, name) else {
        return false;
    };
    ws.modules
        .iter()
        .any(|m| m.nominals.struct_id(key).is_some() || m.nominals.enum_id(key).is_some())
}

pub(crate) fn fn_prefix_struct(
    ws: &Workspace,
    frag: &bone_core::modules::FragmentScope,
    fn_name: &str,
) -> Option<Ustr> {
    // `mob_attack_co` and `spawn_mob` both name `Mob`; the leading segment wins.
    fn_name
        .split('_')
        .filter(|s| !s.is_empty())
        .find_map(|seg| {
            let mut chars = seg.chars();
            let cap: String = chars.next()?.to_ascii_uppercase().to_string() + chars.as_str();
            let key = crate::modules_scope::resolve_type_key(ws, frag, &cap)?;
            ws.modules
                .iter()
                .any(|m| m.nominals.struct_id(key).is_some())
                .then(|| Ustr::from(&cap))
        })
}

pub(crate) fn decorator_candidates(
    ws: &Workspace,
    frag: &bone_core::modules::FragmentScope,
    prefix: &str,
) -> Vec<Cand> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let names =
        crate::modules_scope::spellings_by_key(ws, frag, bone_core::modules::DefKind::Decorator);
    let pfx = prefix.as_bytes();
    for m in &ws.modules {
        for (_, dec) in m.decorators.iter() {
            for &name in names.get(&dec.def_key).into_iter().flatten() {
                if !admit(name.as_bytes(), pfx) || !seen.insert(name) {
                    continue;
                }
                let params = crate::query::rendered_params(ws, &dec.param_decls);
                let (exact, band) = match_band_exact(name.as_str(), pfx, pfx);
                out.push(Cand::new(
                    RankKey {
                        exact,
                        band,
                        len: RankKey::len_key(9999, name.len()),
                        ..Default::default()
                    },
                    CandName::Sym(name),
                    CandShape::Decorator {
                        detail: (!params.is_empty())
                            .then(|| format!("deco ({})", params.join(", "))),
                    },
                ));
            }
        }
    }
    out
}

pub(crate) fn record_candidates(
    ws: &Workspace,
    file: Ustr,
    prefix: &[u8],
    record: &Record,
    written: &str,
) -> Option<Vec<Cand>> {
    let key = crate::modules_scope::resolve_type_key_in_file(ws, file, record.name())?;
    let fields = match record {
        Record::Struct { .. } => crate::query::struct_fields_by_name(ws, key.as_str()),
        Record::Variant { variant, .. } => {
            crate::query::variant_record_fields(ws, key.as_str(), variant)
                .into_iter()
                .map(|(name, ty)| (name, ty, false))
                .collect()
        }
    };
    // An empty field set may be a block head misparsed as a literal.
    if fields.is_empty() {
        return None;
    }
    let present: std::collections::HashSet<_> = written
        .split([',', '{', '}'])
        .filter_map(|seg| seg.split(':').next().map(str::trim))
        .collect();
    Some(
        fields
            .into_iter()
            .enumerate()
            .filter(|(_, (name, _, _))| {
                !present.contains(name.as_str()) && admit(name.as_bytes(), prefix)
            })
            .map(|(i, (name, ty, has_default))| {
                Cand::new(
                    closed_set_rank(&name, prefix, u8::from(has_default), i),
                    CandName::Owned(name),
                    CandShape::Reach {
                        final_ty: ty,
                        is_method: false,
                        takes_args: false,
                    },
                )
            })
            .collect(),
    )
}

pub(crate) fn variant_candidates(
    ws: &Workspace,
    file: Ustr,
    enum_name: &str,
    prefix: &str,
) -> Option<Vec<Cand>> {
    let key = crate::modules_scope::resolve_type_key_in_file(ws, file, enum_name)?;
    let variants = crate::query::enum_variants_with_shape(ws, key.as_str());
    if variants.is_empty() {
        return None;
    }
    let pfx = prefix.as_bytes();
    let matches_prefix = |name: &str| admit(name.as_bytes(), pfx);
    let none_match = !variants.iter().any(|(name, _)| matches_prefix(name));
    Some(
        variants
            .into_iter()
            .enumerate()
            .filter(|(_, (name, _))| none_match || matches_prefix(name))
            .map(|(i, (name, shape))| {
                Cand::new(
                    closed_set_rank(&name, pfx, 0, i),
                    CandName::Owned(name),
                    CandShape::Variant {
                        shape,
                        filter_text: none_match.then(|| prefix.to_string()),
                    },
                )
            })
            .collect(),
    )
}

/// The scrutinee precedes the cursor and survives speculative insertions.
pub(crate) fn pattern_scrutinee_enum(
    session: &mut Session,
    path: &str,
    src: &str,
    offset: usize,
    anchor: usize,
) -> Option<String> {
    let off = offset.min(src.len());
    let (head, tail) = src.split_at(off);
    let arm_munge = balance_delimiters(format!("{head}__rock_completion__ => {{}}{tail}"));
    let [m1, m2] = completion_munges(src, offset);
    for munged in [arm_munge, m1, m2] {
        let hit = analyze_variant(session, path, &munged, |ws, _| {
            let mid = user_module_for_path(ws, path)?;
            let inf = ws.inference(mid)?;
            let at = bone_semantic::query::type_at(ws, mid, inf, anchor)?;
            let bone_semantic::hir::TypeExpr::App {
                ctor: bone_semantic::hir::TypeCtor::Nominal(name),
                ..
            } = ty_of(ws, at.body, at.ty)
            else {
                return None;
            };
            (!crate::query::enum_variants_with_shape(ws, name.as_str()).is_empty())
                .then(|| name.to_string())
        });
        if let Some(Some(name)) = hit {
            return Some(name);
        }
    }
    None
}

pub(crate) fn open_paren_follows(src: &str, offset: usize) -> bool {
    src.as_bytes()[offset..]
        .iter()
        .find(|&&b| b != b' ' && b != b'\t')
        == Some(&b'(')
}

/// Substituted fields can lack an arena type ID and only have rendered text.
fn member_ty_match(
    mut expected: Option<&mut ExpectedArg>,
    ty_id: Option<TyId>,
    ty_render: &str,
) -> bool {
    let Some(exp) = expected.as_mut() else {
        return false;
    };
    match ty_id {
        Some(t) => exp.accepts(t),
        None => exp.rendered.iter().any(|e| e == ty_render),
    }
}

fn field_ref_slot(
    ws: &Workspace,
    recv_struct: Ustr,
    recv_elem: Option<Ustr>,
    f: &bone_semantic::hir::Function,
) -> Option<(usize, Vec<Ustr>)> {
    // A literal `FieldRef<Other, V>` on a `recv` method is not a `recv.method(.field)` slot.
    let (recv_var, recv_owner) = receiver_owner_binding(ws, f, Some(recv_struct), recv_elem);
    let recv_owner = recv_owner?;
    for pi in 1..f.param_decls.len() {
        let Some((owner, v)) = fieldref_slot_owner(ws, f, pi, recv_var, Some(recv_owner), |_| None)
        else {
            continue;
        };
        if owner != recv_owner {
            continue;
        }
        let fields = fields_of_type(ws, owner, v);
        if !fields.is_empty() {
            return Some((pi - 1, fields));
        }
    }
    None
}

/// The FieldRef arg pre-filled: `mob.set_co` -> `set_co(.attack_co, ${1:next})`.
fn field_ref_insert(
    name: &str,
    f: &bone_semantic::hir::Function,
    field_arg_idx: usize,
    fields: &[Ustr],
) -> String {
    let mut stop = if fields.len() > 1 { 2 } else { 1 };
    let mut parts = Vec::new();
    for (j, p) in f.param_decls.iter().skip(1).enumerate() {
        if j == field_arg_idx {
            if fields.len() == 1 {
                parts.push(format!(".{}", fields[0]));
            } else {
                let joined = fields
                    .iter()
                    .map(|x| format!(".{x}"))
                    .collect::<Vec<_>>()
                    .join(",");
                parts.push(format!("${{1|{joined}|}}"));
            }
        } else {
            parts.push(format!("${{{stop}:{}}}", p.name));
            stop += 1;
        }
    }
    format!("{name}({})", parts.join(", "))
}

/// A generic first parameter keeps these out of the nominal method list.
fn field_ref_methods<'a>(
    ws: &'a Workspace,
    recv_struct: Ustr,
    recv_elem: Option<Ustr>,
    frag: &bone_core::modules::FragmentScope,
) -> Vec<(&'a bone_semantic::hir::Function, usize, Vec<Ustr>)> {
    use bone_semantic::hir::{TypeCtor, TypeExpr};
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<Ustr> = std::collections::HashSet::new();
    for m in ws.modules.iter() {
        for (fid, f) in m.functions.iter() {
            if f.name.as_str().starts_with("__") {
                continue;
            }
            if !crate::modules_scope::callable_member_admitted(ws, frag, f.name, m.id, fid) {
                continue;
            }
            let Some(p0) = f.param_decls.first() else {
                continue;
            };
            let p0_ok = match ty_of(ws, None, p0.ty) {
                TypeExpr::Generic(..) => true,
                TypeExpr::App {
                    ctor: TypeCtor::Nominal(n),
                    params,
                } => n == recv_struct && params.len() == 1 && recv_elem.is_some(),
                _ => false,
            };
            if !p0_ok {
                continue;
            }
            let Some((arg_idx, fields)) = field_ref_slot(ws, recv_struct, recv_elem, f) else {
                continue;
            };
            if seen.insert(f.name) {
                out.push((f, arg_idx, fields));
            }
        }
    }
    out
}

pub(crate) fn member_candidates(
    ws: &Workspace,
    mid: ModuleId,
    offset: usize,
    members: crate::query::Members<'_>,
    prefix: &str,
    arg_slot: Option<(&str, usize)>,
    anchor: Option<SlotAnchor>,
    reach: &crate::reachability::ReachabilityIndex,
    bool_ctx: bool,
) -> Vec<Cand> {
    let crate::query::Members {
        fields,
        owner,
        vec_elem,
        sole_arg,
        is_iter,
    } = members;
    let Some(frag) = crate::modules_scope::fragment(ws, ws.module(mid).file) else {
        return Vec::new();
    };
    let methods = crate::query::ufcs_methods_in_scope(ws, owner, vec_elem, is_iter, frag);
    if fields.is_empty() && methods.is_empty() {
        return Vec::new();
    }

    let mut expected = ws.inference(mid).and_then(|inf| {
        arg_slot
            .and_then(|(callee, idx)| {
                ExpectedArg::for_call(ws, mid, inf, offset, callee, idx, None)
            })
            .or_else(|| expected_for_anchor(ws, mid, inf, offset, anchor?))
    });
    let arg_ctx = expected.is_some();
    let type_rank = |is_bool: bool, type_match: bool| {
        u8::from(if bool_ctx {
            !is_bool
        } else {
            arg_ctx && !type_match
        })
    };

    let pfx = prefix.as_bytes();
    let mut out = Vec::new();
    for (name, ty, id) in fields {
        let matches = member_ty_match(expected.as_mut(), id, &ty);
        if (name.starts_with('_') && !prefix.starts_with('_')) || !admit(name.as_bytes(), pfx) {
            continue;
        }
        let boost = if pfx.is_empty() { 0 } else { 25 };
        out.push(Cand::new(
            arg_rank_key(type_rank(ty == "bool", matches), 0, &name, pfx, 1, 0, boost),
            CandName::Owned(name),
            CandShape::Reach {
                final_ty: ty,
                is_method: false,
                takes_args: false,
            },
        ));
    }
    let callable_spellings =
        crate::modules_scope::spellings_by_key(ws, frag, bone_core::modules::DefKind::Callable);
    let field_ref_matches = field_ref_methods(ws, owner, sole_arg, frag);
    let field_ref_names: std::collections::HashSet<_> = field_ref_matches
        .iter()
        .flat_map(|(f, _, _)| callable_spellings.get(&f.name).into_iter().flatten())
        .copied()
        .collect();
    for m in methods {
        let matches = member_ty_match(
            expected.as_mut(),
            m.return_ty,
            m.returns.as_deref().unwrap_or(""),
        );
        if !admit(m.name.as_bytes(), pfx) || field_ref_names.contains(&Ustr::from(m.name.as_str()))
        {
            continue;
        }
        out.push(Cand::new(
            arg_rank_key(type_rank(false, matches), 0, &m.name, pfx, 1, 0, 0),
            CandName::Owned(m.name),
            CandShape::Method {
                signature: m.signature,
                takes_args: m.takes_args,
                snippet: None,
            },
        ));
    }
    for (f, idx, fields) in field_ref_matches {
        let name = bone_core::modules::display_name(f.name.as_str());
        if !admit(name.as_bytes(), pfx) {
            continue;
        }
        out.push(Cand::new(
            arg_rank_key(type_rank(false, false), 0, name, pfx, 1, 0, 0),
            CandName::Owned(name.to_string()),
            CandShape::Method {
                signature: crate::query::render_fn_signature(ws, f.name, f),
                takes_args: true,
                snippet: Some(field_ref_insert(name, f, idx, &fields)),
            },
        ));
    }
    if is_intrinsic_key(owner, "Entity") {
        for cand in &mut out {
            if cand.name.as_str() == "get" && matches!(cand.shape, CandShape::Method { .. }) {
                cand.key = RankKey {
                    len: RankKey::len_key(9999, 3),
                    ..Default::default()
                };
            }
        }
        let unwrap = "get().unwrap()";
        if clean_match(unwrap.as_bytes(), pfx) {
            out.push(Cand::new(
                RankKey {
                    fine: RankKey::fine(9998),
                    len: RankKey::len_key(9998, unwrap.len()),
                    ..Default::default()
                },
                CandName::Sym(Ustr::from(unwrap)),
                CandShape::EntityUnwrap,
            ));
        }
    }
    let targets: &[String] = expected.as_ref().map_or(&[], |e| &e.rendered);
    if targets.is_empty() {
        return out;
    }
    let type_spellings =
        crate::modules_scope::spellings_by_key(ws, frag, bone_core::modules::DefKind::Type);
    let mut seen = std::collections::HashSet::new();
    for p in reach
        .paths_to(owner.as_str(), targets, REACH_CAP, Some((ws, frag)))
        .paths
    {
        if p.dist <= 1 {
            continue;
        }
        let Some(suffix) = crate::modules_scope::display_method_suffix_with_names(
            &p.suffix,
            &p.method_members,
            &callable_spellings,
        ) else {
            continue;
        };
        if !clean_match(suffix.as_bytes(), pfx) || !seen.insert(suffix.clone()) {
            continue;
        }
        out.push(Cand::new(
            arg_rank_key(0, 0, &suffix, pfx, p.dist, u8::from(p.is_method), 0),
            CandName::Owned(suffix),
            CandShape::Reach {
                final_ty: crate::modules_scope::display_type_with_names(
                    ws,
                    &p.final_ty,
                    &type_spellings,
                ),
                is_method: p.is_method,
                takes_args: p.takes_args,
            },
        ));
    }
    out
}
