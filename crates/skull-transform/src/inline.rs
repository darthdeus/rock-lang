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
use std::cell::Cell as ThreadCell;

use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{BoundTree, CompilerError};
use comfy_la_arena::Idx;
use skull_mir::concrete::ConcretePlans;
use skull_mir::instance::Instance;
use skull_mir::lower::{BackendFnAbiPlan, ScalarCtx};
use skull_mir::mir::*;
use ustr::Ustr;

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

thread_local! {
    static INLINE_OFF: ThreadCell<bool> = const { ThreadCell::new(false) };
}

pub struct InlineOffGuard {
    previous: bool,
}

pub fn inline_off_guard() -> InlineOffGuard {
    let previous = INLINE_OFF.with(|off| {
        let previous = off.get();
        off.set(true);
        previous
    });
    InlineOffGuard { previous }
}

impl Drop for InlineOffGuard {
    fn drop(&mut self) {
        INLINE_OFF.with(|off| off.set(self.previous));
    }
}

fn inline_disabled() -> bool {
    INLINE_OFF.with(|off| off.get()) || std::env::var_os("SKULL_INLINE_OFF").is_some()
}

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
    /// The concrete body this snapshot was lowered from. A declaration-level
    /// snapshot is not a valid source for a concrete splice.
    subject: Instance,
    /// The one ABI plan both the snapshot and its call site must use.
    abi: BackendFnAbiPlan,
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
    /// Param cells this callee NEVER whole-`Store`s (only reads / `Field` /
    /// `StoreField`). Their arg can be COALESCED — an arg that is a plain
    /// `Load(c)` binds the cloned param directly to the caller cell `c` instead
    /// of copying `Store(param, Load(c))`. Removing the whole-cell copy is what
    /// lets a mutated-by-ref cursor struct stay a SINGLE cell so
    /// `scalarize_struct_cells` (SROA) can peel it (the zero-alloc iterator
    /// path). Sound because field-writeback through a by-ref arg is already the
    /// language semantics, and a param the callee reassigns whole is excluded
    /// (aliasing it to the caller cell would clobber the caller's variable).
    coalescable_params: Vec<bool>,
}

/// One splice the inliner performed, for the `skull-inline-stats` diagnostic
/// (`inline_functions_with_plans` returns these alongside its dependency set). Records the
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
/// Cache key: the callee [`Instance`] -- the same concrete-body identity the
/// chunk cache and both emitters key on. Each distinct instantiation gets its
/// own snapshot, so compile cost scales with element types used, not call sites.
type CacheKey = Instance;

#[derive(Default)]
pub struct InlineCache {
    cands: HashMap<CacheKey, Option<CalleeBody>>,
}

impl InlineCache {
    /// Drop ALL cached candidates for `fref` (the shared snapshot AND every
    /// specialized one) so the next lookup re-lowers and re-classifies. A fref's
    /// cached body goes stale iff its own body changed OR a fn it calls flipped
    /// scalar ABI -- both are within the emit's `affected` set, which the
    /// incremental driver invalidates wholesale. A body edit must also drop the
    /// specialized snapshots keyed off it, so this matches on the `fref` component.
    pub fn invalidate(&mut self, fref: FuncRef) {
        self.cands.retain(|inst, _| inst.func != fref);
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
fn classify(
    ws: &Workspace,
    scx: &ScalarCtx,
    inst: &Instance,
    concrete_plans: &ConcretePlans,
) -> Result<Option<CalleeBody>, CompilerError> {
    let fref = inst.func;
    let f = &ws.module(fref.module).functions[fref.func];
    if f.is_extern || f.inline_body.is_some() || f.is_coroutine {
        return Ok(None);
    }
    let has_inline = f.annotations.iter().any(|a| a.name.name.as_str() == "inline");
    let has_pure = f.annotations.iter().any(|a| a.name.name.as_str() == "pure");
    // An `Iterator`-bounded body carries placeholder calls until a concrete
    // splice or standalone demand resolves them, so inline eligibility forces
    // the optimization when the body is otherwise eligible.
    let has_bounds = f.generic_bounds.iter().any(Option::is_some);
    let force = match annotation_gate(has_inline, has_pure, size_heuristic()) {
        Some(force) => force || has_bounds,
        None if has_bounds => true,
        None => return Ok(None),
    };
    // A snapshot lowers the body as if hand-written for the concrete
    // instantiation (`docs/flatvec-plan.md`); a non-generic instance's is the
    // declaration's own body.
    let Some(plan) = concrete_plans.get(inst) else { return Ok(None) };
    let abi = BackendFnAbiPlan::for_concrete_plan(ws, scx, plan);
    let mut mf = skull_mir::lower::lower_instance_with_plans(ws, scx, plan, concrete_plans)?;
    // Cleanup first so the size gate reflects emitted size and the splice clones
    // a tight body (the caller re-runs cleanup over the whole result anyway).
    crate::cleanup(&mut mf)?;

    // Structural exclusions over the whole arena.
    let mut has_lambda = false;
    for (_, inst) in mf.insts.iter() {
        match inst {
            Inst::Lambda { .. } => {
                // A lambda's sub-CFG + captured cells only clone through the
                // block-cloning General splice. A bounded fn has no standalone
                // emission, so refusing it would leave the caller a residual
                // bounded call (an emit error); force it through General below.
                // Non-bounded fns keep the v0 refusal (they have a real
                // emission to fall back on).
                if !has_bounds {
                    return Ok(None);
                }
                has_lambda = true;
            }
            Inst::Call { func, .. } | Inst::CallMulti { func, .. } if *func == fref => {
                return Ok(None); // self-recursive
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
            Term::Unreachable | Term::Yield { .. } => return Ok(None),
            _ => {}
        }
    }
    let reachable_insts: usize = reachable.iter().map(|&b| mf.blocks[b].insts.len()).sum();

    // A param is coalescable iff nothing in the body rebinds its cell WHOLE (a
    // `Store` to it, or defining it as a multi-return dest / map cursor). Reads
    // and `Field`/`StoreField` don't count — those are exactly what coalescing
    // preserves (field-writeback through the shared cell).
    let mut rebound: HashSet<Idx<Cell>> = HashSet::default();
    for (_, inst) in mf.insts.iter() {
        match inst {
            Inst::Store { cell, .. } => {
                rebound.insert(*cell);
            }
            Inst::CallMulti { dests, .. }
            | Inst::CallValueMulti { dests, .. }
            | Inst::CallBoundMulti { dests, .. } => {
                rebound.extend(dests.iter().copied());
            }
            Inst::MapNext { key_cell, val_cell, .. } => {
                rebound.insert(*key_cell);
                rebound.insert(*val_cell);
            }
            _ => {}
        }
    }
    let coalescable_params: Vec<bool> =
        mf.params.iter().map(|p| !rebound.contains(p)).collect();

    // A lambda-bearing body must take the General splice even when its entry
    // ends in Return: the straight path clones no blocks, so the lambda's
    // sub-CFG (referenced by `Inst::Lambda { entry }`) would dangle.
    if !has_lambda && matches!(mf.blocks[mf.entry].term, Term::Return(_)) {
        // Straight: entry ends in Return, so it is the only reachable block.
        if !force && reachable_insts > STRAIGHT_INSTS {
            return Ok(None);
        }
        let ret_arity = match &mf.blocks[mf.entry].term {
            Term::Return(vs) => vs.len(),
            _ => unreachable!(),
        };
        Ok(Some(CalleeBody {
            func: mf,
            subject: inst.clone(),
            abi,
            kind: Kind::Straight,
            ret_arity,
            reachable: Vec::new(),
            insts: reachable_insts,
            forced: force,
            coalescable_params,
        }))
    } else {
        // General: uniform return arity across every reachable Return.
        let mut arities = reachable
            .iter()
            .filter_map(|&b| match &mf.blocks[b].term {
                Term::Return(vs) => Some(vs.len()),
                _ => None,
            });
        let Some(ret_arity) = arities.next() else { return Ok(None) }; // no Return -> can't bind a result
        if !arities.all(|a| a == ret_arity) {
            return Ok(None);
        }
        if !force && reachable_insts > GENERAL_INSTS {
            return Ok(None);
        }
        Ok(Some(CalleeBody {
            func: mf,
            subject: inst.clone(),
            abi,
            kind: Kind::General,
            ret_arity,
            reachable,
            insts: reachable_insts,
            forced: force,
            coalescable_params,
        }))
    }
}

/// Get (lowering+classifying on first sight) the candidate for `inst`.
fn get<'a>(
    cache: &'a mut InlineCache,
    ws: &Workspace,
    scx: &ScalarCtx,
    inst: &Instance,
    concrete_plans: &ConcretePlans,
) -> Result<Option<&'a CalleeBody>, CompilerError> {
    if !cache.cands.contains_key(inst) {
        let v = classify(ws, scx, inst, concrete_plans)?;
        cache.cands.insert(inst.clone(), v);
    }
    Ok(cache.cands[inst].as_ref())
}

/// One resolved, eligible call site in the caller, extracted before the splice.
/// `inst` is the concrete body it targets, read from the caller's
/// `callee_instances`; a delivery record is only checked against it when one
/// exists. A missing generic owner leaves the call residual rather than guessing
/// a declaration-level body.
enum Site {
    /// `Inst::Call` — single (or void) result; `call_id` is the call's value.
    Single {
        bid: Idx<Block>,
        idx: usize,
        call_id: Idx<Inst>,
        args: Vec<Idx<Inst>>,
        inst: Instance,
        abi: BackendFnAbiPlan,
    },
    /// `Inst::CallMulti` — N leaves bound positionally into `dests`.
    Multi {
        bid: Idx<Block>,
        idx: usize,
        call_id: Idx<Inst>,
        dests: Vec<Idx<Cell>>,
        args: Vec<Idx<Inst>>,
        inst: Instance,
        abi: BackendFnAbiPlan,
    },
}

impl Site {
    fn inst(&self) -> &Instance {
        match self {
            Site::Single { inst, .. } | Site::Multi { inst, .. } => inst,
        }
    }
    fn fref(&self) -> FuncRef {
        self.inst().func
    }

    fn abi(&self) -> &BackendFnAbiPlan {
        match self {
            Site::Single { abi, .. } | Site::Multi { abi, .. } => abi,
        }
    }
    fn pos(&self) -> (Idx<Block>, usize) {
        match self {
            Site::Single { bid, idx, .. } | Site::Multi { bid, idx, .. } => (*bid, *idx),
        }
    }
    fn call_id(&self) -> Idx<Inst> {
        match self {
            Site::Single { call_id, .. } | Site::Multi { call_id, .. } => *call_id,
        }
    }
}

/// Resolve a lowered call's authoritative concrete owner. The delivery map is
/// only an agreement check: it is absent for valid residual concrete calls and
/// must never be used to invent the candidate identity.
fn call_target(
    func: &MirFunc,
    call_id: Idx<Inst>,
    fref: FuncRef,
    ws: &Workspace,
) -> Result<Option<(Instance, BackendFnAbiPlan)>, CompilerError> {
    let Some(owner) = func.callee_instances.get(&call_id) else {
        return Ok(None);
    };
    if skull_mir::lower::canonical_variant(ws, owner.func)
        != skull_mir::lower::canonical_variant(ws, fref)
    {
        return Err(CompilerError::new_no_location(format!(
            "internal: call in `{}` records owner `{}` but targets another declaration",
            func.name,
            ws.module(owner.func.module).functions[owner.func.func].name,
        )));
    }
    let declaration = &ws.module(fref.module).functions[fref.func];
    if declaration.generics.is_empty() {
        if !owner.args.is_empty() {
            return Err(CompilerError::new_no_location(format!(
                "internal: non-generic call target `{}` carries type arguments",
                declaration.name,
            )));
        }
    } else if owner.args.is_empty() {
        return Ok(None);
    }
    let inst = owner.clone();
    if let Some(delivery) = func.spec_bindings.get(&call_id) {
        if delivery != &inst {
            return Err(CompilerError::new_no_location(format!(
                "internal: delivery instance for `{}` disagrees with its concrete call owner",
                declaration.name,
            )));
        }
    }
    let abi = func.call_abi_plans.get(&call_id).cloned().ok_or_else(|| {
        CompilerError::new_no_location(format!(
            "internal: concrete call to `{}` has no lowered ABI plan",
            declaration.name,
        ))
    })?;
    Ok(Some((inst, abi)))
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
    concrete_plans: &ConcretePlans,
) -> Result<Option<Site>, CompilerError> {
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
                Inst::Call { func: fref, args } if *fref != self_ref => {
                    let Some((inst, abi)) = call_target(func, iid, *fref, ws)? else {
                        continue;
                    };
                    Site::Single { bid, idx, call_id: iid, args: args.clone(), inst, abi }
                }
                Inst::CallMulti { dests, func: fref, args } if *fref != self_ref => {
                    let Some((inst, abi)) = call_target(func, iid, *fref, ws)? else {
                        continue;
                    };
                    Site::Multi {
                        bid,
                        idx,
                        call_id: iid,
                        dests: dests.clone(),
                        args: args.clone(),
                        inst,
                        abi,
                    }
                }
                _ => continue,
            };
            let cell_count = func.cells.len();
            match get(cache, ws, scx, site.inst(), concrete_plans)? {
                // Decline a splice that would breach the per-caller cell budget
                // (keep scanning — a smaller candidate may still fit). A concrete
                // call with a scalarized ABI is exempt: its args are splatted to
                // leaves, so a residual opaque delivery would be invalid -- it
                // must splice, and eligibility is a subset of `classify` so a
                // candidate always exists. Declining it would leave a residual
                // miscompile.
                Some(cand)
                    if cand.subject == *site.inst()
                        && cand.abi == *site.abi()
                        && cand.abi.result_lanes() == site.abi().result_lanes()
                        && arity_ok(cand, &site)
                        && (!site.inst().args.is_empty()
                            || cell_count + cand.func.cells.len() <= CELL_BUDGET) =>
                {
                    return Ok(Some(site))
                }
                _ => {}
            }
        }
    }
    Ok(None)
}

/// Clone the callee's cells + insts into `caller` (NOT blocks — only the straight
/// path uses this; straight candidates are lambda-free so no inst references a
/// block). Returns `(inst_map, cell_map)`.
fn clone_cells_insts(
    caller: &mut MirFunc,
    callee: &MirFunc,
    overrides: &HashMap<Idx<Cell>, Idx<Cell>>,
) -> (HashMap<Idx<Inst>, Idx<Inst>>, HashMap<Idx<Cell>, Idx<Cell>>) {
    let mut cell_map = HashMap::default();
    cell_map.reserve(callee.cells.len());
    for (cid, cell) in callee.cells.iter() {
        // A coalesced param maps to the caller cell its arg loads (no fresh copy).
        let mapped = overrides.get(&cid).copied().unwrap_or_else(|| caller.cells.alloc(cell.clone()));
        cell_map.insert(cid, mapped);
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
    overrides: &HashMap<Idx<Cell>, Idx<Cell>>,
) -> (
    HashMap<Idx<Inst>, Idx<Inst>>,
    HashMap<Idx<Cell>, Idx<Cell>>,
    HashMap<Idx<Block>, Idx<Block>>,
) {
    let mut cell_map = HashMap::default();
    cell_map.reserve(callee.cells.len());
    for (cid, cell) in callee.cells.iter() {
        // A coalesced param maps to the caller cell its arg loads (no fresh copy).
        let mapped = overrides.get(&cid).copied().unwrap_or_else(|| caller.cells.alloc(cell.clone()));
        cell_map.insert(cid, mapped);
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
    caller
        .for_iter_headers
        .extend(callee.for_iter_headers.iter().map(|header| block_map[header]));
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
        Inst::CallMulti { dests, .. }
        | Inst::CallBoundMulti { dests, .. }
        | Inst::CallValueMulti { dests, .. } => {
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

/// For each coalescable param whose arg is a plain `Load(c)`, map the callee
/// param cell -> the caller cell `c` (so the clone binds the param directly to
/// it) and flag the param so the splice skips its `Store(param, arg)` copy.
/// Removing that whole-cell copy is what keeps a by-ref-mutated cursor struct a
/// SINGLE cell, which SROA can then scalarize (the zero-alloc iterator path).
fn coalesce_overrides(
    caller: &MirFunc,
    cand: &CalleeBody,
    args: &[Idx<Inst>],
) -> (HashMap<Idx<Cell>, Idx<Cell>>, Vec<bool>) {
    let callee = &cand.func;
    let mut overrides = HashMap::default();
    let mut coalesced = vec![false; callee.params.len()];
    for (i, (param, arg)) in callee.params.iter().zip(args.iter()).enumerate() {
        if !cand.coalescable_params.get(i).copied().unwrap_or(false) {
            continue;
        }
        // int->float subsumption is identity on luajit, so an `IntToFloat(Load c)`
        // arg coalesces to `c` exactly like a bare `Load c` (this pass runs only on
        // the Lua path; GLSL never inlines) -- otherwise the wrap would defeat
        // coalescing and perturb the emitted Lua.
        let arg_load = match caller.insts[*arg] {
            Inst::Load(c) => Some(c),
            Inst::IntToFloat(inner) => match caller.insts[inner] {
                Inst::Load(c) => Some(c),
                _ => None,
            },
            _ => None,
        };
        if let Some(c) = arg_load {
            overrides.insert(*param, c);
            coalesced[i] = true;
        }
    }
    (overrides, coalesced)
}

fn graft_resolved_bound_tree(
    tree: &skull_mir::bound::ResolvedBoundTree,
    incoming: &skull_mir::bound::ResolvedBoundTree,
) -> Result<skull_mir::bound::ResolvedBoundTree, CompilerError> {
    let mut out = skull_mir::bound::ResolvedBoundTree::default();
    for (&g, next) in &tree.next {
        let next = match next {
            skull_mir::bound::ResolvedBoundNext::Concrete { instance, sub } => {
                skull_mir::bound::ResolvedBoundNext::Concrete {
                    instance: instance.clone(),
                    sub: graft_resolved_bound_tree(sub, incoming)?,
                }
            }
            skull_mir::bound::ResolvedBoundNext::CallerGeneric(g) => {
                incoming.get(*g).cloned().ok_or_else(|| {
                    CompilerError::new_no_location(format!(
                        "internal: unresolved concrete `Iterator` bound (generic #{g}) while inlining"
                    ))
                })?
            }
        };
        out.next.insert(g, next);
    }
    Ok(out)
}

/// After cloning a callee body into the caller: substitute every cloned
/// placeholder [`Inst::CallBoundMulti`] with the concrete `next` from the
/// site's tree, and import the callee's own carried `bound_trees` (keys
/// remapped, `CallerGeneric` leaves grafted through the site tree) so the
/// fixpoint can keep inlining the now-concrete calls.
fn resolve_cloned_bound_calls(
    func: &mut MirFunc,
    callee: &MirFunc,
    inst_map: &HashMap<Idx<Inst>, Idx<Inst>>,
    site_tree: Option<&BoundTree>,
    site_resolved_tree: Option<&skull_mir::bound::ResolvedBoundTree>,
    ws: &Workspace,
    scx: &ScalarCtx,
    concrete_plans: &ConcretePlans,
) -> Result<(), CompilerError> {
    let empty = BoundTree::default();
    let n = site_tree.unwrap_or(&empty);
    let incoming = match site_resolved_tree {
        Some(tree) => tree.clone(),
        None if n.is_empty() => Default::default(),
        None => {
            return Err(CompilerError::new_no_location(
                "internal: concrete splice site has no resolved bound plan",
            ));
        }
    };
    let mut resolved_by_old: HashMap<Idx<Inst>, skull_mir::bound::ResolvedBoundTree> =
        HashMap::default();

    for old_id in callee.bound_trees.keys() {
        let new_id = inst_map[old_id];
        let resolved = callee.resolved_bound_trees.get(old_id).ok_or_else(|| {
            CompilerError::new_no_location(format!(
                "internal: concrete callee `{}` has a raw bound tree without its resolved plan",
                callee.name
            ))
        })?;
        let grafted = graft_resolved_bound_tree(resolved, &incoming)?;
        if !grafted.is_empty() {
            func.resolved_bound_trees.insert(new_id, grafted.clone());
            resolved_by_old.insert(*old_id, grafted);
        }
    }

    for (&old_id, &new_id) in inst_map {
        let Inst::CallBoundMulti { generic_idx, .. } = &func.insts[new_id] else {
            continue;
        };
        debug_assert!(matches!(callee.insts[old_id], Inst::CallBoundMulti { .. }));
        let g = *generic_idx as usize;
        let tree = resolved_by_old.get(&old_id).unwrap_or(&incoming);
        let Some(leaf) = tree.next.get(&g).cloned() else {
            return Err(CompilerError::new_no_location(format!(
                "internal: placeholder bound-call (generic #{g}) has no concrete \
                 resolution at this splice site"
            )));
        };
        if let skull_mir::bound::ResolvedBoundNext::Concrete { sub, .. } = &leaf {
            if !sub.is_empty() {
                func.resolved_bound_trees.insert(new_id, sub.clone());
            }
        }
        crate::bound::substitute_bound_call(
            func,
            new_id,
            callee.bound_next_split.contains(&old_id),
            &leaf,
            scx,
            ws,
            scx.interner(),
            concrete_plans,
            "at this splice site",
        )?;
    }
    if let Some((&old_id, &new_id)) = inst_map.iter().find(|(_, new_id)| {
        matches!(func.insts[**new_id], Inst::CallBoundMulti { .. })
    }) {
        return Err(CompilerError::new_no_location(format!(
            "internal: cloned bounded placeholder {:?} from `{}` survived concrete splice as {:?}",
            old_id, callee.name, new_id
        )));
    }
    Ok(())
}

fn collect_nested_names(nested: &[MirFunc], out: &mut Vec<Ustr>) {
    for nf in nested {
        out.push(nf.name);
        collect_nested_names(&nf.nested_fns, out);
    }
}

fn remap_nested_global(inst: &mut Inst, names: &HashMap<Ustr, Ustr>) {
    if let Inst::Global(name) = inst {
        if let Some(&new) = names.get(name) {
            *name = new;
        }
    }
}

fn rename_nested_tree(nested: &mut [MirFunc], names: &HashMap<Ustr, Ustr>) {
    for nf in nested {
        if let Some(&new) = names.get(&nf.name) {
            nf.name = new;
        }
        for (_, inst) in nf.insts.iter_mut() {
            remap_nested_global(inst, names);
        }
        nf.hoisted_fn_globals = nf
            .hoisted_fn_globals
            .iter()
            .map(|name| names.get(name).copied().unwrap_or(*name))
            .collect();
        rename_nested_tree(&mut nf.nested_fns, names);
    }
}

/// Carry named nested bodies across a splice. Their names are private globals,
/// so each splice needs a fresh namespace and every reference in the cloned
/// parent and child arenas must move with it.
fn import_nested_fns(
    func: &mut MirFunc,
    callee: &MirFunc,
    inst_map: &HashMap<Idx<Inst>, Idx<Inst>>,
) -> HashMap<Ustr, Ustr> {
    if callee.nested_fns.is_empty() {
        return HashMap::default();
    }
    let mut old_names = Vec::new();
    collect_nested_names(&callee.nested_fns, &mut old_names);
    let mut used = Vec::new();
    collect_nested_names(&func.nested_fns, &mut used);
    used.extend(func.hoisted_fn_globals.iter().copied());
    used.extend(old_names.iter().copied());
    let mut used: HashSet<Ustr> = used.into_iter().collect();
    let mut names = HashMap::default();
    let mut nonce = func.nested_fns.len();
    for old in old_names {
        if names.contains_key(&old) {
            continue;
        }
        loop {
            let new = Ustr::from(format!("{old}__inline{nonce}").as_str());
            nonce += 1;
            if used.insert(new) {
                names.insert(old, new);
                break;
            }
        }
    }
    for new_id in inst_map.values() {
        remap_nested_global(&mut func.insts[*new_id], &names);
    }
    let mut nested = callee.nested_fns.clone();
    rename_nested_tree(&mut nested, &names);
    func.nested_fns.extend(nested);
    names
}

/// Import the cloned callee body's `spec_bindings` into the caller (keys remapped
/// through the clone) so a specialized call INSIDE a spliced body is itself found
/// and specialized by the fixpoint -- the `spec_bindings` analogue of
/// `resolve_cloned_bound_calls` importing `bound_trees`.
///
/// Resolved bound trees are instruction-keyed too. They carry concrete leaves
/// from a prior splice or standalone substitution, so dropping one would make
/// the next splice re-derive a foreign tree from the caller's inference arena.
fn import_spec_bindings(
    func: &mut MirFunc,
    callee: &MirFunc,
    inst_map: &HashMap<Idx<Inst>, Idx<Inst>>,
    nested_names: &HashMap<Ustr, Ustr>,
) {
    for (old_id, key) in &callee.spec_bindings {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.spec_bindings.insert(new_id, key.clone());
        }
    }
    // The IDENTITY record travels with the body for the same reason the site
    // does: a spliced call still targets the instance the callee body named.
    for (old_id, inst) in &callee.callee_instances {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.callee_instances.insert(new_id, inst.clone());
        }
    }
    for (old_id, plan) in &callee.call_abi_plans {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.call_abi_plans.insert(new_id, plan.clone());
        }
    }
    for (old_id, plan) in &callee.call_rep_plans {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.call_rep_plans.insert(new_id, plan.clone());
        }
    }
    for (old_id, plan) in &callee.closure_env_plans {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.closure_env_plans.insert(new_id, plan.clone());
        }
    }
    for (old_id, tree) in &callee.resolved_bound_trees {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.resolved_bound_trees.insert(new_id, tree.clone());
        }
    }
    // A minted fn-value symbol travels with the body that mints it: the callee's
    // `let f = adder` is still a reference to `adder` after the splice, and the
    // census joins it there. Dropping it would leave a spelled callee symbol in
    // the caller with no recorded declaration -- exactly the fail-open the
    // record exists to prevent.
    for (old_id, fref) in &callee.fn_value_symbols {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.fn_value_symbols.insert(new_id, *fref);
        }
    }
    for n in &callee.hoisted_fn_globals {
        let n = nested_names.get(n).copied().unwrap_or(*n);
        if !func.hoisted_fn_globals.contains(&n) {
            func.hoisted_fn_globals.push(n);
        }
    }
    // The callee's instruction spans come across with its body, so a rejection
    // raised after inlining still points at the construct in the CALLEE's source
    // rather than at the call site that absorbed it.
    for (old_id, span) in callee.inst_spans.iter() {
        if let Some(&new_id) = inst_map.get(&old_id) {
            func.inst_spans.insert(new_id, *span);
        }
    }
    // Site identity travels with the body, and its OWNER travels with it: a
    // spliced call is the CALLEE instance's obligation, so the owner is
    // re-interned into the caller's table and the entry re-pointed. Copying the
    // `OwnerRef` verbatim would silently reinterpret the callee's index in the
    // caller's table -- index 0 there is the CALLER, so every spliced call
    // would join through the wrong body while looking perfectly well-formed.
    let mut owner_map: HashMap<u32, skull_mir::site::OwnerRef> = HashMap::default();
    for (old_id, cs) in &callee.call_sites {
        let Some(&new_id) = inst_map.get(old_id) else { continue };
        let owner = match owner_map.get(&cs.owner.0) {
            Some(&o) => o,
            None => {
                let Some(inst) = callee.site_owners.get(cs.owner.0 as usize) else { continue };
                let o = skull_mir::site::intern_owner(&mut func.site_owners, inst.clone());
                owner_map.insert(cs.owner.0, o);
                o
            }
        };
        func.call_sites.insert(new_id, skull_mir::site::CallSite { owner, site: cs.site.clone() });
    }
}

/// Splice candidate `cand` into `func` at `site`, removing the call. Returns the
/// new top-level (reachable) blocks the splice added, for the driver to fold into
/// its top-level set (straight adds none).
fn splice(
    func: &mut MirFunc,
    site: Site,
    cand: &CalleeBody,
    ws: &Workspace,
    scx: &ScalarCtx,
    concrete_plans: &ConcretePlans,
) -> Result<Vec<Idx<Block>>, CompilerError> {
    match cand.kind {
        Kind::Straight => {
            splice_straight(func, site, cand, ws, scx, concrete_plans)?;
            Ok(Vec::new())
        }
        Kind::General => splice_general(func, site, cand, ws, scx, concrete_plans),
    }
}

/// Straight splice: the callee is a single Return-terminated block. Inline its
/// insts into the caller block in place of the call; no new blocks (the callee
/// is lambda-free, so its insts reference no block). The caller block keeps its
/// own terminator.
fn splice_straight(
    func: &mut MirFunc,
    site: Site,
    cand: &CalleeBody,
    ws: &Workspace,
    scx: &ScalarCtx,
    concrete_plans: &ConcretePlans,
) -> Result<(), CompilerError> {
    let callee = &cand.func;
    let (bid, idx, args) = match &site {
        Site::Single { bid, idx, args, .. } | Site::Multi { bid, idx, args, .. } => (*bid, *idx, args.clone()),
    };
    let site_tree = func.bound_trees.remove(&site.call_id());
    let site_resolved_tree = func.resolved_bound_trees.remove(&site.call_id());
    let (overrides, coalesced) = coalesce_overrides(func, cand, &args);
    let (inst_map, cell_map) = clone_cells_insts(func, callee, &overrides);
    resolve_cloned_bound_calls(
        func,
        callee,
        &inst_map,
        site_tree.as_ref(),
        site_resolved_tree.as_ref(),
        ws,
        scx,
        concrete_plans,
    )?;
    let nested_names = import_nested_fns(func, callee, &inst_map);
    import_spec_bindings(func, callee, &inst_map, &nested_names);

    // Arg -> param-cell stores (coalesced params are aliased to the caller cell,
    // so they need no copy).
    let mut arg_stores = Vec::new();
    for (i, (param, arg)) in callee.params.iter().zip(args.iter()).enumerate() {
        if coalesced[i] {
            continue;
        }
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
    Ok(())
}

/// General splice: the callee has internal control flow. Split the caller block
/// at the call; the prefix arg-stores + `Goto` the cloned entry; a continuation
/// block holds the post-call tail. Each cloned `Return` binds its values and
/// `Goto`s the continuation.
fn splice_general(
    func: &mut MirFunc,
    site: Site,
    cand: &CalleeBody,
    ws: &Workspace,
    scx: &ScalarCtx,
    concrete_plans: &ConcretePlans,
) -> Result<Vec<Idx<Block>>, CompilerError> {
    let callee = &cand.func;
    let (bid, idx, args) = match &site {
        Site::Single { bid, idx, args, .. } | Site::Multi { bid, idx, args, .. } => (*bid, *idx, args.clone()),
    };
    let site_tree = func.bound_trees.remove(&site.call_id());
    let site_resolved_tree = func.resolved_bound_trees.remove(&site.call_id());
    let (overrides, coalesced) = coalesce_overrides(func, cand, &args);
    let (inst_map, cell_map, block_map) = clone_all(func, callee, &overrides);
    resolve_cloned_bound_calls(
        func,
        callee,
        &inst_map,
        site_tree.as_ref(),
        site_resolved_tree.as_ref(),
        ws,
        scx,
        concrete_plans,
    )?;
    let nested_names = import_nested_fns(func, callee, &inst_map);
    import_spec_bindings(func, callee, &inst_map, &nested_names);
    let entry_clone = block_map[&callee.entry];

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

    // Prefix: before | arg-stores; Goto the cloned entry. Coalesced params are
    // aliased to the caller cell (via `overrides`), so they need no copy.
    let mut prefix = before;
    for (i, (param, arg)) in callee.params.iter().zip(args.iter()).enumerate() {
        if coalesced[i] {
            continue;
        }
        let pc = cell_map[param];
        prefix.push(func.insts.alloc(Inst::Store { cell: pc, value: *arg }));
    }
    func.blocks[bid].insts = prefix;
    func.blocks[bid].term = Term::Goto(entry_clone);

    // Route each cloned TOP-LEVEL Return to the continuation, binding return
    // values. Only the callee's reachable blocks: a lambda sub-CFG's Return is
    // the LAMBDA's return, not the callee's, and must stay put.
    let cloned_blocks: Vec<Idx<Block>> = cand.reachable.iter().map(|b| block_map[b]).collect();
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
    Ok(added)
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
pub fn inline_functions_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    self_ref: FuncRef,
    func: &mut MirFunc,
    cache: &mut InlineCache,
    concrete_plans: &ConcretePlans,
) -> Result<(HashSet<FuncRef>, Vec<SpliceRecord>), CompilerError> {
    inline_functions_inner(ws, scx, self_ref, func, cache, concrete_plans)
}

fn inline_functions_inner(
    ws: &Workspace,
    scx: &ScalarCtx,
    self_ref: FuncRef,
    func: &mut MirFunc,
    cache: &mut InlineCache,
    concrete_plans: &ConcretePlans,
) -> Result<(HashSet<FuncRef>, Vec<SpliceRecord>), CompilerError> {
    let mut spliced = HashSet::default();
    let mut records: Vec<SpliceRecord> = Vec::new();
    // Kill-switch for perf attribution / bisecting a suspected inline bug.
    if inline_disabled() {
        return Ok((spliced, records));
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
        let Some(site) = find_site(
            func,
            ws,
            scx,
            cache,
            self_ref,
            &top,
            resume,
            concrete_plans,
        )? else {
            break;
        };
        let pos = site.pos();
        let fref = site.fref();
        // Re-borrow the candidate immutably for the splice (distinct from `func`).
        let cand = cache.cands[site.inst()]
            .as_ref()
            .expect("site implies a cached candidate");
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
        let added = splice(func, site, cand, ws, scx, concrete_plans)?;
        // The callee's `static` cells now live in THIS body: the spliced code
        // reads and writes them, and a `static` is one cell per owning fn, so
        // the cell is the same one every call site shares. Carrying them here is
        // the only way a cell whose owner has no standalone emission -- every
        // `Iterator`-bounded fn -- reaches the emitted program's registry.
        // Deduped below; one callee can be spliced at many sites.
        func.statics.extend(cand.func.statics.iter().cloned());
        top.extend(added);
        spliced.insert(fref);
        resume = Some(pos);
        budget -= 1;
    }
    // Most bodies own no statics at all, and this runs for every emitted fn --
    // so the set is only built when there is something that could duplicate.
    if func.statics.len() > 1 {
        let mut seen: HashSet<ustr::Ustr> = HashSet::default();
        func.statics.retain(|sc| seen.insert(sc.cell));
    }
    Ok((spliced, records))
}

/// Whether the fn declares any `Iterator` bound on its generics -- such a fn has
/// no standalone emission (it exists only inlined at its call sites).
pub fn fn_has_bounds(f: &bone_semantic::hir::Function) -> bool {
    f.generic_bounds.iter().any(Option::is_some)
}

/// Post-inline gate: a body about to be emitted must contain no placeholder
/// bound-call, and no residual call to a bounded fn that NAMES NOTHING.
///
/// A residual call is fine when the lowerer gave it a specialization: it spells
/// the specialized symbol (`callee_name` / `spec_fn_name`), and that definition
/// is emitted standalone from the demand this same call site raises, with its
/// placeholders resolved by the demand's tree
/// (`skull_transform::bound::resolve_standalone_bound_calls`). What is still a
/// rejection is a residual call the inliner did not splice AND the lowerer did
/// not specialize -- there is then no body anywhere that serves it.
///
/// Shared by the luajit driver (`skull::mir_emit`) and the JS closure emitter.
pub fn validate_no_residual_bound_calls(
    ws: &Workspace,
    f: &bone_semantic::hir::Function,
    mf: &MirFunc,
) -> Result<(), CompilerError> {
    // Walk block inst-lists, not the raw arena: a splice drops the consumed call
    // inst from its block but the arena still holds it.
    for (_, block) in mf.blocks.iter() {
        for &iid in &block.insts {
            match &mf.insts[iid] {
                Inst::CallBoundMulti { .. } => {
                    return Err(CompilerError::new_no_location(format!(
                        "`{}`: an `Iterator`-bounded call retained unresolved placeholders \
                         after neither concrete splicing nor standalone planning (is this a \
                         recursive bounded fn?)",
                        f.name
                    )));
                }
                Inst::Call { func, .. } | Inst::CallMulti { func, .. } => {
                    let callee = &ws.module(func.module).functions[func.func];
                    // A non-empty concrete owner is the admission: a bounded
                    // call without one would have no resolved standalone body.
                    let named = mf.spec_bindings.get(&iid).is_some_and(|i| !i.args.is_empty());
                    if fn_has_bounds(callee) && !named {
                        // Stage-0e: this rejection IS the ablation's
                        // unsupported-surface population, and it fires from
                        // production's own check rather than from
                        // `ablation_decline` -- so record it here or the
                        // taxonomy would report zero for the category with the
                        // largest real membership.
                        skull_mir::ablation::record(
                            skull_mir::ablation::FatalCategory::UnsupportedSurface,
                            callee.name.as_str(),
                            f.name.as_str(),
                            // The CALL's span. `callee.name_span` collapsed
                            // every site of one bounded callee onto its
                            // declaration, so the census reported 27 declaration
                            // spans as though they were call sites.
                            mf.inst_spans.get(iid).copied().unwrap_or(callee.name_span),
                            "an `Iterator`-bounded call that neither spliced nor specialized: \
                             the shared body cannot serve it (its placeholder `next` calls are \
                             resolved per instantiation), and no demand names a standalone copy \
                             (plan S 12)",
                        );
                        return Err(CompilerError::new_no_location(format!(
                            "`{}` calls `Iterator`-bounded `{}` in a context that cannot inline \
                             it, and the call was not specialized either -- so nothing serves \
                             it. A bounded body IS emitted standalone when a call demands one \
                             (plan S 12); what is missing here is the demand, which the lowerer \
                             raises only for an instantiation it specializes. Iterator \
                             combinators inside `co fn` bodies and recursive bounded fns are \
                             still the cases that hit this.",
                            f.name, callee.name
                        )));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ustr::Ustr;

    // The `loop_binding` provenance tag lives ON the cell (unfuck #37), so a
    // transform that clones cells carries it for free -- no side-table to remap.
    // This is what will keep a spliced loop counter per-iteration once the
    // lambda-callee refusal (`classify`, `Inst::Lambda -> None`) is lifted.
    #[test]
    fn loop_binding_tag_survives_cell_clone_through_splice() {
        let mut callee = MirFunc::new(Ustr::from("callee"));
        let counter = callee.new_cell(Some(Ustr::from("i")));
        callee.mark_loop_binding(counter);
        let plain = callee.new_cell(Some(Ustr::from("x")));

        let mut caller = MirFunc::new(Ustr::from("caller"));
        let (_, cell_map) = clone_cells_insts(&mut caller, &callee, &HashMap::default());

        assert!(
            caller.cells[cell_map[&counter]].loop_binding,
            "cloned loop-binding cell must keep its tag"
        );
        assert!(
            !caller.cells[cell_map[&plain]].loop_binding,
            "an ordinary cloned cell must not gain the tag"
        );
    }

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

    fn resolved_tree_count(mf: &MirFunc) -> (usize, usize) {
        let mut bodies = 1;
        let mut trees = mf.resolved_bound_trees.len();
        for nf in &mf.nested_fns {
            let (child_bodies, child_trees) = resolved_tree_count(nf);
            bodies += child_bodies;
            trees += child_trees;
        }
        (bodies, trees)
    }

    #[test]
    fn fresh_candidates_have_no_resolved_bound_trees_including_nested_bodies() {
        let mut modules = vec![
            skull_parser::parse_module_clean(
                r#"
@inline
fn outer(x: int) -> int {
    fn inner(y: int) -> int { return y }
    return inner(x)
}
"#,
                Ustr::from("<inline-test>"),
            )
            .expect("parse"),
        ];
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
        let mid = *ws.module_ids.get(&Ustr::from("<inline-test>")).expect("module");
        let (fid, _) = ws
            .module(mid)
            .functions
            .iter()
            .find(|(_, f)| f.name.as_str() == "outer")
            .expect("outer");
        let root = FuncRef { module: mid, func: fid };
        let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
        let graph = skull_graph::instance_graph::build_in(
            &ws,
            vec![root],
            false,
            std::sync::Arc::clone(&interner),
        );
        let concrete = skull_graph::concrete_body::build(&ws, &graph);
        let scx = ScalarCtx::new(&ws).sharing(interner);
        let plan = concrete
            .plans
            .get(&Instance::base(root))
            .expect("inline fixture concrete plan");
        let body = skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &concrete.plans)
            .expect("the inline fixture must lower");
        let (bodies, trees) = resolved_tree_count(&body);
        assert!(bodies > 1, "the candidate census did not include its nested body");
        assert_eq!(trees, 0, "fresh candidate lowering already carried resolved trees");
    }
}
