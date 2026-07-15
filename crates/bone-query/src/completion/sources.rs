//! Completion sources (completion-v2 stage B): the per-context candidate
//! emitters -- member/struct-literal/enum paths, call-through, decorators,
//! and their workspace-query helpers. Functions here take a `Session` only
//! to run speculative variants through `backend::analyze_variant`; the
//! variant plumbing itself stays in `backend.rs`.

use std::collections::HashMap;

use lsp_types::{CompletionItem, CompletionItemKind, InsertTextFormat};

use bone_incremental::session::Session;
use bone_semantic::prelude::{InferenceResult, ModuleId, TyId, Workspace};
use ustr::Ustr;

use crate::backend::{
    analyze_variant, bind_generic, expected_for_anchor, nominal_base,
    preceding_arg_starts, ty_of, user_module_for_path, ExpectedArg,
};
use crate::completion::match_policy::admit;
use crate::query::{clean_match, SymCat};
use crate::rank_key::RankKey;

use super::classify::{balance_delimiters, completion_munges, in_pattern_position, SlotAnchor};
use super::rank::*;
use super::select::*;

/// Declared params of every fn named `callee`, unioned across overloads in
/// first-seen order (param name + rendered declared type). Drives named-arg
/// completion at a call site.
pub(crate) fn callee_param_names(ws: &Workspace, callee: &str) -> Vec<(Ustr, String)> {
    let name = Ustr::from(callee);
    let mut out: Vec<(Ustr, String)> = Vec::new();
    for m in ws.modules.iter() {
        let Some(fns) = m.functions_by_name.get(&name) else { continue };
        for (fmid, fid) in fns {
            let f = &ws.modules[fmid.index()].functions[*fid];
            for p in &f.param_decls {
                if out.iter().any(|(n, _)| *n == p.name) {
                    continue;
                }
                out.push((p.name, ws.render(None, p.ty)));
            }
        }
    }
    out
}

/// Field-ref completions for a call-arg slot. `None` if the slot's expected
/// param isn't `FieldOf<Struct, V>` (caller falls through to normal
/// completion); `Some(items)` when it is -- offering the struct's fields whose
/// declared type is exactly V (the typechecker's invariance), whether or not
/// the leading `.` is typed. A generic `T` is resolved from a preceding sibling
/// arg's inferred type.
pub(crate) fn field_ref_arg_completions(
    ws: &Workspace,
    path: &str,
    src: &str,
    offset: usize,
    callee: &str,
    arg_idx: usize,
    prefix: &str,
    lead_dot: bool,
) -> Option<Vec<CompletionItem>> {
    use bone_semantic::hir::{TypeCtor, TypeExpr};
    let name = Ustr::from(callee);
    let (fmid, fid) = ws
        .modules
        .iter()
        .find_map(|m| m.functions_by_name.get(&name).and_then(|l| l.first()).copied())?;
    let f = &ws.modules[fmid.index()].functions[fid];
    let param = f.param_decls.get(arg_idx)?.ty;

    // The slot must be `FieldOf<T, V>`.
    let (t_idx, v_idx) = match ty_of(ws, None, param) {
        TypeExpr::App { ctor: TypeCtor::Nominal(n), params }
            if n.as_str() == "FieldOf" && params.len() == 2 =>
        {
            let mut it = params.into_iter();
            (it.next().unwrap(), it.next().unwrap())
        }
        _ => return None,
    };
    let v_str = ws.render(None, param.with_idx(v_idx));

    // Resolve T -> a concrete struct name (directly, or via a sibling arg).
    let t_name = match ty_of(ws, None, param.with_idx(t_idx)) {
        TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => n,
        TypeExpr::Generic(_, gname) => {
            let mid = user_module_for_path(ws, path)?;
            let inf = ws.inference(mid)?;
            let starts = preceding_arg_starts(src, offset);
            let mut resolved = None;
            for (i, &aoff) in starts.iter().enumerate().take(arg_idx) {
                let Some(pty) = f.param_decls.get(i).map(|p| p.ty) else {
                    continue;
                };
                let Some(at) = bone_semantic::query::type_at(ws, mid, inf, aoff) else {
                    continue;
                };
                if let Some(bound) = bind_generic(ws, gname, None, pty, at.body, at.ty) {
                    if let Some(sn) = nominal_base(ws, at.body, bound) {
                        resolved = Some(sn);
                        break;
                    }
                }
            }
            resolved?
        }
        _ => return None,
    };

    let pfx = prefix.as_bytes();
    let mut seen: std::collections::HashSet<Ustr> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in ws.modules.iter() {
        let Some(&idx) = m.struct_by_name.get(&t_name) else {
            continue;
        };
        for fld in &m.structs[idx].fields {
            let fty = ws.render(None, fld.ty);
            if fty != v_str {
                continue;
            }
            if !(prefix.is_empty() || admit(fld.name.as_bytes(), pfx)) {
                continue;
            }
            if !seen.insert(fld.name) {
                continue;
            }
            // Dot already typed -> insert the bare name; otherwise add it.
            let insert = if lead_dot {
                fld.name.to_string()
            } else {
                format!(".{}", fld.name)
            };
            out.push(CompletionItem {
                sort_text: Some(arg_sort_text(0, 0, fld.name.as_str(), pfx, 1, 0, 0)),
                // The label carries the leading `.` -- a field ref IS `.co` (no
                // receiver), unlike member completion where the `recv.` is
                // already typed. `filter_text` stays the bare name so typing
                // `co` (with or without the dot) still matches.
                label: format!(".{}: {}", fld.name, fty),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(fty),
                insert_text: Some(insert),
                filter_text: Some(fld.name.to_string()),
                ..Default::default()
            });
        }
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

/// Complete a whole call through its first parameter: typing `rejtanog` offers
/// `reject_target(TargetError::NotGround)` when the fn name absorbs the start of
/// the query and an arg value for its first param matches the rest. The
/// candidate is `fn(arg)` -- the reachability idea one level up. v1 fills the
/// arg from in-scope values of the param type and the type's enum UNIT variants
/// (the motivating case); reach-path args and tuple/record payloads are TODO
/// (`docs/completion-reachability-plan.md`).
/// Whether a call-through arg value needs a payload snippet: a tuple variant
/// (`Damage::Fire($..)`), a record variant (`Cfg { $.. }`), or none (a direct
/// value, unit variant, or reach path).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ArgPayload {
    None,
    Tuple,
    Record,
}

/// Does the virtual call `name(arg)` subsequence-contain the query `q` (already
/// lowercased), GIVEN that the fn name already absorbs `q[..k]` (where `k =
/// prefix_subseq_len(q, name_lc)` -- the same value the noise guard computes)?
/// The residual `q[k..]` must subsequence-match `arg`, with the `(`/`)`
/// punctuation matched exactly as a scan of the whole `name(arg)` would.
///
/// Because the name-scan always lands at `qi == k`, this is BEHAVIOR-IDENTICAL to
/// scanning the full virtual `name(arg)` per fn -- but it depends only on
/// `(q, k, arg)`, NOT the fn name. That lets the call-through path memoize the
/// matching args per `(param_type, k)` and share the scan across every fn in the
/// group instead of re-testing each (fn, arg) pair (lever (b)). Equivalent to
/// `is_subsequence(q, format!("{name}({arg})").to_lowercase())` without building
/// the string -- the fzf "match, don't build" idea.
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

/// One call-through-eligible function: name (+ precomputed lowercase so the
/// per-keystroke noise guard doesn't re-lowercase ~1700 names every keystroke),
/// its first-param type (rendered), and whether it has more than one param.
pub(crate) struct CtFn {
    pub(crate) name: ustr::Ustr,
    pub(crate) name_lc: String,
    pub(crate) first_param_ty: String,
    pub(crate) multiparam: bool,
}

/// One candidate arg value for a call-through synth, with the structural
/// facts recorded at construction (not re-derived from the rendered text):
/// `method` = the path traverses a method call on ANY hop (the per-fn
/// method-chain cap and the confident-autofill exclusion key off it).
#[derive(Clone)]
pub(crate) struct CtArg {
    pub(crate) text: String,
    pub(crate) payload: ArgPayload,
    pub(crate) method: bool,
}

impl CtArg {
    pub(crate) fn direct(text: String, payload: ArgPayload) -> Self {
        CtArg { text, payload, method: false }
    }

    pub(crate) fn reach(root: &str, p: &crate::reachability::ReachPath) -> Self {
        let text = if p.is_method {
            format!("{root}.{}()", p.suffix)
        } else {
            format!("{root}.{}", p.suffix)
        };
        CtArg { text, payload: ArgPayload::None, method: p.via_call }
    }
}

/// Precomputed call-through index: the function table + the GLOBAL-derived arg
/// values per param type (direct globals of the type, the type's enum variants,
/// and reach paths from globals -- `g.movable.grid`). All a pure function of the
/// type graph, so it is cached in `BoneProject::call_through` and rebuilt only on
/// non-`BodyOnly` edits. The per-keystroke path adds only the (small) in-scope
/// LOCAL args on top. Before this, `call_through_param_completions` rebuilt the
/// whole O(types*globals) reach expansion on EVERY keystroke (20-50ms stall).
pub(crate) struct CallThroughIndex {
    pub(crate) fns: Vec<CtFn>,
    /// `fns` indices bucketed by lowercase first byte of the fn name. The noise
    /// guard requires `name_lc[0] == q[0]`, so the per-keystroke loop walks only
    /// the `q[0]` bucket instead of every fn (and runs `prefix_subseq_len` only
    /// for plausible candidates, not all ~1700). Built with the index (a pure
    /// fn of the type graph).
    pub(crate) fns_by_first_char: HashMap<u8, Vec<u32>>,
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
        for f in m.functions.values() {
            if f.name.as_str().starts_with("__") {
                continue;
            }
            let Some(p0) = f.param_decls.first() else { continue };
            fns.push(CtFn {
                name: f.name,
                name_lc: f.name.as_str().to_lowercase(),
                first_param_ty: ws.render(None, p0.ty),
                multiparam: f.param_decls.len() > 1,
            });
        }
    }
    // Global roots (position-independent), each with its rendered type.
    let mut groots: Vec<(String, String)> = Vec::new();
    for m in ws.modules.iter() {
        for (gname, gidx) in m.global_by_name.iter() {
            groots.push((gname.to_string(), ws.render(None, m.globals[*gidx].ty)));
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
            .filter(|(_, rt)| rt == t)
            .map(|(n, _)| CtArg::direct(n.clone(), ArgPayload::None))
            .collect();
        for (vname, shape) in crate::query::enum_variants_with_shape(ws, t) {
            let payload = match shape {
                crate::query::VariantShape::Unit => ArgPayload::None,
                crate::query::VariantShape::Tuple => ArgPayload::Tuple,
                crate::query::VariantShape::Record => ArgPayload::Record,
            };
            vals.push(CtArg::direct(format!("{t}::{vname}"), payload));
        }
        global_type_args.insert(t.to_string(), vals);
    }
    for (rn, rt) in &groots {
        for p in reach.arg_paths_from(rt) {
            if p.is_method && p.takes_args {
                continue; // can't park a snippet hole inside the arg
            }
            let Some(vals) = global_type_args.get_mut(p.final_ty.as_str()) else {
                continue; // not a first-param type any fn needs
            };
            vals.push(CtArg::reach(rn, p));
        }
    }
    // Bucket fn indices by lowercase first byte for the per-keystroke first-char
    // lookup (the noise guard's `name_lc[0] == q[0]` condition).
    let mut fns_by_first_char: HashMap<u8, Vec<u32>> = HashMap::new();
    for (i, cf) in fns.iter().enumerate() {
        if let Some(&b) = cf.name_lc.as_bytes().first() {
            fns_by_first_char.entry(b).or_default().push(i as u32);
        }
    }
    CallThroughIndex { fns, fns_by_first_char, global_type_args }
}

pub(crate) fn call_through_param_completions(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
    prefix: &str,
    index: &CallThroughIndex,
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
    // In-scope LOCAL roots only -- the position-dependent part. Globals + their
    // reach expansion are precomputed in `index` (the slow part, now cached).
    let locals: Vec<(String, String)> = crate::query::locals_in_scope(ws, mid, inf, offset)
        .into_iter()
        .map(|(n, _k, ty, _ty_id, _decl)| (n.to_string(), ty))
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


    // Pass 1: walk only the first-char bucket (lever (a)) and keep the fns that
    // clear the noise guard, recording `k` (how many leading query chars the
    // name absorbs) and the set of distinct first-param types seen. NOISE GUARD:
    // the name must begin at the query's first char (guaranteed by the bucket),
    // absorb >= half of the query but NOT all of it, and land its chars cleanly
    // (word boundaries / a run). `k` matters because the arg-side match tests the
    // RESIDUAL `q[k..]`, which depends only on `(param_type, k)` -- NOT the name.
    let bucket: &[u32] = index.fns_by_first_char.get(&q[0]).map_or(&[], |v| v.as_slice());
    let mut passing: Vec<(usize, usize)> = Vec::new(); // (fn index, k)
    // Fns whose name FULLY covers the query (`k == q.len()`): normally suppressed
    // as fn-name browsing, but eligible for confident unique-arg autofill below.
    let mut committed: Vec<usize> = Vec::new();

    // Local-derived args, keyed by the type each PRODUCES (a local's own type, or
    // a reach path's final type), built BEFORE pass 1 so the noise guard can
    // admit a fn whose name absorbs < half the query when the residual matches a
    // local arg of its param type (`gmob` -> `give(mob)`). Owned keys: a reach
    // path's `final_ty` is a temporary. Immutable after -- the per-`(type, k)`
    // match memo borrows from it.
    let mut local_args: HashMap<String, Vec<CtArg>> = HashMap::new();
    for (ln, lt) in &locals {
        local_args
            .entry(lt.clone())
            .or_default()
            .push(CtArg::direct(ln.clone(), ArgPayload::None));
        for p in reach.arg_paths_from(lt) {
            if p.is_method && p.takes_args {
                continue;
            }
            local_args.entry(p.final_ty.clone()).or_default().push(CtArg::reach(ln, p));
        }
    }

    for &fi in bucket {
        let cf = &index.fns[fi as usize];
        // Never autofill a call to the fn being defined -- a recursive self-call
        // is not what completing its own body means (mag blood.rock:16:39).
        if enclosing_fn == Some(cf.name.as_str()) {
            continue;
        }
        let k = prefix_subseq_len(q, &cf.name_lc);
        if !clean_match(cf.name_lc.as_bytes(), &q[..k.min(q.len())]) {
            continue;
        }
        let local_arg_matches = || {
            local_args
                .get(cf.first_param_ty.as_str())
                .is_some_and(|args| args.iter().any(|a| arg_matches_residual(q, k, &a.text)))
        };
        if k == q.len() {
            committed.push(fi as usize);
        } else if k >= q.len().div_ceil(2) {
            counts.fns += 1;
            passing.push((fi as usize, k));
        } else if local_arg_matches() {
            // Relaxed noise guard: the fn name absorbs < half the query, but the
            // residual matches a LOCAL arg of its param type -- a promoted
            // local-seeded call-through (`gmob` -> `give(mob)`), not fn-name noise.
            counts.fns += 1;
            passing.push((fi as usize, k));
        }
    }

    // (b)+(A) Per `(param_type, k)` group, share BOTH the matching-arg set AND
    // each arg's SUFFIX score vector (`fuzzy_suffix_scores`, a pure fn of
    // `(arg, q)`): computed once, reused by every fn in the group. The arg-side
    // subsequence match depends only on the residual `q[k..]` (not the fn name),
    // so the group filter is exact (`arg_matches_residual`). The per-fn score is
    // then the partition combine `max_s(prefix_name[s] + suffix_arg[s])` (lever
    // A) -- a length-`|q|` max, not a full DP per (fn,arg) pair. This is byte-
    // EXACT vs the old per-pair `fuzzy_score(name(arg), q)` (the `(` junction
    // breaks any cross-half run, and the +1000 prefix bonus never applies to a
    // passing call-through fn), just without re-scoring the shared arg set per fn.
    let empty: Vec<CtArg> = Vec::new();
    // Per group: (arg, suffix_scores, path_penalty, local). suffix + penalty
    // both depend only on the arg (not the fn), so computed once and shared.
    let mut match_memo: HashMap<(&str, usize), Vec<(&CtArg, Vec<i32>, i32, bool)>> =
        HashMap::new();
    let mut prefix_scores: Vec<i32> = Vec::new(); // reused across fns

    // Collect lightweight candidates (score, fn index, arg, payload) across all
    // fns; materialize ONLY the global top `CALL_THROUGH_CAP`. Per fn we keep ALL simple args
    // (browsable fields) so the candidate pool can be large at a short prefix, but
    // we don't `format!` items we'll drop -- rank cheap, materialize survivors
    // (same pattern as general completion).
    // The trailing `u8` is the emit-time `type_rank`: 4 (bucketed last) for a
    // global/partial-match synth, but a LOCAL-SEEDED confident autofill promotes
    // to 0 -- a fn accepting a local of the right type is what the user reached
    // for, so it ranks among the direct locals, not below every candidate
    // (docs/completion-ranking-design.md, Divergence 3).
    let mut cands: Vec<(i32, usize, &CtArg, u8)> = Vec::new();
    for (fi, k) in passing {
        let cf = &index.fns[fi];
        let t = cf.first_param_ty.as_str();
        let group = match_memo.entry((t, k)).or_insert_with(|| {
            let gargs = index.global_type_args.get(t).unwrap_or(&empty);
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
        fuzzy_prefix_scores(cf.name.as_bytes(), q, &mut prefix_scores);
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
                y.0.cmp(&x.0).then_with(|| ci_cmp(x.1.text.as_bytes(), y.1.text.as_bytes()))
            });
            methods.truncate(PER_FN_METHOD_CAP);
        }
        for (score, a, local) in simple.into_iter().chain(methods) {
            cands.push((score, fi, a, if local { 1 } else { 4 }));
        }
    }
    // CONFIDENT unique-arg autofill: for a committed (full-name-match) fn, the
    // residual query is empty so any arg "matches" -- offer `f(arg)` ONLY when the
    // first-param type has exactly ONE in-scope producer (a local/param of that
    // type or a single clean field path), i.e. the arg is unambiguous. Common
    // types (many producers) stay quiet; distinctive ones (`can_cast(s)`,
    // `spell_hotkey_label(s.kind)`) autofill even though the name covers the whole
    // query. Method-chain args are excluded (not "the obvious value").
    for fi in committed {
        let cf = &index.fns[fi];
        let t = cf.first_param_ty.as_str();
        let confident: Vec<&CtArg> = local_args
            .get(t)
            .map(|v| v.iter().filter(|a| !a.method).collect())
            .unwrap_or_default();
        if let [a] = confident.as_slice() {
            // A full-name match with a unique, unambiguous LOCAL arg is the
            // highest-confidence call-through: a fn accepting a local of the right
            // type. type_rank 0 promotes it out of the bottom bucket to rank among
            // the direct locals (method bit still demotes it just below a plain
            // local), above coincidental partial (`passing`) matches.
            let score = fuzzy_score(cf.name.as_bytes(), q) + 5000;
            cands.push((score, fi, a, 0));
        }
    }
    // Global top `CALL_THROUGH_CAP` by score (then ci-arg tiebreak); materialize only those.
    if cands.len() > CALL_THROUGH_CAP {
        cands.select_nth_unstable_by(CALL_THROUGH_CAP, |x, y| {
            y.0.cmp(&x.0).then_with(|| ci_cmp(x.2.text.as_bytes(), y.2.text.as_bytes()))
        });
        cands.truncate(CALL_THROUGH_CAP);
    }
    let ct_start = sink.cands.len();
    for (score, fi, a, type_rank) in cands {
        let cf = &index.fns[fi];
        let fname = cf.name;
        // A local-seeded promotion (0/1) holds only when the synth PRODUCES the
        // expected slot type -- a fn accepting your local but returning the
        // wrong type must not outrank type-directed candidates
        // (docs/completion-ranking-design.md, principle 3). No expected type ->
        // promotion stands on the local seed alone.
        let type_rank = match expected.as_deref_mut() {
            Some(exp) if type_rank < 4 => {
                if exp.accepts_fn_returning(ws, fname.as_str()) {
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
        // Rank 4 = a synthesized whole-call, below every direct candidate;
        // a LOCAL-SEEDED promotion (0/1) lifts it into the direct region.
        // With an expected type the promotion IS type relevance
        // (`type_rank`); with none it is only the coarse bucket, so the
        // promoted rank rides the KIND rung instead (a confident autofill
        // ranks with locals, a partial with globals) and `type_rank`
        // carries just direct-vs-synth. Reuse the score computed above.
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
            RankKey { exact: ex, type_rank, band: bd, kind: 2, ..Default::default() }
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

/// Render every function's signature once -- the dominant completion cost --
/// keyed by name with first-overload-wins to match `crate::query::completion`'s
/// `seen`-dedup order (modules walked in `ws.modules` order). Stored in
/// `BoneProject::fn_sigs` and rebuilt only on non-`BodyOnly` edits, so body
/// keystrokes reuse it instead of re-rendering the whole workspace.
pub(crate) fn build_fn_sigs(ws: &Workspace) -> HashMap<ustr::Ustr, String> {
    let mut sigs = HashMap::new();
    for m in ws.modules.iter() {
        for (name, fns) in m.functions_by_name.iter() {
            if let Some((fmid, fid)) = fns.first() {
                sigs.entry(*name).or_insert_with(|| {
                    let f = &ws.modules[fmid.index()].functions[*fid];
                    crate::query::render_fn_signature(ws, *name, f)
                });
            }
        }
    }
    sigs
}

/// Workspace reference counts per NAME (one namespace -- overloads pool,
/// matching rename semantics): call sites (incl. `?.` method calls), global
/// reads/writes, struct literals, enum-variant qualifiers, and
/// type-annotation ctors. Feeds the `freq` prior rung, log-bucketed via
/// `FREQ_HOT`/`FREQ_WARM`. Cached on `BoneProject` and rebuilt with the
/// other type caches on non-BodyOnly edits -- a body edit's +-1 ref can't
/// realistically flip a log bucket.
pub(crate) fn build_freq_index(ws: &Workspace) -> HashMap<Ustr, u32> {
    use bone_semantic::hir::{self, TypeCtor, TypeExpr};
    let mut counts: HashMap<Ustr, u32> = HashMap::new();
    for m in ws.modules.iter() {
        for te in m.ty_arena.values() {
            if let TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } = te {
                *counts.entry(*n).or_insert(0) += 1;
            }
        }
        for body in m.bodies.values() {
            for te in body.lowered_tys.values() {
                if let TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } = te {
                    *counts.entry(*n).or_insert(0) += 1;
                }
            }
            for (_, expr) in body.expr_arena.iter() {
                let n = match expr {
                    hir::Expr::Call { callee: hir::Callee::Unresolved { name, .. }, .. } => *name,
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
/// coroutine-fn ranking after a `yield`/`yield*`.
pub(crate) fn fn_is_coroutine(ws: &Workspace, name: &str) -> bool {
    let n = Ustr::from(name);
    ws.modules.iter().any(|m| {
        m.functions_by_name
            .get(&n)
            .is_some_and(|fns| fns.iter().any(|(fmid, fid)| ws.modules[fmid.index()].functions[*fid].is_coroutine))
    })
}

/// Whether a struct named `name` is defined anywhere in the workspace.
pub(crate) fn struct_exists(ws: &Workspace, name: &str) -> bool {
    let u = Ustr::from(name);
    ws.modules.iter().any(|m| m.struct_by_name.contains_key(&u))
}

/// The struct named by a fn-name's first `_`-segment, capitalized (`mob_attack_co`
/// -> `Mob`), if such a struct exists -- the payload `T` for a synthesized
/// `Entity<T>`. None when the segment doesn't name a struct.
pub(crate) fn fn_prefix_struct(ws: &Workspace, fn_name: &str) -> Option<Ustr> {
    // Try each `_`-segment leading-first: `<type>_<verb>` (`mob_attack_co`) and
    // `<verb>_<type>` (`spawn_mob`) both name `Mob`. Leading match wins when
    // more than one segment is a struct (the UFCS convention).
    fn_name.split('_').filter(|s| !s.is_empty()).find_map(|seg| {
        let mut chars = seg.chars();
        let cap: String = chars.next()?.to_ascii_uppercase().to_string() + chars.as_str();
        let u = Ustr::from(&cap);
        ws.modules.iter().any(|m| m.struct_by_name.contains_key(&u)).then_some(u)
    })
}

/// Completion candidates in decorator position: every `deco` declaration in the
/// workspace (game decorators like `@update`/`@late_draw` live in the engine),
/// prefix-filtered. Inserts the bare name -- the `@` is already typed.
pub(crate) fn decorator_completions(ws: &Workspace, prefix: &str) -> Vec<CompletionItem> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    // Materializes through the shared shape; no cap (the namespace is small).
    let no_sigs = HashMap::new();
    for m in ws.modules.iter() {
        for (_, dec) in m.decorators.iter() {
            let name = dec.name.as_str();
            if !admit(name.as_bytes(), prefix.as_bytes()) || !seen.insert(name.to_string()) {
                continue;
            }
            let params: Vec<String> = dec
                .param_decls
                .iter()
                .map(|p| format!("{}: {}", p.name, ws.render(None, p.ty)))
                .collect();
            let (ex, bd) = match_band_exact(name, prefix.as_bytes(), prefix.as_bytes());
            let key = RankKey {
                exact: ex,
                band: bd,
                len: RankKey::len_key(9999, name.len()),
                ..Default::default()
            };
            let cand = Cand::new(
                key,
                CandName::Sym(dec.name),
                CandShape::Decorator {
                    detail: (!params.is_empty()).then(|| format!("deco ({})", params.join(", "))),
                },
            );
            out.push(materialize_cand(cand, &no_sigs, false, false));
        }
    }
    out.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    out
}

/// Field completions for an enum record-variant literal. Reparses (the
/// incomplete `{ }` breaks the live body) and lists the variant's record fields.
/// `None` when the head doesn't resolve to a record variant (a unit/tuple variant
/// block head like `if x == Enum::Variant { .. }` is not a record literal) -- the
/// caller falls through to global completion. `Some(vec![])` only when the variant
/// IS a record but the prefix filtered every field out.
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
            crate::query::variant_record_fields(ws, enum_name, variant)
        }) else {
            continue;
        };
        if fields.is_empty() {
            continue;
        }
        let mut items: Vec<CompletionItem> = fields
            .into_iter()
            .enumerate()
            .filter(|(_, (name, _))| {
                prefix.is_empty() || admit(name.as_bytes(), prefix.as_bytes())
            })
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
    let fields = completion_munges(src, offset).into_iter().find_map(|munged| {
        let f = analyze_variant(session, path, &munged, |ws| {
            crate::query::struct_fields_by_name(ws, name)
        })?;
        (!f.is_empty()).then_some(f)
    })?;

    // Fields already written between the `{` and the cursor (`name :`), to skip.
    let written = src.get(brace_open + 1..offset.min(src.len())).unwrap_or("");
    let present: std::collections::HashSet<String> = written
        .split(|c: char| c == ',' || c == '{' || c == '}')
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

/// Variant completions for `EnumName::<variant>`. The trailing `::<partial>` is
/// a hard error that aborts the whole module, so we truncate it back to the bare
/// enum name (accepted as a value) and balance -- that keeps the enum *def* in
/// the parse, which is all the variant list needs.
///
/// In a value position each payload-carrying variant is templated as a snippet
/// so the caller doesn't hand-fill it: a tuple variant inserts `Variant($0)`, a
/// record variant `Variant { $0 }`, with the cursor parked inside. Unit variants
/// (and any variant in a match-arm pattern position) insert the bare name.
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
            crate::query::enum_variants_with_shape(ws, enum_name)
        });
        if let Some(v) = attempt {
            if !v.is_empty() {
                all = v;
                break;
            }
        }
    }

    let pattern = in_pattern_position(src, offset);
    let matches_prefix =
        |v: &str| prefix.is_empty() || admit(v.as_bytes(), prefix.as_bytes());
    // When the in-progress prefix matches NO variant (a typo like `TextAlign::top`),
    // returning empty hands the popup to the client's buffer-word fallback -- the
    // worst outcome, since an enum is a closed set. Show every variant instead, and
    // force each past the CLIENT's own prefix filter via `filter_text = prefix` (cmp
    // re-matches the typed text against each item; without this they vanish again).
    // The response is `is_incomplete`, so the client re-requests each keystroke and
    // the prefix stays fresh. A partial match keeps normal narrowing + default filter.
    let none_match = !prefix.is_empty() && !all.iter().any(|(v, _)| matches_prefix(v));
    let force_filter = none_match.then(|| prefix.to_string());

    let mut items: Vec<CompletionItem> = all
        .into_iter()
        .enumerate()
        .filter(|(_, (v, _))| none_match || matches_prefix(v))
        .map(|(i, (v, shape))| {
            use crate::query::VariantShape;
            let (insert_text, insert_text_format) = match (pattern, shape) {
                (false, VariantShape::Tuple) => {
                    (Some(format!("{v}($0)")), Some(InsertTextFormat::SNIPPET))
                }
                (false, VariantShape::Record) => {
                    (Some(format!("{v} {{ $0 }}")), Some(InsertTextFormat::SNIPPET))
                }
                _ => (None, None),
            };
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

/// The enum a match-arm pattern HEAD expects: the scrutinee's nominal type,
/// when it names an enum. A half-typed head (`match s { co`) breaks the live
/// parse at the missing `=>`, so each attempt is analyzed as a speculative
/// Session variant; the first munge COMPLETES the arm
/// (`<head>__rock_completion__ => {}`) for a fresh head, the standard
/// sentinel pair covers a head whose `=>` is already typed. The anchor sits
/// before the cursor, so it resolves identically in every variant workspace.
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
            let name = nominal_base(ws, at.body, at.ty)?;
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

/// Struct-field completions for `receiver.<field>`. The live body is broken by
/// the trigger `.`, so this munges in a synthetic identifier (rust-analyzer's
/// technique) to get a parseable file, then resolves the receiver's struct
/// **structurally** (no inference required). Field labels are `name: type`.
/// Each munge is analyzed as a speculative Session variant (one incremental
/// update + restore), not a from-scratch project pass.
pub(crate) fn member_completions(
    session: &mut Session,
    path: &str,
    src: &str,
    offset: usize,
    segments: &[String],
    prefix: &str,
    arg_slot: Option<(String, usize)>,
    anchor: Option<SlotAnchor>,
    reach: Option<&crate::reachability::ReachabilityIndex>,
    bool_ctx: bool,
) -> Vec<CompletionItem> {
    let paren_follows = open_paren_follows(src, offset);
    for munged in completion_munges(src, offset) {
        let attempt = analyze_variant(session, path, &munged, |ws| {
            member_completion_items(
                ws, path, offset, segments, prefix, arg_slot.clone(), anchor, reach, bool_ctx,
                paren_follows,
            )
        });
        if let Some(items) = attempt {
            if !items.is_empty() {
                return items;
            }
        }
    }
    Vec::new()
}

/// One munge attempt's member items, resolved against the variant workspace.
/// Whether a member of type `ty_id` (rendered `ty_render`) type-directs into
/// the expected arg slot. Bone's `coerce` on the TyId when present (so an `int`
/// field boosts in an `int?`/`float`/`any` slot); rendered-string equality when
/// the member has no stored TyId (a generic-substituted field) or there's no
/// session. False with no session at all.
pub(crate) fn member_ty_match(mut expected: Option<&mut ExpectedArg>, ty_id: Option<TyId>, ty_render: &str) -> bool {
    let Some(exp) = expected.as_mut() else { return false };
    match ty_id {
        Some(t) => exp.accepts(t),
        None => exp.rendered.iter().any(|e| e == ty_render),
    }
}

/// Member completion for `recv.<cursor>`, any receiver shape. Its type comes
/// from inference: `recv_end` is the receiver expr's end offset (BEFORE the
/// cursor, so stable across the completion munges), and
/// `struct_members_of_receiver_expr` reads its TyId via `type_at`. Fields +
/// struct name then feed the shared builder.
///
/// `None` means the receiver did NOT resolve to a members-bearing type (a
/// non-struct like `scale(x)` -> int) -- the caller may fall through to general
/// completion. `Some(items)` means it DID resolve; `items` may be empty when the
/// prefix matched no member, and the caller must then SUPPRESS general
/// completion (offering globals after `recv.` would be invalid UFCS suggestions).
#[allow(clippy::too_many_arguments)]
pub(crate) fn member_completions_by_receiver(
    session: &mut Session,
    path: &str,
    src: &str,
    offset: usize,
    recv_end: usize,
    prefix: &str,
    arg_slot: Option<(String, usize)>,
    anchor: Option<SlotAnchor>,
    reach: Option<&crate::reachability::ReachabilityIndex>,
    bool_ctx: bool,
) -> Option<Vec<CompletionItem>> {
    let paren_follows = open_paren_follows(src, offset);
    let mut resolved_empty = false;
    for munged in completion_munges(src, offset) {
        let attempt = analyze_variant(session, path, &munged, |ws| {
            let mid = user_module_for_path(ws, path)?;
            let (fields, recv_struct, vec_elem) =
                crate::query::struct_members_of_receiver_expr(ws, mid, recv_end)?;
            Some(build_member_items(
                ws, mid, offset, fields, Some(recv_struct), vec_elem, prefix, arg_slot.clone(),
                anchor, reach, bool_ctx, paren_follows,
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn member_completion_items(
    ws: &Workspace,
    path: &str,
    offset: usize,
    segments: &[String],
    prefix: &str,
    arg_slot: Option<(String, usize)>,
    anchor: Option<SlotAnchor>,
    reach: Option<&crate::reachability::ReachabilityIndex>,
    bool_ctx: bool,
    paren_follows: bool,
) -> Vec<CompletionItem> {
    // The CURRENT file's module (where the receiver/offset live) -- not just
    // "first non-builtin", which is wrong now that the whole project loads.
    let Some(mid) = user_module_for_path(ws, path) else {
        return Vec::new();
    };
    // Name-path receiver (`recv.field.field`): resolve struct + fields
    // structurally (no inference). A call/index receiver is resolved from its
    // INFERRED type instead -- see `member_completion_items_by_receiver`.
    let fields = crate::query::struct_fields_for_path(ws, mid, offset, segments);
    let recv_struct = crate::query::struct_name_for_path(ws, mid, offset, segments);
    let vec_elem = crate::query::receiver_vec_elem(ws, mid, offset, segments);
    build_member_items(
        ws, mid, offset, fields, recv_struct, vec_elem, prefix, arg_slot, anchor, reach, bool_ctx,
        paren_follows,
    )
}

/// Shared item-builder for both receiver-resolution strategies (structural
/// name-walk and inferred receiver type): UFCS methods, type-directed
/// field/method ordering, Entity `get` boost, arg-position transitive reach.
#[allow(clippy::too_many_arguments)]
fn build_member_items(
    ws: &Workspace,
    mid: ModuleId,
    offset: usize,
    fields: Vec<(String, String, Option<TyId>)>,
    recv_struct: Option<Ustr>,
    // Receiver's rendered element type for a Vec (`[Vec2]` -> "Vec2"); filters
    // UFCS methods to those whose element unifies. None for nominal receivers.
    vec_elem: Option<String>,
    prefix: &str,
    arg_slot: Option<(String, usize)>,
    anchor: Option<SlotAnchor>,
    reach: Option<&crate::reachability::ReachabilityIndex>,
    bool_ctx: bool,
    // A `(` already follows the cursor -- method items insert the bare name so
    // accepting doesn't double the parens (`recv.le|()` -> `recv.len()`).
    paren_follows: bool,
) -> Vec<CompletionItem> {
    {
        // UFCS methods: functions whose first param is the receiver's struct,
        // callable as `recv.method(...)`. Surfaced alongside fields, as rock does.
        let methods = recv_struct
            .map(|name| crate::query::ufcs_methods(ws, name, vec_elem.as_deref()))
            .unwrap_or_default();
        if fields.is_empty() && methods.is_empty() {
            return Vec::new();
        }
        // Typed expected-arg session for the enclosing call, built against THIS
        // (variant) workspace so its Gamma resolves the member TyIds the
        // candidates carry (docs/assignable-plan.md phase 2). `coerce` decides
        // the member type-match, not a rendered-string `==`; the receiver-
        // filtered `.rendered` still feeds the string-keyed reach index. None
        // when not in an arg slot or the body has no cached inference -- then
        // there's no type direction (kind/name order), same as before.
        let mut expected = arg_slot.as_ref().and_then(|(callee, idx)| {
            let inf = ws.inference(mid)?;
            ExpectedArg::for_call(ws, mid, inf, offset, callee, *idx, None)
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
            .map(|m| member_ty_match(expected.as_mut(), m.return_ty, m.returns.as_deref().unwrap_or("")))
            .collect();
        // Direct members are 1-hop (dist 1): they carry a sort_text too, so they
        // order consistently with -- and ahead of, by distance -- the transitive
        // reach paths added below (fuzzy score is the dominant term either way).
        let pfx = prefix.as_bytes();
        // In an `if`/`while <chain>` condition, `bool` fields are the likely
        // target -- soft-boost them (score-tie tiebreak only). Mutually exclusive
        // with arg-position reach paths, so the bool-format sort_text is safe to
        // mix only with the field/method items here.
        // Call-arg position (a known expected param type) -- a direct field/method
        // whose type matches that param ranks first. Mutually exclusive with
        // `bool_ctx` (a condition chain isn't inside a call).
        // `_`-prefixed fields are soft-private: hidden from member completion
        // unless the user is explicitly typing an `_` prefix. Fields stay public
        // (no type-check change) -- this is purely a display convention.
        let show_private = prefix.starts_with('_');
        // Member fields get a small SCORE boost over methods (not a hard rung):
        // typing `.` most often reaches for a field, so on a match tie the field
        // leans first -- but a method that matches the prefix better still wins.
        // Only with a real query: at empty prefix score is 0 and any bonus would
        // spuriously trip the length tiebreak (empty-prefix browse stays
        // alphabetical). Tunable; eyeball with `rock lsp completion <pos> --repl`.
        let field_boost = if prefix.is_empty() { 0 } else { 25 };
        let field_items = fields.into_iter().zip(field_match).filter_map(|((name, ty, _), tmatch)| {
            if name.starts_with('_') && !show_private {
                return None;
            }
            (prefix.is_empty() || admit(name.as_bytes(), prefix.as_bytes())).then(|| CompletionItem {
                sort_text: Some(if bool_ctx {
                    member_bool_sort_text(&name, pfx, ty == "bool", 0, field_boost)
                } else if arg_ctx {
                    member_arg_sort_text(&name, pfx, tmatch, 0, 1, field_boost)
                } else {
                    arg_sort_text(0, 0, &name, pfx, 1, 0, field_boost)
                }),
                label: format!("{name}: {ty}"),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(ty),
                insert_text: Some(name),
                ..Default::default()
            })
        });
        let method_items = methods.into_iter().zip(method_match).filter_map(|(m, rmatch)| {
            (prefix.is_empty() || admit(m.name.as_bytes(), prefix.as_bytes())).then(|| {
                // Inline `(params) -> ret`. A method with args left after the
                // receiver accepts as a snippet `name($0)` (`recv.name()`, cursor
                // inside); one with no remaining args inserts `name()` as plain
                // text so the cursor lands after the `)`. When a `(` already
                // follows, insert the bare name -- the parens are already there.
                let (insert_text, insert_text_format) = if paren_follows {
                    (Some(m.name.clone()), None)
                } else if m.takes_args {
                    (Some(format!("{}($0)", m.name)), Some(InsertTextFormat::SNIPPET))
                } else {
                    (Some(format!("{}()", m.name)), None)
                };
                CompletionItem {
                    // `method` rung = 0 (same as fields): in member position the
                    // `.` already means "give me a member", so a method is not
                    // demoted below a field by a hard rung. Fields get only a soft
                    // SCORE lean (MEMBER_FIELD_BOOST); methods take no bonus, so a
                    // better-matching method still outranks a field. (In non-member
                    // position fns are still down-ranked, by `kind`.)
                    sort_text: Some(if bool_ctx {
                        member_bool_sort_text(&m.name, pfx, false, 0, 0)
                    } else if arg_ctx {
                        member_arg_sort_text(&m.name, pfx, rmatch, 0, 1, 0)
                    } else {
                        arg_sort_text(0, 0, &m.name, pfx, 1, 0, 0)
                    }),
                    label: m.name.clone(),
                    label_details: fn_or_type_label_details(SymCat::Function, Some(&m.signature)),
                    kind: Some(CompletionItemKind::METHOD),
                    detail: Some(m.signature),
                    insert_text,
                    insert_text_format,
                    ..Default::default()
                }
            })
        });
        let mut items: Vec<CompletionItem> = field_items.chain(method_items).collect();
        // Entity receiver: `.get()` (-> T?) is the dominant accessor, almost
        // always followed by `.unwrap()`. Rank `get` first, and offer the whole
        // `get().unwrap()` chain (matches a `getu`/`getunwrap` prefix, which the
        // bare `get` method can't since it filters on `starts_with`).
        if recv_struct.is_some_and(|s| s.as_str() == "Entity") {
            for it in items.iter_mut() {
                if it.label == "get" {
                    it.sort_text = Some(RankKey { len: RankKey::len_key(9999, "get".len()), ..Default::default() }.encode("get"));
                }
            }
            if clean_match(b"get().unwrap()", prefix.as_bytes()) {
                items.push(CompletionItem {
                    label: "get().unwrap()".to_string(),
                    kind: Some(CompletionItemKind::METHOD),
                    detail: Some("unwrap the Entity's value (T)".to_string()),
                    insert_text: Some("get().unwrap()".to_string()),
                    sort_text: Some(RankKey { fine: RankKey::fine(9998), len: RankKey::len_key(9998, "get().unwrap()".len()), ..Default::default() }.encode("get().unwrap()")),
                    ..Default::default()
                });
            }
        }
        // In a call-arg position with a known expected type, also offer
        // TRANSITIVE suffix paths through the receiver -- fields, UFCS method
        // returns, or a MIX (`movable.to_ivec()` when the param wants an IVec2)
        // -- via the reachability index. Multi-hop only: 1-hop fields/methods are
        // already in `field_items`/`method_items` above. The index keys on
        // type-name strings, so the live-built one serves the munged buffer; if
        // it's missing, build a local one from this workspace.
        let reach_targets: &[String] = expected.as_ref().map_or(&[], |e| &e.rendered);
        if !reach_targets.is_empty() {
            if let Some(rs) = recv_struct {
                let local_idx;
                let idx = match reach {
                    Some(r) => r,
                    None => {
                        local_idx = crate::reachability::ReachabilityIndex::build(ws);
                        &local_idx
                    }
                };
                let res = idx.paths_to(rs.as_str(), reach_targets, REACH_CAP);
                let pfx = prefix.as_bytes();
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                for p in res.paths {
                    if !p.suffix.contains('.') {
                        continue; // 1-hop field/method, already offered above
                    }
                    if !pfx.is_empty() && !clean_match(p.suffix.as_bytes(), pfx) {
                        continue;
                    }
                    if !seen.insert(p.suffix.clone()) {
                        continue;
                    }
                    let (label, kind, insert_text, insert_text_format) = if p.is_method {
                        let call = format!("{}()", p.suffix);
                        let (insert, fmt) = if paren_follows {
                            (p.suffix.clone(), None)
                        } else if p.takes_args {
                            (format!("{}($0)", p.suffix), Some(InsertTextFormat::SNIPPET))
                        } else {
                            (call.clone(), None)
                        };
                        (format!("{call} -> {}", p.final_ty), CompletionItemKind::METHOD, insert, fmt)
                    } else {
                        (format!("{}: {}", p.suffix, p.final_ty), CompletionItemKind::FIELD, p.suffix.clone(), None)
                    };
                    // Reach paths are type-matches by construction (paths TO the
                    // expected type) -- prio 0, same format as the direct items.
                    let sort_text = member_arg_sort_text(&p.suffix, pfx, true, u8::from(p.is_method), p.dist, 0);
                    items.push(CompletionItem {
                        label,
                        kind: Some(kind),
                        detail: Some(p.final_ty),
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
