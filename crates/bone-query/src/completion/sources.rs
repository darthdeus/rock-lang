//! Candidates for contextual completion and whole-call synthesis.

use std::collections::HashMap;

use lsp_types::{CompletionItem, CompletionItemKind, InsertTextFormat};

use bone_core::type_equiv::TyRef;
use bone_incremental::session::Session;
use bone_semantic::prelude::{
    Idx, InferenceResult, ModuleId, TyId, Workspace, is_field_ref_former, is_intrinsic_key,
};
use ustr::Ustr;

use crate::backend::{
    ExpectedArg, analyze_variant, bind_generic, expected_for_anchor, preceding_arg_starts,
    user_module_for_path,
};
use crate::completion::match_policy::admit;
use crate::query::{SymCat, clean_match};
use crate::rank_key::RankKey;
use crate::types::{self, ty_of};

use super::classify::{SlotAnchor, balance_delimiters, completion_munges, in_pattern_position};
use super::rank::*;
use super::select::*;

/// Declared params of every accessible fn named `callee`, unioned across
/// overloads in first-seen order (param name + rendered declared type). Drives
/// named-arg completion at a call site.
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

/// Struct `t_name`'s field names whose declared type is equivalent to the
/// static type `v` -- the field-set a `FieldRef<t_name, V>` slot admits
/// (typechecker invariance). Deduped, first-seen order. Shared by the arg-position field-ref
/// path and the method-selection-time pre-fill in `build_member_items`.
pub(crate) fn fields_of_type(ws: &Workspace, t_name: Ustr, v: TyId) -> Vec<Ustr> {
    let mut seen: std::collections::HashSet<Ustr> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in ws.modules.iter() {
        let Some(&idx) = m.struct_by_name.get(&t_name) else {
            continue;
        };
        for fld in &m.structs[idx].fields {
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

/// Bind the FieldRef owner from a receiver of type T or Ctor<T>.
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

/// Resolve the owner from a nominal, receiver or preceding argument.
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

/// Bind a generic named `gname` from a PRECEDING paren arg's inferred type --
/// the non-UFCS prefix-call owner path (`poll_slot(e, .co)` binds `T` = Mob
/// from `e`). `recv_offset` shifts the param index for a UFCS receiver (paren
/// arg `j` is `param_decls[j + recv_offset]`).
fn sibling_owner_bind(
    ws: &Workspace,
    path: &str,
    src: &str,
    offset: usize,
    arg_idx: usize,
    recv_offset: usize,
    f: &bone_semantic::hir::Function,
    gname: Ustr,
) -> Option<Ustr> {
    let mid = user_module_for_path(ws, path)?;
    let inf = ws.inference(mid)?;
    let starts = preceding_arg_starts(src, offset);
    for (j, &aoff) in starts
        .iter()
        .enumerate()
        .take(arg_idx.saturating_sub(recv_offset))
    {
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

/// Suggest fields compatible with at least one overload's FieldRef slot.
pub(crate) fn field_ref_arg_completions(
    ws: &Workspace,
    path: &str,
    src: &str,
    offset: usize,
    callee: &str,
    arg_idx: usize,
    prefix: &str,
    lead_dot: bool,
    recv_end: Option<usize>,
) -> Option<Vec<CompletionItem>> {
    let name = Ustr::from(callee);
    let frag = crate::modules_scope::fragment(ws, Ustr::from(path))?;
    // UFCS receiver struct + sole-type-arg element (`Entity<Glyph>` ->
    // `("Entity", Some("Glyph"))`); `(None, None)` for a non-UFCS prefix call.
    let (recv_struct, recv_elem) = recv_end
        .and_then(|e| {
            let mid = user_module_for_path(ws, path)?;
            let m = crate::query::members_of_receiver_expr(ws, mid, e)?;
            Some((Some(m.owner), m.sole_arg))
        })
        .unwrap_or((None, None));
    let recv_offset = usize::from(recv_end.is_some());

    // First overload whose `arg_idx` slot resolves to a FieldRef with >=1 field.
    let (t_name, v) =
        crate::modules_scope::admitted_overloads(ws, frag, name).find_map(|(_fmid, _fid, f)| {
            let (recv_var, recv_owner) = receiver_owner_binding(ws, f, recv_struct, recv_elem);
            let (owner, v) = fieldref_slot_owner(ws, f, arg_idx, recv_var, recv_owner, |gname| {
                sibling_owner_bind(ws, path, src, offset, arg_idx, recv_offset, f, gname)
            })?;
            (!fields_of_type(ws, owner, v).is_empty()).then_some((owner, v))
        })?;

    let pfx = prefix.as_bytes();
    let mut out = Vec::new();
    let v_str = ws.render(None, v);
    for fld_name in fields_of_type(ws, t_name, v) {
        if !(prefix.is_empty() || admit(fld_name.as_bytes(), pfx)) {
            continue;
        }
        // Dot already typed -> insert the bare name; otherwise add it.
        let insert = if lead_dot {
            fld_name.to_string()
        } else {
            format!(".{}", fld_name)
        };
        out.push(CompletionItem {
            sort_text: Some(arg_sort_text(0, 0, fld_name.as_str(), pfx, 1, 0, 0)),
            // The label carries the leading `.` -- a field ref IS `.co` (no
            // receiver), unlike member completion where the `recv.` is
            // already typed. `filter_text` stays the bare name so typing
            // `co` (with or without the dot) still matches.
            label: format!(".{}: {}", fld_name, v_str),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(v_str.clone()),
            insert_text: Some(insert),
            filter_text: Some(fld_name.to_string()),
            ..Default::default()
        });
    }
    out.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    Some(out)
}

/// Per-fn counters accumulated inside `call_through_param_completions`, returned
/// to `completion()` so it can fold them into the phase timings.
#[derive(Clone, Copy, Default)]
pub(crate) struct CtCounts {
    pub(crate) fns: usize,
    pub(crate) arg_iters: usize,
    pub(crate) survivors: usize,
}

/// Complete a prefix such as `drawr` to a call such as `draw(rect)`.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ArgPayload {
    None,
    Tuple,
    Record,
}

/// Filter the virtual call without constructing its text.
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

/// One call-through-eligible function: its canonical key, the first-param type
/// (rendered), and whether it has more than one param. A scoped request builds
/// display spellings and their lowercase forms at read time.
pub(crate) struct CtFn {
    pub(crate) key: ustr::Ustr,
    pub(crate) mid: ModuleId,
    pub(crate) fid: Idx<bone_semantic::hir::Function>,
    pub(crate) first_param_ty: String,
    pub(crate) multiparam: bool,
}

#[derive(Clone)]
pub(crate) struct ScopedCtFn {
    index: usize,
    name: Ustr,
    name_lc: String,
}

/// One candidate arg value for a call-through synth, with the structural
/// facts recorded at construction (not re-derived from the rendered text):
/// `method` = the path traverses a method call on ANY hop (the per-fn
/// method-chain cap and the confident-autofill exclusion key off it).
#[derive(Clone)]
pub(crate) struct CtArg {
    pub(crate) text: String,
    /// Canonical global or nominal type root, when text starts at one. v1
    /// requests rewrite this root through the fragment's visible spellings.
    pub(crate) root_key: Option<ustr::Ustr>,
    pub(crate) reach_suffix: Option<String>,
    pub(crate) reach_is_method: bool,
    pub(crate) payload: ArgPayload,
    pub(crate) method: bool,
    pub(crate) method_members: Vec<crate::reachability::MethodRef>,
    pub(crate) field_members: Vec<crate::reachability::FieldRef>,
}

impl CtArg {
    pub(crate) fn direct(root_key: Option<Ustr>, text: String, payload: ArgPayload) -> Self {
        CtArg {
            text,
            root_key,
            reach_suffix: None,
            reach_is_method: false,
            payload,
            method: false,
            method_members: Vec::new(),
            field_members: Vec::new(),
        }
    }

    pub(crate) fn reach(
        root_key: Option<Ustr>,
        root: &str,
        p: &crate::reachability::ReachPath,
    ) -> Self {
        let text = if p.is_method {
            format!("{root}.{}()", p.suffix)
        } else {
            format!("{root}.{}", p.suffix)
        };
        CtArg {
            text,
            root_key,
            reach_suffix: Some(p.suffix.clone()),
            reach_is_method: p.is_method,
            payload: ArgPayload::None,
            method: p.via_call,
            method_members: p.method_members.clone(),
            field_members: p.field_members.clone(),
        }
    }
}

/// Cache functions and global-derived arguments by type for call-through.
pub(crate) struct CallThroughIndex {
    pub(crate) fns: Vec<CtFn>,
    pub(crate) global_type_args: HashMap<String, Vec<CtArg>>,
}

/// Build the call-through index from a checked workspace + reach index. This is
/// the work that used to run per keystroke -- the O(types*globals) reach
/// expansion -- now done once per non-`BodyOnly` edit (alongside `reach`).
pub(crate) fn build_call_through(
    ws: &Workspace,
    reach: &crate::reachability::ReachabilityIndex,
) -> CallThroughIndex {
    let mut fns: Vec<CtFn> = Vec::new();
    for m in ws.modules.iter() {
        for (fid, f) in m.functions.iter() {
            if f.name.as_str().starts_with("__") {
                continue;
            }
            let Some(p0) = f.param_decls.first() else {
                continue;
            };
            fns.push(CtFn {
                key: f.name,
                mid: m.id,
                fid,
                first_param_ty: crate::reachability::type_key(ws, p0.ty).to_string(),
                multiparam: f.param_decls.len() > 1,
            });
        }
    }
    // Global roots (position-independent), each with its rendered type.
    let mut groots: Vec<(ustr::Ustr, String, String)> = Vec::new();
    for m in ws.modules.iter() {
        for (gname, gidx) in m.global_by_name.iter() {
            groots.push((
                *gname,
                gname.to_string(),
                crate::reachability::type_key(ws, m.globals[*gidx].ty).to_string(),
            ));
        }
    }
    // Static args per distinct first-param type. Seed each needed type with its
    // global-direct values + enum variants (no reach), then fill reach-from-
    // globals in ONE pass over globals -- a depth-<=2 path LOOKUP per global
    // (`arg_paths_from`), bucketed by target type. No per-(type,global) DFS.
    let needed: std::collections::HashSet<&str> =
        fns.iter().map(|c| c.first_param_ty.as_str()).collect();
    let mut global_type_args: HashMap<String, Vec<CtArg>> = HashMap::new();
    for t in &needed {
        let mut vals: Vec<CtArg> = groots
            .iter()
            .filter(|(_, _, rt)| rt == t)
            .map(|(key, n, _)| CtArg::direct(Some(*key), n.clone(), ArgPayload::None))
            .collect();
        for (vname, shape) in crate::query::enum_variants_with_shape(ws, t) {
            let payload = match shape {
                crate::query::VariantShape::Unit => ArgPayload::None,
                crate::query::VariantShape::Tuple => ArgPayload::Tuple,
                crate::query::VariantShape::Record => ArgPayload::Record,
            };
            vals.push(CtArg::direct(
                Some(Ustr::from(t)),
                format!("{t}::{vname}"),
                payload,
            ));
        }
        global_type_args.insert(t.to_string(), vals);
    }
    for (rkey, rn, rt) in &groots {
        for p in reach.arg_paths_from(rt) {
            if p.is_method && p.takes_args {
                continue; // can't park a snippet hole inside the arg
            }
            let Some(vals) = global_type_args.get_mut(p.final_ty.as_str()) else {
                continue; // not a first-param type any fn needs
            };
            vals.push(CtArg::reach(Some(*rkey), rn, p));
        }
    }
    CallThroughIndex {
        fns,
        global_type_args,
    }
}

/// The fragment view of the workspace call-through index. Namespace spellings
/// and member visibility are scope facts, so build this once per fragment after
/// an interface/scope edit and reuse it for body-only completion requests.
pub(crate) struct ScopedCallThroughIndex {
    pub(crate) fns_by_first_char: HashMap<u8, Vec<ScopedCtFn>>,
    pub(crate) global_type_args: HashMap<String, Vec<CtArg>>,
    pub(crate) callable_spellings: HashMap<Ustr, Vec<Ustr>>,
}

pub(crate) fn build_scoped_call_through(
    ws: &Workspace,
    frag: &bone_core::modules::FragmentScope,
    index: &CallThroughIndex,
) -> ScopedCallThroughIndex {
    let globals =
        crate::modules_scope::spellings_by_key(ws, frag, bone_core::modules::DefKind::Global);
    let types = crate::modules_scope::spellings_by_key(ws, frag, bone_core::modules::DefKind::Type);
    let callable_spellings =
        crate::modules_scope::spellings_by_key(ws, frag, bone_core::modules::DefKind::Callable);
    let global_type_args = index
        .global_type_args
        .iter()
        .map(|(ty, args)| {
            (
                ty.clone(),
                global_args_in_scope(args, &globals, &types, &callable_spellings),
            )
        })
        .collect();
    let mut fns_by_first_char: HashMap<u8, Vec<ScopedCtFn>> = HashMap::new();
    for (index, cf) in index.fns.iter().enumerate() {
        if !crate::modules_scope::callable_member_admitted(ws, frag, cf.key, cf.mid, cf.fid) {
            continue;
        }
        let Some(names) = callable_spellings.get(&cf.key) else {
            continue;
        };
        for &name in names {
            let name_lc = name.as_str().to_lowercase();
            let Some(&first) = name_lc.as_bytes().first() else {
                continue;
            };
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
    ScopedCallThroughIndex {
        fns_by_first_char,
        global_type_args,
        callable_spellings,
    }
}

fn global_args_in_scope(
    args: &[CtArg],
    globals: &HashMap<Ustr, Vec<Ustr>>,
    types: &HashMap<Ustr, Vec<Ustr>>,
    callable_names: &HashMap<Ustr, Vec<Ustr>>,
) -> Vec<CtArg> {
    let mut out = Vec::new();
    for arg in args {
        let Some(key) = arg.root_key else {
            out.push(arg.clone());
            continue;
        };
        let Some(names) = globals.get(&key).or_else(|| types.get(&key)) else {
            continue;
        };
        for name in names {
            let Some(suffix) = arg.text.strip_prefix(key.as_str()) else {
                continue;
            };
            let mut rendered = arg.clone();
            rendered.text = format!("{name}{suffix}");
            if let Some(reach_suffix) = &arg.reach_suffix {
                let Some(reach_suffix) = crate::modules_scope::display_method_suffix_with_names(
                    reach_suffix,
                    &arg.method_members,
                    callable_names,
                ) else {
                    continue;
                };
                rendered.text = if arg.reach_is_method {
                    format!("{name}.{reach_suffix}()")
                } else {
                    format!("{name}.{reach_suffix}")
                };
            }
            out.push(rendered);
        }
    }
    out
}

fn local_args_in_scope(callable_names: &HashMap<Ustr, Vec<Ustr>>, args: Vec<CtArg>) -> Vec<CtArg> {
    args.into_iter()
        .filter_map(|mut arg| {
            let Some(reach_suffix) = &arg.reach_suffix else {
                return Some(arg);
            };
            let reach_suffix = crate::modules_scope::display_method_suffix_with_names(
                reach_suffix,
                &arg.method_members,
                callable_names,
            )?;
            let root = arg.text.split('.').next()?;
            arg.text = if arg.reach_is_method {
                format!("{root}.{reach_suffix}()")
            } else {
                format!("{root}.{reach_suffix}")
            };
            Some(arg)
        })
        .collect()
}

pub(crate) fn call_through_param_completions(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
    prefix: &str,
    index: &CallThroughIndex,
    frag: &bone_core::modules::FragmentScope,
    scoped_index: &ScopedCallThroughIndex,
    reach: &crate::reachability::ReachabilityIndex,
    score_leads: bool,
    mut expected: Option<&mut ExpectedArg>,
    enclosing_fn: Option<&str>,
    sink: &mut Sink,
) -> CtCounts {
    let mut counts = CtCounts::default();
    let q = prefix.as_bytes();
    if q.len() < 2 {
        // 1 char only completes the fn-name's first letter -- no room for an
        // arg yet. At 2+ chars the fn name can absorb the lead and the arg can
        // start to match, so call-through is meaningful.
        return counts;
    }
    let scoped_fns = scoped_index
        .fns_by_first_char
        .get(&q[0])
        .cloned()
        .unwrap_or_default();
    let global_type_args = &scoped_index.global_type_args;
    let callable_spellings = &scoped_index.callable_spellings;
    // In-scope LOCAL roots only -- the position-dependent part. Globals + their
    // reach expansion are precomputed in `index` (the slow part, now cached).
    let owner = ws.module(mid).body_at(offset);
    let body_inference = owner.and_then(|owner| inf.per_fn.get(&owner));
    let locals: Vec<(String, String, String)> = crate::query::locals_in_scope(ws, mid, inf, offset)
        .into_iter()
        .map(|(n, _k, ty, ty_id, _decl)| {
            let node_ty = ty_id.map_or_else(
                || ty.clone(),
                |ty_id| {
                    crate::reachability::type_key_in_body(ws, body_inference, ty_id).to_string()
                },
            );
            (n.to_string(), ty, node_ty)
        })
        .collect();

    // Call-through is a synthesized, bottom-ranked (bucket 4) convenience; the
    // GLOBAL `CALL_THROUGH_CAP` keeps it from burying the direct candidates. Per fn, the
    // per-fn cap applies ONLY to METHOD-chain args (`x.foo().bar()`), which are
    // unbounded reach-noise; direct values + field paths are NOT capped, so a
    // committed query (`tilecenp` -> `tile_center`) can explore all the sibling
    // fields it could pass (e.g. `player.movable.grid` AND `.prev_grid`).
    // Call-through is the ONLY arg-suggestion path while the fn name is still an
    // abbreviation -- arg completion needs a resolved callee to know the param
    // type, which `tilecen(` doesn't give -- so it must be browsable, not capped.

    // Group by expected type and residual query after matching the function name.
    let mut passing: Vec<(usize, usize)> = Vec::new(); // (fn index, k)
    // Fns whose name FULLY covers the query (`k == q.len()`): normally suppressed
    // as fn-name browsing, but eligible for confident unique-arg autofill below.
    let mut committed: Vec<usize> = Vec::new();

    // Add local seeds once per request. The match memo borrows these entries.
    let mut local_args: HashMap<String, Vec<CtArg>> = HashMap::new();
    for (ln, _lt, node_ty) in &locals {
        local_args
            .entry(node_ty.clone())
            .or_default()
            .push(CtArg::direct(None, ln.clone(), ArgPayload::None));
        let paths = reach.arg_paths_in_scope(ws, frag, node_ty);
        for p in paths {
            if p.is_method && p.takes_args {
                continue;
            }
            local_args
                .entry(p.final_ty.clone())
                .or_default()
                .push(CtArg::reach(None, ln, p));
        }
    }
    for args in local_args.values_mut() {
        let owned = std::mem::take(args);
        *args = local_args_in_scope(callable_spellings, owned);
    }

    for (si, scoped) in scoped_fns.iter().enumerate() {
        let cf = &index.fns[scoped.index];
        // Never autofill a call to the fn being defined -- a recursive self-call
        // is not what completing its own body means (a struct fill inside its own function body).
        if enclosing_fn == Some(cf.key.as_str()) {
            continue;
        }
        let k = prefix_subseq_len(q, &scoped.name_lc);
        if !clean_match(scoped.name_lc.as_bytes(), &q[..k.min(q.len())]) {
            continue;
        }
        let local_arg_matches = || {
            local_args
                .get(cf.first_param_ty.as_str())
                .is_some_and(|args| args.iter().any(|a| arg_matches_residual(q, k, &a.text)))
        };
        if k == q.len() {
            committed.push(si);
        } else if k >= q.len().div_ceil(2) {
            counts.fns += 1;
            passing.push((si, k));
        } else if local_arg_matches() {
            // Relaxed noise guard: the fn name absorbs < half the query, but the
            // residual matches a LOCAL arg of its param type -- a promoted
            // local-seeded call-through (`gmob` -> `give(mob)`), not fn-name noise.
            counts.fns += 1;
            passing.push((si, k));
        }
    }

    // Share argument matches and scores across functions with the same residual query.
    let empty: Vec<CtArg> = Vec::new();
    // Per group: (arg, suffix_scores, path_penalty, local). suffix + penalty
    // both depend only on the arg (not the fn), so computed once and shared.
    let mut match_memo: HashMap<(&str, usize), Vec<(&CtArg, Vec<i32>, i32, bool)>> = HashMap::new();
    let mut prefix_scores: Vec<i32> = Vec::new(); // reused across fns

    // Render call text only for candidates that survive the caps.
    let mut cands: Vec<(i32, usize, &CtArg, u8)> = Vec::new();
    for (si, k) in passing {
        let cf = &index.fns[scoped_fns[si].index];
        let t = cf.first_param_ty.as_str();
        let group = match_memo.entry((t, k)).or_insert_with(|| {
            let gargs = global_type_args.get(t).unwrap_or(&empty);
            let largs = local_args.get(t).unwrap_or(&empty);
            // Trailing `bool` = LOCAL-seeded: an arg drawn from an in-scope local
            // (or reachable from one), vs a global. Local-seeded partial matches
            // promote (type_rank 1) so a fn accepting your local surfaces above
            // unrelated fns; globals stay bucketed last.
            let mut v: Vec<(&CtArg, Vec<i32>, i32, bool)> = Vec::new();
            for (a, local) in gargs
                .iter()
                .map(|a| (a, false))
                .chain(largs.iter().map(|a| (a, true)))
            {
                if !arg_admitted(ws, frag, a) {
                    continue;
                }
                counts.arg_iters += 1;
                if arg_matches_residual(q, k, &a.text) {
                    let mut suffix = Vec::new();
                    fuzzy_suffix_scores(a.text.as_bytes(), q, &mut suffix);
                    v.push((a, suffix, arg_path_penalty(&a.text), local));
                }
            }
            v
        });
        counts.survivors += group.len();
        // Prefix scores for THIS fn's name (one forward DP, reused buffer), then
        // combine with each arg's shared suffix vector.
        fuzzy_prefix_scores(scoped_fns[si].name.as_bytes(), q, &mut prefix_scores);
        let mut scored: Vec<(i32, &CtArg, bool)> = Vec::with_capacity(group.len());
        for (a, suffix, penalty, local) in group.iter() {
            // Bias toward simpler args: a direct value / clean field path beats a
            // deep method chain (which the boundary-rich score over-rewards). The
            // penalty is arg-only, precomputed once per group.
            let score = combine_scores(&prefix_scores, suffix) - penalty;
            scored.push((score, a, *local));
        }
        // Cap only METHOD-chain args (`CtArg::method` -- the unbounded
        // `x.foo().bar()` reach-noise); keep ALL direct values / field
        // paths so sibling fields are browsable. The global `CALL_THROUGH_CAP` backstops the
        // total. (Method args still sort against everything in the final cap.)
        let (mut methods, simple): (Vec<_>, Vec<_>) =
            scored.into_iter().partition(|(_, a, _)| a.method);
        if methods.len() > PER_FN_METHOD_CAP {
            // Best by score DESC, then arg lexical CASE-INSENSITIVE (so an
            // UPPER_CASE global doesn't beat a lowercase field path on a tie).
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
    // A fully typed name can prefill one unambiguous local argument.
    for si in committed {
        let cf = &index.fns[scoped_fns[si].index];
        let t = cf.first_param_ty.as_str();
        let confident: Vec<&CtArg> = local_args
            .get(t)
            // No locals of this type means no confident autofill candidate.
            .map(|v| v.iter().filter(|a| !a.method).collect())
            .unwrap_or_default();
        if let [a] = confident.as_slice() {
            let score = fuzzy_score(scoped_fns[si].name.as_bytes(), q) + 5000;
            cands.push((score, si, a, 0));
        }
    }
    // Global top `CALL_THROUGH_CAP` by score (then ci-arg tiebreak); materialize only those.
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
        // Promote a local seed only if the completed call also satisfies the slot.
        let type_rank = match expected.as_deref_mut() {
            Some(exp) if type_rank < 4 => {
                if exp.accepts_fn_returning(ws, cf.key.as_str()) {
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
        // Unpromoted synthesized calls rank below direct candidates.
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
            // Typed slot: kind 2 (a call synth is fn-shaped), so a real
            // local of the slot type still beats a same-rank synth.
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
    // Keep the emitted tail in rank order (the final stable sort preserves
    // insertion order on full ties); the global select above already bounded
    // the count.
    sink.cands[ct_start..].sort_by(cand_cmp);
    counts
}

/// Pool overload references under one name to compute the frequency prior.
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

/// True if any overload of `name` declares a return type rendering to one of
/// `expected`. Used to rank functions returning the wanted type just below
/// variables of that type.
/// Whether any function named `name` in the workspace is a `co fn`. Drives
/// coroutine-fn ranking after a `yield`/`await`.
pub(crate) fn fn_is_coroutine_in_scope(
    ws: &Workspace,
    name: &str,
    frag: &bone_core::modules::FragmentScope,
) -> bool {
    let n = Ustr::from(name);
    crate::modules_scope::admitted_overloads(ws, frag, n).any(|(_, _, f)| f.is_coroutine)
}

fn arg_admitted(ws: &Workspace, frag: &bone_core::modules::FragmentScope, arg: &CtArg) -> bool {
    arg.method_members.iter().all(|(key, mid, fid)| {
        crate::modules_scope::callable_member_admitted(ws, frag, *key, *mid, *fid)
    }) && crate::reachability::fields_admitted(ws, frag, &arg.field_members)
}

/// Whether a nominal type (struct OR enum) named `name` is defined anywhere in
/// the workspace. The host's `Wait` yield type is an enum, so the coroutine
/// ergonomics probe both.
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
        .any(|m| m.struct_by_name.contains_key(&key) || m.enums_by_name.contains_key(&key))
}

/// The struct named by a fn-name's first `_`-segment, capitalized (`mob_attack_co`
/// -> `Mob`), if such a struct exists -- the payload `T` for a synthesized
/// `Entity<T>`. None when the segment doesn't name a struct.
pub(crate) fn fn_prefix_struct(
    ws: &Workspace,
    frag: &bone_core::modules::FragmentScope,
    fn_name: &str,
) -> Option<Ustr> {
    // Try each `_`-segment leading-first: `<type>_<verb>` (`mob_attack_co`) and
    // `<verb>_<type>` (`spawn_mob`) both name `Mob`. Leading match wins when
    // more than one segment is a struct (the UFCS convention).
    fn_name
        .split('_')
        .filter(|s| !s.is_empty())
        .find_map(|seg| {
            let mut chars = seg.chars();
            let cap: String = chars.next()?.to_ascii_uppercase().to_string() + chars.as_str();
            let key = crate::modules_scope::resolve_type_key(ws, frag, &cap)?;
            ws.modules
                .iter()
                .any(|m| m.struct_by_name.contains_key(&key))
                .then(|| Ustr::from(&cap))
        })
}

/// Completion candidates in decorator position: every `deco` declaration
/// visible to the fragment, prefix-filtered.
pub(crate) fn decorator_completions(
    ws: &Workspace,
    frag: &bone_core::modules::FragmentScope,
    fragment_file: Ustr,
    prefix: &str,
) -> Vec<CompletionItem> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let names =
        crate::modules_scope::spellings_by_key(ws, frag, bone_core::modules::DefKind::Decorator);
    for m in ws.modules.iter() {
        for (_, dec) in m.decorators.iter() {
            for &name in names.get(&dec.def_key).into_iter().flatten() {
                let name = name.as_str();
                if !admit(name.as_bytes(), prefix.as_bytes()) || !seen.insert(name.to_string()) {
                    continue;
                }
                let params = crate::query::rendered_params(ws, &dec.param_decls);
                let (ex, bd) = match_band_exact(name, prefix.as_bytes(), prefix.as_bytes());
                let key = RankKey {
                    exact: ex,
                    band: bd,
                    len: RankKey::len_key(9999, name.len()),
                    ..Default::default()
                };
                let cand = Cand::new(
                    key,
                    CandName::Sym(Ustr::from(name)),
                    CandShape::Decorator {
                        detail: (!params.is_empty())
                            .then(|| format!("deco ({})", params.join(", "))),
                    },
                );
                out.push(materialize_cand(cand, ws, fragment_file, false, false));
            }
        }
    }
    out.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    out
}

/// Return an empty list for a resolved record whose fields all fail the filter.
pub(crate) fn enum_record_field_completions(
    session: &mut Session,
    path: &str,
    src: &str,
    offset: usize,
    enum_name: &str,
    variant: &str,
    prefix: &str,
) -> Option<Vec<CompletionItem>> {
    for munged in completion_munges(src, offset) {
        let Some(fields) = analyze_variant(session, path, &munged, |ws| {
            let Some(mid) = user_module_for_path(ws, path) else {
                return Vec::new();
            };
            let Some(enum_key) =
                crate::modules_scope::resolve_type_key_in_file(ws, ws.module(mid).file, enum_name)
            else {
                return Vec::new();
            };
            crate::query::variant_record_fields(ws, enum_key.as_str(), variant)
        }) else {
            continue;
        };
        if fields.is_empty() {
            continue;
        }
        let mut items: Vec<CompletionItem> = fields
            .into_iter()
            .enumerate()
            .filter(|(_, (name, _))| prefix.is_empty() || admit(name.as_bytes(), prefix.as_bytes()))
            .map(|(i, (name, ty))| CompletionItem {
                label: format!("{name}: {ty}"),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(ty),
                sort_text: Some(closed_set_sort_text(&name, prefix.as_bytes(), 0, i)),
                insert_text: Some(name),
                ..Default::default()
            })
            .collect();
        items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
        return Some(items);
    }

    None
}

/// Field completions for `Name { <field> }`. Offers the struct's fields not
/// already written in the literal, filtered by the prefix. Returns None (so the
/// caller falls through to the global namespace) when `name` isn't a struct.
pub(crate) fn struct_literal_field_completions(
    session: &mut Session,
    path: &str,
    src: &str,
    offset: usize,
    name: &str,
    brace_open: usize,
    prefix: &str,
) -> Option<Vec<CompletionItem>> {
    // Analyze a balanced/synthetic munge as a Session variant so the struct def
    // survives the broken literal -- and so a struct defined in another file /
    // dependency resolves.
    let fields = completion_munges(src, offset)
        .into_iter()
        .find_map(|munged| {
            let f = analyze_variant(session, path, &munged, |ws| {
                let Some(mid) = user_module_for_path(ws, path) else {
                    return Vec::new();
                };
                let Some(name) =
                    crate::modules_scope::resolve_type_key_in_file(ws, ws.module(mid).file, name)
                else {
                    return Vec::new();
                };
                crate::query::struct_fields_by_name(ws, name.as_str())
            })?;
            (!f.is_empty()).then_some(f)
        })?;

    // Fields already written between the `{` and the cursor (`name :`), to skip.
    let written = src.get(brace_open + 1..offset.min(src.len())).unwrap_or("");
    let present: std::collections::HashSet<String> = written
        .split([',', '{', '}'])
        .filter_map(|seg| seg.split(':').next())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();

    let mut items: Vec<CompletionItem> = fields
        .into_iter()
        .enumerate()
        .filter(|(_, (n, _, _))| !present.contains(n))
        .filter(|(_, (n, _, _))| prefix.is_empty() || admit(n.as_bytes(), prefix.as_bytes()))
        .map(|(i, (n, ty, has_default))| CompletionItem {
            label: format!("{n}: {ty}"),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(ty),
            // Required fields (no default) tier above defaulted ones -- the
            // literal is incomplete until they're supplied; declaration
            // order within each tier.
            sort_text: Some(closed_set_sort_text(
                &n,
                prefix.as_bytes(),
                u8::from(has_default),
                i,
            )),
            insert_text: Some(n),
            ..Default::default()
        })
        .collect();
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    Some(items)
}

/// Resolve the qualifier and insert the matching variant payload template.
pub(crate) fn enum_qualified_completions(
    session: &mut Session,
    path: &str,
    src: &str,
    offset: usize,
    enum_name: &str,
    prefix: &str,
) -> Vec<CompletionItem> {
    // The query only needs the enum DEF resolvable, so the standard sentinel
    // munges (body-local -- a cheap variant tier) serve it; a truncate-at-`::`
    // munge would delete every item after the cursor and turn the variant into
    // an interface-tier edit.
    let mut all = Vec::new();
    for munged in completion_munges(src, offset) {
        let attempt = analyze_variant(session, path, &munged, |ws| {
            let Some(mid) = user_module_for_path(ws, path) else {
                return Vec::new();
            };
            let Some(enum_key) =
                crate::modules_scope::resolve_type_key_in_file(ws, ws.module(mid).file, enum_name)
            else {
                return Vec::new();
            };
            crate::query::enum_variants_with_shape(ws, enum_key.as_str())
        });
        if let Some(v) = attempt {
            if !v.is_empty() {
                all = v;
                break;
            }
        }
    }

    let pattern = in_pattern_position(src, offset);
    let matches_prefix = |v: &str| prefix.is_empty() || admit(v.as_bytes(), prefix.as_bytes());
    // Recover a mistyped qualifier suffix by offering every variant.
    let none_match = !prefix.is_empty() && !all.iter().any(|(v, _)| matches_prefix(v));
    let force_filter = none_match.then(|| prefix.to_string());

    let mut items: Vec<CompletionItem> = all
        .into_iter()
        .enumerate()
        .filter(|(_, (v, _))| none_match || matches_prefix(v))
        .map(|(i, (v, shape))| {
            let (insert_text, insert_text_format) =
                super::select::variant_insert(&v, shape, pattern, false);
            CompletionItem {
                sort_text: Some(closed_set_sort_text(&v, prefix.as_bytes(), 0, i)),
                label: v,
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                filter_text: force_filter.clone(),
                insert_text,
                insert_text_format,
                ..Default::default()
            }
        })
        .collect();
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    items
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
        let hit = analyze_variant(session, path, &munged, |ws| {
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

/// Whether an open paren already follows the cursor (skipping inline
/// whitespace) -- `recv.le|()`. A method item must then insert the bare name,
/// not `name()`/`name($0)`, or the accepted completion doubles the parens.
/// Read from the ORIGINAL source, before any completion munge.
pub(crate) fn open_paren_follows(src: &str, offset: usize) -> bool {
    src.as_bytes()[offset..]
        .iter()
        .find(|&&b| b != b' ' && b != b'\t')
        == Some(&b'(')
}

/// Whether a member of type `ty_id` (rendered `ty_render`) type-directs into
/// the expected arg slot. Bone's `coerce` on the TyId when present (so an `int`
/// field boosts in an `int?`/`float`/`any` slot); rendered-string equality when
/// the member has no stored TyId (a generic-substituted field) or there's no
/// session. False with no session at all.
pub(crate) fn member_ty_match(
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

/// Try speculative reparses. A resolved receiver with no matching members
/// still suppresses general completion.
pub(crate) fn member_completions(
    session: &mut Session,
    path: &str,
    src: &str,
    offset: usize,
    prefix: &str,
    arg_slot: Option<(&str, usize)>,
    anchor: Option<SlotAnchor>,
    reach: &crate::reachability::ReachabilityIndex,
    bool_ctx: bool,
    members: impl Fn(&Workspace, ModuleId) -> Option<crate::query::Members<'_>>,
) -> Option<Vec<CompletionItem>> {
    let paren_follows = open_paren_follows(src, offset);
    let mut resolved_empty = false;
    for munged in completion_munges(src, offset) {
        let attempt = analyze_variant(session, path, &munged, |ws| {
            // The CURRENT file's module (where the receiver/offset live).
            let mid = user_module_for_path(ws, path)?;
            Some(build_member_items(
                ws,
                mid,
                offset,
                members(ws, mid)?,
                prefix,
                arg_slot,
                anchor,
                reach,
                bool_ctx,
                paren_follows,
            ))
        });
        // `Some(Some(items))`: this variant resolved the receiver. Prefer the
        // first non-empty result; remember a resolved-but-empty one so we still
        // suppress general rather than leak globals.
        if let Some(Some(items)) = attempt {
            if !items.is_empty() {
                return Some(items);
            }
            resolved_empty = true;
        }
    }
    resolved_empty.then(Vec::new)
}

/// Find a compatible field-reference argument after the receiver.
fn field_ref_slot(
    ws: &Workspace,
    recv_struct: Ustr,
    recv_elem: Option<Ustr>,
    f: &bone_semantic::hir::Function,
) -> Option<(usize, Vec<Ustr>)> {
    // A UFCS receiver always binds the owner; a sibling arg never contributes
    // here (that path serves the non-UFCS prefix call). The FieldRef's owner must
    // be the receiver's own struct -- a literal `FieldRef<Other, V>` on a `recv`
    // method isn't a `recv.method(.field)` slot, so gate nominal owners on that.
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

/// Snippet `insert_text` for a field-ref method: the FieldRef arg pre-filled with
/// the field (a `${1|.a,.b|}` choice when >1 field), every other arg a numbered
/// placeholder named after its param. `mob.kill_co` -> `kill_co(.attack_co)`;
/// `mob.set_co` -> `set_co(.attack_co, ${1:next})`.
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

/// Find compatible methods even when a generic first parameter would
/// keep them out of the nominal method list.
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
            // A bare-generic receiver (`o: T`), or a wrapper `Ctor<T>` whose ctor
            // is the receiver struct (`e: Entity<T>` on an `Entity<Glyph>`
            // receiver) -- the two shapes `field_ref_slot` resolves an owner for.
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

/// Shared item-builder for both receiver-resolution strategies (receiver
/// expression and name path): UFCS methods, type-directed
/// field/method ordering, Entity `get` boost, arg-position transitive reach.
fn build_member_items(
    ws: &Workspace,
    mid: ModuleId,
    offset: usize,
    members: crate::query::Members<'_>,
    prefix: &str,
    arg_slot: Option<(&str, usize)>,
    anchor: Option<SlotAnchor>,
    reach: &crate::reachability::ReachabilityIndex,
    bool_ctx: bool,
    // A `(` already follows the cursor -- method items insert the bare name so
    // accepting doesn't double the parens (`recv.le|()` -> `recv.len()`).
    paren_follows: bool,
) -> Vec<CompletionItem> {
    let crate::query::Members {
        fields,
        owner,
        // A Vec receiver's element filters UFCS methods to matching elements.
        vec_elem,
        // The FieldRef owner `T` binds through it on wrapper-receiver
        // overloads (`e.run_into(.co)`).
        sole_arg: recv_elem,
        is_iter: recv_is_iter,
    } = members;
    let recv_struct = Some(owner);
    {
        let Some(frag) = crate::modules_scope::fragment(ws, ws.module(mid).file) else {
            return Vec::new();
        };
        // UFCS methods: functions whose first param is the receiver's struct,
        // callable as `recv.method(...)`. Surfaced alongside fields, as rock does.
        // Non-struct receivers have no nominal UFCS methods.
        let methods = recv_struct
            .map(|name| crate::query::ufcs_methods_in_scope(ws, name, vec_elem, recv_is_iter, frag))
            .unwrap_or_default();
        let type_spellings =
            crate::modules_scope::spellings_by_key(ws, frag, bone_core::modules::DefKind::Type);
        let callable_spellings =
            crate::modules_scope::spellings_by_key(ws, frag, bone_core::modules::DefKind::Callable);
        if fields.is_empty() && methods.is_empty() {
            return Vec::new();
        }
        // Use the variant workspace so inferred types and the oracle share an arena.
        let mut expected = arg_slot.and_then(|(callee, idx)| {
            ExpectedArg::for_call(ws, mid, ws.inference(mid)?, offset, callee, idx, None)
        });
        if expected.is_none() {
            // Slot-anchored contexts (assignment/comparison RHS, typed-`let`,
            // `return`): the anchor ends before the cursor, so it resolves
            // against THIS variant workspace directly.
            expected = anchor.and_then(|a| {
                let inf = ws.inference(mid)?;
                expected_for_anchor(ws, mid, inf, offset, a)
            });
        }
        let arg_ctx = expected.is_some();
        // Precompute the type-match decision per member (bone's `coerce` on the
        // member's TyId; rendered-string equality only where a TyId is absent --
        // e.g. a generic-substituted field). Sequential because `accepts` takes
        // `&mut` (it memoizes / rolls back trial bindings).
        let field_match: Vec<bool> = fields
            .iter()
            .map(|(_, ty, ty_id)| member_ty_match(expected.as_mut(), *ty_id, ty))
            .collect();
        let method_match: Vec<bool> = methods
            .iter()
            .map(|m| {
                member_ty_match(
                    expected.as_mut(),
                    m.return_ty,
                    m.returns.as_deref().unwrap_or(""),
                )
            })
            .collect();
        // Direct members are 1-hop (dist 1): they carry a sort_text too, so they
        // order consistently with -- and ahead of, by distance -- the transitive
        // reach paths added below (fuzzy score is the dominant term either way).
        let pfx = prefix.as_bytes();
        // Prefer bool fields in conditions and display nullable bool as an explicit comparison.
        let show_private = prefix.starts_with('_');
        // A small score bonus puts fields above equally matched methods.
        let field_boost = if prefix.is_empty() { 0 } else { 25 };
        // The type-relevance rung: a `bool` member in a bool context
        // (`if`/`while <chain>`), a member of the expected param type in a
        // call-arg slot.
        let type_rank = |is_bool: bool, type_match: bool| {
            u8::from(if bool_ctx {
                !is_bool
            } else {
                arg_ctx && !type_match
            })
        };
        let field_items =
            fields
                .into_iter()
                .zip(field_match)
                .filter_map(|((name, ty, _), tmatch)| {
                    if name.starts_with('_') && !show_private {
                        return None;
                    }
                    (prefix.is_empty() || admit(name.as_bytes(), prefix.as_bytes())).then(|| {
                        CompletionItem {
                            sort_text: Some(arg_sort_text(
                                type_rank(ty == "bool", tmatch),
                                0,
                                &name,
                                pfx,
                                1,
                                0,
                                field_boost,
                            )),
                            label: format!("{name}: {ty}"),
                            kind: Some(CompletionItemKind::FIELD),
                            detail: Some(ty),
                            insert_text: Some(name),
                            ..Default::default()
                        }
                    })
                });
        let field_ref_matches = recv_struct
            .map(|rs| field_ref_methods(ws, rs, recv_elem, frag))
            .unwrap_or_default();
        let mut field_ref_names = std::collections::HashSet::new();
        for (f, _, _) in &field_ref_matches {
            if let Some(names) = callable_spellings.get(&f.name) {
                field_ref_names.extend(names.iter().copied());
            }
        }
        let method_items = methods
            .into_iter()
            .zip(method_match)
            .filter_map(|(m, rmatch)| {
                if field_ref_names.contains(&Ustr::from(m.name.as_str())) {
                    return None;
                }
                (prefix.is_empty() || admit(m.name.as_bytes(), prefix.as_bytes())).then(|| {
                    // Omit the receiver argument and reuse any existing call parentheses.
                    let (insert_text, insert_text_format) = if paren_follows {
                        (Some(m.name.clone()), None)
                    } else if m.takes_args {
                        (
                            Some(format!("{}($0)", m.name)),
                            Some(InsertTextFormat::SNIPPET),
                        )
                    } else {
                        (Some(format!("{}()", m.name)), None)
                    };
                    CompletionItem {
                        // Give fields and methods the same rank rung in a member slot.
                        sort_text: Some(arg_sort_text(
                            type_rank(false, rmatch),
                            0,
                            &m.name,
                            pfx,
                            1,
                            0,
                            0,
                        )),
                        label: m.name.clone(),
                        label_details: fn_or_type_label_details(
                            SymCat::Function,
                            Some(&m.signature),
                        ),
                        kind: Some(CompletionItemKind::METHOD),
                        detail: Some(m.signature),
                        insert_text,
                        insert_text_format,
                        ..Default::default()
                    }
                })
            });
        let mut items: Vec<CompletionItem> = field_items.chain(method_items).collect();
        // Replace the ordinary method candidate with its prefilled FieldRef form.
        {
            for (fdef, arg_idx, fields) in field_ref_matches {
                let key = fdef.name;
                let name = bone_core::modules::display_name(key.as_str());
                if !(prefix.is_empty() || admit(name.as_bytes(), pfx)) {
                    continue;
                }
                let (insert_text, insert_text_format) = if paren_follows {
                    (Some(name.to_string()), None)
                } else {
                    (
                        Some(field_ref_insert(name, fdef, arg_idx, &fields)),
                        Some(InsertTextFormat::SNIPPET),
                    )
                };
                let sort_text = arg_sort_text(type_rank(false, false), 0, name, pfx, 1, 0, 0);
                let signature = crate::query::render_fn_signature(ws, key, fdef);
                items.push(CompletionItem {
                    sort_text: Some(sort_text),
                    label: name.to_string(),
                    label_details: fn_or_type_label_details(SymCat::Function, Some(&signature)),
                    kind: Some(CompletionItemKind::METHOD),
                    detail: Some(signature),
                    insert_text,
                    insert_text_format,
                    ..Default::default()
                });
            }
        }
        // Entity receiver: `.get()` (-> T?) is the dominant accessor, almost
        // always followed by `.unwrap()`. Rank `get` first, and offer the whole
        // `get().unwrap()` chain (matches a `getu`/`getunwrap` prefix, which the
        // bare `get` method can't since it filters on `starts_with`).
        if recv_struct.is_some_and(|s| is_intrinsic_key(s, "Entity")) {
            for it in items.iter_mut() {
                if it.label == "get" {
                    it.sort_text = Some(
                        RankKey {
                            len: RankKey::len_key(9999, "get".len()),
                            ..Default::default()
                        }
                        .encode("get"),
                    );
                }
            }
            if clean_match(b"get().unwrap()", prefix.as_bytes()) {
                items.push(CompletionItem {
                    label: "get().unwrap()".to_string(),
                    kind: Some(CompletionItemKind::METHOD),
                    detail: Some("unwrap the Entity's value (T)".to_string()),
                    insert_text: Some("get().unwrap()".to_string()),
                    sort_text: Some(
                        RankKey {
                            fine: RankKey::fine(9998),
                            len: RankKey::len_key(9998, "get().unwrap()".len()),
                            ..Default::default()
                        }
                        .encode("get().unwrap()"),
                    ),
                    ..Default::default()
                });
            }
        }
        // The live index uses type names, so it also serves the speculative workspace.
        let reach_targets: &[String] = expected.as_ref().map_or(&[], |e| &e.rendered);
        if !reach_targets.is_empty() {
            if let Some(rs) = recv_struct {
                let res = reach.paths_to(rs.as_str(), reach_targets, REACH_CAP, Some((ws, frag)));
                let pfx = prefix.as_bytes();
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                for p in res.paths {
                    if p.dist <= 1 {
                        continue; // 1-hop field/method, already offered above
                    }
                    let suffix = crate::modules_scope::display_method_suffix_with_names(
                        &p.suffix,
                        &p.method_members,
                        &callable_spellings,
                    );
                    let Some(suffix) = suffix else {
                        continue;
                    };
                    if !pfx.is_empty() && !clean_match(suffix.as_bytes(), pfx) {
                        continue;
                    }
                    if !seen.insert(suffix.clone()) {
                        continue;
                    }
                    let final_ty = crate::modules_scope::display_type_with_names(
                        ws,
                        &p.final_ty,
                        &type_spellings,
                    );
                    let (label, kind, insert_text, insert_text_format) = if p.is_method {
                        let call = format!("{}()", suffix);
                        let (insert, fmt) = if paren_follows {
                            (suffix.clone(), None)
                        } else if p.takes_args {
                            (format!("{}($0)", suffix), Some(InsertTextFormat::SNIPPET))
                        } else {
                            (call.clone(), None)
                        };
                        (
                            format!("{call} -> {final_ty}"),
                            CompletionItemKind::METHOD,
                            insert,
                            fmt,
                        )
                    } else {
                        (
                            format!("{suffix}: {final_ty}"),
                            CompletionItemKind::FIELD,
                            suffix.clone(),
                            None,
                        )
                    };
                    // Reach paths are type-matches by construction (paths TO the
                    // expected type) -- prio 0, same format as the direct items.
                    let sort_text =
                        arg_sort_text(0, 0, &suffix, pfx, p.dist, u8::from(p.is_method), 0);
                    items.push(CompletionItem {
                        label,
                        kind: Some(kind),
                        detail: Some(final_ty),
                        insert_text: Some(insert_text),
                        insert_text_format,
                        sort_text: Some(sort_text),
                        ..Default::default()
                    });
                }
            }
        }
        // Response arrays go out in rank order (array == sortText, the
        // invariant every path upholds).
        items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
        items
    }
}
