//! Coroutine transform: a `co fn` (lowered to a cell CFG with `Term::Yield`)
//! becomes an ordinary stackless state-machine `__step` function plus a
//! constructor. No LuaJIT
//! coroutines.
//!
//! The cell model SIMPLIFIES this vs the old SSA backend: **only cells cross an
//! await** (values are single-block), so there is NO value spill/reload and NO
//! "block-param value crossing a suspend" case. The frame (state-table fields)
//! is just cells — rendered as `s.<field>` by `skull-codegen-lua`'s `FrameCtx`,
//! so the transform doesn't even rewrite cell Load/Store; it only:
//!   1. turns each `Term::Yield { value, resume }` block into `s.__state = k;
//!      return value` (the `resume` block becomes a dispatch target),
//!   2. turns a normal `return v` into `s.__return = v; s.__state = -3; return`
//!      (-3 = completed normally, distinct from killed -1 / errored -2 so
//!      `completed()` can tell a natural finish from a kill; every runtime
//!      guard tests `< 0`, never an exact terminal). Both stamps (yield and
//!      completion) are guarded on `state >= 0`: a body that killed its own
//!      handle mid-run stays killed,
//!   3. rewrites `CoroSetInner(x)` (the `await` pseudo-op) to `s.__inner = x`,
//!   4. prepends a dispatch entry that branches on `s.__state` to the resume
//!      block for each state (falling through to the original entry for state 0).
//!
//! Frame = cells live at a resume point, plus parameters and cells retained by a
//! closure or detached lifecycle body. The constructor seeds only the declared
//! param fields.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use bone_semantic::inference::Workspace;
use bone_semantic::prelude::CompilerError;
use comfy_la_arena::{Idx, RawIdx};
use skull_mir::mir::*;
use ustr::Ustr;

pub mod bound;
mod closure;
mod forl;
mod inline;
mod mem2reg;
pub mod scope;
pub use closure::{FrameInfo, convert_closures, convert_closures_framed};
pub use forl::recover_counted_loops;
pub use inline::{
    InlineCache, InlineCandidateProfile, InlineCandidateTable, InlineCandidateTableStats,
    InlineCandidateWarm, InlineOffGuard, InlineScanStats, SpliceRecord, fn_has_bounds,
    inline_functions_with_plans, inline_off_active, inline_off_guard,
    validate_no_residual_bound_calls,
};
pub use mem2reg::{Mem2RegCensus, Mem2RegPromotion, mem2reg_census, promote_direct_cells};

pub const STATE: &str = "__state";

/// Every emitted implicit `nobox` materialization in `func` -- EVERY
/// one, not the first. A body with two sinks is two findings: reporting one
/// makes the second invisible until the first is fixed, so any "N sinks left"
/// count read off this check would be a floor rather than a total (the
/// `pass_uniforms` gap hid behind an earlier rejection in the same body exactly
/// this way).
pub fn nobox_materialization_violations(
    ws: &Workspace,
    func: &MirFunc,
) -> Vec<(String, Option<bone_semantic::prelude::Span>)> {
    let mut out = Vec::new();
    // Closure conversion retains cloned, unreachable parent/sibling blocks.
    // Follow the emission oracle, including unhoisted coroutine lambdas/hooks.
    for bid in skull_mir::reach::emitted_blocks(func) {
        let block = &func.blocks[bid];
        for &iid in &block.insts {
            let Inst::MakeValueStruct { name, .. } = &func.insts[iid] else {
                continue;
            };
            // A cdata ENUM packs through `MakeValueStruct` under the enum's own
            // name, which is correctly absent from `struct_by_name` -- `nobox` is
            // a struct marker, so those are genuinely out of scope. Any OTHER
            // unknown name is not a licence to skip: this rung enforces a
            // GUARANTEE, so it is reported rather than passed over.
            let Some(sid) = ws.struct_by_name.get(name) else {
                if skull_mir::lower::EnumPlan::for_name(ws, *name)
                    .is_some_and(|plan| plan.cdata().is_some())
                {
                    continue;
                }
                out.push((
                    format!(
                        "`{name}` is constructed as a value struct here but is neither a struct \
                         nor a cdata enum in the workspace, so the `nobox` guarantee cannot be \
                         checked for it"
                    ),
                    func.inst_spans.get(iid).copied(),
                ));
                continue;
            };
            if ws.modules[sid.module.index()].structs[sid.def].is_nobox {
                out.push((
                    format!(
                        "nobox value `{name}` survived lowering as an implicit materialization"
                    ),
                    func.inst_spans.get(iid).copied(),
                ));
            }
        }
    }
    out
}

/// Dead-code elimination over a [`MirFunc`] (a CFG->CFG cleanup, run before emit).
/// Removes, to a fixpoint: (a) pure value-producing insts with no users — the
/// dead `Load`/`Const`/arith/`Field` left by the cell-model's literal lowering;
/// (b) `Store` to a cell that is never `Load`ed anywhere — the dead result cells
/// of statement-position control flow. Side-effecting insts (`Call`/`CallValue`/
/// `CallMulti`/`StoreField`/`StoreIndex`/`StoreGlobal`/`MapNext`/`ArrayPush`/
/// coroutine ops) are always kept (an unused call result still runs the call).
/// This shrinks LuaJIT's snapshot width (fewer live locals at a side-exit), the
/// named trigger of skull's trace-cache thrash. Scans ALL blocks (incl. closure
/// sub-CFGs in the same arena), so a value/cell used only inside a lambda body
/// is correctly seen as live.
pub fn dce(func: &mut MirFunc) {
    dce_core(func, None);
}

/// DCE, optionally fusing a copy-prop analysis into the count scan: with [`CopyProp`]
/// the value operands (and each terminator's) are remapped through its
/// substitution as they are counted, and its proven no-op stores are seeded
/// dead -- so DCE's post-substitution reference counts and the store deletion
/// fall out of work DCE needs anyway. With `None` it is plain DCE.
fn dce_core(func: &mut MirFunc, cp: Option<&CopyProp>) {
    let ni = func.insts.len();
    let nc = func.cells.len();
    // Reference counts over the CURRENT program (dense, raw-id keyed): live value
    // uses per inst, `Load(c)` count per cell, and stores per cell (for the CSR
    // index below). A value/store dies when its count reaches 0; removing an inst
    // decrements the counts of what it referenced, cascading via a worklist — so
    // the load->store->value chains the cell model leaves converge in ONE pass
    // instead of the old fixpoint's ~2.2 full re-scans.
    let mut use_count = vec![0u32; ni];
    let mut load_count = vec![0u32; nc];
    let mut store_n = vec![0u32; nc];
    {
        // Split the `&mut MirFunc` into independent field borrows so a `Store`
        // value can be remapped in `insts` while iterating `blocks` (a term is
        // remapped through `&mut b.term`). Cells/opcodes are never rewritten, so
        // the Load/Store accounting below is unaffected by the substitution.
        let MirFunc { blocks, insts, .. } = &mut *func;
        for (_, b) in blocks.iter_mut() {
            for &i in &b.insts {
                if let Some(cp) = cp {
                    remap_inst(&mut insts[i], &|x| resolve_subst(&cp.subst, x));
                }
                insts[i].uses_each(&mut |op| use_count[raw(op) as usize] += 1);
                match &insts[i] {
                    Inst::Load(c) => load_count[raw(*c) as usize] += 1,
                    Inst::Store { cell, .. } => store_n[raw(*cell) as usize] += 1,
                    _ => {}
                }
            }
            if let Some(cp) = cp {
                remap_term(&mut b.term, &|x| resolve_subst(&cp.subst, x));
            }
            // Terminator operands are permanent uses (terms are never removed), so
            // their +1 is never decremented — a value used by a term stays live.
            b.term.uses_each(&mut |v| use_count[raw(v) as usize] += 1);
        }
    }
    // Compact templates summarize these loads instead of cloning their dead blocks.
    for &cell in &func.inline_cleanup_load_cells {
        load_count[raw(cell) as usize] = load_count[raw(cell) as usize].saturating_add(1);
    }
    // CSR: stores grouped by cell, so a cell whose last `Load` is removed cascades
    // its now-dead stores in O(stores-of-that-cell).
    let mut store_off = vec![0u32; nc + 1];
    for c in 0..nc {
        store_off[c + 1] = store_off[c] + store_n[c];
    }
    let mut store_list = vec![0u32; store_off[nc] as usize];
    let mut fill = store_off.clone();
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            if let Inst::Store { cell, .. } = &func.insts[i] {
                let c = raw(*cell) as usize;
                store_list[fill[c] as usize] = raw(i);
                fill[c] += 1;
            }
        }
    }

    // Seed the worklist with everything dead in the initial program, then cascade;
    // a store proven a no-op by the copy-prop analysis is dead whatever its cell's
    // load count.
    let mut dead = vec![false; ni];
    let mut work: Vec<Idx<Inst>> = Vec::new();
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            if is_dead_rc(&func.insts[i], i, &use_count, &load_count)
                || cp.is_some_and(|cp| {
                    cp.self_stores[raw(i) as usize]
                        || (cp.nonthrowing[raw(i) as usize] && use_count[raw(i) as usize] == 0)
                })
            {
                dead[raw(i) as usize] = true;
                work.push(i);
            }
        }
    }
    while let Some(i) = work.pop() {
        // Each value operand of the removed inst loses a use; a value that hits
        // zero (and is pure) is itself now dead.
        func.insts[i].uses_each(&mut |op| {
            let k = raw(op) as usize;
            use_count[k] = use_count[k].saturating_sub(1);
            if !dead[k]
                && use_count[k] == 0
                && (is_dead_value(&func.insts[op]) || cp.is_some_and(|cp| cp.nonthrowing[k]))
            {
                dead[k] = true;
                work.push(op);
            }
        });
        // Removing a `Load` drops its cell's load count; a cell with no remaining
        // loads makes its stores dead (their value operands then cascade on pop).
        if let Inst::Load(c) = &func.insts[i] {
            let ck = raw(*c) as usize;
            load_count[ck] = load_count[ck].saturating_sub(1);
            if load_count[ck] == 0 {
                for si in store_off[ck]..store_off[ck + 1] {
                    let s = Idx::from_raw(RawIdx::from_u32(store_list[si as usize]));
                    if !dead[raw(s) as usize] {
                        dead[raw(s) as usize] = true;
                        work.push(s);
                    }
                }
            }
        }
    }

    drop_flagged(func, &dead);
}

/// Remove every instruction whose `dead` flag is set from its block.
fn drop_flagged(func: &mut MirFunc, dead: &[bool]) {
    let block_ids: Vec<Idx<Block>> = func.blocks.iter().map(|(id, _)| id).collect();
    for bid in block_ids {
        let mut insts = std::mem::take(&mut func.blocks[bid].insts);
        insts.retain(|&i| !dead[raw(i) as usize]);
        func.blocks[bid].insts = insts;
    }
}

/// Whether an `Index` read must execute even when its result is unused -- i.e.
/// whether the bounds check rides ON this inst. Every ordinary container index
/// errors on OOB, so it does.
///
/// LEAF reads are the exception: one ELEMENT access is decomposed into K leaf
/// reads sharing ONE hoisted check (`__nb_checked_index` for a NativeBuffer;
/// `Inst::VecCheckedIndex` for columns and strided rows -- "one call per
/// ELEMENT access, not per column"). Each leaf read is then a raw pointer read
/// carrying no error of its own, so reading one field of a K-leaf element would
/// otherwise emit all K loads and discard K-1. The shared check itself stays
/// live, so an element read whose leaves are all unused still raises.
///
/// Consulted by BOTH [`is_dead_rc`] (initial seeding) and [`is_dead_value`]
/// (cascade). They must agree: a kind dead in one and live in the other either
/// leaks the loads back or deletes a real bounds error.
fn index_read_is_checked(kind: &ContainerKind) -> bool {
    !matches!(
        kind,
        ContainerKind::NativeBufferLeaf | ContainerKind::VecColumn | ContainerKind::VecStrideSlot
    )
}

/// Whether a value-producing inst is pure enough that an unused result is dead.
/// `Call`/`CallValue` run for effect; a checked `Index` must still execute.
fn is_dead_value(inst: &Inst) -> bool {
    match inst {
        Inst::Index { kind, .. } => !index_read_is_checked(kind),
        _ => inst.produces_value() && !skull_mir::sinking::may_throw(inst),
    }
}

/// The pre-emit MIR cleanup: copy-propagate intra-block loads, then DCE the dead
/// loads/values/stores they leave (including the stores copy propagation proves
/// write back the value their cell already holds). Run per-function in `mir_emit`.
///
/// The two passes are FUSED: copy-prop's substitution is applied as DCE's count
/// scan walks each inst, so the value-operand remap (`remap_inst` over every
/// inst) and the use-counting share one traversal instead of two back-to-back.
/// The standalone `dce` keeps its independent behavior.
pub fn cleanup(func: &mut MirFunc) -> Result<(), CompilerError> {
    cleanup_in(None, func)
}

fn cleanup_in(ws: Option<&Workspace>, func: &mut MirFunc) -> Result<(), CompilerError> {
    scalarize_native_buffer_stores(func);
    // SROA and copy-prop/DCE INTERLEAVE to a fixpoint: peeling a cursor struct
    // leaves its (now-dead) ctor behind, and that ctor's operand uses pin the
    // NEXT nesting level's cell as escaping (iterator combinator chains nest
    // cursors: MapIter{__inner: VecIter{..}}). Sweeping between rounds lets
    // each level peel; depth = chain nesting, so this converges fast. The ctor
    // store prefilter avoids allocating the per-cell analysis for no-op rounds.
    let dbg = std::env::var("SROA_DEBUG").is_ok();
    let mut round = 0;
    loop {
        if dbg {
            eprintln!("--- SROA round {round} input ({}) ---\n{func}", func.name);
            round += 1;
        }
        let changed = if has_scalarizable_store(func) {
            scalarize_struct_cells_once(func)?
        } else {
            false
        };
        // Inside the loop, because SROA is what CREATES the shape: forwarding a
        // nullable value field's read to the literal that filled it leaves a
        // presence test whose operand is now a fresh ctor.
        let folded = fold_never_nil_compares(func);
        let cp = copy_propagate_analysis(func, ws);
        dce_core(func, cp.as_ref());
        if !changed && !folded {
            break;
        }
    }
    if dbg {
        eprintln!("--- SROA final ({}) ---\n{func}", func.name);
    }
    if fold_const_branches(func) {
        dce(func);
    }
    Ok(())
}

/// A store through a NativeBuffer's `float` leaf view converts to f32 itself,
/// so an `f32` write's explicit rounding feeding it is redundant: an inlined
/// constructor's per-leaf rounding meets the buffer store here.
fn drop_f32_rounding_into_float_views(func: &mut MirFunc) -> bool {
    let float_view = skull_mir::lower::leaf_view_field("float");
    let mut changed = false;
    let insts: Vec<Idx<Inst>> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .collect();
    for i in insts {
        let Inst::StoreIndex {
            base,
            value,
            kind: ContainerKind::NativeBufferLeaf,
            ..
        } = func.insts[i]
        else {
            continue;
        };
        if !matches!(func.insts[base], Inst::Field { field, .. } if field == float_view) {
            continue;
        }
        if let Inst::FloatToF32(unrounded) = func.insts[value] {
            if let Inst::StoreIndex { value, .. } = &mut func.insts[i] {
                *value = unrounded;
                changed = true;
            }
        }
    }
    changed
}

fn has_scalarizable_store(func: &MirFunc) -> bool {
    func.blocks.iter().any(|(_, b)| {
        b.insts.iter().any(|&i| {
            matches!(
                &func.insts[i],
                Inst::Store { value, .. }
                    if matches!(
                        &func.insts[*value],
                        Inst::MakeValueStruct { .. }
                            | Inst::MakeStruct { .. }
                            | Inst::MakeEnum { .. }
                            | Inst::MakeEnumDyn { .. }
                            | Inst::MakeTuple(_)
                    )
            )
        })
    })
}

/// Fold a nil test whose operand is a freshly CONSTRUCTED aggregate: a ctor result
/// is never nil, so `Eq(ctor, nil)` is statically false and `NotEq` statically true.
/// [`fold_const_branches`] then turns the branch into a `Goto` and DCE removes the
/// unreachable arm along with the boxes only it used.
///
/// This exists because SROA manufactures the shape rather than the lowering emitting
/// it: a presence-aware read of a nullable value field tests `Inst::Field` against
/// nil, which is a real test, and forwarding that field read to the literal behind
/// it turns the operand into a ctor. 18 generated shapes boxed a value twice and
/// branched on the result purely for this, and the value oracle read every one as
/// CORRECT because the ANSWER was right.
fn fold_never_nil_compares(func: &mut MirFunc) -> bool {
    let never_nil = |func: &MirFunc, v: Idx<Inst>| {
        matches!(
            &func.insts[v],
            Inst::MakeValueStruct { .. }
                | Inst::MakeStruct { .. }
                | Inst::MakeEnum { .. }
                | Inst::MakeTuple(_)
        )
    };
    let is_nil = |func: &MirFunc, v: Idx<Inst>| matches!(&func.insts[v], Inst::Const(Lit::Nil));

    let mut rewrites: Vec<(Idx<Inst>, bool)> = Vec::new();
    for (iid, inst) in func.insts.iter() {
        let (a, b, eq) = match inst {
            Inst::Eq(a, b) => (*a, *b, true),
            Inst::NotEq(a, b) => (*a, *b, false),
            _ => continue,
        };
        let known =
            (never_nil(func, a) && is_nil(func, b)) || (never_nil(func, b) && is_nil(func, a));
        if known {
            rewrites.push((iid, !eq));
        }
    }
    if rewrites.is_empty() {
        return false;
    }
    for (iid, value) in rewrites {
        func.insts[iid] = Inst::Const(Lit::Bool(value));
    }
    true
}

/// Fold `Branch` on a constant bool condition into a `Goto`, then clear blocks
/// no longer reachable from any root (fn entry, lambda entries, co-fn defer
/// guards -- the latter two are detached sub-CFGs by design and must survive).
/// `loop {}` lowers to a `while true` shape whose const branch hides the loop
/// head from the FORL matcher's goto-chase (the filter combinator's skip loop)
/// and leaves a dead exit arm in the emitted Lua. Returns whether anything
/// changed (the dead `Const` conditions are left for a DCE round).
pub fn fold_const_branches(func: &mut MirFunc) -> bool {
    let bids: Vec<Idx<Block>> = func.blocks.iter().map(|(b, _)| b).collect();
    let mut changed = false;
    for b in &bids {
        if let Term::Branch {
            cond,
            then_bb,
            else_bb,
        } = func.blocks[*b].term
        {
            if let Inst::Const(Lit::Bool(v)) = func.insts[cond] {
                func.blocks[*b].term = Term::Goto(if v { then_bb } else { else_bb });
                changed = true;
            }
        }
    }
    if !changed {
        return false;
    }
    let mut roots = vec![func.entry];
    roots.extend(func.coro_defers.iter().map(|(_, e)| *e));
    for (_, inst) in func.insts.iter() {
        if let Inst::Lambda { entry, .. } = inst {
            roots.push(*entry);
        }
    }
    let mut reach: rustc_hash::FxHashSet<Idx<Block>> = roots.iter().copied().collect();
    let mut stack = roots;
    while let Some(b) = stack.pop() {
        skull_mir::cfg::successors(&func.blocks[b].term, |t| {
            if reach.insert(t) {
                stack.push(t);
            }
        });
    }
    let reachable_emit_blocks: rustc_hash::FxHashSet<u32> = reach
        .iter()
        .map(|block| func.emit_block_id(*block))
        .collect();
    if let Some(ids) = &mut func.emit_ids {
        debug_assert_eq!(ids.forced_cells.len(), ids.forced_cell_blocks.len());
        let mut forced_cells = Vec::with_capacity(ids.forced_cells.len());
        let mut forced_cell_blocks = Vec::with_capacity(ids.forced_cell_blocks.len());
        for (&cell, &block) in ids.forced_cells.iter().zip(&ids.forced_cell_blocks) {
            if reachable_emit_blocks.contains(&block) {
                forced_cells.push(cell);
                forced_cell_blocks.push(block);
            }
        }
        ids.forced_cells = forced_cells;
        ids.forced_cell_blocks = forced_cell_blocks;
    }
    for b in bids {
        if !reach.contains(&b) {
            func.blocks[b].insts.clear();
            func.blocks[b].term = Term::Unreachable;
        }
    }
    true
}

/// Scalarize `buf[i] = <value-struct literal>` for a `NativeBuffer<value>`.
///
/// `StoreIndex { kind: NativeBuffer, value: MakeValueStruct }` otherwise builds
/// the WHOLE nested cdata (`Vertex_ct(Vec2_ct(..), Color_ct(..), ..)`) and copies
/// it into the slot -- ~5 cdata allocations per element that don't sink across
/// the buffer store. rock instead grabs the slot reference and writes each leaf
/// field in place (`slot.pos.x = ..`), allocating nothing. This benefits
/// glyph-quad loops such as `verts[ci+N] = make_vertex(..)`.
///
/// Rewrite the store to `slot = buf[i]` (a bounds-checked `__nb_checked_get`,
/// same as the original `__nb_checked_set`) followed by per-leaf `StoreField`s,
/// descending into nested value-struct fields via `Field(slot, name)` (FFI
/// returns a reference for aggregate fields, so the writes go through). The dead
/// `MakeValueStruct`s fall to DCE. Only fires when the stored value is a literal
/// `MakeValueStruct` -- an opaque cdata (a variable) is copied whole as before.
pub fn scalarize_native_buffer_stores(func: &mut MirFunc) {
    // (store inst, base, index, value-MakeValueStruct, kind) for each rewritable
    // store. Both NativeBuffer flavors: a value-element buffer arrives as
    // `NativeBufferValue` (carried through to the slot so the JS emitter keeps
    // the flat-slot element info).
    let targets: Vec<(Idx<Inst>, Idx<Inst>, Idx<Inst>, Idx<Inst>, ContainerKind)> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .filter_map(|i| match &func.insts[i] {
            Inst::StoreIndex {
                base,
                index,
                value,
                kind,
            } if kind.is_native_buffer()
                && matches!(&func.insts[*value], Inst::MakeValueStruct { .. }) =>
            {
                Some((i, *base, *index, *value, *kind))
            }
            _ => None,
        })
        .collect();
    if targets.is_empty() {
        return;
    }
    let mut replacement: HashMap<Idx<Inst>, Vec<Idx<Inst>>> = HashMap::default();
    for (store_i, base, index, value, kind) in targets {
        let slot = func.alloc_inst(Inst::Index { base, index, kind });
        let mut new = vec![slot];
        gen_leaf_stores(func, slot, value, &mut new);
        replacement.insert(store_i, new);
    }
    let block_ids: Vec<Idx<Block>> = func.blocks.iter().map(|(id, _)| id).collect();
    for bid in block_ids {
        if !func.blocks[bid]
            .insts
            .iter()
            .any(|i| replacement.contains_key(i))
        {
            continue;
        }
        let old = std::mem::take(&mut func.blocks[bid].insts);
        let mut new = Vec::with_capacity(old.len());
        for i in old {
            match replacement.get(&i) {
                Some(ins) => new.extend_from_slice(ins),
                None => new.push(i),
            }
        }
        func.blocks[bid].insts = new;
    }
}

/// Whether `mvs`'s constructor tree transitively contains an IMPLICIT nobox
/// materialization. The trigger is recursive on purpose: for
/// `Outer { b: Box2 { lo: NV2 {..} } }` the outer's immediate operand is a
/// `Box2` literal, which is not itself nobox. A `MaterializeValueStruct`
/// operand is the SANCTIONED escape and is never a violation.
fn ctor_tree_has_implicit_nobox(ws: &Workspace, func: &MirFunc, mvs: Idx<Inst>) -> bool {
    let (name, fields) = match &func.insts[mvs] {
        Inst::MakeValueStruct { name, fields, .. } => (name, fields),
        // An EXPLICIT box is a sanctioned root, but its nested fields are still
        // implicit: `materialize(E5 { pos: NV2 { .. } })` authorizes the E5 box,
        // never a separate NV2 one.
        Inst::MaterializeValueStruct { name, fields, .. } => (name, fields),
        _ => return false,
    };
    if let Some(sid) = ws.struct_by_name.get(name) {
        if ws.modules[sid.module.index()].structs[sid.def].is_nobox {
            return true;
        }
    }
    fields.iter().any(|(_, op)| {
        // A `materialize` OPERAND is the sanctioned escape -- never a violation,
        // and not descended into.
        !matches!(&func.insts[*op], Inst::MaterializeValueStruct { .. })
            && ctor_tree_has_implicit_nobox(ws, func, *op)
    })
}

/// Resolve one dotted leaf path against a ctor tree, emitting `Field` reads for
/// any OPAQUE operand the path passes through (a nested value read off an
/// existing cdata is legal and must be decomposed, not rejected). `None` when
/// the path does not resolve, which the caller turns into a rejection rather
/// than a guess.
fn leaf_operand(
    func: &mut MirFunc,
    root: Idx<Inst>,
    path: &str,
    out: &mut Vec<Idx<Inst>>,
) -> Option<Idx<Inst>> {
    let mut cur = root;
    for comp in path.split('.') {
        let comp = Ustr::from(comp);
        let next = match &func.insts[cur] {
            // Reads THROUGH an explicit `materialize` operand, so the box it
            // requested is DCE'd when the parent flattens for some other field.
            // Deliberate: a value struct has no identity, so eliding a box of one
            // is unobservable -- `materialize`'s contract is about what the
            // program computes, not about a heap object surviving. Preserving it
            // instead needs the `Field` reads ordered after the box, which the
            // splice position does not currently give (they land at the parent
            // ctor's slot, ahead of where emit places the operand).
            Inst::MakeValueStruct { fields, .. } | Inst::MaterializeValueStruct { fields, .. } => {
                fields.iter().find(|(n, _)| *n == comp).map(|(_, op)| *op)
            }
            // Opaque: read the field off the existing value.
            _ => {
                let f = func.alloc_inst(Inst::Field {
                    base: cur,
                    field: comp,
                });
                out.push(f);
                Some(f)
            }
        }?;
        cur = next;
    }
    Some(cur)
}

/// Flatten a value-struct ctor whose tree implicitly materializes a `nobox`
/// field into one carrying that type's COMPLETE primitive leaf list, keyed by
/// dotted path (`("pos.x", ..)`). The nested ctors then have no users and DCE
/// removes them.
///
/// Flattening happens at the OUTERMOST cdata materialization: rewriting
/// bottom-up would satisfy the guarantee too, but allocate an intermediate cdata
/// per construction. A ctor that is itself a field operand of another cdata
/// ctor is therefore skipped -- the outer one absorbs it.
///
/// Only the CDATA tier is rewritten, because only a cdata parent has physical
/// fields to splice the child's leaves into. The other two tiers need nothing
/// from this pass: a leaf-table box is already built from leaves by `pack`, and
/// a GC-table box keeps its declared keys, so a `nobox` child inside one is a
/// real box that the verifier rejects at its own span. A plain cdata child of
/// a GC-table box (`RoomC { span: Span2 { lo: Cell { .. } } }`) is flattened
/// here like any outermost cdata ctor.
///
/// The dotted form exists only between here (end of `cleanup`) and emit. Field
/// names are documented as MIR-pass metadata rather than the wire shape, and
/// every pass that runs in between (`convert_closures`, `recover_counted_loops`,
/// `inline_hoisted_bodies`, the verifier) remaps operands or matches the struct
/// NAME only -- none interpret field names.
pub fn flatten_nobox_nested_ctors(
    ws: &Workspace,
    func: &mut MirFunc,
) -> Result<bool, CompilerError> {
    let all: Vec<Idx<Inst>> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .collect();
    // A ctor used as a field operand of another CDATA ctor is not the
    // outermost one. A table parent absorbs nothing, so a cdata child of a
    // GC-table or leaf-table ctor is itself the outermost cdata materialization.
    let mut inner: HashSet<Idx<Inst>> = HashSet::default();
    for &i in &all {
        if let Inst::MakeValueStruct {
            fields,
            representation,
            ..
        }
        | Inst::MaterializeValueStruct {
            fields,
            representation,
            ..
        } = &func.insts[i]
        {
            let table_parent = matches!(
                func.rep_arena
                    .as_ref()
                    .and_then(|arena| arena.try_get(*representation))
                    .and_then(|plan| plan.value_storage()),
                Some(
                    skull_mir::lower::ValueStorage::GcTable
                        | skull_mir::lower::ValueStorage::LeafTable
                )
            );
            if !table_parent {
                inner.extend(fields.iter().map(|(_, op)| *op));
            }
        }
    }
    let targets: Vec<Idx<Inst>> = all
        .iter()
        .copied()
        .filter(|i| !inner.contains(i) && ctor_tree_has_implicit_nobox(ws, func, *i))
        .collect();
    if targets.is_empty() {
        return Ok(false);
    }
    let rep_arena = func.rep_arena.clone().ok_or_else(|| {
        CompilerError::new_no_location(format!(
            "nobox constructor in `{}` has no build-owned representation arena",
            func.name
        ))
    })?;
    let mut replacement: HashMap<Idx<Inst>, Vec<Idx<Inst>>> = HashMap::default();
    for t in targets {
        let span = func.inst_spans.get(t).copied();
        let reject = |msg: String| match span {
            Some(sp) => CompilerError::new_with_label(sp, msg),
            None => CompilerError::new_no_location(msg),
        };
        let (name, representation, form, explicit) = match &func.insts[t] {
            Inst::MakeValueStruct {
                name,
                representation,
                form,
                ..
            } => (*name, *representation, *form, false),
            Inst::MaterializeValueStruct {
                name,
                representation,
                form,
                ..
            } => (*name, *representation, *form, true),
            _ => continue,
        };
        // A ctor ALREADY in leaf form carries the canonical leaves as its
        // operands, so there is nothing left to splice. It still matches the
        // trigger (a `nobox` name is its own), and re-resolving its dotted keys
        // as a field path fails whenever a path has more than one component --
        // `lo` is not a field of a ctor keyed `lo.x` -- which is what made
        // `materialize` of a nested cdata `nobox` value reject on the pass's
        // second iteration.
        if matches!(form, ValueCtorForm::Leaves) {
            continue;
        }
        let plan = rep_arena.try_get(representation).ok_or_else(|| {
            reject(format!(
                "nobox constructor `{name}` carries representation {representation:?} that does not belong to its build arena"
            ))
        })?;
        // Flattening splices a nested ctor's leaves into the PARENT's physical
        // fields, which only the cdata tier has -- a GC-table parent keeps its
        // declared keys and a leaf-table parent is already built from leaves by
        // `pack`. Leaving those trees alone is not a fallback: an explicit
        // `materialize` at those tiers is legal exactly as it stands, and an
        // IMPLICIT nobox ctor still inside one is rejected, at its own span, by
        // `nobox_materialization_violations`.
        if plan.value_storage() != Some(skull_mir::lower::ValueStorage::CData) {
            continue;
        }
        let paths = plan.leaf_paths().ok_or_else(|| {
            reject(format!(
                "nobox constructor `{name}` carries representation {representation:?} without a leaf-path plan"
            ))
        })?;
        let mut pre: Vec<Idx<Inst>> = Vec::new();
        let mut leaves: Vec<(Ustr, Idx<Inst>)> = Vec::with_capacity(paths.len());
        for path in &paths {
            let op = leaf_operand(func, t, path.as_str(), &mut pre).ok_or_else(|| {
                reject(format!(
                    "nobox constructor `{name}` representation {representation:?} leaf `{path}` does not resolve through its physical fields"
                ))
            })?;
            leaves.push((*path, op));
        }
        // An already-flat ctor rewrites to ITSELF -- a `nobox` name is its own
        // trigger, so `materialize(Color { .. })` is a target on every pass. The
        // rewrite is harmless but the "changed" answer is not: `cleanup_with_nobox`
        // iterates on it, so reporting a no-op as progress never terminates.
        let unchanged = matches!(
            &func.insts[t],
            Inst::MakeValueStruct { fields, .. } | Inst::MaterializeValueStruct { fields, .. }
                if *fields == leaves
        );
        if unchanged {
            continue;
        }
        // Preserve the variant: an explicit box stays explicit, it just stops
        // building a nested one on the way.
        let flat = func.alloc_inst(if explicit {
            Inst::MaterializeValueStruct {
                name,
                representation,
                form: ValueCtorForm::Leaves,
                fields: leaves,
            }
        } else {
            Inst::MakeValueStruct {
                name,
                representation,
                form: ValueCtorForm::Leaves,
                fields: leaves,
            }
        });
        // Minted insts inherit the outer ctor's span: a rejection downstream must
        // still name the user's line.
        if let Some(sp) = span {
            for &n in pre.iter().chain(std::iter::once(&flat)) {
                func.inst_spans.insert(n, sp);
            }
        }
        pre.push(flat);
        replacement.insert(t, pre);
    }
    if replacement.is_empty() {
        return Ok(false);
    }
    // The flattened ctor takes the original's slot, so every existing user of
    // `t` now reads the flat form.
    let remap: HashMap<Idx<Inst>, Idx<Inst>> = replacement
        .iter()
        .map(|(k, v)| (*k, *v.last().unwrap()))
        .collect();
    let m = |o: Idx<Inst>| remap.get(&o).copied().unwrap_or(o);
    for i in func.insts.iter_mut().map(|(id, _)| id).collect::<Vec<_>>() {
        remap_inst(&mut func.insts[i], &m);
    }
    // Terminators too: a `Term` still pointing at the replaced ctor references an
    // inst that is no longer in any block, which regalloc later reads as an
    // unassigned value index and panics rather than failing anywhere useful.
    let block_ids: Vec<Idx<Block>> = func.blocks.iter().map(|(id, _)| id).collect();
    for bid in &block_ids {
        remap_term(&mut func.blocks[*bid].term, &m);
    }
    for bid in block_ids {
        if !func.blocks[bid]
            .insts
            .iter()
            .any(|i| replacement.contains_key(i))
        {
            continue;
        }
        let old = std::mem::take(&mut func.blocks[bid].insts);
        let mut new = Vec::with_capacity(old.len());
        for i in old {
            match replacement.get(&i) {
                Some(ins) => new.extend_from_slice(ins),
                None => new.push(i),
            }
        }
        func.blocks[bid].insts = new;
    }
    Ok(true)
}

/// [`cleanup`] plus nobox nested-ctor normalization and the DCE that clears the
/// ctors it orphans. Every emit path that is followed by
/// [`nobox_materialization_violations`] must use THIS, not `cleanup`:
/// normalization has to run before the verifier, and after `cleanup`'s own final
/// DCE (the verifier walks `block.insts`, so an orphaned inner ctor left in the
/// list still rejects). Idempotent -- a rewritten ctor is in LEAF form and the
/// pass skips those -- which matters because `cleanup` is called from many
/// sites. The name-based trigger still fires on it, so the skip has to be the
/// form and not the trigger.
///
/// Every emit-path caller uses this; plain `cleanup` survives only as this
/// function's own first step. Were a site to miss it, the failure would be
/// CLOSED -- a spurious rejection from the verifier, never a silent miscompile.
pub fn cleanup_with_nobox(ws: &Workspace, func: &mut MirFunc) -> Result<(), CompilerError> {
    cleanup_in(Some(ws), func)?;
    // INTERLEAVED, not once each. Flattening replaces a nested nobox ctor's
    // users with leaf reads, and that is exactly the shape SROA needs to split
    // the field cell the ctor is stored to: while the outer ctor still reads the
    // nested one whole, the nested ctor has more than one user and SROA leaves
    // the field cell alone, so the box survives. Terminates because the pass is
    // idempotent -- a flattened ctor no longer matches the trigger.
    while flatten_nobox_nested_ctors(ws, func)? {
        cleanup_in(Some(ws), func)?;
    }
    // Each successful flatten is followed by cleanup's fixed-point DCE;
    // the final unsuccessful flatten leaves the live program unchanged.
    promote_direct_cells(func);
    // After promotion, so a rounding that reached the store through a cell
    // (an inlined constructor with a defaulted argument) is seen directly.
    if drop_f32_rounding_into_float_views(func) {
        dce(func);
    }
    Ok(())
}

/// Emit one `StoreField(slot, field, value)` per IMMEDIATE field of `mvs` (a
/// `MakeValueStruct`). Does NOT descend into nested value-struct fields: a
/// nested field's value is assigned whole (`slot.pos = Vec2_ct(x, y)`), matching
/// rock. The nested ctor then SINKS on the trace (it never escapes -- only its
/// bytes are copied into the slot). Recursing instead (`slot.pos.x = x`) would
/// READ `slot.pos`, and indexing an aggregate cdata field allocates a reference
/// cdata per access -- the very allocation we are removing.
fn gen_leaf_stores(func: &mut MirFunc, slot: Idx<Inst>, mvs: Idx<Inst>, out: &mut Vec<Idx<Inst>>) {
    let Inst::MakeValueStruct { fields, .. } = &func.insts[mvs] else {
        return;
    };
    let fields = fields.clone();
    for (fname, op) in fields {
        out.push(func.alloc_inst(Inst::StoreField {
            base: slot,
            field: fname,
            value: op,
        }));
    }
}

/// A scalarizable aggregate ctor's identity, consistent across all stores to one
/// cell (a cell holds one type). The rewrite is identical for all; the variant
/// only distinguishes the ctor instructions for the consistency check.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StructId {
    /// `MakeValueStruct` -- an immutable `value` cdata.
    Value(ustr::Ustr),
    /// `MakeStruct` -- a mutable plain-struct table (`ty` is the `__ty` nominal,
    /// `None` for a foreign nominal).
    Plain(Option<ustr::Ustr>),
    /// `MakeEnum` / `MakeEnumDyn` -- a payload-variant enum table
    /// (`{__variant, __ty, [i]=..}`).
    /// Keyed by the ENUM name (`__ty`), so every variant of one enum stored to a
    /// cell is consistent (the field union covers all variants' payload slots).
    Enum(ustr::Ustr),
    /// `MakeTuple` -- a positional table (`{v0, v1, ..}`), keyed by width. Fields
    /// are `_<i>`, the same naming `TupleIndex` reads already rewrite through.
    Tuple(usize),
}

/// The source of a scalarized field's value: an existing operand, or a string
/// CONST to materialize (an enum table bakes `__variant`/`__ty` as literals, not
/// operands, so splitting it must emit the `Const(Str)` insts).
enum FieldSrc {
    Operand(Idx<Inst>),
    ConstStr(ustr::Ustr),
    /// Leaf-wise copy from another family member's field cell (a value-struct
    /// alias member, see `copy_alias`) -- materializes a `Load` at the store site.
    CellLoad(Idx<Cell>),
    /// A nullable value cell's presence leaf on a ctor store.
    ConstTrue,
    ConstNil,
}

/// If `inst` is a scalarizable aggregate ctor, its identity + logical field list.
/// A `value`/plain struct's fields are its declared (name, operand) pairs. An
/// enum's are `__variant`/`__ty` (string consts) + one `_<i>` per positional
/// payload + each record-payload field by name -- the table's runtime keys (the
/// payload is at positional Lua index `[i+1]`, read via `TupleIndex`, named `_<i>`
/// here so a `match` payload read rewrites to the same field cell).
///
/// `enum_hint` is the cell's enum name (from its `MakeEnum` stores). A unit
/// variant of a MIXED enum is stored as a reference to a shared singleton GLOBAL
/// `__rock_enum_<symbol(E)>_<V>` (not a `MakeEnum`); given `enum_hint = E` we recognize it
/// as the same enum's ctor with fields `{__variant=V, __ty=E}` (no payload), so a
/// cell mixing payload + unit variants stays uniform and splits. The hint is what
/// disambiguates `V` from `E` in the underscore-joined global name.
fn ctor_fields(
    inst: &Inst,
    enum_hint: Option<ustr::Ustr>,
) -> Option<(StructId, Vec<(ustr::Ustr, FieldSrc)>)> {
    let op = |fs: &[(ustr::Ustr, Idx<Inst>)]| {
        fs.iter()
            .map(|(n, v)| (*n, FieldSrc::Operand(*v)))
            .collect::<Vec<_>>()
    };
    match inst {
        Inst::MakeValueStruct { name, fields, .. } => Some((StructId::Value(*name), op(fields))),
        Inst::MakeStruct { ty, fields } => Some((StructId::Plain(*ty), op(fields))),
        Inst::MakeTuple(vs) => Some((
            StructId::Tuple(vs.len()),
            vs.iter()
                .enumerate()
                .map(|(i, v)| (ustr::Ustr::from(&format!("_{i}")), FieldSrc::Operand(*v)))
                .collect(),
        )),
        Inst::MakeEnum {
            tag,
            ty,
            positional,
            named,
        } => {
            let mut f = vec![
                (ustr::Ustr::from("__variant"), FieldSrc::ConstStr(*tag)),
                (
                    ustr::Ustr::from("__ty"),
                    FieldSrc::ConstStr(skull_mir::mangle::symbol_of_key(*ty)),
                ),
            ];
            for (i, p) in positional.iter().enumerate() {
                f.push((ustr::Ustr::from(&format!("_{i}")), FieldSrc::Operand(*p)));
            }
            for (n, v) in named {
                f.push((*n, FieldSrc::Operand(*v)));
            }
            Some((StructId::Enum(*ty), f))
        }
        Inst::MakeEnumDyn {
            tag,
            ty,
            positional,
            named,
        } => {
            let mut f = vec![
                (ustr::Ustr::from("__variant"), FieldSrc::Operand(*tag)),
                (
                    ustr::Ustr::from("__ty"),
                    FieldSrc::ConstStr(skull_mir::mangle::symbol_of_key(*ty)),
                ),
            ];
            for (i, p) in positional.iter().enumerate() {
                f.push((ustr::Ustr::from(&format!("_{i}")), FieldSrc::Operand(*p)));
            }
            for (n, v) in named {
                f.push((*n, FieldSrc::Operand(*v)));
            }
            Some((StructId::Enum(*ty), f))
        }
        // A unit-variant singleton ref of the cell's enum: `__rock_enum_<E>_<V>`.
        Inst::Global(g) => {
            let e = enum_hint?;
            // The singleton and the `__ty` leaf carry the minted type symbol, as
            // the emitted global and every constructed box do; for a v1 enum that
            // differs from its DefKey.
            let symbol = skull_mir::mangle::symbol_of_key(e);
            let variant = g.as_str().strip_prefix(&format!("__rock_enum_{symbol}_"))?;
            Some((
                StructId::Enum(e),
                vec![
                    (
                        ustr::Ustr::from("__variant"),
                        FieldSrc::ConstStr(ustr::Ustr::from(variant)),
                    ),
                    (ustr::Ustr::from("__ty"), FieldSrc::ConstStr(symbol)),
                ],
            ))
        }
        _ => None,
    }
}

fn value_ctor_plan(inst: &Inst) -> Option<(skull_mir::lower::RepId, ValueCtorForm)> {
    match inst {
        Inst::MakeValueStruct {
            representation,
            form,
            ..
        }
        | Inst::MaterializeValueStruct {
            representation,
            form,
            ..
        } => Some((*representation, *form)),
        _ => None,
    }
}

/// One SROA round; returns whether it rewrote anything (drives the fixpoint).
fn scalarize_struct_cells_once(func: &mut MirFunc) -> Result<bool, CompilerError> {
    let nc = func.cells.len();
    let captured_cells: HashSet<Idx<Cell>> = if func
        .insts
        .iter()
        .any(|(_, inst)| matches!(inst, Inst::Lambda { .. }))
    {
        let tree = scope::ScopeTree::build(func);
        let ownership = tree.analyze(func);
        ownership
            .captures
            .iter()
            .flat_map(|captures| captures.iter().copied())
            .collect()
    } else {
        HashSet::default()
    };
    // Escape-site rematerialization (item 8): a whole-value load of a
    // `nonnull_value_struct` binding that would otherwise ESCAPE (disqualifying
    // its family) is instead rebuilt from the ctor's per-field cells AT the use
    // site. `value_order` keeps each value ctor's declared field order (the
    // rebuilt `MakeValueStruct` is positional); `escape_loads` are the loads to
    // rewrite. The flag -- set at lowering only for non-null value bindings --
    // is the ONLY license: no store/CFG heuristic (both proven unsound/too weak).
    let mut value_order: HashMap<usize, (Vec<ustr::Ustr>, skull_mir::lower::RepId, ValueCtorForm)> =
        HashMap::default();
    let mut escape_loads: Vec<(Idx<Inst>, Idx<Cell>)> = Vec::new();
    // A cell is a candidate iff every store to it is a struct ctor of one
    // consistent type. `cand[c]`: None = not yet seen / disqualified, Some(id) =
    // all stores so far are that struct. `cand_fields[c]` is its declared field
    // set, used to reject a `Field`/`StoreField` of an undeclared field (e.g. a
    // `__ty` reflective read) -- splitting one would read a never-stored cell.
    let mut cand: Vec<Option<StructId>> = vec![None; nc];
    let mut cand_fields: Vec<Option<HashSet<ustr::Ustr>>> = vec![None; nc];
    let mut disq = vec![false; nc];
    let mut has_store = vec![false; nc];
    for c in &func.params {
        disq[raw(*c) as usize] = true; // params are the Lua signature; never split
    }
    // Written through a `ref` alias this pass cannot see (`Cell::borrowed`).
    for (c, cell) in func.cells.iter() {
        if cell.borrowed {
            disq[raw(c) as usize] = true;
        }
    }
    if let Some(ra) = func.resume_arg {
        disq[raw(ra) as usize] = true;
    }
    // A cell defined by anything OTHER than a `Store` (a call multi-return dest, a
    // closure param, a map-iteration variable) has no struct ctor to split, so it
    // must stay whole.
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            match &func.insts[i] {
                Inst::CallMulti { dests, .. }
                | Inst::CallValueMulti { dests, .. }
                | Inst::CallBoundMulti { dests, .. } => {
                    dests.iter().for_each(|c| disq[raw(*c) as usize] = true)
                }
                Inst::Lambda { params, .. } => {
                    params.iter().for_each(|c| disq[raw(*c) as usize] = true)
                }
                Inst::MapNext {
                    key_cell, val_cell, ..
                } => {
                    disq[raw(*key_cell) as usize] = true;
                    disq[raw(*val_cell) as usize] = true;
                }
                _ => {}
            }
        }
    }
    // Per-cell enum name from its `MakeEnum` stores -- the hint that lets a
    // unit-variant singleton-ref store (`__rock_enum_<E>_<V>`) be recognized as the
    // same enum's ctor (it carries no `ty` of its own). A cell with no `MakeEnum`
    // store has `None` and so never matches a singleton (all-singleton cells don't
    // allocate -- nothing to scalarize). Conflicting enums leave the first; the
    // `StructId` consistency check then disqualifies the cell.
    let mut cell_enum: Vec<Option<ustr::Ustr>> = vec![None; nc];
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            if let Inst::Store { cell, value } = &func.insts[i] {
                if let Inst::MakeEnum { ty, .. } | Inst::MakeEnumDyn { ty, .. } =
                    &func.insts[*value]
                {
                    cell_enum[raw(*cell) as usize].get_or_insert(*ty);
                }
            }
        }
    }
    // Only the main CFG -- a cell touched inside a closure body is opaque (the
    // closure may run on any call), so leave it whole.
    //
    // ALIAS stores (`Store(c, Load(o))`): a cell whose every store copies
    // another candidate cell is the SAME object under Rock's by-ref struct
    // semantics (the shape a spliced inner `next`'s param has after the outer
    // cursor peels -- combinator chains nest cursors). Such a cell is UNIFIED
    // with its root: its field accesses share the root's field cells (so
    // by-ref mutation still lands in the shared cells) and the alias store
    // deletes. `alias[c] = Some(o)`; mixed ctor+alias stores stay whole.
    let mut alias: Vec<Option<Idx<Cell>>> = vec![None; nc];
    let main: HashSet<Idx<Block>> = top_level_blocks(func).into_iter().collect();
    // Value-use counts: a ctor stored into a candidate cell must have NO use
    // besides that store. Any other consumer (a `Field` read the interleaved
    // copy-prop forwarded onto the ctor, a second cell store) still sees the
    // ORIGINAL table -- splitting the cell would diverge the two.
    let mut vuse = vec![0u32; func.insts.len()];
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            func.insts[i].uses_each(&mut |op| vuse[raw(op) as usize] += 1);
        }
        b.term.uses_each(&mut |op| vuse[raw(op) as usize] += 1);
    }
    for (bid, b) in func.blocks.iter() {
        let in_main = main.contains(&bid);
        for &i in &b.insts {
            if let Inst::Store { cell, value } = &func.insts[i] {
                let ck = raw(*cell) as usize;
                has_store[ck] = true;
                if !in_main {
                    disq[ck] = true;
                    continue;
                }
                if let Inst::Load(o) = &func.insts[*value] {
                    if *o == *cell || alias[ck].is_some_and(|p| p != *o) || cand[ck].is_some() {
                        disq[ck] = true;
                    } else {
                        alias[ck] = Some(*o);
                    }
                    continue;
                }
                if alias[ck].is_some() {
                    disq[ck] = true;
                    continue;
                }
                // A plain nil store (a folded `phantom()` on an iterator done
                // path) is compatible with any ctor candidate: it neither
                // establishes nor breaks the cell's struct id. The rewrite
                // expands it to per-field nil stores.
                if matches!(&func.insts[*value], Inst::Const(Lit::Nil)) {
                    continue;
                }
                match ctor_fields(&func.insts[*value], cell_enum[ck]) {
                    Some((id, fields)) => {
                        if cand[ck].is_some_and(|n| n != id) || vuse[raw(*value) as usize] != 1 {
                            disq[ck] = true;
                        } else {
                            cand[ck] = Some(id);
                            if matches!(id, StructId::Value(_)) {
                                let Some((representation, form)) =
                                    value_ctor_plan(&func.insts[*value])
                                else {
                                    disq[ck] = true;
                                    continue;
                                };
                                value_order.entry(ck).or_insert_with(|| {
                                    (
                                        fields.iter().map(|(n, _)| *n).collect(),
                                        representation,
                                        form,
                                    )
                                });
                            }
                            cand_fields[ck]
                                .get_or_insert_with(HashSet::default)
                                .extend(fields.iter().map(|(n, _)| *n));
                        }
                    }
                    None => disq[ck] = true,
                }
            }
        }
    }

    // Resolve alias chains to their ctor-rooted representative. A chain that
    // cycles, or doesn't end at a live ctor candidate, stays whole. Alias
    // members adopt the root's struct id; their field set IS the root's.
    let mut rep: Vec<usize> = (0..nc).collect();
    for k in 0..nc {
        if alias[k].is_none() || disq[k] {
            continue;
        }
        let mut cur = k;
        let mut steps = 0usize;
        let root = loop {
            match alias[cur] {
                Some(o) if steps <= nc => {
                    cur = raw(o) as usize;
                    steps += 1;
                    // An alias THROUGH a disqualified cell must not resolve past it:
                    // a disqualified intermediate stays a whole aggregate whose value
                    // is not its first alias's (e.g. a mixed-alias `if`-result cell,
                    // stored `a` on one arm and `b` on the other). Following its stale
                    // `alias[]` to that first root would unify this member with the
                    // wrong value -- latent until escape-remat actually reads the rep.
                    if disq[cur] {
                        break None;
                    }
                }
                Some(_) => break None, // cycle
                None => break Some(cur),
            }
        };
        match root {
            Some(r) if !disq[r] && cand[r].is_some() => {
                rep[k] = r;
                cand[k] = cand[r];
            }
            _ => disq[k] = true,
        }
    }

    // An immutable aggregate alias member whose lifetime can escape is a
    // distinct BINDING, not the same object. Value structs already keep their
    // own leaves because whole-value rematerialization is available for them.
    // An enum needs distinct leaves only when a closure captures the alias;
    // otherwise the root cannot change before the alias's last observable use,
    // and sharing its scalar cells avoids rematerializing enum tables.
    // In either case,
    // collapsing the cells hands the member the ROOT's live range -- which a
    // closure capture observes: the leaves of a per-iteration `for` binding
    // become the loop cursor's multi-store slots, boxed once for the whole
    // function and nil-ed on the exhausted path. Such a member keeps its own
    // leaf cells and its alias store expands to leaf-wise copies. Reference
    // structs still unify: there `let q = p` aliases and by-ref mutation must
    // land in shared cells.
    let mut copy_alias: Vec<Option<Idx<Cell>>> = vec![None; nc];
    for k in 0..nc {
        let cell = Idx::from_raw(RawIdx::from_u32(k as u32));
        let copies_binding = matches!(cand[rep[k]], Some(StructId::Value(_)))
            || (matches!(cand[rep[k]], Some(StructId::Enum(_))) && captured_cells.contains(&cell));
        if rep[k] == k || disq[k] || !copies_binding {
            continue;
        }
        copy_alias[k] = Some(Idx::from_raw(RawIdx::from_u32(rep[k] as u32)));
        cand_fields[k] = cand_fields[rep[k]].clone();
        if let Some(order) = value_order.get(&rep[k]).cloned() {
            value_order.insert(k, order);
        }
        rep[k] = k;
    }

    // A `Load` of a candidate cell may be used ONLY as a `Field`/`StoreField`
    // base of a declared field -- or as the value of an accepted alias store
    // within the same family (that store deletes). Map each such load to its
    // cell, then scan every operand use: any other consumer means the whole
    // struct escapes -> disqualify.
    let mut load_cell: HashMap<Idx<Inst>, Idx<Cell>> = HashMap::default();
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            if let Inst::Load(c) = &func.insts[i] {
                let ck = raw(*c) as usize;
                if cand[ck].is_some() && !disq[ck] {
                    load_cell.insert(i, *c);
                }
            }
        }
    }
    let mut enum_nil_tests: Vec<(Idx<Inst>, Idx<Inst>, Idx<Cell>)> = Vec::new();
    // A nullable value cell (a `V?` read joined with `nil`, as `if let` and an
    // inlined `unwrap()` produce) is tested against nil before its fields are
    // read. The split cell answers that test from a presence leaf.
    let mut value_nil_tests: Vec<(Idx<Inst>, Idx<Inst>, Idx<Cell>)> = Vec::new();
    // Roots of families that split only because a copy of a copy is accepted.
    let mut chained_roots: HashSet<usize> = HashSet::default();
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            func.insts[i].uses_each(&mut |op| {
                if let Some(&c) = load_cell.get(&op) {
                    let known = |f: &ustr::Ustr| {
                        cand_fields[rep[raw(c) as usize]]
                            .as_ref()
                            .is_some_and(|s| s.contains(f))
                    };
                    let ok = match &func.insts[i] {
                        Inst::Field { base, field } => *base == op && known(field),
                        Inst::StoreField { base, field, .. } => *base == op && known(field),
                        // An enum positional payload read: `v[index+1]`, named `_<index>`.
                        Inst::TupleIndex { base, index } => {
                            *base == op && known(&ustr::Ustr::from(&format!("_{index}")))
                        }
                        Inst::Eq(a, b) | Inst::NotEq(a, b)
                            if matches!(cand[rep[raw(c) as usize]], Some(StructId::Value(_))) =>
                        {
                            let other = if *a == op { *b } else { *a };
                            let is_nil = (*a == op) != (*b == op)
                                && matches!(func.insts[other], Inst::Const(Lit::Nil));
                            if is_nil {
                                value_nil_tests.push((i, op, c));
                            }
                            is_nil
                        }
                        Inst::Eq(a, b)
                            if matches!(cand[rep[raw(c) as usize]], Some(StructId::Enum(_))) =>
                        {
                            let other = if *a == op {
                                Some(*b)
                            } else if *b == op {
                                Some(*a)
                            } else {
                                None
                            };
                            let is_nil = other
                                .is_some_and(|v| matches!(func.insts[v], Inst::Const(Lit::Nil)));
                            if is_nil {
                                enum_nil_tests.push((i, op, c));
                            }
                            is_nil
                        }
                        // The alias store itself: `Store(y, Load(x))` where y and x
                        // unify to the same root (deleted by the rewrite), or where
                        // y copies x's leaves (expanded to leaf-wise stores). A copy
                        // of a copy (`let t = h.t ?? return` feeding a loop body's
                        // binding) reads its immediate source's leaves.
                        Inst::Store { cell, value } => {
                            let yk = raw(*cell) as usize;
                            let accepted = *value == op
                                && alias[yk].is_some()
                                && !disq[yk]
                                && (rep[yk] == rep[raw(c) as usize]
                                    || (copy_alias[yk].is_some() && alias[yk] == Some(c)));
                            if accepted && copy_alias[yk].is_some_and(|r| r != c) {
                                chained_roots.insert(raw(copy_alias[yk].unwrap()) as usize);
                            }
                            accepted
                        }
                        _ => false,
                    };
                    if !ok {
                        // A flagged (non-null value-struct) binding rebuilds at the
                        // use instead of pinning the family as an escaping aggregate.
                        if func.cells[c].nonnull_value_struct {
                            escape_loads.push((op, c));
                        } else {
                            disq[raw(c) as usize] = true;
                        }
                    }
                }
            });
        }
        b.term.uses_each(&mut |op| {
            if let Some(&c) = load_cell.get(&op) {
                if func.cells[c].nonnull_value_struct {
                    escape_loads.push((op, c)); // rebuild at the terminator use
                } else {
                    disq[raw(c) as usize] = true; // a load used by a terminator escapes
                }
            }
        });
    }

    // A chained family that also escapes would rebuild its value at every
    // escaping use, which can cost more than the one copy it replaces.
    for (_, c) in &escape_loads {
        let k = raw(*c) as usize;
        let root = copy_alias[k].map_or(rep[k], |r| raw(r) as usize);
        if chained_roots.contains(&root) {
            disq[root] = true;
        }
    }

    // Disqualification is family-wide: an escaping alias member keeps the
    // whole object (root + every member) as an aggregate, and a dead root
    // takes its members with it. Two passes settle it (rep is one level deep).
    for _ in 0..2 {
        for k in 0..nc {
            if rep[k] != k {
                if disq[k] {
                    disq[rep[k]] = true;
                }
                if disq[rep[k]] || cand[rep[k]].is_none() {
                    disq[k] = true;
                }
            }
            // A copy member reads the root's leaf cells, so a whole root takes
            // the member with it -- and a whole member's surviving `Store(k,
            // Load(root))` would read the root's now-uninit aggregate, so it
            // takes the root too.
            if let Some(root) = copy_alias[k] {
                let rk = raw(root) as usize;
                if disq[k] {
                    disq[rk] = true;
                }
                if disq[rk] || cand[rk].is_none() {
                    disq[k] = true;
                }
            }
        }
    }

    // A family whose nullable value is nil-tested carries a presence leaf,
    // written by every store: a constant on a ctor, nil on a nil store. Copy
    // members read the root's leaves, so the whole family gets it.
    let present_field = ustr::Ustr::from("__present");
    let mut presence_roots: HashSet<usize> = HashSet::default();
    for (_, _, c) in &value_nil_tests {
        let k = raw(*c) as usize;
        let root = copy_alias[k].map_or(rep[k], |r| raw(r) as usize);
        presence_roots.insert(root);
    }
    for k in 0..nc {
        let root = copy_alias[k].map_or(rep[k], |r| raw(r) as usize);
        if presence_roots.contains(&root) {
            if let Some(fields) = cand_fields[k].as_mut() {
                fields.insert(present_field);
            }
        }
    }
    let needs_presence = |c: Idx<Cell>| {
        let k = raw(c) as usize;
        k < nc && presence_roots.contains(&copy_alias[k].map_or(rep[k], |r| raw(r) as usize))
    };

    // `k < nc` guards the field cells `get_field_cell` allocates during the
    // rewrite (a `StoreField`->`Store` rewrite emits a `Store` to a fresh field
    // cell that the later `store_expansions` scan sees): they are never candidates
    // this round, and indexing the round's `nc`-sized arrays with them would panic.
    let is_cand = |c: Idx<Cell>| {
        let k = raw(c) as usize;
        k < nc && cand[k].is_some() && !disq[k] && has_store[k]
    };
    if (0..nc).all(|k| !(cand[k].is_some() && !disq[k] && has_store[k])) {
        return Ok(false);
    }

    // Allocate per-field cells on demand: (cell, field name) -> field cell.
    // Alias members resolve through `rep` FIRST, so a family shares one set of
    // field cells (by-ref mutation through any member lands in the shared cell).
    let to_rep = |c: Idx<Cell>| -> Idx<Cell> {
        let k = raw(c) as usize;
        if k < nc && rep[k] != k {
            Idx::from_raw(RawIdx::from_u32(rep[k] as u32))
        } else {
            c
        }
    };
    let mut field_cell: HashMap<(Idx<Cell>, ustr::Ustr), Idx<Cell>> = HashMap::default();
    let mut get_field_cell = |func: &mut MirFunc, c: Idx<Cell>, f: ustr::Ustr| -> Idx<Cell> {
        *field_cell.entry((c, f)).or_insert_with(|| {
            let base = func.cells[c].name.map(|n| n.as_str().to_string());
            let name = base.map(|b| ustr::Ustr::from(&format!("{b}__{f}")));
            let capture_identity = func.cells[c]
                .capture_identity
                .as_ref()
                .map(|identity| identity.projected(f));
            let cell = func.new_cell(name);
            func.cells[cell].capture_identity = capture_identity;
            cell
        })
    };

    let mut nil_test_loads: HashMap<Idx<Inst>, Idx<Inst>> = HashMap::default();
    for (test, old, c) in enum_nil_tests {
        if !is_cand(c) {
            continue;
        }
        // Every enum ctor writes a nonnil `__ty`; exhaustion nils every lane.
        let type_cell = get_field_cell(func, to_rep(c), ustr::Ustr::from("__ty"));
        let present = func.alloc_inst(Inst::Load(type_cell));
        match &mut func.insts[test] {
            Inst::Eq(a, b) => {
                if *a == old {
                    *a = present;
                } else if *b == old {
                    *b = present;
                }
            }
            _ => unreachable!("recorded enum nil test changed instruction kind"),
        }
        nil_test_loads.insert(test, present);
    }
    for (test, old, c) in value_nil_tests {
        if !is_cand(c) {
            continue;
        }
        let present_cell = get_field_cell(func, to_rep(c), present_field);
        let present = func.alloc_inst(Inst::Load(present_cell));
        match &mut func.insts[test] {
            Inst::Eq(a, b) | Inst::NotEq(a, b) => {
                if *a == old {
                    *a = present;
                } else if *b == old {
                    *b = present;
                }
            }
            _ => unreachable!("recorded value nil test changed instruction kind"),
        }
        nil_test_loads.insert(test, present);
    }
    if !nil_test_loads.is_empty() {
        for (_, block) in func.blocks.iter_mut() {
            if !block.insts.iter().any(|i| nil_test_loads.contains_key(i)) {
                continue;
            }
            let old = std::mem::take(&mut block.insts);
            let mut new = Vec::with_capacity(old.len() + nil_test_loads.len());
            for i in old {
                if let Some(&load) = nil_test_loads.get(&i) {
                    new.push(load);
                }
                new.push(i);
            }
            block.insts = new;
        }
    }

    // Rewrite `Field { base: load(cand), field }` in place into `Load(field_cell)`,
    // and `StoreField { base: load(cand), field, value }` into `Store(field_cell,
    // value)`. Re-check `is_cand`: `load_cell` is not pruned as cells get
    // disqualified mid-analysis, so a cell escaping via a non-projection use (e.g.
    // whole-struct `==`) keeps its aggregate, and its field accesses must stay on
    // it -- rewriting them without expanding the store gives an uninit cell.
    // Only a field the CTOR actually provides has a cell to redirect to. A
    // nested value member is stored under its dotted LEAF names (`params.x`,
    // `params.y`), so the immediate `.params` read names no cell, and minting
    // one gives a load nothing ever stores -- the uninit-cell failure the
    // comment above describes, reached the other way. Such a read stays a
    // `Field` on the aggregate, which the `escape_loads` rebuild below
    // reconstitutes at exactly this load.
    let provides = |c: Idx<Cell>, f: &ustr::Ustr| {
        cand_fields[rep[raw(c) as usize]]
            .as_ref()
            .is_some_and(|s| s.contains(f))
    };
    let field_rewrites: Vec<(Idx<Inst>, Idx<Cell>, ustr::Ustr)> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .filter_map(|i| match &func.insts[i] {
            Inst::Field { base, field } => load_cell
                .get(base)
                .copied()
                .filter(|&c| is_cand(c) && provides(c, field))
                .map(|c| (i, c, *field)),
            _ => None,
        })
        .collect();
    for (i, c, field) in field_rewrites {
        let fc = get_field_cell(func, to_rep(c), field);
        func.insts[i] = Inst::Load(fc);
    }
    // Same for an enum positional payload read `TupleIndex { base, index }` ->
    // `Load(field_cell "_<index>")` (the `match`-with-binding payload extract).
    let tuple_rewrites: Vec<(Idx<Inst>, Idx<Cell>, ustr::Ustr)> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .filter_map(|i| match &func.insts[i] {
            Inst::TupleIndex { base, index } => load_cell
                .get(base)
                .copied()
                .filter(|&c| is_cand(c))
                .map(|c| (i, c, ustr::Ustr::from(&format!("_{index}")))),
            _ => None,
        })
        .collect();
    for (i, c, field) in tuple_rewrites {
        let fc = get_field_cell(func, to_rep(c), field);
        func.insts[i] = Inst::Load(fc);
    }
    let storefield_rewrites: Vec<(Idx<Inst>, Idx<Cell>, ustr::Ustr, Idx<Inst>)> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .filter_map(|i| match &func.insts[i] {
            Inst::StoreField { base, field, value } => load_cell
                .get(base)
                .copied()
                .filter(|&c| is_cand(c))
                .map(|c| (i, c, *field, *value)),
            _ => None,
        })
        .collect();
    for (i, c, field, value) in storefield_rewrites {
        let fc = get_field_cell(func, to_rep(c), field);
        func.insts[i] = Inst::Store { cell: fc, value };
    }

    let candidate_fields = |c: Idx<Cell>| -> Result<Vec<ustr::Ustr>, CompilerError> {
        let k = raw(c) as usize;
        let root = *rep.get(k).ok_or_else(|| {
            CompilerError::new_no_location("SROA candidate cell is outside its analysis tables")
        })?;
        let fields = cand_fields
            .get(root)
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                CompilerError::new_no_location(format!(
                    "SROA candidate cell {} has no planned physical field set",
                    k
                ))
            })?;
        Ok(fields.iter().copied().collect())
    };

    // Expand each `Store { cand, ctor }` into per-field stores. Collect first
    // (immutable read of the ctor fields), then rebuild the affected blocks.
    let store_expansions: Vec<(Idx<Inst>, Idx<Cell>, Vec<(ustr::Ustr, FieldSrc)>)> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .map(
            |i| -> Result<
                Option<(Idx<Inst>, Idx<Cell>, Vec<(ustr::Ustr, FieldSrc)>)>,
                CompilerError,
            > {
                match &func.insts[i] {
                    Inst::Store { cell, value } if is_cand(*cell) => {
                        // A nil store writes nil to EVERY field cell (sorted for
                        // deterministic emission), reusing the nil value inst.
                        if matches!(&func.insts[*value], Inst::Const(Lit::Nil)) {
                            let mut fs = candidate_fields(*cell)?;
                            fs.sort_unstable();
                            return Ok(Some((
                                i,
                                *cell,
                                fs.into_iter()
                                    .map(|f| (f, FieldSrc::Operand(*value)))
                                    .collect(),
                            )));
                        }
                        // A value-struct alias member copies its source's leaves.
                        if let (Inst::Load(src), Some(_)) =
                            (&func.insts[*value], copy_alias[raw(*cell) as usize])
                        {
                            let src = to_rep(*src);
                            let mut fs = candidate_fields(*cell)?;
                            fs.sort_unstable();
                            return Ok(Some((
                                i,
                                *cell,
                                fs.into_iter()
                                    .map(|f| (f, FieldSrc::CellLoad(src)))
                                    .collect(),
                            )));
                        }
                        let Some((_, mut fields)) =
                            ctor_fields(&func.insts[*value], cell_enum[raw(*cell) as usize])
                        else {
                            return Ok(None);
                        };
                        if needs_presence(*cell) {
                            fields.push((present_field, FieldSrc::ConstTrue));
                        }
                        // An omitted box key reads nil, even after a previous variant
                        // populated the scalar cell in a loop or coroutine frame.
                        let mut missing = candidate_fields(*cell)?;
                        missing.retain(|f| !fields.iter().any(|(name, _)| name == f));
                        missing.sort_unstable();
                        fields.extend(missing.into_iter().map(|f| (f, FieldSrc::ConstNil)));
                        Ok(Some((i, *cell, fields)))
                    }
                    _ => Ok(None),
                }
            },
        )
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    // Accepted alias stores (`Store(member, Load(..))`) DELETE: the member's
    // accesses were rewritten onto the root's field cells above, so the
    // whole-cell copy has nothing left to do (and keeping it would resurrect
    // the aggregate).
    let alias_deletions: Vec<Idx<Inst>> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .filter(|&i| match &func.insts[i] {
            Inst::Store { cell, value } => {
                let ck = raw(*cell) as usize;
                ck < nc
                    && alias[ck].is_some()
                    && copy_alias[ck].is_none()
                    && !disq[ck]
                    && matches!(&func.insts[*value], Inst::Load(_))
            }
            _ => false,
        })
        .collect();

    // old store inst -> the per-field store insts that replace it (in order). An
    // enum's `__variant`/`__ty` are `ConstStr` (baked literals, not operands), so
    // materialize the `Const(Str)` inst here before storing it to the field cell.
    let mut replacement: HashMap<Idx<Inst>, Vec<Idx<Inst>>> = HashMap::default();
    for i in alias_deletions {
        replacement.insert(i, Vec::new());
    }
    // A field cell that ALWAYS holds a value-struct ctor inherits the parent's
    // non-null licence, so a later round can split it in turn instead of
    // disqualifying its family when the field escapes whole (`Outer { ..o }`
    // rebuilds `o.inner` as one operand). Nullable fields are excluded by
    // construction: a nil store expands to `Const(Nil)` per field, which is not
    // a ctor -- the licence `nonnull_value_struct` grants is exactly "this cell
    // can be rebuilt from its leaves", and nil cannot be.
    let mut nn_field: HashMap<(Idx<Cell>, ustr::Ustr), bool> = HashMap::default();
    for (_, c, fields) in &store_expansions {
        if !func.cells[*c].nonnull_value_struct {
            continue;
        }
        for (fname, src) in fields {
            let is_value_ctor = matches!(src,
                FieldSrc::Operand(v) if matches!(&func.insts[*v], Inst::MakeValueStruct { .. }));
            *nn_field.entry((*c, *fname)).or_insert(true) &= is_value_ctor;
        }
    }
    for (store_i, c, fields) in store_expansions {
        let mut news = Vec::with_capacity(fields.len());
        for (fname, src) in fields {
            let fval = match src {
                FieldSrc::Operand(v) => v,
                // A materialized const is a NEW value-producing inst: it must be
                // spliced into the block (in `news`) BEFORE the store that reads it,
                // or it floats free of any block and regalloc/codegen never emit it.
                FieldSrc::ConstStr(s) => {
                    let cv = func.alloc_inst(Inst::Const(Lit::Str(s)));
                    news.push(cv);
                    cv
                }
                FieldSrc::ConstTrue => {
                    let cv = func.alloc_inst(Inst::Const(Lit::Bool(true)));
                    news.push(cv);
                    cv
                }
                FieldSrc::ConstNil => {
                    let cv = func.alloc_inst(Inst::Const(Lit::Nil));
                    news.push(cv);
                    cv
                }
                FieldSrc::CellLoad(root) => {
                    let rc = get_field_cell(func, root, fname);
                    let lv = func.alloc_inst(Inst::Load(rc));
                    news.push(lv);
                    lv
                }
            };
            let fc = get_field_cell(func, c, fname);
            if nn_field.get(&(c, fname)).copied().unwrap_or(false) {
                func.mark_nonnull_value_struct(fc);
            }
            news.push(func.alloc_inst(Inst::Store {
                cell: fc,
                value: fval,
            }));
        }
        replacement.insert(store_i, news);
    }
    // Rematerialize each escaping value-struct load L in place: `Load(cell)` ->
    // `MakeValueStruct{name, [(field, Load(field_cell))..]}`, with the field loads
    // spliced before it. Reads the same field cells the ctor store now writes;
    // a load only reached on a non-materializing path (an arena-scan MISS) runs
    // no ctor. The escaping cell is flagged non-null, so its family root is a
    // value ctor candidate carrying `value_order` -- else the family stayed whole
    // and the load is untouched (a boxed read of the intact aggregate).
    escape_loads.sort_by_key(|(l, _)| raw(*l));
    escape_loads.dedup_by_key(|(l, _)| *l);
    for (l, c) in escape_loads {
        if !is_cand(c) {
            continue;
        }
        let root = to_rep(c);
        let Some(StructId::Value(name)) = cand[raw(root) as usize] else {
            continue;
        };
        let Some((order, representation, form)) = value_order.get(&(raw(root) as usize)).cloned()
        else {
            continue;
        };
        let mut news = Vec::with_capacity(order.len() + 1);
        let mut mvs_fields = Vec::with_capacity(order.len());
        for fname in order {
            let fc = get_field_cell(func, root, fname);
            let fl = func.alloc_inst(Inst::Load(fc));
            news.push(fl);
            mvs_fields.push((fname, fl));
        }
        func.insts[l] = Inst::MakeValueStruct {
            name,
            representation,
            form,
            fields: mvs_fields,
        };
        news.push(l);
        replacement.insert(l, news);
    }
    if !replacement.is_empty() {
        let block_ids: Vec<Idx<Block>> = func.blocks.iter().map(|(id, _)| id).collect();
        for bid in block_ids {
            if !func.blocks[bid]
                .insts
                .iter()
                .any(|i| replacement.contains_key(i))
            {
                continue;
            }
            let old = std::mem::take(&mut func.blocks[bid].insts);
            let mut new = Vec::with_capacity(old.len());
            for i in old {
                match replacement.get(&i) {
                    Some(news) => new.extend_from_slice(news),
                    None => new.push(i),
                }
            }
            func.blocks[bid].insts = new;
        }
    }
    Ok(true)
}

/// The intra-block copy-propagation analysis: the value->value substitution
/// chain, plus the stores the same walk proves are no-ops.
struct CopyProp {
    subst: Vec<Option<Idx<Inst>>>,
    /// Per-inst flags; a true entry is a `Store` writing back the value its cell
    /// already holds, i.e. removable even though the cell still has loads.
    self_stores: Vec<bool>,
    /// Projections proven not to raise; their abandoned nodes need not survive DCE.
    nonthrowing: Vec<bool>,
}

/// Build the analysis without applying it. Returns `None` when the walk proves
/// nothing (the common case for small bodies), so callers skip the remap
/// entirely. [`cleanup`] folds the remap into DCE's count scan.
fn copy_propagate_analysis(func: &MirFunc, ws: Option<&Workspace>) -> Option<CopyProp> {
    // A cell STORED inside a closure body can be mutated indirectly by ANY call
    // (the call may invoke that closure — e.g. `with_callback(() => { count = .. })`),
    // so its cached value is not stable across calls. Exclude such cells entirely
    // (treat as opaque). Closure bodies are the blocks NOT reachable from entry.
    let main: HashSet<Idx<Block>> = top_level_blocks(func).into_iter().collect();
    // Dense per-id side-tables (raw-keyed) replace the Idx HashMaps/HashSets:
    // `escaping` cell flag, `subst` value->value chain, and per-block `cur`
    // cell->value. `cur` is reset between blocks via a touched-list so the clear
    // stays O(stores-in-block), not O(cells).
    // A `ref`-borrowed cell is written through an alias this pass cannot see.
    let mut escaping: Vec<bool> = func.cells.iter().map(|(_, cell)| cell.borrowed).collect();
    for (bid, b) in func.blocks.iter() {
        if main.contains(&bid) {
            continue;
        }
        for &i in &b.insts {
            if let Inst::Store { cell, .. } = &func.insts[i] {
                escaping[raw(*cell) as usize] = true;
            }
        }
    }
    // A leaf write THROUGH a value field (`h.p.x = 5`, base `Field(Load(c), p)`)
    // mutates the cell's contents in place, so its stored value is not stable for
    // the rest of the block. Forwarding the load would move the write onto the
    // ctor -- a temporary -- while the `Field`-of-ctor fold below sends later
    // reads to the ORIGINAL construction operands, disconnecting the two. Keeping
    // the cell also lets SROA peel it on the next round, where the write becomes a
    // leaf-cell store. Same exclusion as `escaping`, different reason.
    //
    // Only cells holding a VALUE qualify. A direct `obj.f = v` on a cell holding
    // a `MakeStruct` table targets one runtime object shared by every use, so
    // forwarding stays sound (the fold below excludes tables with any other
    // use) -- excluding those keeps the hot `obj.field = v` shape
    // copy-propagated. SROA peels one level per round, so the base is a `Field`
    // chain on the first round and a bare `Load` of the peeled value cell after.
    let mut holds_value = vec![false; func.cells.len()];
    let mut has_fresh_projection = false;
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            if let Inst::Field { base, .. } = &func.insts[i] {
                has_fresh_projection |= matches!(func.insts[*base], Inst::MakeStruct { .. });
            }
            if let Inst::Store { cell, value } = &func.insts[i] {
                if matches!(&func.insts[*value], Inst::MakeValueStruct { .. }) {
                    holds_value[raw(*cell) as usize] = true;
                }
            }
        }
    }
    // Only direct temporary projections need use counts. An aliased mutable
    // constructor cannot be forwarded using a pre-substitution count.
    let struct_uses = if has_fresh_projection {
        let mut uses = vec![0u32; func.insts.len()];
        for (_, b) in func.blocks.iter() {
            for &i in &b.insts {
                func.insts[i].uses_each(&mut |op| uses[raw(op) as usize] += 1);
            }
            b.term.uses_each(&mut |op| uses[raw(op) as usize] += 1);
        }
        uses
    } else {
        Vec::new()
    };
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            let Inst::StoreField { base, .. } = &func.insts[i] else {
                continue;
            };
            // `d.o.i.a = 9` nests the reads; the write lands in the root cell.
            let mut cur = *base;
            loop {
                match &func.insts[cur] {
                    Inst::Load(c) => {
                        if holds_value[raw(*c) as usize] {
                            escaping[raw(*c) as usize] = true;
                        }
                        break;
                    }
                    Inst::Field { base, .. } => cur = *base,
                    _ => break,
                }
            }
        }
    }
    let mut subst: Vec<Option<Idx<Inst>>> = vec![None; func.insts.len()];
    let mut self_stores = vec![false; func.insts.len()];
    let mut nonthrowing = vec![false; func.insts.len()];
    let mut any_subst = false;
    let mut cur: Vec<Option<Idx<Inst>>> = vec![None; func.cells.len()];
    let mut touched: Vec<usize> = Vec::new();
    for (_, b) in func.blocks.iter() {
        for &t in &touched {
            cur[t] = None;
        }
        touched.clear();
        let mut checked_native_buffers = HashSet::default();
        for &id in &b.insts {
            match &func.insts[id] {
                Inst::Call { func: target, args }
                    if args.len() == 2
                        && ws.is_some_and(|ws| {
                            let module = ws.module(target.module);
                            let f = &module.functions[target.func];
                            bone_semantic::prelude::is_stdlib_module_file(module.file.as_str())
                                && f.name.as_str() == "__nb_checked_index"
                                && f.inline_body.is_some()
                                && f.annotations.iter().any(|a| a.is_builtin("nomangle"))
                        }) =>
                {
                    checked_native_buffers.insert(resolve_subst(&subst, args[0]));
                }
                Inst::Store { cell, value } if !escaping[raw(*cell) as usize] => {
                    let ck = raw(*cell) as usize;
                    let v = resolve_subst(&subst, *value);
                    // A store that writes back the value the cell already holds is a
                    // no-op -- invisible to `is_dead_rc`, which only drops a store
                    // whose cell has no loads. `cur` is the value last stored to this
                    // cell in THIS block, so comparing against it rejects the
                    // load-then-other-store shape: a load taken BEFORE an intervening
                    // store is not the cell's current value, and storing it back is a
                    // RESTORE, not a no-op. Before the block's first store `cur` is
                    // None, so a load of the cell itself is the value the store writes
                    // (no store can have intervened), which is the proof there.
                    let noop = match cur[ck] {
                        Some(w) => resolve_subst(&subst, w) == v,
                        None => matches!(&func.insts[*value], Inst::Load(d) if *d == *cell),
                    };
                    if noop {
                        self_stores[raw(id) as usize] = true;
                    } else {
                        if cur[ck].is_none() {
                            touched.push(ck);
                        }
                        cur[ck] = Some(v);
                    }
                }
                Inst::Load(c) => {
                    if let Some(v) = cur[raw(*c) as usize] {
                        subst[raw(id) as usize] = Some(v);
                        any_subst = true;
                    }
                }
                // `Field(<value-struct ctor>, f) -> the ctor's operand for f`. A
                // `value` cdata (incl. a cdata ENUM, which packs to a `MakeValueStruct`
                // `<Enum>_ct(tag, _0)`) is IMMUTABLE, so its field is always the
                // construction operand -- sound to forward regardless of intervening
                // code. Kills the repack-to-match cdata: a scalar-ABI enum param that
                // `match` rebuilds (`X_ct(tag,_0).__tag`) folds to the tag leaf, and the
                // now-dead ctor DCEs. A mutable `MakeStruct` needs the stricter
                // single-use proof below, since a store could change its field.
                Inst::Field { base, field } => {
                    let (base, field) = (*base, *field);
                    let rb = resolve_subst(&subst, base);
                    if checked_native_buffers.contains(&rb)
                        && skull_mir::lower::is_leaf_view_field(field.as_str())
                    {
                        // The retained bounds check already accessed this exact
                        // buffer. Its generated view keys are plain table fields.
                        nonthrowing[raw(id) as usize] = true;
                    }
                    if let Inst::MakeValueStruct { fields, form, .. } = &func.insts[rb] {
                        nonthrowing[raw(id) as usize] =
                            fields.iter().any(|(name, _)| *name == field)
                                || (*form == ValueCtorForm::Leaves
                                    && fields.iter().any(|(name, _)| {
                                        name.as_str()
                                            .strip_prefix(field.as_str())
                                            .is_some_and(|tail| tail.starts_with('.'))
                                    }));
                        if let Some((_, op)) = fields.iter().find(|(n, _)| *n == field) {
                            subst[raw(id) as usize] = Some(resolve_subst(&subst, *op));
                            any_subst = true;
                        }
                    } else if base == rb
                        && !struct_uses.is_empty()
                        && struct_uses[raw(base) as usize] == 1
                        && let Inst::MakeStruct { fields, .. } = &func.insts[base]
                        && let Some((_, op)) = fields.iter().find(|(name, _)| *name == field)
                    {
                        // The fresh object is used only by this field read: no
                        // alias can mutate it or observe its identity. Operands
                        // have already run in source order, including unused ones.
                        nonthrowing[raw(id) as usize] = true;
                        subst[raw(id) as usize] = Some(resolve_subst(&subst, *op));
                        any_subst = true;
                    } else if let Some((ctor, prefix)) = leaf_form_path(func, &subst, rb) {
                        // `Field(Field(ctor, c0), x)` is the ctor's `c0.x` leaf.
                        let Inst::MakeValueStruct { fields, .. } = &func.insts[ctor] else {
                            unreachable!("leaf_form_path roots at a value ctor")
                        };
                        let key = format!("{prefix}.{field}");
                        nonthrowing[raw(id) as usize] = fields.iter().any(|(name, _)| {
                            name.as_str() == key
                                || name
                                    .as_str()
                                    .strip_prefix(&key)
                                    .is_some_and(|tail| tail.starts_with('.'))
                        });
                        if let Some((_, op)) = fields.iter().find(|(n, _)| n.as_str() == key) {
                            subst[raw(id) as usize] = Some(resolve_subst(&subst, *op));
                            any_subst = true;
                        }
                    }
                }
                // `TupleIndex(<tuple ctor>, i) -> the ctor's i-th operand`. A Rock
                // tuple has no assignable place, so a `MakeTuple` is immutable for
                // the same reason a `MakeValueStruct` is. This is what lets a
                // combinator's `let (ok, x) = next(inner)` reach the element the
                // mechanism-I repack built: without it the element stays behind an
                // un-forwarded `TupleIndex`, and the presence test on it is a
                // whole-value use that pins the box `nobox` forbids.
                Inst::TupleIndex { base, index } => {
                    let (base, index) = (*base, *index);
                    let rb = resolve_subst(&subst, base);
                    if let Inst::MakeTuple(vs) = &func.insts[rb] {
                        if let Some(op) = vs.get(index) {
                            nonthrowing[raw(id) as usize] = true;
                            subst[raw(id) as usize] = Some(resolve_subst(&subst, *op));
                            any_subst = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    (any_subst || self_stores.iter().any(|&noop| noop) || nonthrowing.iter().any(|&safe| safe))
        .then_some(CopyProp {
            subst,
            self_stores,
            nonthrowing,
        })
}

/// The value ctor and dotted child path `x` reads when it is an unfolded
/// `Field` chain over a leaf-form `MakeValueStruct`: that ctor keys every leaf
/// by its plan path (`c0.x`), so `Field(ctor, c0)` has no operand of its own.
fn leaf_form_path(
    func: &MirFunc,
    subst: &[Option<Idx<Inst>>],
    x: Idx<Inst>,
) -> Option<(Idx<Inst>, String)> {
    let Inst::Field { base, field } = &func.insts[x] else {
        return None;
    };
    let rb = resolve_subst(subst, *base);
    match &func.insts[rb] {
        Inst::MakeValueStruct {
            form: ValueCtorForm::Leaves,
            ..
        } => Some((rb, field.to_string())),
        Inst::MakeValueStruct { .. } => None,
        _ => leaf_form_path(func, subst, rb).map(|(ctor, p)| (ctor, format!("{p}.{field}"))),
    }
}

/// Follow a copy-prop substitution chain to its final value (guards self-loops).
fn resolve_subst(subst: &[Option<Idx<Inst>>], mut x: Idx<Inst>) -> Idx<Inst> {
    while let Some(y) = subst[raw(x) as usize] {
        if y == x {
            break;
        }
        x = y;
    }
    x
}

/// Whether instruction `i` is removable given the current reference counts: a
/// pure value-producing inst with no users (`use_count == 0`), or a `Store` to a
/// never-loaded cell (`load_count == 0`). `Call`/`CallValue` produce a value but
/// have side effects, so an unused result does NOT make them dead; a bounds-
/// checked `Index` read can error, so a discarded read must still run.
fn is_dead_rc(inst: &Inst, i: Idx<Inst>, use_count: &[u32], load_count: &[u32]) -> bool {
    match inst {
        Inst::Store { cell, .. } => load_count[raw(*cell) as usize] == 0,
        _ => is_dead_value(inst) && use_count[raw(i) as usize] == 0,
    }
}

/// Field name for a frame cell in the state table.
fn raw<T>(i: Idx<T>) -> u32 {
    i.into_raw().into()
}

/// The physical frame layout chosen for one transformed coroutine body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoroutineFramePlan {
    frame_cells: HashSet<Idx<Cell>>,
    frame_slots: HashMap<Idx<Cell>, u32>,
    spill_values: Vec<Idx<Inst>>,
    layout_hash: i64,
}

impl CoroutineFramePlan {
    fn new(
        frame_cells: HashSet<Idx<Cell>>,
        frame_slots: HashMap<Idx<Cell>, u32>,
        spill_values: Vec<Idx<Inst>>,
        layout_hash: i64,
    ) -> Self {
        Self {
            frame_cells,
            frame_slots,
            spill_values,
            layout_hash,
        }
    }

    pub fn fields(&self) -> &HashSet<Idx<Cell>> {
        &self.frame_cells
    }

    pub fn slots(&self) -> &HashMap<Idx<Cell>, u32> {
        &self.frame_slots
    }

    pub fn slot(&self, cell: Idx<Cell>) -> Option<u32> {
        self.frame_slots.get(&cell).copied()
    }

    pub fn spill_values(&self) -> &[Idx<Inst>] {
        &self.spill_values
    }

    pub fn layout_hash(&self) -> i64 {
        self.layout_hash
    }
}

pub struct CoroLowered {
    pub step: MirFunc,
    pub s_cell: Idx<Cell>,
    pub frame_plan: CoroutineFramePlan,
    /// Declared param cells (constructor seeds a state-table field for each).
    pub params: Vec<Idx<Cell>>,
    pub num_states: usize,
    /// `on_killed`/`on_resume`/`on_hot_reload` hooks peeled to the constructor:
    /// `(kind, detached body entry)`. The body is emitted as its own ordinary
    /// `MirFunc`, just like a closure sibling.
    pub hooks: Vec<(CoroHookKind, Idx<Block>)>,
    /// The scope tree + ownership built (once) for frame membership, threaded to
    /// `emit_named_framed` so the step's hoist plan reuses them instead of
    /// rebuilding on the post-rewrite CFG (they are stable across the rewrite).
    pub step_tree: crate::scope::ScopeTree,
    pub step_own: crate::scope::Ownership,
}

/// FNV-1a 32 over the canonical layout string. 32-bit so the constant is
/// exactly representable in a Lua number.
fn fnv1a32(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Append `inst` to block `bid` of `f`, returning its id.
fn emit(f: &mut MirFunc, bid: Idx<Block>, inst: Inst) -> Idx<Inst> {
    let id = f.alloc_inst(inst);
    f.blocks[bid].insts.push(id);
    id
}

/// Successor blocks of a terminator (NOT closure entries — those are operands).
/// Visit each successor block of `term` (callback form — no per-call `Vec`
/// alloc; the sole caller, `top_level_blocks`, only iterates).
pub(crate) fn term_succs(term: &Term, mut f: impl FnMut(Idx<Block>)) {
    match term {
        Term::Goto(b) => f(*b),
        Term::Branch {
            then_bb, else_bb, ..
        } => {
            f(*then_bb);
            f(*else_bb);
        }
        Term::Switch { cases, default, .. } => {
            for (_, b) in cases {
                f(*b);
            }
            f(*default);
        }
        Term::Yield { resume, .. } => f(*resume),
        Term::Return(_) | Term::Unreachable => {}
    }
}

/// Blocks reachable from the function entry via terminators — the co-fn's own
/// CFG, EXCLUDING closure sub-blocks (reached only through `Inst::Lambda`).
pub(crate) fn top_level_blocks(f: &MirFunc) -> Vec<Idx<Block>> {
    let mut seen = HashSet::default();
    seen.reserve(f.blocks.len());
    let mut stack = vec![f.entry];
    seen.insert(f.entry);
    let mut order = Vec::with_capacity(f.blocks.len());
    while let Some(b) = stack.pop() {
        order.push(b);
        term_succs(&f.blocks[b].term, |s| {
            if seen.insert(s) {
                stack.push(s);
            }
        });
    }
    order
}

/// Remap an instruction's value operands through `m`.
pub(crate) fn remap_inst(inst: &mut Inst, m: &impl Fn(Idx<Inst>) -> Idx<Inst>) {
    let mut f = |x: &mut Idx<Inst>| *x = m(*x);
    match inst {
        Inst::Const(_)
        | Inst::Global(_)
        | Inst::Load(_)
        | Inst::Lambda { .. }
        | Inst::CoroHook { .. } => {}
        Inst::FfiViewCast { ptr, .. } => f(ptr),
        Inst::IntAdd(a, b)
        | Inst::IntSub(a, b)
        | Inst::IntMul(a, b)
        | Inst::IntDiv(a, b)
        | Inst::IntMod(a, b)
        | Inst::FloatAdd(a, b)
        | Inst::FloatSub(a, b)
        | Inst::FloatMul(a, b)
        | Inst::FloatDiv(a, b)
        | Inst::FloatMod(a, b)
        | Inst::StringConcat(a, b)
        | Inst::IntLt(a, b)
        | Inst::IntLeq(a, b)
        | Inst::IntGt(a, b)
        | Inst::IntGeq(a, b)
        | Inst::UintAdd(a, b)
        | Inst::UintSub(a, b)
        | Inst::UintMul(a, b)
        | Inst::UintDiv(a, b)
        | Inst::UintMod(a, b)
        | Inst::UintLt(a, b)
        | Inst::UintLeq(a, b)
        | Inst::UintGt(a, b)
        | Inst::UintGeq(a, b)
        | Inst::FloatLt(a, b)
        | Inst::FloatLeq(a, b)
        | Inst::FloatGt(a, b)
        | Inst::FloatGeq(a, b)
        | Inst::Eq(a, b)
        | Inst::NotEq(a, b)
        | Inst::BitAnd(a, b)
        | Inst::BitOr(a, b)
        | Inst::BitXor(a, b)
        | Inst::Shl(a, b)
        | Inst::Shr(a, b)
        | Inst::UintBitAnd(a, b)
        | Inst::UintBitOr(a, b)
        | Inst::UintBitXor(a, b)
        | Inst::UintShl(a, b)
        | Inst::UintShr(a, b) => {
            f(a);
            f(b);
        }
        Inst::Neg(x)
        | Inst::Not(x)
        | Inst::BitNot(x)
        | Inst::UintBitNot(x)
        | Inst::ToString(x)
        | Inst::FloatToInt(x)
        | Inst::IntToFloat(x)
        | Inst::UintToFloat(x)
        | Inst::FloatToF32(x)
        | Inst::IntToUint(x)
        | Inst::UintToInt(x)
        | Inst::FloatToUint(x)
        | Inst::UintBitcastInt(x)
        | Inst::IntBitcastUint(x)
        | Inst::CoroSetInner(x)
        | Inst::VecStrideNilToFalse(x)
        | Inst::VecStrideFalseToNil(x) => f(x),
        Inst::Format { value, .. } => f(value),
        Inst::FormatDyn { value, fmt } => {
            f(value);
            f(fmt);
        }
        Inst::Call { args, .. }
        | Inst::CallMulti { args, .. }
        | Inst::CallBoundMulti { args, .. } => args.iter_mut().for_each(&mut f),
        Inst::CallValue { callee, args } => {
            f(callee);
            args.iter_mut().for_each(&mut f);
        }
        Inst::CallValueMulti { callee, args, .. } => {
            f(callee);
            args.iter_mut().for_each(&mut f);
        }
        Inst::MakeTuple(vs) | Inst::MakeVec(vs) => vs.iter_mut().for_each(&mut f),
        Inst::MakeVecColumns {
            columns, capacity, ..
        } => {
            f(capacity);
            columns.iter_mut().flat_map(|(_, vs)| vs).for_each(&mut f)
        }
        Inst::MakeVecColumnHeader { capacity, .. } => f(capacity),
        Inst::MakeVecStrided {
            values, capacity, ..
        } => {
            f(capacity);
            values.iter_mut().for_each(&mut f);
        }
        Inst::MakeVecStridedHeader { capacity, .. } => f(capacity),
        Inst::MakeStruct { fields, .. }
        | Inst::MakeValueStruct { fields, .. }
        | Inst::MaterializeValueStruct { fields, .. }
        | Inst::MakeEnv { fields, .. } => fields.iter_mut().for_each(|(_, a)| f(a)),
        Inst::MakeNullableValue { present, value } => {
            f(present);
            f(value);
        }
        Inst::MakeEnum {
            positional, named, ..
        } => {
            positional.iter_mut().for_each(&mut f);
            named.iter_mut().for_each(|(_, a)| f(a));
        }
        Inst::MakeEnumDyn {
            tag,
            positional,
            named,
            ..
        } => {
            f(tag);
            positional.iter_mut().for_each(&mut f);
            named.iter_mut().for_each(|(_, a)| f(a));
        }
        Inst::FfiNewArray { count, .. } => f(count),
        Inst::Field { base, .. } | Inst::TupleIndex { base, .. } => f(base),
        Inst::Index { base, index, .. } => {
            f(base);
            f(index);
        }
        Inst::VecCheckedIndex { vec, index } => {
            f(vec);
            f(index);
        }
        Inst::Len { base, .. } => f(base),
        Inst::Store { value, .. } => f(value),
        Inst::StoreField { base, value, .. } => {
            f(base);
            f(value);
        }
        Inst::StoreIndex {
            base, index, value, ..
        } => {
            f(base);
            f(index);
            f(value);
        }
        Inst::StoreGlobal { value, .. } => f(value),
        Inst::ArrayPush { arr, value, .. } => {
            f(arr);
            f(value);
        }
        Inst::MapNext { table, .. } => f(table),
    }
}

/// Value operand of a terminator (the only ones are `cond`/`discr`/return value).
fn term_value(term: &Term) -> Option<Idx<Inst>> {
    match term {
        // Coroutine returns are single/zero by the time spill runs (a co fn never
        // scalarizes its completion value); the first covers it.
        Term::Return(vs) => vs.first().copied(),
        Term::Branch { cond, .. } => Some(*cond),
        Term::Switch { discr, .. } => Some(*discr),
        Term::Yield { values, .. } => values.first().copied(),
        _ => None,
    }
}

pub(crate) fn remap_term(term: &mut Term, m: &impl Fn(Idx<Inst>) -> Idx<Inst>) {
    match term {
        Term::Return(vs) => vs.iter_mut().for_each(|v| *v = m(*v)),
        Term::Branch { cond: v, .. } | Term::Switch { discr: v, .. } => *v = m(*v),
        Term::Yield { values, .. } => values.iter_mut().for_each(|v| *v = m(*v)),
        _ => {}
    }
}

fn vfield(v: Idx<Inst>) -> ustr::Ustr {
    ustr::Ustr::from(format!("v{}", raw(v)).as_str())
}

/// Spill a CROSS-BLOCK value that is also live across a suspension to the state
/// table at its def and reload it at each using block. Values crossing ordinary
/// branches stay as step locals; only a value live into a yield's resume block
/// needs persistence. Constants are rematerialized at each use block: they have
/// no evaluation-time identity to preserve, and polymorphic null has no
/// standalone physical type for a frame field. Returns the sorted spilled
/// values; their `v<N>` fields are part of the frame layout stamp.
fn spill_cross_block_values(
    step: &mut MirFunc,
    s_cell: Idx<Cell>,
    block_ids: &[Idx<Block>],
    resume_blocks: &[Idx<Block>],
) -> Vec<Idx<Inst>> {
    // def block of every value.
    let mut def_block: HashMap<Idx<Inst>, Idx<Block>> = HashMap::default();
    for &bid in block_ids {
        for &id in &step.blocks[bid].insts {
            if step.insts[id].produces_value() {
                def_block.insert(id, bid);
            }
        }
    }
    // cross_block_values = values used in a block other than where defined.
    let mut cross_block_values: HashSet<Idx<Inst>> = HashSet::default();
    for &bid in block_ids {
        let mut used: Vec<Idx<Inst>> = Vec::new();
        for &id in &step.blocks[bid].insts {
            step.insts[id].uses_each(&mut |v| used.push(v));
        }
        used.extend(term_value(&step.blocks[bid].term));
        for v in used {
            if def_block.get(&v).copied() != Some(bid) {
                cross_block_values.insert(v);
            }
        }
    }
    let live_values = values_live_at_resume_blocks(step, block_ids, resume_blocks);
    let persistent_values = cross_block_values
        .intersection(&live_values)
        .copied()
        .collect::<HashSet<_>>();
    let rematerialized = persistent_values
        .iter()
        .copied()
        .filter(|value| matches!(step.insts[*value], Inst::Const(_)))
        .collect::<HashSet<_>>();
    let frame_values = persistent_values
        .difference(&rematerialized)
        .copied()
        .collect::<HashSet<_>>();
    if persistent_values.is_empty() {
        return Vec::new();
    }

    for &bid in block_ids {
        // Reloads needed in this block: frame values used here but defined
        // elsewhere. Sorted for deterministic output.
        let mut used_here: Vec<Idx<Inst>> = Vec::new();
        {
            let mut seen = HashSet::default();
            for &id in &step.blocks[bid].insts {
                step.insts[id].uses_each(&mut |v| {
                    if persistent_values.contains(&v)
                        && def_block.get(&v).copied() != Some(bid)
                        && seen.insert(v)
                    {
                        used_here.push(v);
                    }
                });
            }
            if let Some(v) = term_value(&step.blocks[bid].term) {
                if persistent_values.contains(&v)
                    && def_block.get(&v).copied() != Some(bid)
                    && seen.insert(v)
                {
                    used_here.push(v);
                }
            }
        }
        used_here.sort_by_key(|v| raw(*v));

        let old: Vec<Idx<Inst>> = std::mem::take(&mut step.blocks[bid].insts);
        let mut reload: HashMap<Idx<Inst>, Idx<Inst>> = HashMap::default();
        let mut new_list: Vec<Idx<Inst>> = Vec::new();
        for v in used_here {
            let rv = if rematerialized.contains(&v) {
                step.alloc_inst(step.insts[v].clone())
            } else {
                let sv = step.alloc_inst(Inst::Load(s_cell));
                new_list.push(sv);
                step.alloc_inst(Inst::Field {
                    base: sv,
                    field: vfield(v),
                })
            };
            new_list.push(rv);
            reload.insert(v, rv);
        }
        let m = |x: Idx<Inst>| reload.get(&x).copied().unwrap_or(x);
        for id in old {
            remap_inst(&mut step.insts[id], &m);
            new_list.push(id);
            // Spill right after a frame value's def.
            if frame_values.contains(&id) && def_block.get(&id) == Some(&bid) {
                let sv = step.alloc_inst(Inst::Load(s_cell));
                let st = step.alloc_inst(Inst::StoreField {
                    base: sv,
                    field: vfield(id),
                    value: id,
                });
                new_list.push(sv);
                new_list.push(st);
            }
        }
        step.blocks[bid].insts = new_list;
        let mut term = step.blocks[bid].term.clone();
        remap_term(&mut term, &m);
        step.blocks[bid].term = term;
    }
    let mut values: Vec<Idx<Inst>> = frame_values.into_iter().collect();
    values.sort_by_key(|value| raw(*value));
    values
}

/// Cells needed on the far side of at least one suspension. Ordinary CFG
/// branches do not make a cell persistent; only a cell live into a yield's
/// resume block does. The dataflow is deliberately cell-only because MIR
/// values crossing blocks already use `spill_cross_block_values`.
fn cells_live_at_yield_resumes(func: &MirFunc, blocks: &[Idx<Block>]) -> HashSet<Idx<Cell>> {
    let mut block_use: HashMap<Idx<Block>, HashSet<Idx<Cell>>> = HashMap::default();
    let mut block_def: HashMap<Idx<Block>, HashSet<Idx<Cell>>> = HashMap::default();
    for &block in blocks {
        let mut uses = HashSet::default();
        let mut defs = HashSet::default();
        for &id in &func.blocks[block].insts {
            match &func.insts[id] {
                Inst::Load(cell) => {
                    if !defs.contains(cell) {
                        uses.insert(*cell);
                    }
                }
                Inst::Store { cell, .. } => {
                    defs.insert(*cell);
                }
                Inst::CallMulti { dests, .. }
                | Inst::CallValueMulti { dests, .. }
                | Inst::CallBoundMulti { dests, .. } => {
                    defs.extend(dests.iter().copied());
                }
                Inst::MapNext {
                    key_cell, val_cell, ..
                } => {
                    if !defs.contains(key_cell) {
                        uses.insert(*key_cell);
                    }
                    defs.insert(*key_cell);
                    defs.insert(*val_cell);
                }
                _ => {}
            }
        }
        block_use.insert(block, uses);
        block_def.insert(block, defs);
    }

    let mut live_in: HashMap<Idx<Block>, HashSet<Idx<Cell>>> = blocks
        .iter()
        .copied()
        .map(|block| (block, HashSet::default()))
        .collect();
    let mut live_out: HashMap<Idx<Block>, HashSet<Idx<Cell>>> = blocks
        .iter()
        .copied()
        .map(|block| (block, HashSet::default()))
        .collect();
    loop {
        let mut changed = false;
        for &block in blocks.iter().rev() {
            let mut out = HashSet::default();
            term_succs(&func.blocks[block].term, |succ| {
                if let Some(next) = live_in.get(&succ) {
                    out.extend(next.iter().copied());
                }
            });
            let mut input = block_use[&block].clone();
            input.extend(out.difference(&block_def[&block]).copied());
            if live_out
                .insert(block, out.clone())
                .is_some_and(|old| old != out)
            {
                changed = true;
            }
            if live_in
                .insert(block, input.clone())
                .is_some_and(|old| old != input)
            {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut live = HashSet::default();
    for &block in blocks {
        if let Term::Yield { resume, .. } = func.blocks[block].term {
            live.extend(live_in[&resume].iter().copied());
        }
    }
    live
}

/// Cells used by detached defer/hook regions must remain in the shared frame;
/// those regions run independently of the current step segment.
fn cells_referenced_by_detached_regions(
    func: &MirFunc,
    entries: impl IntoIterator<Item = Idx<Block>>,
) -> HashSet<Idx<Cell>> {
    let mut cells = HashSet::default();
    for entry in entries {
        for block in detached_region_blocks(func, entry) {
            for &id in &func.blocks[block].insts {
                func.insts[id].cells_each(&mut |cell| {
                    cells.insert(cell);
                });
            }
        }
    }
    cells
}

/// Values live into resume blocks, used to narrow the cross-block spill
/// candidates to values that must survive a suspension rather than a branch.
fn values_live_at_resume_blocks(
    func: &MirFunc,
    blocks: &[Idx<Block>],
    resume_blocks: &[Idx<Block>],
) -> HashSet<Idx<Inst>> {
    let mut block_use: HashMap<Idx<Block>, HashSet<Idx<Inst>>> = HashMap::default();
    let mut block_def: HashMap<Idx<Block>, HashSet<Idx<Inst>>> = HashMap::default();
    for &block in blocks {
        let mut uses = HashSet::default();
        let mut defs = HashSet::default();
        for &id in &func.blocks[block].insts {
            func.insts[id].uses_each(&mut |value| {
                if !defs.contains(&value) {
                    uses.insert(value);
                }
            });
            if func.insts[id].produces_value() {
                defs.insert(id);
            }
        }
        func.blocks[block].term.uses_each(&mut |value| {
            if !defs.contains(&value) {
                uses.insert(value);
            }
        });
        block_use.insert(block, uses);
        block_def.insert(block, defs);
    }

    let mut live_in: HashMap<Idx<Block>, HashSet<Idx<Inst>>> = blocks
        .iter()
        .copied()
        .map(|block| (block, HashSet::default()))
        .collect();
    let mut live_out: HashMap<Idx<Block>, HashSet<Idx<Inst>>> = blocks
        .iter()
        .copied()
        .map(|block| (block, HashSet::default()))
        .collect();
    loop {
        let mut changed = false;
        for &block in blocks.iter().rev() {
            let mut out = HashSet::default();
            term_succs(&func.blocks[block].term, |succ| {
                if let Some(next) = live_in.get(&succ) {
                    out.extend(next.iter().copied());
                }
            });
            let mut input = block_use[&block].clone();
            input.extend(out.difference(&block_def[&block]).copied());
            if live_out
                .insert(block, out.clone())
                .is_some_and(|old| old != out)
            {
                changed = true;
            }
            if live_in
                .insert(block, input.clone())
                .is_some_and(|old| old != input)
            {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut live = HashSet::default();
    for &resume in resume_blocks {
        live.extend(live_in[&resume].iter().copied());
    }
    live
}

/// Transform a lowered `co fn` into its `__step` state machine + frame info.
pub fn transform_coroutine(
    func: &MirFunc,
    channel: Option<&skull_mir::lower::YieldChannel>,
    has_return_value: bool,
) -> CoroLowered {
    transform_coroutine_with_nonescaping_lambdas(
        func,
        channel,
        has_return_value,
        &HashSet::default(),
    )
}

/// Transform a coroutine while excluding captures proven to be consumed by a
/// synchronous callback call from the persistent frame. The proof is supplied
/// by the backend that owns the callable ABI; an empty set preserves the
/// conservative frame pin for backends without that proof.
pub fn transform_coroutine_with_nonescaping_lambdas(
    func: &MirFunc,
    channel: Option<&skull_mir::lower::YieldChannel>,
    has_return_value: bool,
    nonescaping_lambdas: &HashSet<Idx<Inst>>,
) -> CoroLowered {
    let frame_slot_channel = channel.is_some();
    let mut step = func.clone();
    let resume_arg = func.resume_arg.expect("co fn must carry a resume-arg cell");
    let params = func.params.clone();
    let s_cell = step.new_cell(Some(ustr::Ustr::from("s")));
    step.cells[s_cell].capture_identity =
        Some(skull_mir::code_identity::CaptureIdentity::Generated {
            role: ustr::Ustr::from("coroutine-frame"),
        });

    // Only the co-fn's TOP-LEVEL blocks are transformed — NOT closure sub-blocks
    // (which share the arena but are their own CFGs, reached via `Inst::Lambda`,
    // not terminator successors). A closure body must keep its own `return`s and
    // never get coroutine state machinery; it reads captured frame cells as
    // `s.<field>` purely via codegen's `FrameCtx`.
    let orig_blocks: Vec<Idx<Block>> = top_level_blocks(&step);

    // 0. Peel `on_killed`/`on_resume` hook registrations to the constructor so a
    // kill before the first resume still fires (the body would otherwise run the
    // registration only on first resume). Both the `CoroHook` effect and its
    // lambda `Inst` are removed from every block's inst list; the lambda stays in
    // the arena (its body is hoisted to a top-level `<name>__on_<hook>` fn).
    let mut hooks: Vec<(CoroHookKind, Idx<Block>)> = Vec::new();
    let mut hook_lambdas: HashSet<Idx<Inst>> = HashSet::default();
    for &bid in &orig_blocks {
        for &id in &step.blocks[bid].insts {
            if let Inst::CoroHook { kind, lambda } = step.insts[id] {
                let Inst::Lambda { entry, .. } = step.insts[lambda] else {
                    panic!("co-fn hook {} must point at a lambda", kind.slot());
                };
                hooks.push((kind, entry));
                hook_lambdas.insert(lambda);
            }
        }
    }
    if !hooks.is_empty() {
        for &bid in &orig_blocks {
            step.blocks[bid].insts.retain(|&id| {
                !matches!(step.insts[id], Inst::CoroHook { .. }) && !hook_lambdas.contains(&id)
            });
        }
    }

    // 0b. Frame membership is liveness-based with ownership pins (P1):
    // root cells live at a resume point
    // become fields. Root cells captured by a live lambda or referenced by a
    // detached hook/defer stay fields even when the local CFG has no later use.
    // Built post-peel so hook lambdas are detached, pre-rewrite so `Yield`
    // resume edges still link the CFG.
    // The tree + ownership are built ONCE here (post-hook-peel, pre-yield-rewrite)
    // and threaded to emission through `CoroLowered` -- emission reuses them for
    // the step's hoist plan instead of rebuilding on the post-rewrite CFG. The
    // facts are stable across the yield rewrite: lambda sub-CFGs (which own every
    // hoisted cell) are untouched, and the rewrite only ADDS ROOT-region blocks +
    // `s`-field accesses, which the frame collapses to `s` anyway. Byte-identity
    // of emitted Lua is the standing oracle for that stability.
    let tree = crate::scope::ScopeTree::build(&step);
    let own = tree.analyze(&step);
    let frame_cells: HashSet<Idx<Cell>> = {
        let root = crate::scope::ROOT;
        let root_owned = |cell: &Idx<Cell>| own.owner.get(cell).is_some_and(|&owner| owner == root);
        let captured_by_escaping_lambda = own
            .captures
            .iter()
            .enumerate()
            .filter(|(scope, _)| {
                tree.scopes[*scope]
                    .lambda
                    .is_none_or(|lambda| !nonescaping_lambdas.contains(&lambda))
            })
            .flat_map(|(_, cells)| cells.iter().copied())
            .collect::<HashSet<_>>();
        let mut fields = cells_live_at_yield_resumes(&step, &orig_blocks);
        fields.extend(params.iter().copied());
        fields.extend(
            captured_by_escaping_lambda
                .into_iter()
                .filter(|cell| root_owned(cell)),
        );
        let detached_entries = step
            .coro_defers
            .iter()
            .map(|(_, entry)| *entry)
            .chain(hooks.iter().map(|(_, entry)| *entry));
        fields.extend(
            cells_referenced_by_detached_regions(&step, detached_entries)
                .into_iter()
                .filter(|cell| root_owned(cell)),
        );
        fields.retain(|cell| *cell != s_cell && *cell != resume_arg && root_owned(cell));
        fields
    };
    // Dense per-frame slots: frame cells numbered 0.. in raw-id order. Both the
    // emitted `s.c<slot>` field names and the layout canon key on the slot, so a
    // lambda-internal cell-count edit (which perturbs raw ids after the lambda)
    // leaves every frame slot fixed -- the P2 finding fix.
    let frame_slots = dense_slots(frame_cells.iter().copied());

    // 1. Rewrite `CoroSetInner(x)` -> `s.__inner = x` (rebuild affected blocks).
    for &bid in &orig_blocks {
        let has = step.blocks[bid]
            .insts
            .iter()
            .any(|&i| matches!(step.insts[i], Inst::CoroSetInner(_)));
        if !has {
            continue;
        }
        let old: Vec<Idx<Inst>> = std::mem::take(&mut step.blocks[bid].insts);
        for id in old {
            if let Inst::CoroSetInner(x) = step.insts[id] {
                let sv = step.alloc_inst(Inst::Load(s_cell));
                step.blocks[bid].insts.push(sv);
                let store = step.alloc_inst(Inst::StoreField {
                    base: sv,
                    field: ustr::Ustr::from("__inner"),
                    value: x,
                });
                step.blocks[bid].insts.push(store);
            } else {
                step.blocks[bid].insts.push(id);
            }
        }
    }

    // 2. Yields: `s.__state = k; return value`. Record (resume block) per state.
    // Per state, also record the yielded variant tag when statically known (a
    // `Const` tag leaf) -- a layout-stamp input that catches cross-variant yield
    // reordering, which cell/spill/state-count hashing alone cannot see.
    let suspension_resumes: Vec<Idx<Block>> = orig_blocks
        .iter()
        .filter_map(|&block| match step.blocks[block].term {
            Term::Yield { resume, .. } => Some(resume),
            _ => None,
        })
        .collect();
    let mut resume_of_state: Vec<Idx<Block>> = Vec::new();
    let mut state_tags: Vec<String> = Vec::new();
    let mut yield_blocks: HashSet<Idx<Block>> = HashSet::default();
    for &bid in &orig_blocks {
        if let Term::Yield { values, resume } = &step.blocks[bid].term {
            let values = values.clone();
            let resume = *resume;
            let k = resume_of_state.len() as i64 + 1;
            resume_of_state.push(resume);
            let tag = values
                .first()
                .and_then(|&v| match &step.insts[v] {
                    Inst::Const(Lit::Str(s)) => Some(s.as_str().to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| "dyn".to_string());
            state_tags.push(tag);
            yield_blocks.insert(bid);
            // Frame-slot channel (flat-enum yield): write the leaves to reused
            // `s.__y*` slots and return nothing (0 alloc per yield). The consumer
            // reads the slots at the concrete resume site. Otherwise the value(s)
            // multi-return directly (scalar / bare yield -- no box to remove).
            if frame_slot_channel {
                for (i, &v) in values.iter().enumerate() {
                    let sv = emit(&mut step, bid, Inst::Load(s_cell));
                    emit(
                        &mut step,
                        bid,
                        Inst::StoreField {
                            base: sv,
                            field: skull_mir::lower::yield_slot(i),
                            value: v,
                        },
                    );
                }
            }
            // The stamp is guarded on `state >= 0` (mirror the completion
            // stamp): a body that killed its own handle mid-run must stay
            // killed, not resurrect via a positive resume state. The killed
            // arm returns nothing -- a dead coroutine's resume yields nil.
            let sv = emit(&mut step, bid, Inst::Load(s_cell));
            let st = emit(
                &mut step,
                bid,
                Inst::Field {
                    base: sv,
                    field: ustr::Ustr::from(STATE),
                },
            );
            let zero = emit(&mut step, bid, Inst::Const(Lit::Int(0)));
            let already_terminal = emit(&mut step, bid, Inst::IntLt(st, zero));
            let killed_bb = step.new_block();
            let stamp_bb = step.new_block();
            step.blocks[bid].term = Term::Branch {
                cond: already_terminal,
                then_bb: killed_bb,
                else_bb: stamp_bb,
            };

            let kc = emit(&mut step, stamp_bb, Inst::Const(Lit::Int(k)));
            let sv2 = emit(&mut step, stamp_bb, Inst::Load(s_cell));
            emit(
                &mut step,
                stamp_bb,
                Inst::StoreField {
                    base: sv2,
                    field: ustr::Ustr::from(STATE),
                    value: kc,
                },
            );
            step.blocks[stamp_bb].term = if frame_slot_channel {
                Term::Return(Vec::new())
            } else {
                Term::Return(values)
            };
            step.blocks[killed_bb].term = Term::Return(Vec::new());
        }
    }
    let num_states = resume_of_state.len();

    // 3. Normal returns -> stash `s.__return`, mark completed (-3), bare
    // return. The stamp is guarded on `state >= 0`: a body that killed its own
    // handle mid-run (state already -1) must STAY killed -- neither clobber
    // the terminal to -3 (completed() would lie) nor keep a stashed result a
    // kill promised not to have.
    for &bid in &orig_blocks {
        if yield_blocks.contains(&bid) {
            continue;
        }
        let ret_val = match (&step.blocks[bid].term, has_return_value) {
            // A co fn's completion value is single (never scalarized); take it.
            (Term::Return(vs), true) => Some(vs.first().copied()),
            (Term::Return(_), false) => Some(None),
            _ => None,
        };
        if let Some(v0) = ret_val {
            if let Some(v) = v0 {
                let sv = emit(&mut step, bid, Inst::Load(s_cell));
                emit(
                    &mut step,
                    bid,
                    Inst::StoreField {
                        base: sv,
                        field: ustr::Ustr::from("__return"),
                        value: v,
                    },
                );
            }
            let sv = emit(&mut step, bid, Inst::Load(s_cell));
            let st = emit(
                &mut step,
                bid,
                Inst::Field {
                    base: sv,
                    field: ustr::Ustr::from(STATE),
                },
            );
            let zero = emit(&mut step, bid, Inst::Const(Lit::Int(0)));
            let already_terminal = emit(&mut step, bid, Inst::IntLt(st, zero));
            let killed_bb = step.new_block();
            let stamp_bb = step.new_block();
            step.blocks[bid].term = Term::Branch {
                cond: already_terminal,
                then_bb: killed_bb,
                else_bb: stamp_bb,
            };

            let dc = emit(&mut step, stamp_bb, Inst::Const(Lit::Int(-3)));
            let sv2 = emit(&mut step, stamp_bb, Inst::Load(s_cell));
            emit(
                &mut step,
                stamp_bb,
                Inst::StoreField {
                    base: sv2,
                    field: ustr::Ustr::from(STATE),
                    value: dc,
                },
            );
            step.blocks[stamp_bb].term = Term::Return(Vec::new());

            if v0.is_some() {
                let nil = emit(&mut step, killed_bb, Inst::Const(Lit::Nil));
                let sv3 = emit(&mut step, killed_bb, Inst::Load(s_cell));
                emit(
                    &mut step,
                    killed_bb,
                    Inst::StoreField {
                        base: sv3,
                        field: ustr::Ustr::from("__return"),
                        value: nil,
                    },
                );
            }
            step.blocks[killed_bb].term = Term::Return(Vec::new());
        }
    }

    // 3b. Spill cross-block values (loop-carried collections/bounds, multi-yield
    // args) so they survive a suspend/resume — see `spill_cross_block_values`.
    let spill_values =
        spill_cross_block_values(&mut step, s_cell, &orig_blocks, &suspension_resumes);

    // 3c. Frame layout stamp. Hash everything that fixes the frame's field
    // meanings + the state numbering: (cell idx, source name) pairs, the spill
    // field set, per-state variant tags, and the yield-channel shape. Body
    // LOGIC/values/spans are deliberately excluded so a layout-stable edit keeps
    // live coroutines running (the live-update feature). The ctor writes the
    // stamp; the step's entry (below) rejects a frame with a different one --
    // through `error()`, so the pcall in `__advance` turns it into a clean
    // errored kill.
    let mut canon = String::new();
    {
        use std::fmt::Write as _;
        // Slot order == raw-id order (dense_slots numbers by raw id, cells.iter()
        // yields by raw id), so hashing `c<slot>:<name>` here matches both the
        // emitted field names and the slot ordinals.
        for (id, cell) in step.cells.iter() {
            if let Some(&slot) = frame_slots.get(&id) {
                let _ = write!(
                    canon,
                    "c{}:{};",
                    slot,
                    cell.name
                        .map(|n| n.as_str().to_string())
                        .unwrap_or_default()
                );
            }
        }
        for &value in &spill_values {
            let _ = write!(canon, "{};", vfield(value));
        }
        for (i, t) in state_tags.iter().enumerate() {
            let _ = write!(canon, "s{i}:{t};");
        }
        match channel {
            Some(ch) => {
                let _ = write!(canon, "ch:{}:{}", ch.enum_name, ch.tuple_width);
                for rf in &ch.record_fields {
                    let _ = write!(canon, ":{rf}");
                }
            }
            None => canon.push_str("ch:-"),
        }
    }
    let layout_hash = fnv1a32(&canon) as i64;

    // 4. Dispatch entry: read s.__state, branch to each resume block; fall
    // through to the original entry (state 0). A chain of one block per state.
    let orig_entry = step.entry;
    if num_states == 0 {
        let d = step.new_block();
        step.blocks[d].term = Term::Goto(orig_entry);
        step.entry = d;
    } else {
        // Build check blocks bottom-up so each `else` targets the next check.
        let check_blocks: Vec<Idx<Block>> = (0..num_states).map(|_| step.new_block()).collect();
        for k in 0..num_states {
            let bid = check_blocks[k];
            let sv = emit(&mut step, bid, Inst::Load(s_cell));
            let st = emit(
                &mut step,
                bid,
                Inst::Field {
                    base: sv,
                    field: ustr::Ustr::from(STATE),
                },
            );
            let kc = emit(&mut step, bid, Inst::Const(Lit::Int(k as i64 + 1)));
            let eq = emit(&mut step, bid, Inst::Eq(st, kc));
            let else_bb = if k + 1 < num_states {
                check_blocks[k + 1]
            } else {
                orig_entry
            };
            step.blocks[bid].term = Term::Branch {
                cond: eq,
                then_bb: resume_of_state[k],
                else_bb,
            };
        }
        step.entry = check_blocks[0];
    }

    // 4b. Layout-stamp check ABOVE the dispatch: a frame written by a
    // different layout (a live coroutine surviving a layout-shifting reload)
    // must not be driven -- raise instead; `__advance`'s pcall turns it into a
    // clean errored kill (on_killed fires, last_error explains, the poller
    // logs). A fresh coroutine always matches (ctor + step emitted together).
    {
        let dispatch = step.entry;
        let chk = step.new_block();
        let sv = emit(&mut step, chk, Inst::Load(s_cell));
        let lay = emit(
            &mut step,
            chk,
            Inst::Field {
                base: sv,
                field: ustr::Ustr::from("__layout"),
            },
        );
        let kc = emit(&mut step, chk, Inst::Const(Lit::Int(layout_hash)));
        let ne = emit(&mut step, chk, Inst::NotEq(lay, kc));
        let raise = step.new_block();
        let err_fn = emit(&mut step, raise, Inst::Global(ustr::Ustr::from("error")));
        let msg = emit(
            &mut step,
            raise,
            Inst::Const(Lit::Str(ustr::Ustr::from(
                format!(
                    "coroutine killed: frame layout changed by hot reload ({})",
                    func.name
                )
                .as_str(),
            ))),
        );
        emit(
            &mut step,
            raise,
            Inst::CallValue {
                callee: err_fn,
                args: vec![msg],
            },
        );
        // Unreachable (error() raised); a terminator is still required.
        step.blocks[raise].term = Term::Goto(dispatch);
        step.blocks[chk].term = Term::Branch {
            cond: ne,
            then_bb: raise,
            else_bb: dispatch,
        };
        step.entry = chk;
    }

    // 5. The step is an ordinary fn `(s, __resume_arg)`.
    step.name = ustr::Ustr::from(format!("{}__step", func.name).as_str());
    step.params = vec![s_cell, resume_arg];
    step.is_coroutine = false;
    step.resume_arg = None;

    CoroLowered {
        step,
        s_cell,
        frame_plan: CoroutineFramePlan::new(frame_cells, frame_slots, spill_values, layout_hash),
        params,
        num_states,
        hooks,
        step_tree: tree,
        step_own: own,
    }
}

/// Dense per-frame slot map: the given cells numbered 0.. by raw-id order.
fn dense_slots(cells: impl Iterator<Item = Idx<Cell>>) -> HashMap<Idx<Cell>, u32> {
    let mut v: Vec<Idx<Cell>> = cells.collect();
    v.sort_by_key(|c| raw(*c));
    v.into_iter()
        .enumerate()
        .map(|(i, c)| (c, i as u32))
        .collect()
}

impl CoroLowered {
    /// Frame cells owned by the coroutine's root and therefore shared with a
    /// detached hook/defer body. Cells introduced only by that detached body
    /// remain ordinary locals in its own `MirFunc`.
    pub fn root_frame_info(&self) -> FrameInfo {
        let fields: HashSet<Idx<Cell>> = self
            .frame_plan
            .fields()
            .iter()
            .copied()
            .filter(|c| {
                self.params.contains(c)
                    || self
                        .step_own
                        .owner
                        .get(c)
                        .is_some_and(|owner| *owner == crate::scope::ROOT)
            })
            .collect();
        let slots = self
            .frame_plan
            .slots()
            .iter()
            .filter(|(cell, _)| fields.contains(cell))
            .map(|(&cell, &slot)| (cell, slot))
            .collect();
        FrameInfo {
            s_cell: self.s_cell,
            fields,
            slots,
        }
    }
}

/// Blocks reachable from a detached entry through ordinary CFG edges and
/// nested lambda entries. A detached region owns its own ordinary `MirFunc`;
/// the source co-fn arena is only used as the physical source of the clone.
fn detached_region_blocks(func: &MirFunc, entry: Idx<Block>) -> HashSet<Idx<Block>> {
    let mut seen = HashSet::default();
    let mut stack = vec![entry];
    while let Some(block) = stack.pop() {
        if !seen.insert(block) {
            continue;
        }
        for &id in &func.blocks[block].insts {
            if let Inst::Lambda { entry, .. } = func.insts[id] {
                stack.push(entry);
            }
        }
        term_succs(&func.blocks[block].term, |next| stack.push(next));
    }
    seen
}

/// Copy one hook/defer region into an ordinary MIR function and run the shared
/// closure conversion over it. The detached function receives the coroutine
/// state table as its only parameter; cells owned by the coroutine root still
/// resolve through that table, while cells introduced by the region are locals.
pub struct LoweredDetachedRegion {
    pub source: MirFunc,
    pub lowered: MirFunc,
}

pub fn lower_detached_region_with_source(
    source: &MirFunc,
    entry: Idx<Block>,
    name: &str,
    frame: &FrameInfo,
) -> LoweredDetachedRegion {
    let region = detached_region_blocks(source, entry);
    let mut aux = source.clone();
    aux.name = Ustr::from(name);
    aux.entry = entry;
    aux.code_identity = source
        .code_identity
        .as_ref()
        .and_then(skull_mir::code_identity::CodeIdentity::owner)
        .zip(detached_region_site(source, &region))
        .map(
            |(enclosing, site)| skull_mir::code_identity::CodeIdentity::Nested {
                enclosing,
                site,
                name: aux.name,
            },
        );
    aux.code_abi_fingerprint = Some(format!("detached:{name}"));
    let slots = {
        let mut slots: Vec<u32> = frame.slots.values().copied().collect();
        slots.sort_unstable();
        slots
    };
    aux.code_compatibility =
        aux.code_identity
            .as_ref()
            .map(|_| skull_mir::code_identity::CodeCompatibility {
                call_abi: format!("detached:{name}"),
                capture: skull_mir::code_identity::CaptureSchema::empty(),
                frame: Some(skull_mir::code_identity::FrameSchema { slots }),
                lambda_topology: Default::default(),
            });
    aux.params = vec![frame.s_cell];
    aux.is_coroutine = false;
    aux.resume_arg = None;
    aux.coro_defers.clear();
    aux.nested_fns.clear();
    aux.closure_env_plans.clear();
    aux.hoisted_fn_globals.clear();
    aux.code_value_identities.retain(|inst, _| {
        !matches!(aux.insts[*inst], Inst::Lambda { entry, .. } if !region.contains(&entry))
    });

    // The hook's outer lambda is a registration artifact, not a closure inside
    // the detached body. Remove it from the clone before ScopeTree sees it;
    // nested lambdas whose entries are inside the region remain for conversion.
    for (_, inst) in aux.insts.iter_mut() {
        if let Inst::Lambda {
            entry: lambda_entry,
            ..
        } = inst
            && (*lambda_entry == entry || !region.contains(lambda_entry))
        {
            *inst = Inst::Const(Lit::Nil);
        }
    }

    let source = aux.clone();
    convert_closures_framed(&mut aux, name, Some(frame), None);
    let aux_entry = aux.entry;
    materialize_frame_cells(&mut aux, aux_entry, frame.s_cell, &frame.slots);
    assert!(
        !aux.insts
            .iter()
            .any(|(_, inst)| matches!(inst, Inst::Lambda { .. })),
        "detached region `{name}` retained an Inst::Lambda after conversion"
    );
    LoweredDetachedRegion {
        source,
        lowered: aux,
    }
}

pub fn lower_detached_region(
    source: &MirFunc,
    entry: Idx<Block>,
    name: &str,
    frame: &FrameInfo,
) -> MirFunc {
    lower_detached_region_with_source(source, entry, name, frame).lowered
}

fn detached_region_site(
    source: &MirFunc,
    region: &HashSet<Idx<Block>>,
) -> Option<skull_mir::code_identity::LambdaSiteId> {
    region
        .iter()
        .flat_map(|block| source.blocks[*block].insts.iter().copied())
        .filter_map(|inst| source.inst_spans.get(inst).copied())
        .min_by_key(|span| span.offset_range.0)
        .and_then(skull_mir::code_identity::LambdaSiteId::from_span)
}

/// Remove the detached closure producers from the shared coroutine arena after
/// their ordinary `MirFunc` copies have been made. This keeps the old inline
/// emitter path structurally unreachable and gives the residual census a single
/// zero-valued answer.
pub fn erase_detached_lambdas(func: &mut MirFunc, entries: impl IntoIterator<Item = Idx<Block>>) {
    let mut detached = HashSet::default();
    for entry in entries {
        detached.extend(detached_region_blocks(func, entry));
    }
    let detached_values: Vec<_> = func
        .code_value_identities
        .keys()
        .copied()
        .filter(|inst| {
            matches!(func.insts[*inst], Inst::Lambda { entry, .. } if detached.contains(&entry))
        })
        .collect();
    for inst in detached_values {
        func.code_value_identities.remove(&inst);
    }
    for (_, inst) in func.insts.iter_mut() {
        if let Inst::Lambda { entry, .. } = inst
            && detached.contains(entry)
        {
            *inst = Inst::Const(Lit::Nil);
        }
    }
}

/// Materialize a coroutine region's frame accesses as MIR (the JS path's
/// analog of codegen-lua's root `FrameCtx` rendering): over the blocks
/// reachable from `entry` via terminators, a bare `Load`/`Store` of a cell in
/// `slots` becomes a `Field`/`StoreField` on `s` (field `c<slot>`), and a
/// multi-call dest that is a frame cell is redirected through a fresh temp
/// cell + a spliced store-back. `MapNext`'s key and value cells use the same
/// seeded-temp/store-back shape. Non-frame cells remain ordinary step locals;
/// every remaining bare frame cell is a transform finding.
pub fn materialize_frame(
    mf: &mut MirFunc,
    entry: Idx<Block>,
    s_cell: Idx<Cell>,
    slots: &HashMap<Idx<Cell>, u32>,
) {
    materialize_frame_inner(mf, entry, s_cell, slots, true);
}

/// Materialize only the selected shared frame cells in an ordinary detached
/// function. Its other cells are real locals, so the strict exhaustiveness check
/// used by the coroutine step does not apply here.
pub fn materialize_frame_cells(
    mf: &mut MirFunc,
    entry: Idx<Block>,
    s_cell: Idx<Cell>,
    slots: &HashMap<Idx<Cell>, u32>,
) {
    materialize_frame_inner(mf, entry, s_cell, slots, false);
}

fn materialize_frame_inner(
    mf: &mut MirFunc,
    entry: Idx<Block>,
    s_cell: Idx<Cell>,
    slots: &HashMap<Idx<Cell>, u32>,
    strict: bool,
) {
    let field_of = |slot: u32| ustr::Ustr::from(format!("c{slot}").as_str());
    let mut region: Vec<Idx<Block>> = Vec::new();
    {
        let mut seen: HashSet<Idx<Block>> = HashSet::default();
        let mut stack = vec![entry];
        seen.insert(entry);
        while let Some(b) = stack.pop() {
            region.push(b);
            term_succs(&mf.blocks[b].term, |t| {
                if seen.insert(t) {
                    stack.push(t);
                }
            });
        }
    }
    let mut minted: HashSet<Idx<Cell>> = HashSet::default();
    for &bid in &region {
        let old: Vec<Idx<Inst>> = std::mem::take(&mut mf.blocks[bid].insts);
        let mut new: Vec<Idx<Inst>> = Vec::with_capacity(old.len());
        for id in old {
            match mf.insts[id].clone() {
                Inst::Load(c) if slots.contains_key(&c) => {
                    let sv = mf.alloc_inst(Inst::Load(s_cell));
                    new.push(sv);
                    mf.insts[id] = Inst::Field {
                        base: sv,
                        field: field_of(slots[&c]),
                    };
                    new.push(id);
                }
                Inst::Store { cell, value } if slots.contains_key(&cell) => {
                    let sv = mf.alloc_inst(Inst::Load(s_cell));
                    new.push(sv);
                    mf.insts[id] = Inst::StoreField {
                        base: sv,
                        field: field_of(slots[&cell]),
                        value,
                    };
                    new.push(id);
                }
                Inst::CallMulti { dests, .. }
                | Inst::CallValueMulti { dests, .. }
                | Inst::CallBoundMulti { dests, .. } => {
                    // Redirect frame-cell dests to fresh temps, then store each
                    // back to its `s` field right after the call.
                    let redirects: Vec<(usize, Idx<Cell>, Idx<Cell>)> = dests
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| slots.contains_key(c))
                        .map(|(i, &c)| {
                            let temp = mf.new_cell(None);
                            minted.insert(temp);
                            (i, c, temp)
                        })
                        .collect();
                    if let Inst::CallMulti { dests, .. }
                    | Inst::CallValueMulti { dests, .. }
                    | Inst::CallBoundMulti { dests, .. } = &mut mf.insts[id]
                    {
                        for &(i, _, temp) in &redirects {
                            dests[i] = temp;
                        }
                    }
                    new.push(id);
                    for &(_, frame_cell, temp) in &redirects {
                        let lt = mf.alloc_inst(Inst::Load(temp));
                        let sv = mf.alloc_inst(Inst::Load(s_cell));
                        let sf = mf.alloc_inst(Inst::StoreField {
                            base: sv,
                            field: field_of(slots[&frame_cell]),
                            value: lt,
                        });
                        new.extend([lt, sv, sf]);
                    }
                }
                // `key, val = next(t, key)` with a framed cursor/value cell:
                // redirect both to fresh temps, SEED the key temp from its frame
                // field first (the key cell is IN/OUT -- the cursor carried across
                // iterations), then store both temps back to their fields. Without
                // this a framed cursor would read from `s.c<slot>` (materialized
                // `Load`) but be written to a bare local, so the loop never advances.
                Inst::MapNext {
                    key_cell,
                    val_cell,
                    table,
                } if slots.contains_key(&key_cell) || slots.contains_key(&val_cell) => {
                    let k_use = if slots.contains_key(&key_cell) {
                        let t = mf.new_cell(None);
                        minted.insert(t);
                        let sv = mf.alloc_inst(Inst::Load(s_cell));
                        let fld = mf.alloc_inst(Inst::Field {
                            base: sv,
                            field: field_of(slots[&key_cell]),
                        });
                        let st = mf.alloc_inst(Inst::Store {
                            cell: t,
                            value: fld,
                        });
                        new.extend([sv, fld, st]);
                        t
                    } else {
                        key_cell
                    };
                    let v_use = if slots.contains_key(&val_cell) {
                        let t = mf.new_cell(None);
                        minted.insert(t);
                        t
                    } else {
                        val_cell
                    };
                    mf.insts[id] = Inst::MapNext {
                        key_cell: k_use,
                        val_cell: v_use,
                        table,
                    };
                    new.push(id);
                    for (frame_cell, temp) in [(key_cell, k_use), (val_cell, v_use)]
                        .into_iter()
                        .filter(|(c, _)| slots.contains_key(c))
                    {
                        let lt = mf.alloc_inst(Inst::Load(temp));
                        let sv = mf.alloc_inst(Inst::Load(s_cell));
                        let sf = mf.alloc_inst(Inst::StoreField {
                            base: sv,
                            field: field_of(slots[&frame_cell]),
                            value: lt,
                        });
                        new.extend([lt, sv, sf]);
                    }
                }
                _ => new.push(id),
            }
        }
        mf.blocks[bid].insts = new;
    }
    if !strict {
        return;
    }
    // Exhaustiveness: a surviving bare cell is permitted only when it is not a
    // planned frame slot. Precise liveness intentionally leaves segment-local
    // cells bare; the slot check still catches a missed materialization.
    for &bid in &region {
        for &id in &mf.blocks[bid].insts {
            let bad = match &mf.insts[id] {
                Inst::Load(c) if slots.contains_key(c) => Some(*c),
                Inst::Store { cell, .. } if slots.contains_key(cell) => Some(*cell),
                Inst::MapNext { key_cell, .. } if slots.contains_key(key_cell) => Some(*key_cell),
                Inst::MapNext { val_cell, .. } if slots.contains_key(val_cell) => Some(*val_cell),
                _ => None,
            };
            if let Some(c) = bad {
                panic!(
                    "materialize_frame: unframed cell c{} ({:?}) survives in region of `{}` \
                     (frame-membership finding)",
                    raw(c),
                    mf.cells[c].name,
                    mf.name
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lower(src: &str, fn_name: &str) -> MirFunc {
        let ast = skull_parser::parse_module(src, ustr::Ustr::from("<test>")).expect("parse");
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut [ast]).expect("semantic_pass");
        let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
        let (fid, _) = ws
            .module(mid)
            .functions
            .iter()
            .find(|(_, f)| f.name.as_str() == fn_name)
            .expect("function");
        let root = skull_mir::mir::FuncRef {
            module: mid,
            func: fid,
        };
        let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
        let graph = skull_graph::instance_graph::build_in(
            &ws,
            vec![root],
            false,
            std::sync::Arc::clone(&interner),
        );
        let concrete = skull_graph::concrete_body::build(&ws, &graph);
        let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(interner);
        let plan = concrete
            .plans
            .get(&skull_mir::instance::Instance::base(root))
            .expect("test fixture concrete plan");
        skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &concrete.plans)
            .expect("lower")
    }

    #[test]
    fn cleanup_load_root_keeps_the_written_cell() {
        let mut unrooted = MirFunc::new(ustr::Ustr::from("cleanup_root"));
        let cell = unrooted.new_cell(None);
        let value = unrooted.emit(unrooted.entry, Inst::Const(Lit::Int(1)));
        let store = unrooted.emit(unrooted.entry, Inst::Store { cell, value });
        unrooted.set_term(unrooted.entry, Term::Return(Vec::new()));
        let _ = cleanup(&mut unrooted);
        assert!(!unrooted.blocks[unrooted.entry].insts.contains(&store));

        let mut rooted = MirFunc::new(ustr::Ustr::from("cleanup_root"));
        let cell = rooted.new_cell(None);
        let value = rooted.emit(rooted.entry, Inst::Const(Lit::Int(1)));
        let store = rooted.emit(rooted.entry, Inst::Store { cell, value });
        rooted.inline_cleanup_load_cells.push(cell);
        rooted.set_term(rooted.entry, Term::Return(Vec::new()));
        let _ = cleanup(&mut rooted);
        assert!(rooted.blocks[rooted.entry].insts.contains(&store));
    }

    #[test]
    fn cleanup_prefilter_finds_only_aggregate_stores() {
        let scalar = lower("fn f(x: int) -> int { return x }", "f");
        assert!(!has_scalarizable_store(&scalar));

        let aggregate = lower(
            "struct Pair { a: int, b: int }\n\
             fn f(flag: bool) -> int {\n\
                 let p = Pair { a: 1, b: 2 }\n\
                 if flag { return p.a }\n\
                 return p.b\n\
             }",
            "f",
        );
        assert!(has_scalarizable_store(&aggregate), "{aggregate}");
    }

    #[test]
    fn coroutine_transform_exposes_one_owned_frame_plan() {
        let func = lower(
            "co fn f(x: int) -> Coroutine<int> { let saved = x yield saved }",
            "f",
        );
        let lowered = transform_coroutine(&func, None, false);
        assert!(
            !lowered.frame_plan.slots().is_empty(),
            "the coroutine fixture must have a nonzero frame population"
        );
        assert_eq!(
            lowered.frame_plan.slots().len(),
            lowered.frame_plan.fields().len(),
            "one frame plan must own both membership and physical slots"
        );
    }

    #[test]
    fn coroutine_demotes_a_local_confined_to_one_resume_segment() {
        let func = lower(
            "co fn f(x: int) -> Coroutine<int> {\n\
                 yield 1\n\
                 let after = x\n\
             }",
            "f",
        );
        let lowered = transform_coroutine(&func, None, false);
        assert!(
            lowered
                .frame_plan
                .fields()
                .iter()
                .all(|&cell| lowered.step.cells[cell].name != Some(ustr::Ustr::from("after"))),
            "a local used only after the last yield must stay out of the frame:\n{}",
            lowered.step
        );
    }

    #[test]
    fn coroutine_frame_cells_retain_types_across_conditional_yields() {
        let func = lower(
            "co fn f(flag: bool) -> Coroutine<int> {\n\
                 if flag { yield 1 yield 2 } else { yield 10 yield 20 }\n\
             }",
            "f",
        );
        let lowered = transform_coroutine(&func, None, false);
        let untyped: Vec<_> = lowered
            .frame_plan
            .fields()
            .iter()
            .copied()
            .filter(|&cell| lowered.step.cells[cell].ty.is_none())
            .map(|cell| (cell, lowered.step.cells[cell].name))
            .collect();
        assert!(
            untyped.is_empty(),
            "untyped frame cells: {untyped:?}\n{}",
            lowered.step
        );
    }

    #[test]
    fn coroutine_frame_cells_retain_types_across_null_coalesce() {
        let func = lower(
            "co fn f(x: int?) -> Coroutine<int> {\n\
                 yield x ?? -1\n\
                 yield x ?? -2\n\
             }",
            "f",
        );
        let lowered = transform_coroutine(&func, None, false);
        let untyped: Vec<_> = lowered
            .frame_plan
            .fields()
            .iter()
            .copied()
            .filter(|&cell| lowered.step.cells[cell].ty.is_none())
            .map(|cell| (cell, lowered.step.cells[cell].name))
            .collect();
        assert!(
            untyped.is_empty(),
            "untyped frame cells: {untyped:?}\n{}",
            lowered.step
        );
    }

    #[test]
    fn coroutine_frame_cells_retain_types_across_tuple_destructure() {
        let func = lower(
            "fn pair() -> (int, int) { return (10, 20) }\n\
             co fn f() -> Coroutine<int> {\n\
                 let (a, b) = pair()\n\
                 yield a\n\
                 yield b\n\
             }",
            "f",
        );
        let lowered = transform_coroutine(&func, None, false);
        let untyped: Vec<_> = lowered
            .frame_plan
            .fields()
            .iter()
            .copied()
            .filter(|&cell| lowered.step.cells[cell].ty.is_none())
            .map(|cell| (cell, lowered.step.cells[cell].name))
            .collect();
        assert!(
            untyped.is_empty(),
            "untyped frame cells: {untyped:?}\n{}",
            lowered.step
        );
    }

    #[test]
    fn coroutine_frame_rematerializes_cross_block_constants_but_spills_a_live_global() {
        let mut func = MirFunc::new(ustr::Ustr::from("f"));
        func.is_coroutine = true;
        func.resume_arg = Some(func.new_cell(Some(ustr::Ustr::from("__resume_arg"))));
        let resume = func.new_block();
        let then_bb = func.new_block();
        let else_bb = func.new_block();
        let global = func.emit(func.entry, Inst::Global(ustr::Ustr::from("SELF")));
        let constant = func.emit(func.entry, Inst::Const(Lit::Int(7)));
        func.set_term(
            func.entry,
            Term::Yield {
                values: vec![constant],
                resume,
            },
        );
        func.emit(
            resume,
            Inst::StoreGlobal {
                name: ustr::Ustr::from("SAVED"),
                value: global,
            },
        );
        let condition = func.emit(resume, Inst::IntLt(constant, constant));
        func.set_term(
            resume,
            Term::Branch {
                cond: condition,
                then_bb,
                else_bb,
            },
        );
        func.set_term(then_bb, Term::Return(Vec::new()));
        func.set_term(else_bb, Term::Return(Vec::new()));
        let lowered = transform_coroutine(&func, None, false);
        let spills = lowered.frame_plan.spill_values();
        assert!(
            spills
                .iter()
                .any(|value| matches!(lowered.step.insts[*value], Inst::Global(name) if name.as_str() == "SELF")),
            "the evaluated global handle must retain its pre-branch identity:\n{}",
            lowered.step
        );
        assert!(
            spills
                .iter()
                .all(|value| !matches!(lowered.step.insts[*value], Inst::Const(_))),
            "constants must not acquire frame fields: {spills:?}\n{}",
            lowered.step
        );
    }

    #[test]
    fn closure_conversion_exposes_one_owned_environment_plan() {
        let mut func = lower(
            "fn f() -> int { let captured = 3; let g = () => captured; return g() }",
            "f",
        );
        assert!(
            func.insts
                .iter()
                .any(|(_, inst)| matches!(inst, Inst::Lambda { .. })),
            "the closure fixture must have a nonzero lambda population"
        );
        convert_closures(&mut func, "f");
        assert_eq!(
            func.closure_env_plans.len(),
            1,
            "one lambda must have one env plan"
        );
        let plan = func
            .closure_env_plans
            .values()
            .next()
            .expect("closure env plan");
        assert!(
            !plan.captures().is_empty(),
            "the closure environment must carry a capture"
        );
        assert!(
            plan.env_cell().is_some(),
            "a capturing closure must own an env cell"
        );
    }

    #[test]
    fn escaping_lambdas_get_distinct_owner_site_identities_and_census() {
        let mut func = lower(
            "fn f() -> int {\n\
                 let captured = true\n\
                 let a = () => captured\n\
                 let b = () => captured\n\
                 let c = () => 7\n\
                 if a() { return 1 }\n\
                 if b() { return 2 }\n\
                 return c()\n\
             }",
            "f",
        );
        let owner = match func.code_identity.clone().expect("concrete owner") {
            skull_mir::code_identity::CodeIdentity::Named(instance) => instance,
            other => panic!("unexpected owner identity: {other:?}"),
        };
        convert_closures(&mut func, "f");
        let artifact = skull_mir::code_identity::CodeArtifact::from_mir_tree(&func);
        let lambdas: Vec<_> = artifact
            .entries
            .keys()
            .filter_map(|identity| match identity {
                skull_mir::code_identity::CodeIdentity::Lambda { enclosing, site }
                    if *enclosing
                        == skull_mir::code_identity::CodeOwner::Instance(owner.clone()) =>
                {
                    Some(*site)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            lambdas.len(),
            3,
            "all escaping lambda bodies need identities"
        );
        assert_eq!(
            lambdas
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3,
            "source sites, not ordinals, distinguish identical-looking lambdas"
        );
        assert_eq!(artifact.census.lambda_values, 3);
        assert_eq!(artifact.census.capturing_lambdas, 2);
        assert_eq!(artifact.census.capture_free_lambdas, 1);
        assert!(
            artifact.census.is_total(),
            "identity census: {:?}",
            artifact.census
        );
    }

    #[test]
    fn lambda_topology_rejects_insertion_but_body_edits_keep_identity() {
        use skull_mir::code_identity::{
            CaptureSchema, CodeArtifact, CodeCompatibility, CodeEntry, CodeIdentity, LambdaSiteId,
            LambdaTopologyFingerprint,
        };
        use skull_mir::instance::Instance;

        let owner = Instance::base(skull_mir::mir::FuncRef {
            module: bone_semantic::prelude::ModuleId::builtin(),
            func: comfy_la_arena::Idx::from_raw(comfy_la_arena::RawIdx::from_u32(0)),
        });
        let site_a = LambdaSiteId {
            file: Ustr::from("f.rock"),
            offset: 10,
        };
        let site_b = LambdaSiteId {
            file: Ustr::from("f.rock"),
            offset: 20,
        };
        let topology = LambdaTopologyFingerprint::from_sites(&[(site_a, 0), (site_b, 0)]);
        let inserted = LambdaTopologyFingerprint::from_sites(&[
            (site_a, 0),
            (
                LambdaSiteId {
                    file: Ustr::from("f.rock"),
                    offset: 15,
                },
                0,
            ),
            (site_b, 0),
        ]);
        let removed = LambdaTopologyFingerprint::from_sites(&[(site_b, 0)]);
        let compat = |topology| CodeCompatibility {
            call_abi: "params=[];result=none;lanes=0;scalar=false".to_string(),
            capture: CaptureSchema::empty(),
            frame: None,
            lambda_topology: topology,
        };
        let id = CodeIdentity::Lambda {
            enclosing: skull_mir::code_identity::CodeOwner::Instance(owner.clone()),
            site: site_b,
        };
        let mut old = CodeArtifact::default();
        old.insert(CodeEntry {
            identity: id.clone(),
            compatibility: compat(topology),
            body_fingerprint: 1,
            body_name: Some(Ustr::from("old")),
            escaping: true,
            capture_free: true,
        });
        let mut body_edit = CodeArtifact::default();
        body_edit.insert(CodeEntry {
            identity: id.clone(),
            compatibility: compat(topology),
            body_fingerprint: 2,
            body_name: Some(Ustr::from("edited")),
            escaping: true,
            capture_free: true,
        });
        assert!(
            old.compatible_overlay(&body_edit),
            "body-only edit must retain the slot"
        );
        let mut topology_edit = CodeArtifact::default();
        topology_edit.insert(CodeEntry {
            identity: id,
            compatibility: compat(inserted),
            body_fingerprint: 3,
            body_name: Some(Ustr::from("inserted")),
            escaping: true,
            capture_free: true,
        });
        assert!(
            !old.compatible_overlay(&topology_edit),
            "insertion before a lambda must fail closed"
        );
        let mut removal_edit = CodeArtifact::default();
        removal_edit.insert(CodeEntry {
            identity: CodeIdentity::Lambda {
                enclosing: skull_mir::code_identity::CodeOwner::Instance(owner.clone()),
                site: site_b,
            },
            compatibility: compat(removed),
            body_fingerprint: 4,
            body_name: Some(Ustr::from("removed")),
            escaping: true,
            capture_free: true,
        });
        assert!(
            !old.compatible_overlay(&removal_edit),
            "removing a lambda must fail closed"
        );

        let mut capture_edit = CodeArtifact::default();
        let mut incompatible_capture = CaptureSchema::empty();
        incompatible_capture.complete = false;
        capture_edit.insert(CodeEntry {
            identity: CodeIdentity::Lambda {
                enclosing: skull_mir::code_identity::CodeOwner::Instance(owner),
                site: site_b,
            },
            compatibility: CodeCompatibility {
                capture: incompatible_capture,
                ..compat(topology)
            },
            body_fingerprint: 5,
            body_name: Some(Ustr::from("capture")),
            escaping: true,
            capture_free: false,
        });
        assert!(
            !old.compatible_overlay(&capture_edit),
            "incomplete capture schema must fail closed"
        );
    }

    #[test]
    fn owner_topology_ignores_non_lambda_entries_without_topology() {
        use skull_mir::code_identity::{
            CaptureSchema, CodeArtifact, CodeCompatibility, CodeEntry, CodeIdentity, CodeOwner,
            LambdaSiteId, LambdaTopologyFingerprint,
        };
        use skull_mir::instance::Instance;

        let owner = Instance::base(skull_mir::mir::FuncRef {
            module: bone_semantic::prelude::ModuleId::builtin(),
            func: comfy_la_arena::Idx::from_raw(comfy_la_arena::RawIdx::from_u32(0)),
        });
        let owner = CodeOwner::Instance(owner);
        let topology = LambdaTopologyFingerprint::from_sites(&[(
            LambdaSiteId {
                file: Ustr::from("f.rock"),
                offset: 10,
            },
            0,
        )]);
        let compat = |lambda_topology| CodeCompatibility {
            call_abi: "unit".to_string(),
            capture: CaptureSchema::empty(),
            frame: None,
            lambda_topology,
        };
        let mut old = CodeArtifact::default();
        old.insert(CodeEntry {
            identity: CodeIdentity::Lambda {
                enclosing: owner.clone(),
                site: LambdaSiteId {
                    file: Ustr::from("f.rock"),
                    offset: 10,
                },
            },
            compatibility: compat(topology),
            body_fingerprint: 1,
            body_name: Some(Ustr::from("lambda_old")),
            escaping: true,
            capture_free: true,
        });
        old.insert(CodeEntry {
            identity: CodeIdentity::Nested {
                enclosing: owner.clone(),
                site: LambdaSiteId {
                    file: Ustr::from("f.rock"),
                    offset: 20,
                },
                name: Ustr::from("nested"),
            },
            compatibility: compat(LambdaTopologyFingerprint::default()),
            body_fingerprint: 2,
            body_name: Some(Ustr::from("nested_old")),
            escaping: true,
            capture_free: true,
        });
        let mut next = CodeArtifact::default();
        next.insert(CodeEntry {
            identity: CodeIdentity::Lambda {
                enclosing: owner.clone(),
                site: LambdaSiteId {
                    file: Ustr::from("f.rock"),
                    offset: 10,
                },
            },
            compatibility: compat(topology),
            body_fingerprint: 3,
            body_name: Some(Ustr::from("lambda_new")),
            escaping: true,
            capture_free: true,
        });
        next.insert(CodeEntry {
            identity: CodeIdentity::Nested {
                enclosing: owner,
                site: LambdaSiteId {
                    file: Ustr::from("f.rock"),
                    offset: 20,
                },
                name: Ustr::from("nested"),
            },
            compatibility: compat(LambdaTopologyFingerprint::default()),
            body_fingerprint: 4,
            body_name: Some(Ustr::from("nested_new")),
            escaping: true,
            capture_free: true,
        });
        assert!(old.compatible_overlay(&next));
    }

    #[test]
    fn named_values_are_counted_but_direct_calls_do_not_acquire_slots() {
        let mut direct = lower(
            "fn target(x: int) -> int { return x }\n\
             fn f() -> int { return target(1) }",
            "f",
        );
        convert_closures(&mut direct, "f");
        let direct_artifact = skull_mir::code_identity::CodeArtifact::from_mir_tree(&direct);
        assert_eq!(direct_artifact.census.named_values, 0);
        assert_eq!(direct_artifact.census.direct_call_only, 1);

        let mut value = lower(
            "fn target(x: int) -> int { return x }\n\
             fn f() -> int { let callback = target; return callback(1) }",
            "f",
        );
        convert_closures(&mut value, "f");
        let value_artifact = skull_mir::code_identity::CodeArtifact::from_mir_tree(&value);
        assert_eq!(value_artifact.census.named_values, 1);
        assert_eq!(
            value_artifact.census.unresolved_body, 1,
            "the per-body artifact must expose an unjoined target"
        );
        assert!(
            !value_artifact.census.is_total(),
            "a partial artifact must not report a total census"
        );
    }

    #[test]
    fn foreign_function_values_remain_explicit_boundaries() {
        let func = lower(
            "inline(lua) fn foreign(x: int) -> int = \"return x\"\n\
             fn f() -> int { let callback = foreign; return callback(1) }",
            "f",
        );
        assert!(func.code_value_identities.is_empty());
        assert_eq!(
            func.fn_value_symbols.len(),
            1,
            "the foreign value still has a boundary symbol"
        );
    }

    #[test]
    fn detached_coroutine_regions_are_ordinary_converted_functions() {
        let mut source = lower("fn f() -> int { let add = (n: int) => n; add(1) }", "f");
        let entry = source.entry();
        let s_cell = source.new_cell(Some(Ustr::from("s")));
        let frame = FrameInfo {
            s_cell,
            fields: HashSet::default(),
            slots: HashMap::default(),
        };
        let aux = lower_detached_region(&source, entry, "f__detached", &frame);
        assert!(
            !aux.is_coroutine,
            "detached body must be an ordinary MirFunc"
        );
        assert!(
            !aux.nested_fns.is_empty(),
            "detached lambda must own a nested MirFunc"
        );
        assert!(
            aux.insts
                .iter()
                .any(|(_, inst)| matches!(inst, Inst::Global(_))),
            "capture-free detached lambda must become a Global value"
        );
        assert!(
            aux.insts
                .iter()
                .all(|(_, inst)| !matches!(inst, Inst::Lambda { .. })),
            "detached conversion must leave no residual lambda"
        );
        erase_detached_lambdas(&mut source, [entry]);
        assert!(
            source
                .insts
                .iter()
                .all(|(_, inst)| !matches!(inst, Inst::Lambda { .. })),
            "the emitted artifact must have no detached residual lambda"
        );
    }

    #[test]
    fn detached_nested_closure_reaches_frame_only_capture_through_state() {
        let mut source = lower(
            "fn f(seed: int) -> int { let get = () => seed; return get() }",
            "f",
        );
        let entry = source.entry();
        let seed = source.params[0];
        let s_cell = source.new_cell(Some(Ustr::from("s")));
        let frame = FrameInfo {
            s_cell,
            fields: [seed].into_iter().collect(),
            slots: [(seed, 0)].into_iter().collect(),
        };
        let aux = lower_detached_region(&source, entry, "f__detached", &frame);
        let plan = aux
            .closure_env_plans
            .values()
            .next()
            .expect("nested closure must have an environment plan");
        assert_eq!(plan.captures(), &[s_cell]);
    }
}
