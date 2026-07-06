//! Function inlining: a MIR->MIR pass that splices an eligible callee's body
//! into the caller so a hot loop traces with no per-call frame (rock-codegen
//! runtime parity). Re-fit of the deleted hybrid-SSA `skull/src/inline.rs`
//! (`7b1615424^`) onto the cell-model CFG. Design + perf-deferral ledger:
//! `docs/skull-inlining-plan.md`.
//!
//! The cell model makes the splice simpler than the old SSA one: there are NO
//! block params, so arg-binding is a `Store` into the callee's (cloned) param
//! cell and a returned value is a `Store` into a result/dest cell — the caller's
//! `cleanup` (copy-prop + DCE) then elides the round-trips.
//!
//! Policy is ANNOTATION-GATED: only `@inline`/`@pure` functions inline (both
//! authoritative -- they bypass the size caps). This keeps the hot-reload re-emit
//! fan-out + overlay small (a body edit re-emits every fn that inlined the
//! changed callee, so fewer inlines = fewer dependents). The hot stdlib helpers
//! (math, getters, value-struct ctors) carry `@inline`/`@pure`, so they still
//! inline; incidental small accessors no longer do.
//!
//! `SKULL_INLINE_SIZE_HEURISTIC=1` opts back into the OLD size-based policy (any
//! small body inlines; the choice the old skull validated at `skull-diff` = 0) --
//! the A/B lever for the inlining-cost investigation (TODO.md). Both policies are
//! behavior-preserving (`skull-diff` = 0); they differ only in how much inlines,
//! i.e. in compile/emit cost, hot-reload size, and runtime trace shape.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use bone_semantic::inference::Workspace;
use comfy_la_arena::Idx;
use skull_mir::lower::{lower_function, ScalarCtx};
use skull_mir::mir::*;

use crate::top_level_blocks;

/// Straight-line callee inst cap (entry-Return body). A small body is the win;
/// a large one bloats emit and overflows LuaJIT's snapshot budget (old skull:
/// 300 regressed perf). Re-tune only with `-jv` / `--gc-window` evidence.
const STRAIGHT_INSTS: usize = 48;
/// Multi-block callee total-reachable-inst cap.
const GENERAL_INSTS: usize = 72;
/// Per-caller splice budget — bounds mutual-recursion expansion. A remaining
/// real call is always correct.
const MAX_INLINES_PER_FN: usize = 1000;
/// Max cells a caller may hold before inlining stops (the call stays — always
/// correct). This is a RAW cell count, but skull-codegen-lua now COLORS cells
/// (liveness-coalesced onto Lua's 200-local cap), so a function with N raw cells
/// emits far fewer locals (bitgun `update_mobs`: 193 raw -> 28 colored). The
/// budget therefore guards against runaway expansion / the rare all-simultaneously
/// -live pathological case, not the typical local count; 400 matches the value
/// the old (coloring-aware) skull validated across the corpus.
const CELL_BUDGET: usize = 400;

/// Whether a candidate callee is a single straight-line block or a CFG.
#[derive(Copy, Clone, PartialEq)]
enum Kind {
    Straight,
    General,
}

/// A pristine lowered snapshot of an eligible callee — every splice clones from
/// this clean body (transitive inlining re-scans the caller, never mutates a
/// candidate).
struct CalleeBody {
    func: MirFunc,
    kind: Kind,
    /// Uniform `Return` value count (0 for void, 1 for a scalar, N for a
    /// scalarized value struct). Matched against the call site's result shape.
    ret_arity: usize,
    /// The callee's reachable blocks (general candidates only — the straight path
    /// clones no blocks). After a general splice their clones become reachable
    /// top-level blocks of the caller; the driver adds them to its top-level set.
    reachable: Vec<Idx<Block>>,
    /// Reachable inst count of the (cleaned) body — the code each splice copies.
    /// Diagnostic only (drives `skull-inline-stats`); not used by the policy.
    insts: usize,
    /// `@inline`-forced (bypassed the size caps). Diagnostic only.
    forced: bool,
}

/// One splice the inliner performed, for the `skull-inline-stats` diagnostic
/// (`inline_functions` returns these alongside its dependency set). Records the
/// callee whose body was copied in, its shape, and how much code it added.
pub struct SpliceRecord {
    pub callee: FuncRef,
    pub straight: bool,
    pub forced: bool,
    /// Reachable inst count of the callee body copied at this site.
    pub callee_insts: usize,
}

/// Lazy callee-eligibility cache, keyed by [`FuncRef`]. `None` = looked up and
/// not inlinable. Lives across a whole-workspace emit so each callee lowers +
/// classifies once -- and, in the incremental emit, ACROSS body edits: a callee
/// whose body and scalar ABI are unchanged keeps its cached lowered body instead
/// of re-lowering every edit. The driver [`invalidate`](Self::invalidate)s the
/// affected callees per edit so no stale body is ever cloned.
#[derive(Default)]
pub struct InlineCache {
    cands: HashMap<FuncRef, Option<CalleeBody>>,
}

impl InlineCache {
    /// Drop the cached candidate for `fref` so the next lookup re-lowers and
    /// re-classifies it. A fref's cached lowered body goes stale iff its own body
    /// changed OR a fn it calls flipped scalar ABI -- both are within the emit's
    /// `affected` set, which the incremental driver invalidates wholesale.
    pub fn invalidate(&mut self, fref: FuncRef) {
        self.cands.remove(&fref);
    }
}

/// `SKULL_INLINE_SIZE_HEURISTIC=1`: opt back into the OLD size-based policy (any
/// small body inlines, `@inline` bypasses the caps). The default is now
/// annotation-gated -- only `@inline`/`@pure` inline -- which keeps the
/// hot-reload re-emit fan-out + overlay small. Kept as the A/B lever for the
/// inlining-cost investigation (TODO.md). Read once per process.
fn size_heuristic() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("SKULL_INLINE_SIZE_HEURISTIC").is_ok())
}

/// The annotation gate for inlining: `None` = this fn must not inline at all;
/// `Some(force)` = eligible, with `force` meaning "bypass the size caps". Pure
/// (no env) so it is unit-testable. DEFAULT: ONLY `@inline`/`@pure` inline, and
/// both force (the annotation is authoritative). `size_heuristic` opts back into
/// the old policy: any fn eligible, only `@inline` forces.
fn annotation_gate(has_inline: bool, has_pure: bool, size_heuristic: bool) -> Option<bool> {
    if size_heuristic {
        Some(has_inline)
    } else {
        (has_inline || has_pure).then_some(true)
    }
}

/// Lower `fref` and classify it as an inline candidate, or `None` if it is not
/// eligible. Hard exclusions: extern, `inline(lua)`, `co fn`, lambda-bearing,
/// self-recursive, or any reachable non-`Return`/`Goto`/`Branch`/`Switch`
/// terminator (a reachable `Unreachable`/`Yield` would mis-lower to a bare
/// `return` inside the caller). `@inline` bypasses the size caps only.
fn classify(ws: &Workspace, scx: &ScalarCtx, fref: FuncRef) -> Option<CalleeBody> {
    let f = &ws.module(fref.module).functions[fref.func];
    if f.is_extern || f.lua_body.is_some() || f.is_coroutine {
        return None;
    }
    let has_inline = f.annotations.iter().any(|a| a.name.name.as_str() == "inline");
    let has_pure = f.annotations.iter().any(|a| a.name.name.as_str() == "pure");
    let force = annotation_gate(has_inline, has_pure, size_heuristic())?;
    let mut mf = lower_function(ws, scx, fref.module, fref.func).ok()?;
    // Cleanup first so the size gate reflects emitted size and the splice clones
    // a tight body (the caller re-runs cleanup over the whole result anyway).
    crate::cleanup(&mut mf);

    // Structural exclusions over the whole arena.
    for (_, inst) in mf.insts.iter() {
        match inst {
            Inst::Lambda { .. } => return None, // captured-cell cloning deferred (v0)
            Inst::Call { func, .. } | Inst::CallMulti { func, .. } if *func == fref => {
                return None; // self-recursive
            }
            _ => {}
        }
    }

    let reachable = top_level_blocks(&mf);
    // A reachable terminator must be one the splice can route. `Unreachable`
    // lowers to a bare `return` and `Yield` is coroutine-only; either reachable
    // here would corrupt caller control flow.
    for &b in &reachable {
        match &mf.blocks[b].term {
            Term::Unreachable | Term::Yield { .. } => return None,
            _ => {}
        }
    }
    let reachable_insts: usize = reachable.iter().map(|&b| mf.blocks[b].insts.len()).sum();

    if matches!(mf.blocks[mf.entry].term, Term::Return(_)) {
        // Straight: entry ends in Return, so it is the only reachable block.
        if !force && reachable_insts > STRAIGHT_INSTS {
            return None;
        }
        let ret_arity = match &mf.blocks[mf.entry].term {
            Term::Return(vs) => vs.len(),
            _ => unreachable!(),
        };
        Some(CalleeBody { func: mf, kind: Kind::Straight, ret_arity, reachable: Vec::new(), insts: reachable_insts, forced: force })
    } else {
        // General: uniform return arity across every reachable Return.
        let mut arities = reachable
            .iter()
            .filter_map(|&b| match &mf.blocks[b].term {
                Term::Return(vs) => Some(vs.len()),
                _ => None,
            });
        let ret_arity = arities.next()?; // no Return -> can't bind a result
        if !arities.all(|a| a == ret_arity) {
            return None;
        }
        if !force && reachable_insts > GENERAL_INSTS {
            return None;
        }
        Some(CalleeBody { func: mf, kind: Kind::General, ret_arity, reachable, insts: reachable_insts, forced: force })
    }
}

/// Get (lowering+classifying on first sight) the candidate for `fref`.
fn get<'a>(
    cache: &'a mut InlineCache,
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
) -> Option<&'a CalleeBody> {
    if !cache.cands.contains_key(&fref) {
        let v = classify(ws, scx, fref);
        cache.cands.insert(fref, v);
    }
    cache.cands[&fref].as_ref()
}

/// One resolved, eligible call site in the caller, extracted before the splice.
enum Site {
    /// `Inst::Call` — single (or void) result; `call_id` is the call's value.
    Single { bid: Idx<Block>, idx: usize, call_id: Idx<Inst>, fref: FuncRef, args: Vec<Idx<Inst>> },
    /// `Inst::CallMulti` — N leaves bound positionally into `dests`.
    Multi { bid: Idx<Block>, idx: usize, dests: Vec<Idx<Cell>>, fref: FuncRef, args: Vec<Idx<Inst>> },
}

impl Site {
    fn fref(&self) -> FuncRef {
        match self {
            Site::Single { fref, .. } | Site::Multi { fref, .. } => *fref,
        }
    }
    fn pos(&self) -> (Idx<Block>, usize) {
        match self {
            Site::Single { bid, idx, .. } | Site::Multi { bid, idx, .. } => (*bid, *idx),
        }
    }
}

/// Whether candidate `cand` matches the call site's arg/result arity.
fn arity_ok(cand: &CalleeBody, site: &Site) -> bool {
    match site {
        Site::Single { args, .. } => {
            cand.func.params.len() == args.len() && cand.ret_arity <= 1
        }
        Site::Multi { args, dests, .. } => {
            cand.func.params.len() == args.len() && cand.ret_arity == dests.len()
        }
    }
}

/// Scan the caller for the next eligible call site (Call/CallMulti to a cached
/// candidate with matching arity, not the caller itself), starting AT `resume`.
///
/// `top` is the caller's own top-level blocks (NEVER a closure sub-CFG — a cell
/// used in a lambda body is an upvalue, so inlining there piles onto LuaJIT's
/// 60-upvalue cap, and the block-split assumes the top-level CFG). The driver
/// maintains it incrementally so this never recomputes reachability.
///
/// Blocks are visited in arena-id order (stable: a splice only appends blocks
/// with higher ids and only inserts insts AT/after its own position), so resuming
/// at the last splice point is sound — no eligible call can appear before it (the
/// prefix was already scanned and a splice introduces calls only within the
/// inlined body, at/after that point).
fn find_site(
    func: &MirFunc,
    ws: &Workspace,
    scx: &ScalarCtx,
    cache: &mut InlineCache,
    self_ref: FuncRef,
    top: &HashSet<Idx<Block>>,
    resume: Option<(Idx<Block>, usize)>,
) -> Option<Site> {
    let mut started = resume.is_none();
    for (bid, _) in func.blocks.iter() {
        if !top.contains(&bid) {
            continue;
        }
        // Find the inst index to start scanning this block from.
        let start = match resume {
            Some((rb, ri)) if !started => {
                if bid != rb {
                    continue; // haven't reached the resume block yet
                }
                started = true;
                ri
            }
            _ => 0,
        };
        let insts = func.blocks[bid].insts.clone();
        for (idx, iid) in insts.into_iter().enumerate().skip(start) {
            let site = match &func.insts[iid] {
                Inst::Call { func: fref, args } if *fref != self_ref => Site::Single {
                    bid,
                    idx,
                    call_id: iid,
                    fref: *fref,
                    args: args.clone(),
                },
                Inst::CallMulti { dests, func: fref, args } if *fref != self_ref => Site::Multi {
                    bid,
                    idx,
                    dests: dests.clone(),
                    fref: *fref,
                    args: args.clone(),
                },
                _ => continue,
            };
            let cell_count = func.cells.len();
            match get(cache, ws, scx, site.fref()) {
                // Decline a splice that would breach the per-caller cell budget
                // (keep scanning — a smaller candidate may still fit).
                Some(cand)
                    if arity_ok(cand, &site)
                        && cell_count + cand.func.cells.len() <= CELL_BUDGET =>
                {
                    return Some(site)
                }
                _ => {}
            }
        }
    }
    None
}

/// Clone the callee's cells + insts into `caller` (NOT blocks — only the straight
/// path uses this; straight candidates are lambda-free so no inst references a
/// block). Returns `(inst_map, cell_map)`.
fn clone_cells_insts(
    caller: &mut MirFunc,
    callee: &MirFunc,
) -> (HashMap<Idx<Inst>, Idx<Inst>>, HashMap<Idx<Cell>, Idx<Cell>>) {
    let mut cell_map = HashMap::default();
    cell_map.reserve(callee.cells.len());
    for (cid, cell) in callee.cells.iter() {
        cell_map.insert(cid, caller.cells.alloc(cell.clone()));
    }
    let mut inst_map = HashMap::default();
    inst_map.reserve(callee.insts.len());
    let new_ids: Vec<(Idx<Inst>, Idx<Inst>)> = callee
        .insts
        .iter()
        .map(|(iid, inst)| {
            let nid = caller.insts.alloc(inst.clone());
            inst_map.insert(iid, nid);
            (iid, nid)
        })
        .collect();
    for (_, nid) in new_ids {
        remap_clone(&mut caller.insts[nid], &inst_map, &cell_map, &HashMap::default());
    }
    (inst_map, cell_map)
}

/// Clone the callee's cells, blocks, AND insts into `caller`. Returns
/// `(inst_map, cell_map, block_map)`. Two-pass (alloc-then-remap) so it is
/// independent of arena order.
fn clone_all(
    caller: &mut MirFunc,
    callee: &MirFunc,
) -> (
    HashMap<Idx<Inst>, Idx<Inst>>,
    HashMap<Idx<Cell>, Idx<Cell>>,
    HashMap<Idx<Block>, Idx<Block>>,
) {
    let mut cell_map = HashMap::default();
    cell_map.reserve(callee.cells.len());
    for (cid, cell) in callee.cells.iter() {
        cell_map.insert(cid, caller.cells.alloc(cell.clone()));
    }
    let mut block_map = HashMap::default();
    block_map.reserve(callee.blocks.len());
    for (bid, _) in callee.blocks.iter() {
        block_map.insert(bid, caller.blocks.alloc(Block::new()));
    }
    let mut inst_map = HashMap::default();
    inst_map.reserve(callee.insts.len());
    let new_ids: Vec<Idx<Inst>> = callee
        .insts
        .iter()
        .map(|(iid, inst)| {
            let nid = caller.insts.alloc(inst.clone());
            inst_map.insert(iid, nid);
            nid
        })
        .collect();
    for nid in new_ids {
        remap_clone(&mut caller.insts[nid], &inst_map, &cell_map, &block_map);
    }
    // Fill cloned block inst-lists + terminators.
    for (bid, block) in callee.blocks.iter() {
        let nbid = block_map[&bid];
        caller.blocks[nbid].insts = block.insts.iter().map(|i| inst_map[i]).collect();
        let mut term = block.term.clone();
        remap_term_clone(&mut term, &inst_map, &block_map);
        caller.blocks[nbid].term = term;
    }
    (inst_map, cell_map, block_map)
}

/// Remap a freshly-cloned inst's value (via [`remap_inst`]), cell, and block
/// operands through the clone maps.
fn remap_clone(
    inst: &mut Inst,
    im: &HashMap<Idx<Inst>, Idx<Inst>>,
    cm: &HashMap<Idx<Cell>, Idx<Cell>>,
    bm: &HashMap<Idx<Block>, Idx<Block>>,
) {
    crate::remap_inst(inst, &|x| im[&x]);
    match inst {
        Inst::Load(c) => *c = cm[c],
        Inst::Store { cell, .. } => *cell = cm[cell],
        Inst::CallMulti { dests, .. } => {
            for d in dests {
                *d = cm[d];
            }
        }
        Inst::MapNext { key_cell, val_cell, .. } => {
            *key_cell = cm[key_cell];
            *val_cell = cm[val_cell];
        }
        Inst::Lambda { params, entry } => {
            for p in params {
                *p = cm[p];
            }
            *entry = bm[entry];
        }
        _ => {}
    }
}

/// Remap a cloned terminator's inst operands AND block targets.
fn remap_term_clone(
    term: &mut Term,
    im: &HashMap<Idx<Inst>, Idx<Inst>>,
    bm: &HashMap<Idx<Block>, Idx<Block>>,
) {
    let mi = |x: Idx<Inst>| im[&x];
    match term {
        Term::Return(vs) => vs.iter_mut().for_each(|v| *v = mi(*v)),
        Term::Goto(b) => *b = bm[b],
        Term::Branch { cond, then_bb, else_bb } => {
            *cond = mi(*cond);
            *then_bb = bm[then_bb];
            *else_bb = bm[else_bb];
        }
        Term::Switch { discr, cases, default } => {
            *discr = mi(*discr);
            for (_, b) in cases.iter_mut() {
                *b = bm[b];
            }
            *default = bm[default];
        }
        Term::Yield { values, resume } => {
            values.iter_mut().for_each(|v| *v = mi(*v));
            *resume = bm[resume];
        }
        Term::Unreachable => {}
    }
}

/// Substitute `from` -> `to` in every value operand across the whole function
/// (all insts + terminators). The straight path needs this because a call's
/// result value can be live across blocks (skull's regalloc colors cross-block
/// values via liveness — e.g. the LHS of `?? ` is tested in one block and reused
/// in another). `to` is defined where the call was, which dominates every use of
/// `from`, so the replacement is sound. (Whole-arena walk; the incremental
/// dest-binding perf refinement is deferred — `docs/skull-inlining-plan.md`.)
fn subst_all(func: &mut MirFunc, from: Idx<Inst>, to: Idx<Inst>) {
    let m = |x: Idx<Inst>| if x == from { to } else { x };
    let ids: Vec<Idx<Inst>> = func.insts.iter().map(|(i, _)| i).collect();
    for id in ids {
        crate::remap_inst(&mut func.insts[id], &m);
    }
    let bids: Vec<Idx<Block>> = func.blocks.iter().map(|(b, _)| b).collect();
    for b in bids {
        let mut term = func.blocks[b].term.clone();
        crate::remap_term(&mut term, &m);
        func.blocks[b].term = term;
    }
}

/// Splice candidate `cand` into `func` at `site`, removing the call. Returns the
/// new top-level (reachable) blocks the splice added, for the driver to fold into
/// its top-level set (straight adds none).
fn splice(func: &mut MirFunc, site: Site, cand: &CalleeBody) -> Vec<Idx<Block>> {
    match cand.kind {
        Kind::Straight => {
            splice_straight(func, site, cand);
            Vec::new()
        }
        Kind::General => splice_general(func, site, cand),
    }
}

/// Straight splice: the callee is a single Return-terminated block. Inline its
/// insts into the caller block in place of the call; no new blocks (the callee
/// is lambda-free, so its insts reference no block). The caller block keeps its
/// own terminator.
fn splice_straight(func: &mut MirFunc, site: Site, cand: &CalleeBody) {
    let callee = &cand.func;
    let (inst_map, cell_map) = clone_cells_insts(func, callee);

    // Arg -> param-cell stores.
    let mut arg_stores = Vec::new();
    let (bid, idx, args) = match &site {
        Site::Single { bid, idx, args, .. } | Site::Multi { bid, idx, args, .. } => (*bid, *idx, args.clone()),
    };
    for (param, arg) in callee.params.iter().zip(args.iter()) {
        let pc = cell_map[param];
        arg_stores.push(func.insts.alloc(Inst::Store { cell: pc, value: *arg }));
    }

    // Cloned body insts (the entry block's inst list, mapped).
    let body_insts: Vec<Idx<Inst>> =
        callee.blocks[callee.entry].insts.iter().map(|i| inst_map[i]).collect();

    // Returned values (mapped).
    let ret_vals: Vec<Idx<Inst>> = match &callee.blocks[callee.entry].term {
        Term::Return(vs) => vs.iter().map(|v| inst_map[v]).collect(),
        _ => unreachable!("straight candidate entry must end in Return"),
    };

    // Rebuild the caller block: before | arg-stores | body | dest-stores | after.
    let old = std::mem::take(&mut func.blocks[bid].insts);
    let mut new_insts: Vec<Idx<Inst>> = old[..idx].to_vec();
    new_insts.extend(arg_stores);
    new_insts.extend(body_insts);

    match &site {
        Site::Multi { dests, .. } => {
            for (dest, rv) in dests.iter().zip(ret_vals.iter()) {
                let st = func.insts.alloc(Inst::Store { cell: *dest, value: *rv });
                new_insts.push(st);
            }
            new_insts.extend_from_slice(&old[idx + 1..]);
            func.blocks[bid].insts = new_insts;
        }
        Site::Single { call_id, .. } => {
            // Rewrite uses of the call value to the returned value (or an emitted
            // nil for a void callee). For a void callee the nil const must live
            // in a block, so append it to the spliced body before the tail.
            let to = match ret_vals.first() {
                Some(v) => *v,
                None => {
                    let nil = func.insts.alloc(Inst::Const(Lit::Nil));
                    new_insts.push(nil);
                    nil
                }
            };
            new_insts.extend_from_slice(&old[idx + 1..]);
            func.blocks[bid].insts = new_insts;
            subst_all(func, *call_id, to);
        }
    }
}

/// General splice: the callee has internal control flow. Split the caller block
/// at the call; the prefix arg-stores + `Goto` the cloned entry; a continuation
/// block holds the post-call tail. Each cloned `Return` binds its values and
/// `Goto`s the continuation.
fn splice_general(func: &mut MirFunc, site: Site, cand: &CalleeBody) -> Vec<Idx<Block>> {
    let callee = &cand.func;
    let (inst_map, cell_map, block_map) = clone_all(func, callee);
    let entry_clone = block_map[&callee.entry];

    let (bid, idx, args) = match &site {
        Site::Single { bid, idx, args, .. } | Site::Multi { bid, idx, args, .. } => (*bid, *idx, args.clone()),
    };

    // Split the caller block at the call.
    let old = std::mem::take(&mut func.blocks[bid].insts);
    let before: Vec<Idx<Inst>> = old[..idx].to_vec();
    let after: Vec<Idx<Inst>> = old[idx + 1..].to_vec();
    let orig_term = func.blocks[bid].term.clone();

    // Continuation block: post-call tail + the caller's original terminator.
    let cont = func.new_block();
    func.blocks[cont].insts = after;
    func.blocks[cont].term = orig_term;

    // Single-result: a result cell read back in the continuation. Repurpose the
    // call inst id in place as `Load(rc)` at the front of `cont` so downstream
    // refs (single-block, all in `cont`) resolve unchanged.
    let result_cell = match &site {
        Site::Single { call_id, .. } => {
            let rc = func.new_cell(None);
            func.insts[*call_id] = Inst::Load(rc);
            func.blocks[cont].insts.insert(0, *call_id);
            Some(rc)
        }
        Site::Multi { .. } => None,
    };

    // Prefix: before | arg-stores; Goto the cloned entry.
    let mut prefix = before;
    for (param, arg) in callee.params.iter().zip(args.iter()) {
        let pc = cell_map[param];
        prefix.push(func.insts.alloc(Inst::Store { cell: pc, value: *arg }));
    }
    func.blocks[bid].insts = prefix;
    func.blocks[bid].term = Term::Goto(entry_clone);

    // Route each cloned Return to the continuation, binding return values.
    let cloned_blocks: Vec<Idx<Block>> = callee.blocks.iter().map(|(b, _)| block_map[&b]).collect();
    for nb in cloned_blocks {
        let vs = match &func.blocks[nb].term {
            Term::Return(vs) => vs.clone(),
            _ => continue,
        };
        match &site {
            Site::Single { .. } => {
                let rc = result_cell.unwrap();
                let v = match vs.first() {
                    Some(v) => *v,
                    None => {
                        let nil = func.insts.alloc(Inst::Const(Lit::Nil));
                        func.blocks[nb].insts.push(nil);
                        nil
                    }
                };
                let st = func.insts.alloc(Inst::Store { cell: rc, value: v });
                func.blocks[nb].insts.push(st);
            }
            Site::Multi { dests, .. } => {
                for (dest, rv) in dests.iter().zip(vs.iter()) {
                    let st = func.insts.alloc(Inst::Store { cell: *dest, value: *rv });
                    func.blocks[nb].insts.push(st);
                }
            }
        }
        func.blocks[nb].term = Term::Goto(cont);
    }
    let _ = inst_map;

    // New top-level blocks: the REACHABLE clones (cand.reachable mapped through
    // the clone) + the continuation. Dead cloned blocks are unreachable and must
    // NOT enter the top-level set (the old full re-scan never visited them).
    let mut added: Vec<Idx<Block>> = cand.reachable.iter().map(|b| block_map[b]).collect();
    added.push(cont);
    added
}

/// Inline eligible direct calls in `func` (an ordinary, non-coroutine caller).
/// Drives [`find_site`] -> [`splice`] to a fixpoint, bounded by the per-caller
/// budget. `self_ref` is the caller's own [`FuncRef`] (so a self-call is never
/// inlined). Run BEFORE `cleanup`.
///
/// Returns the set of callee [`FuncRef`]s spliced in — the transitive closure of
/// bodies whose source now lives in `func`'s emitted chunk (the driver visits
/// each, including ones the inlined bodies themselves introduced). These are the
/// caller chunk's INLINE codegen dependencies: incremental emit must re-emit
/// `func` when any of them change (`docs/skull-incremental-emit-plan.md`).
pub fn inline_functions(
    ws: &Workspace,
    scx: &ScalarCtx,
    self_ref: FuncRef,
    func: &mut MirFunc,
    cache: &mut InlineCache,
) -> (HashSet<FuncRef>, Vec<SpliceRecord>) {
    let mut spliced = HashSet::default();
    let mut records: Vec<SpliceRecord> = Vec::new();
    // Kill-switch for perf attribution / bisecting a suspected inline bug.
    if std::env::var("SKULL_INLINE_OFF").is_ok() {
        return (spliced, records);
    }
    // The caller's top-level blocks (excludes closure sub-CFGs), maintained
    // incrementally so neither find_site nor the loop recomputes reachability.
    let mut top: HashSet<Idx<Block>> = top_level_blocks(func).into_iter().collect();
    // Resume cursor: the position of the last splice. No eligible call can appear
    // before it, so the next scan starts there (turns the per-splice full re-scan
    // from O(splices x size) into a single forward sweep).
    let mut resume: Option<(Idx<Block>, usize)> = None;
    let mut budget = MAX_INLINES_PER_FN;
    while budget > 0 {
        let Some(site) = find_site(func, ws, scx, cache, self_ref, &top, resume) else {
            break;
        };
        let pos = site.pos();
        let fref = site.fref();
        // Re-borrow the candidate immutably for the splice (distinct from `func`).
        let cand = cache.cands[&fref].as_ref().expect("site implies a cached candidate");
        // SAFETY-of-borrows: `splice` mutates `func`, reads `cand` (in `cache`);
        // they are different objects. The borrow checker can't see that through
        // the `&mut cache` in `find_site`, so split the lookup out here where
        // only `&cache` is held.
        records.push(SpliceRecord {
            callee: fref,
            straight: cand.kind == Kind::Straight,
            forced: cand.forced,
            callee_insts: cand.insts,
        });
        let added = splice(func, site, cand);
        top.extend(added);
        spliced.insert(fref);
        resume = Some(pos);
        budget -= 1;
    }
    (spliced, records)
}

#[cfg(test)]
mod tests {
    use super::annotation_gate;

    #[test]
    fn annotation_gate_default_annotated_only() {
        // Default (size_heuristic=false): ONLY @inline / @pure inline, both force.
        assert_eq!(annotation_gate(false, false, false), None);       // plain -> NOT inlined
        assert_eq!(annotation_gate(true, false, false), Some(true));  // @inline -> forced
        assert_eq!(annotation_gate(false, true, false), Some(true));  // @pure -> forced
        assert_eq!(annotation_gate(true, true, false), Some(true));
    }

    #[test]
    fn annotation_gate_size_heuristic_escape_hatch() {
        // SKULL_INLINE_SIZE_HEURISTIC: old policy -- any fn eligible, @inline forces.
        assert_eq!(annotation_gate(false, false, true), Some(false)); // plain -> eligible, not forced
        assert_eq!(annotation_gate(true, false, true), Some(true));   // @inline -> forced
        assert_eq!(annotation_gate(false, true, true), Some(false));  // @pure inert in this mode
        assert_eq!(annotation_gate(true, true, true), Some(true));
    }
}
