//! Coroutine transform: a `co fn` (lowered to a cell CFG with `Term::Yield`)
//! becomes an ordinary stackless state-machine `__step` function plus a
//! constructor, per `docs/stackless-coroutines.md` (decision 6). No LuaJIT
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
//! Frame = every cell except the state table `s` and `__resume_arg` (the step's
//! two params). This is a correct over-approximation (hoisting a block-local
//! cell to `s` is still correct; precise liveness is a later perf refinement —
//! the plan's P2). The constructor seeds only the declared param fields.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use comfy_la_arena::{Idx, RawIdx};
use skull_mir::mir::*;

mod closure;
mod forl;
mod inline;
pub mod scope;
pub use closure::{convert_closures, convert_closures_framed, FrameInfo};
pub use forl::recover_counted_loops;
pub use inline::{
    fn_has_bounds, inline_functions, validate_no_residual_bound_calls, InlineCache, SpliceRecord,
};

pub const STATE: &str = "__state";

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

/// DCE, optionally fusing a copy-prop substitution into the count scan: when
/// `subst` is `Some`, each inst's value operands (and each terminator's) are
/// remapped through it as they are counted, so the post-substitution reference
/// counts fall out of the same walk that DCE needs anyway — no separate
/// full-arena remap pass. With `None` it is plain DCE over the current program.
fn dce_core(func: &mut MirFunc, subst: Option<&[Option<Idx<Inst>>]>) {
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
                if let Some(s) = subst {
                    remap_inst(&mut insts[i], &|x| resolve_subst(s, x));
                }
                value_operands(&insts[i], &mut |op| use_count[raw(op) as usize] += 1);
                match &insts[i] {
                    Inst::Load(c) => load_count[raw(*c) as usize] += 1,
                    Inst::Store { cell, .. } => store_n[raw(*cell) as usize] += 1,
                    _ => {}
                }
            }
            if let Some(s) = subst {
                remap_term(&mut b.term, &|x| resolve_subst(s, x));
            }
            // Terminator operands are permanent uses (terms are never removed), so
            // their +1 is never decremented — a value used by a term stays live.
            term_uses_each(&b.term, &mut |v| use_count[raw(v) as usize] += 1);
        }
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

    // Seed the worklist with everything dead in the initial program, then cascade.
    let mut dead = vec![false; ni];
    let mut work: Vec<Idx<Inst>> = Vec::new();
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            if is_dead_rc(&func.insts[i], i, &use_count, &load_count) {
                dead[raw(i) as usize] = true;
                work.push(i);
            }
        }
    }
    while let Some(i) = work.pop() {
        // Each value operand of the removed inst loses a use; a value that hits
        // zero (and is pure) is itself now dead.
        value_operands(&func.insts[i], &mut |op| {
            let k = raw(op) as usize;
            use_count[k] = use_count[k].saturating_sub(1);
            if !dead[k] && use_count[k] == 0 && is_dead_value(&func.insts[op]) {
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

    // Single filter pass; `retain` reuses each block's allocation.
    let block_ids: Vec<Idx<Block>> = func.blocks.iter().map(|(id, _)| id).collect();
    for bid in block_ids {
        let mut insts = std::mem::take(&mut func.blocks[bid].insts);
        insts.retain(|&i| !dead[raw(i) as usize]);
        func.blocks[bid].insts = insts;
    }
}


/// Value operands a terminator reads (the live-use contribution of a `Term`).
fn term_uses_each(term: &Term, push: &mut impl FnMut(Idx<Inst>)) {
    match term {
        Term::Return(vs) => vs.iter().for_each(|v| push(*v)),
        Term::Branch { cond, .. } => push(*cond),
        Term::Switch { discr, .. } => push(*discr),
        Term::Yield { values, .. } => values.iter().for_each(|v| push(*v)),
        _ => {}
    }
}

/// Whether a value-producing inst is pure enough that an unused result is dead.
/// `Call`/`CallValue` run for effect; an `Index` read is bounds-checked (errors),
/// so a discarded result must still execute.
fn is_dead_value(inst: &Inst) -> bool {
    !matches!(inst, Inst::Call { .. } | Inst::CallValue { .. } | Inst::Index { .. })
        && inst.produces_value()
}

/// The pre-emit MIR cleanup: copy-propagate intra-block loads, then DCE the dead
/// loads/values/stores they leave. Run per-function in `mir_emit`.
///
/// The two passes are FUSED: copy-prop's substitution is applied as DCE's count
/// scan walks each inst, so the value-operand remap (`remap_inst` over every
/// inst) and the use-counting share one traversal instead of two back-to-back.
/// The standalone `copy_propagate` / `dce` keep their independent behavior.
pub fn cleanup(func: &mut MirFunc) {
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
        let changed = has_scalarizable_store(func) && scalarize_struct_cells_once(func);
        let subst = copy_propagate_subst(func);
        dce_core(func, subst.as_deref());
        if !changed {
            break;
        }
    }
    if dbg {
        eprintln!("--- SROA final ({}) ---\n{func}", func.name);
    }
    if fold_const_branches(func) {
        dce(func);
    }
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
                            | Inst::MakeTuple(_)
                    )
            )
        })
    })
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
        if let Term::Branch { cond, then_bb, else_bb } = func.blocks[*b].term {
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
        term_targets_each(&func.blocks[b].term, &mut |t| {
            if reach.insert(t) {
                stack.push(t);
            }
        });
    }
    for b in bids {
        if !reach.contains(&b) {
            func.blocks[b].insts.clear();
            func.blocks[b].term = Term::Unreachable;
        }
    }
    true
}

/// Block targets of a terminator.
fn term_targets_each(term: &Term, f: &mut impl FnMut(Idx<Block>)) {
    match term {
        Term::Goto(t) => f(*t),
        Term::Branch { then_bb, else_bb, .. } => {
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

/// Scalarize `buf[i] = <value-struct literal>` for a `NativeBuffer<value>`.
///
/// `StoreIndex { kind: NativeBuffer, value: MakeValueStruct }` otherwise builds
/// the WHOLE nested cdata (`Vertex_ct(Vec2_ct(..), Color_ct(..), ..)`) and copies
/// it into the slot -- ~5 cdata allocations per element that don't sink across
/// the buffer store. rock instead grabs the slot reference and writes each leaf
/// field in place (`slot.pos.x = ..`), allocating nothing. This is the mag
/// HUD/text glyph-quad path (`queue_ui_text_with_font`'s
/// `verts[ci+N] = make_text_vertex(..)`).
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
            Inst::StoreIndex { base, index, value, kind }
                if kind.is_native_buffer()
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
        let slot = func.insts.alloc(Inst::Index { base, index, kind });
        let mut new = vec![slot];
        gen_leaf_stores(func, slot, value, &mut new);
        replacement.insert(store_i, new);
    }
    let block_ids: Vec<Idx<Block>> = func.blocks.iter().map(|(id, _)| id).collect();
    for bid in block_ids {
        if !func.blocks[bid].insts.iter().any(|i| replacement.contains_key(i)) {
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
        out.push(func.insts.alloc(Inst::StoreField { base: slot, field: fname, value: op }));
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
    /// `MakeEnum` -- a payload-variant enum table (`{__variant, __ty, [i]=..}`).
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
/// `__rock_enum_<E>_<V>` (not a `MakeEnum`); given `enum_hint = E` we recognize it
/// as the same enum's ctor with fields `{__variant=V, __ty=E}` (no payload), so a
/// cell mixing payload + unit variants stays uniform and splits. The hint is what
/// disambiguates `V` from `E` in the underscore-joined global name.
fn ctor_fields(
    inst: &Inst,
    enum_hint: Option<ustr::Ustr>,
) -> Option<(StructId, Vec<(ustr::Ustr, FieldSrc)>)> {
    let op = |fs: &[(ustr::Ustr, Idx<Inst>)]| {
        fs.iter().map(|(n, v)| (*n, FieldSrc::Operand(*v))).collect::<Vec<_>>()
    };
    match inst {
        Inst::MakeValueStruct { name, fields } => Some((StructId::Value(*name), op(fields))),
        Inst::MakeStruct { ty, fields } => Some((StructId::Plain(*ty), op(fields))),
        Inst::MakeTuple(vs) => Some((
            StructId::Tuple(vs.len()),
            vs.iter()
                .enumerate()
                .map(|(i, v)| (ustr::Ustr::from(&format!("_{i}")), FieldSrc::Operand(*v)))
                .collect(),
        )),
        Inst::MakeEnum { tag, ty, positional, named } => {
            let mut f = vec![
                (ustr::Ustr::from("__variant"), FieldSrc::ConstStr(*tag)),
                (ustr::Ustr::from("__ty"), FieldSrc::ConstStr(*ty)),
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
            let variant = g.as_str().strip_prefix(&format!("__rock_enum_{e}_"))?;
            Some((
                StructId::Enum(e),
                vec![
                    (ustr::Ustr::from("__variant"), FieldSrc::ConstStr(ustr::Ustr::from(variant))),
                    (ustr::Ustr::from("__ty"), FieldSrc::ConstStr(e)),
                ],
            ))
        }
        _ => None,
    }
}

/// Scalar replacement of struct cells (SROA) -- both `value` cdata and plain
/// `struct` tables. A cell whose every `Store` is a struct ctor of one
/// consistent type and whose every `Load` feeds ONLY `Field` reads / `StoreField`
/// writes never needs the aggregate: split it into one cell per declared field,
/// turning the ctor store into per-field stores, each `Field` read into a plain
/// field-cell `Load`, and each `StoreField` write into a field-cell `Store`. The
/// now-unreferenced ctor and whole-cell `Load`s die in the `cleanup` DCE that
/// runs right after.
///
/// skull already unboxes value structs WITHIN a body (`ValueRepr::Group`), but a
/// `@pure`/`@inline` helper that returns a struct is lowered standalone and packs
/// its return; the MIR inliner then splices that packed ctor into the caller,
/// where the field reads land in branch arms (other blocks). The value model is
/// single-block, so the per-field VALUES can't cross the branch -- but per-field
/// CELLS can, which is exactly how rock scalarizes (`pos` -> `pos__x`/`pos__y`).
///
/// For `value` structs this removes a cdata that LuaJIT cannot sink across a
/// side-trace split (the mag `mag.grid.floors_walls` regression). For plain
/// structs it removes a Lua TABLE that LuaJIT's SINK only elides on a compiled
/// trace -- in the interpreter (mag's aborted HUD/text loops) the table
/// allocates every iteration; rock scalarizes it to per-field locals so it pays
/// 0 even interpreted. Plain structs are MUTABLE, so `StoreField` writes are
/// legal (they become field-cell stores); any WHOLE-struct use (pass-by-value,
/// `==`, return, store as another aggregate's field) escapes and keeps the table
/// -- a struct that never escapes has no observable identity, so dropping it is
/// sound (rock proves it). Runs to a fixpoint so nested value structs (a field
/// cell that itself holds a ctor) peel too.
pub fn scalarize_struct_cells(func: &mut MirFunc) {
    let dbg = std::env::var("SROA_DEBUG").is_ok();
    let mut round = 0;
    while {
        if dbg {
            eprintln!("--- SROA round {round} input ---\n{func}");
            round += 1;
        }
        has_scalarizable_store(func) && scalarize_struct_cells_once(func)
    } {}
    if dbg {
        eprintln!("--- SROA final ---\n{func}");
    }
}

/// One SROA round; returns whether it rewrote anything (drives the fixpoint).
fn scalarize_struct_cells_once(func: &mut MirFunc) -> bool {
    let nc = func.cells.len();
    // Escape-site rematerialization (item 8): a whole-value load of a
    // `nonnull_value_struct` binding that would otherwise ESCAPE (disqualifying
    // its family) is instead rebuilt from the ctor's per-field cells AT the use
    // site. `value_order` keeps each value ctor's declared field order (the
    // rebuilt `MakeValueStruct` is positional); `escape_loads` are the loads to
    // rewrite. The flag -- set at lowering only for non-null value bindings --
    // is the ONLY license: no store/CFG heuristic (both proven unsound/too weak).
    let mut value_order: HashMap<usize, Vec<ustr::Ustr>> = HashMap::default();
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
                Inst::MapNext { key_cell, val_cell, .. } => {
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
                if let Inst::MakeEnum { ty, .. } = &func.insts[*value] {
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
            value_operands(&func.insts[i], &mut |op| vuse[raw(op) as usize] += 1);
        }
        term_uses_each(&b.term, &mut |op| vuse[raw(op) as usize] += 1);
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
                                value_order
                                    .entry(ck)
                                    .or_insert_with(|| fields.iter().map(|(n, _)| *n).collect());
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
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            value_operands(&func.insts[i], &mut |op| {
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
                        // The alias store itself: `Store(y, Load(x))` where y and x
                        // unify to the same root. It is deleted by the rewrite.
                        Inst::Store { cell, value } => {
                            let yk = raw(*cell) as usize;
                            *value == op
                                && alias[yk].is_some()
                                && !disq[yk]
                                && rep[yk] == rep[raw(c) as usize]
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
        term_uses_each(&b.term, &mut |op| {
            if let Some(&c) = load_cell.get(&op) {
                if func.cells[c].nonnull_value_struct {
                    escape_loads.push((op, c)); // rebuild at the terminator use
                } else {
                    disq[raw(c) as usize] = true; // a load used by a terminator escapes
                }
            }
        });
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
        }
    }

    // `k < nc` guards the field cells `get_field_cell` allocates during the
    // rewrite (a `StoreField`->`Store` rewrite emits a `Store` to a fresh field
    // cell that the later `store_expansions` scan sees): they are never candidates
    // this round, and indexing the round's `nc`-sized arrays with them would panic.
    let is_cand = |c: Idx<Cell>| {
        let k = raw(c) as usize;
        k < nc && cand[k].is_some() && !disq[k] && has_store[k]
    };
    if (0..nc).all(|k| !(cand[k].is_some() && !disq[k] && has_store[k])) {
        return false;
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
    let mut get_field_cell =
        |func: &mut MirFunc, c: Idx<Cell>, f: ustr::Ustr| -> Idx<Cell> {
            *field_cell.entry((c, f)).or_insert_with(|| {
                let base = func.cells[c].name.map(|n| n.as_str().to_string());
                let name = base.map(|b| ustr::Ustr::from(&format!("{b}__{f}")));
                func.new_cell(name)
            })
        };

    // Rewrite `Field { base: load(cand), field }` in place into `Load(field_cell)`,
    // and `StoreField { base: load(cand), field, value }` into `Store(field_cell,
    // value)`. Re-check `is_cand`: `load_cell` is not pruned as cells get
    // disqualified mid-analysis, so a cell escaping via a non-projection use (e.g.
    // whole-struct `==`) keeps its aggregate, and its field accesses must stay on
    // it -- rewriting them without expanding the store gives an uninit cell.
    let field_rewrites: Vec<(Idx<Inst>, Idx<Cell>, ustr::Ustr)> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .filter_map(|i| match &func.insts[i] {
            Inst::Field { base, field } => load_cell
                .get(base)
                .copied()
                .filter(|&c| is_cand(c))
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

    // Expand each `Store { cand, ctor }` into per-field stores. Collect first
    // (immutable read of the ctor fields), then rebuild the affected blocks.
    let store_expansions: Vec<(Idx<Inst>, Idx<Cell>, Vec<(ustr::Ustr, FieldSrc)>)> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .filter_map(|i| match &func.insts[i] {
            Inst::Store { cell, value } if is_cand(*cell) => {
                // A nil store writes nil to EVERY field cell (sorted for
                // deterministic emission), reusing the nil value inst.
                if matches!(&func.insts[*value], Inst::Const(Lit::Nil)) {
                    let mut fs: Vec<ustr::Ustr> = cand_fields[rep[raw(*cell) as usize]]
                        .as_ref()
                        .map(|s| s.iter().copied().collect())
                        .unwrap_or_default();
                    fs.sort_unstable();
                    return Some((
                        i,
                        *cell,
                        fs.into_iter().map(|f| (f, FieldSrc::Operand(*value))).collect(),
                    ));
                }
                ctor_fields(&func.insts[*value], cell_enum[raw(*cell) as usize])
                    .map(|(_, fields)| (i, *cell, fields))
            }
            _ => None,
        })
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
    for (store_i, c, fields) in store_expansions {
        let mut news = Vec::with_capacity(fields.len());
        for (fname, src) in fields {
            let fval = match src {
                FieldSrc::Operand(v) => v,
                // A materialized const is a NEW value-producing inst: it must be
                // spliced into the block (in `news`) BEFORE the store that reads it,
                // or it floats free of any block and regalloc/codegen never emit it.
                FieldSrc::ConstStr(s) => {
                    let cv = func.insts.alloc(Inst::Const(Lit::Str(s)));
                    news.push(cv);
                    cv
                }
            };
            let fc = get_field_cell(func, c, fname);
            news.push(func.insts.alloc(Inst::Store { cell: fc, value: fval }));
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
        let Some(StructId::Value(name)) = cand[raw(root) as usize] else { continue };
        let Some(order) = value_order.get(&(raw(root) as usize)).cloned() else { continue };
        let mut news = Vec::with_capacity(order.len() + 1);
        let mut mvs_fields = Vec::with_capacity(order.len());
        for fname in order {
            let fc = get_field_cell(func, root, fname);
            let fl = func.insts.alloc(Inst::Load(fc));
            news.push(fl);
            mvs_fields.push((fname, fl));
        }
        func.insts[l] = Inst::MakeValueStruct { name, fields: mvs_fields };
        news.push(l);
        replacement.insert(l, news);
    }
    if !replacement.is_empty() {
        let block_ids: Vec<Idx<Block>> = func.blocks.iter().map(|(id, _)| id).collect();
        for bid in block_ids {
            if !func.blocks[bid].insts.iter().any(|i| replacement.contains_key(i)) {
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
    true
}

/// Intra-block copy propagation: the cell model emits a `Load(c)` for every use
/// of a binding even when its value is already in hand, leaving
/// `c = v; r = c; r2 = c` churn. Within ONE block a cell's value is stable until
/// the next `Store` to it (cells are plain locals, not aliased — only `Store{cell}`
/// changes one), and a value is single-block (born+consumed in its block, design
/// invariant), so replacing a `Load(c)` with the value last stored to `c` EARLIER
/// IN THE SAME BLOCK is sound with no dominance/coroutine-spill hazard. The now-
/// dead `Load`s (and any store to a now-unloaded cell) are removed by [`dce`].
/// Cross-block loads are left untouched (conservative — a later mem2reg, P7).
pub fn copy_propagate(func: &mut MirFunc) {
    let Some(subst) = copy_propagate_subst(func) else {
        return;
    };
    let m = |x: Idx<Inst>| resolve_subst(&subst, x);
    for (_, inst) in func.insts.iter_mut() {
        remap_inst(inst, &m);
    }
    for (_, b) in func.blocks.iter_mut() {
        remap_term(&mut b.term, &m);
    }
}

/// The copy-propagation ANALYSIS: build the value->value substitution table
/// without applying it. Returns `None` when there is nothing to substitute (the
/// common case for small bodies), so callers skip the remap entirely. Shared by
/// the standalone [`copy_propagate`] (which then remaps the whole arena) and
/// [`cleanup`] (which folds the remap into DCE's count scan).
fn copy_propagate_subst(func: &MirFunc) -> Option<Vec<Option<Idx<Inst>>>> {
    // A cell STORED inside a closure body can be mutated indirectly by ANY call
    // (the call may invoke that closure — e.g. `with_callback(() => { count = .. })`),
    // so its cached value is not stable across calls. Exclude such cells entirely
    // (treat as opaque). Closure bodies are the blocks NOT reachable from entry.
    let main: HashSet<Idx<Block>> = top_level_blocks(func).into_iter().collect();
    // Dense per-id side-tables (raw-keyed) replace the Idx HashMaps/HashSets:
    // `escaping` cell flag, `subst` value->value chain, and per-block `cur`
    // cell->value. `cur` is reset between blocks via a touched-list so the clear
    // stays O(stores-in-block), not O(cells).
    let mut escaping = vec![false; func.cells.len()];
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
    // forwarding stays sound (and the fold below never forwards through a table
    // anyway) -- excluding those keeps the hot `obj.field = v` shape
    // copy-propagated. SROA peels one level per round, so the base is a `Field`
    // chain on the first round and a bare `Load` of the peeled value cell after.
    let mut holds_value = vec![false; func.cells.len()];
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            if let Inst::Store { cell, value } = &func.insts[i] {
                if matches!(&func.insts[*value], Inst::MakeValueStruct { .. }) {
                    holds_value[raw(*cell) as usize] = true;
                }
            }
        }
    }
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            let Inst::StoreField { base, .. } = &func.insts[i] else { continue };
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
    let mut any_subst = false;
    let mut cur: Vec<Option<Idx<Inst>>> = vec![None; func.cells.len()];
    let mut touched: Vec<usize> = Vec::new();
    for (_, b) in func.blocks.iter() {
        for &t in &touched {
            cur[t] = None;
        }
        touched.clear();
        for &id in &b.insts {
            match &func.insts[id] {
                Inst::Store { cell, value } if !escaping[raw(*cell) as usize] => {
                    let v = resolve_subst(&subst, *value);
                    let ck = raw(*cell) as usize;
                    if cur[ck].is_none() {
                        touched.push(ck);
                    }
                    cur[ck] = Some(v);
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
                // now-dead ctor DCEs. (Plain `MakeStruct` is a MUTABLE table -- a
                // `StoreField` could change it -- so it is NOT forwarded here.)
                Inst::Field { base, field } => {
                    let (base, field) = (*base, *field);
                    let rb = resolve_subst(&subst, base);
                    if let Inst::MakeValueStruct { fields, .. } = &func.insts[rb] {
                        if let Some((_, op)) = fields.iter().find(|(n, _)| *n == field) {
                            subst[raw(id) as usize] = Some(resolve_subst(&subst, *op));
                            any_subst = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    any_subst.then_some(subst)
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
        Inst::Call { .. } | Inst::CallValue { .. } | Inst::Index { .. } => false,
        Inst::Store { cell, .. } => load_count[raw(*cell) as usize] == 0,
        _ if inst.produces_value() => use_count[raw(i) as usize] == 0,
        _ => false,
    }
}

/// Field name for a frame cell in the state table.
fn raw<T>(i: Idx<T>) -> u32 {
    i.into_raw().into()
}

pub struct CoroLowered {
    pub step: MirFunc,
    pub s_cell: Idx<Cell>,
    pub frame_cells: HashSet<Idx<Cell>>,
    /// Frame cell -> dense slot: frame cells numbered 0.. by raw-id order.
    /// Lambda-cell allocation shifts raw ids but never reorders existing frame
    /// cells, so a slot is stable under a lambda-internal edit -- the naming and
    /// stamp basis the P2 finding needs (docs/coroutine-frame-lambda-plan.md).
    pub frame_slots: HashMap<Idx<Cell>, u32>,
    /// Declared param cells (constructor seeds a state-table field for each).
    pub params: Vec<Idx<Cell>>,
    pub num_states: usize,
    /// `on_killed`/`on_resume`/`on_hot_reload` hooks peeled to the constructor:
    /// `(kind, lambda inst)`. The lambda still lives in `step`'s arenas (its body
    /// hoists to a top-level `<name>__on_<hook>` fn in `emit_coroutine`; the ctor
    /// stores that fn by name), just removed from every block's inst list.
    pub hooks: Vec<(CoroHookKind, Idx<Inst>)>,
    /// Frame layout stamp: the ctor writes it (`__layout`), the step's entry
    /// checks it. See `layout_stamp_canon` for what it hashes.
    pub layout_hash: i64,
    /// The scope tree + ownership built (once) for frame membership, threaded to
    /// `emit_named_framed` so the step's hoist plan reuses them instead of
    /// rebuilding on the post-rewrite CFG (they are stable across the rewrite).
    pub step_tree: crate::scope::ScopeTree,
    pub step_own: crate::scope::Ownership,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lower(src: &str, fn_name: &str) -> MirFunc {
        let ast = skull_parser::parse_module(src, ustr::Ustr::from("<test>")).expect("parse");
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut vec![ast]).expect("semantic_pass");
        let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
        let (fid, _) = ws
            .module(mid)
            .functions
            .iter()
            .find(|(_, f)| f.name.as_str() == fn_name)
            .expect("function");
        let scx = skull_mir::lower::ScalarCtx::new(&ws);
        skull_mir::lower::lower_function(&ws, &scx, mid, fid).expect("lower")
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
    let id = f.insts.alloc(inst);
    f.blocks[bid].insts.push(id);
    id
}

/// Successor blocks of a terminator (NOT closure entries — those are operands).
/// Visit each successor block of `term` (callback form — no per-call `Vec`
/// alloc; the sole caller, `top_level_blocks`, only iterates).
pub(crate) fn term_succs(term: &Term, mut f: impl FnMut(Idx<Block>)) {
    match term {
        Term::Goto(b) => f(*b),
        Term::Branch { then_bb, else_bb, .. } => {
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

/// Value operands an instruction READS, pushed to `push` (callback form — avoids
/// a per-inst `Vec` allocation in the dce hot loop).
pub(crate) fn value_operands(inst: &Inst, push: &mut impl FnMut(Idx<Inst>)) {
    let mut p = |x: &Idx<Inst>| push(*x);
    match inst {
        Inst::Const(_) | Inst::Global(_) | Inst::Load(_) | Inst::Lambda { .. }
        | Inst::CoroHook { .. } => {}
        Inst::IntAdd(a, b) | Inst::IntSub(a, b) | Inst::IntMul(a, b) | Inst::IntDiv(a, b)
        | Inst::IntMod(a, b) | Inst::FloatAdd(a, b) | Inst::FloatSub(a, b) | Inst::FloatMul(a, b)
        | Inst::FloatDiv(a, b) | Inst::FloatMod(a, b) | Inst::StringConcat(a, b)
        | Inst::IntLt(a, b) | Inst::IntLeq(a, b) | Inst::IntGt(a, b) | Inst::IntGeq(a, b)
        | Inst::FloatLt(a, b) | Inst::FloatLeq(a, b) | Inst::FloatGt(a, b) | Inst::FloatGeq(a, b)
        | Inst::Eq(a, b) | Inst::NotEq(a, b) | Inst::BitAnd(a, b) | Inst::BitOr(a, b)
        | Inst::BitXor(a, b) | Inst::Shl(a, b) | Inst::Shr(a, b) => { p(a); p(b); }
        Inst::Neg(x) | Inst::Not(x) | Inst::BitNot(x) | Inst::ToString(x) | Inst::FloatToInt(x) | Inst::IntToFloat(x)
        | Inst::CoroSetInner(x) => p(x),
        Inst::Format { value, .. } => p(value),
        Inst::FormatDyn { value, fmt } => { p(value); p(fmt); }
        Inst::Call { args, .. } | Inst::CallMulti { args, .. } | Inst::CallBoundMulti { args, .. } => args.iter().for_each(&mut p),
        Inst::CallValue { callee, args } => { p(callee); args.iter().for_each(&mut p); }
        Inst::CallValueMulti { callee, args, .. } => { p(callee); args.iter().for_each(&mut p); }
        Inst::MakeTuple(vs) | Inst::MakeVec(vs) => vs.iter().for_each(&mut p),
        Inst::MakeVecColumns { columns, .. } => {
            columns.iter().flat_map(|(_, vs)| vs).for_each(&mut p)
        }
        Inst::MakeStruct { fields, .. } | Inst::MakeValueStruct { fields, .. } | Inst::MakeEnv { fields, .. } => fields.iter().for_each(|(_, a)| p(a)),
        Inst::MakeEnum { positional, named, .. } => {
            positional.iter().for_each(&mut p);
            named.iter().for_each(|(_, a)| p(a));
        }
        Inst::MakeEnumDyn { tag, positional, named, .. } => {
            p(tag);
            positional.iter().for_each(&mut p);
            named.iter().for_each(|(_, a)| p(a));
        }
        Inst::FfiNewArray { count, .. } => p(count),
        Inst::Field { base, .. } | Inst::TupleIndex { base, .. } => p(base),
        Inst::Index { base, index, .. } => { p(base); p(index); }
        Inst::VecCheckedIndex { vec, index } => { p(vec); p(index); }
        Inst::Len { base, .. } => p(base),
        Inst::Store { value, .. } => p(value),
        Inst::StoreField { base, value, .. } => { p(base); p(value); }
        Inst::StoreIndex { base, index, value, .. } => { p(base); p(index); p(value); }
        Inst::StoreGlobal { value, .. } => p(value),
        Inst::ArrayPush { arr, value, .. } => { p(arr); p(value); }
        Inst::MapNext { table, .. } => p(table),
    }
}

/// Remap an instruction's value operands through `m`.
pub(crate) fn remap_inst(inst: &mut Inst, m: &impl Fn(Idx<Inst>) -> Idx<Inst>) {
    let mut f = |x: &mut Idx<Inst>| *x = m(*x);
    match inst {
        Inst::Const(_) | Inst::Global(_) | Inst::Load(_) | Inst::Lambda { .. }
        | Inst::CoroHook { .. } => {}
        Inst::IntAdd(a, b) | Inst::IntSub(a, b) | Inst::IntMul(a, b) | Inst::IntDiv(a, b)
        | Inst::IntMod(a, b) | Inst::FloatAdd(a, b) | Inst::FloatSub(a, b) | Inst::FloatMul(a, b)
        | Inst::FloatDiv(a, b) | Inst::FloatMod(a, b) | Inst::StringConcat(a, b)
        | Inst::IntLt(a, b) | Inst::IntLeq(a, b) | Inst::IntGt(a, b) | Inst::IntGeq(a, b)
        | Inst::FloatLt(a, b) | Inst::FloatLeq(a, b) | Inst::FloatGt(a, b) | Inst::FloatGeq(a, b)
        | Inst::Eq(a, b) | Inst::NotEq(a, b) | Inst::BitAnd(a, b) | Inst::BitOr(a, b)
        | Inst::BitXor(a, b) | Inst::Shl(a, b) | Inst::Shr(a, b) => { f(a); f(b); }
        Inst::Neg(x) | Inst::Not(x) | Inst::BitNot(x) | Inst::ToString(x) | Inst::FloatToInt(x) | Inst::IntToFloat(x)
        | Inst::CoroSetInner(x) => f(x),
        Inst::Format { value, .. } => f(value),
        Inst::FormatDyn { value, fmt } => { f(value); f(fmt); }
        Inst::Call { args, .. } | Inst::CallMulti { args, .. } | Inst::CallBoundMulti { args, .. } => args.iter_mut().for_each(&mut f),
        Inst::CallValue { callee, args } => { f(callee); args.iter_mut().for_each(&mut f); }
        Inst::CallValueMulti { callee, args, .. } => { f(callee); args.iter_mut().for_each(&mut f); }
        Inst::MakeTuple(vs) | Inst::MakeVec(vs) => vs.iter_mut().for_each(&mut f),
        Inst::MakeVecColumns { columns, .. } => {
            columns.iter_mut().flat_map(|(_, vs)| vs).for_each(&mut f)
        }
        Inst::MakeStruct { fields, .. } | Inst::MakeValueStruct { fields, .. } | Inst::MakeEnv { fields, .. } => fields.iter_mut().for_each(|(_, a)| f(a)),
        Inst::MakeEnum { positional, named, .. } => {
            positional.iter_mut().for_each(&mut f);
            named.iter_mut().for_each(|(_, a)| f(a));
        }
        Inst::MakeEnumDyn { tag, positional, named, .. } => {
            f(tag);
            positional.iter_mut().for_each(&mut f);
            named.iter_mut().for_each(|(_, a)| f(a));
        }
        Inst::FfiNewArray { count, .. } => f(count),
        Inst::Field { base, .. } | Inst::TupleIndex { base, .. } => f(base),
        Inst::Index { base, index, .. } => { f(base); f(index); }
        Inst::VecCheckedIndex { vec, index } => { f(vec); f(index); }
        Inst::Len { base, .. } => f(base),
        Inst::Store { value, .. } => f(value),
        Inst::StoreField { base, value, .. } => { f(base); f(value); }
        Inst::StoreIndex { base, index, value, .. } => { f(base); f(index); f(value); }
        Inst::StoreGlobal { value, .. } => f(value),
        Inst::ArrayPush { arr, value, .. } => { f(arr); f(value); }
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

/// Spill every CROSS-BLOCK value (defined in one block, used in another) to the
/// state table at its def and reload it at each using block — so a value that
/// outlives a yield (a loop-carried collection/bound, a multi-yield arg)
/// survives the suspend/resume. (P2 — "only cells cross a yield" — is an
/// over-statement: lowering routes some data through values; this is the
/// canonical stackless spill, documented as a finding.) `protect` are param-like
/// values never spilled. Returns the sorted spill field names (`v<N>`) — an
/// input to the frame layout stamp.
fn spill_cross_block_values(step: &mut MirFunc, s_cell: Idx<Cell>, block_ids: &[Idx<Block>]) -> Vec<String> {
    // def block of every value.
    let mut def_block: HashMap<Idx<Inst>, Idx<Block>> = HashMap::default();
    for &bid in block_ids {
        for &id in &step.blocks[bid].insts {
            if step.insts[id].produces_value() {
                def_block.insert(id, bid);
            }
        }
    }
    // frame_values = values used in a block other than where defined.
    let mut frame_values: HashSet<Idx<Inst>> = HashSet::default();
    for &bid in block_ids {
        let mut used: Vec<Idx<Inst>> = Vec::new();
        for &id in &step.blocks[bid].insts {
            value_operands(&step.insts[id], &mut |v| used.push(v));
        }
        used.extend(term_value(&step.blocks[bid].term));
        for v in used {
            if def_block.get(&v).copied() != Some(bid) {
                frame_values.insert(v);
            }
        }
    }
    if frame_values.is_empty() {
        return Vec::new();
    }

    for &bid in block_ids {
        // Reloads needed in this block: frame values used here but defined
        // elsewhere. Sorted for deterministic output.
        let mut used_here: Vec<Idx<Inst>> = Vec::new();
        {
            let mut seen = HashSet::default();
            for &id in &step.blocks[bid].insts {
                value_operands(&step.insts[id], &mut |v| {
                    if frame_values.contains(&v) && def_block.get(&v).copied() != Some(bid) && seen.insert(v) {
                        used_here.push(v);
                    }
                });
            }
            if let Some(v) = term_value(&step.blocks[bid].term) {
                if frame_values.contains(&v) && def_block.get(&v).copied() != Some(bid) && seen.insert(v) {
                    used_here.push(v);
                }
            }
        }
        used_here.sort_by_key(|v| raw(*v));

        let old: Vec<Idx<Inst>> = std::mem::take(&mut step.blocks[bid].insts);
        let mut reload: HashMap<Idx<Inst>, Idx<Inst>> = HashMap::default();
        let mut new_list: Vec<Idx<Inst>> = Vec::new();
        for v in used_here {
            let sv = step.insts.alloc(Inst::Load(s_cell));
            let rv = step.insts.alloc(Inst::Field { base: sv, field: vfield(v) });
            new_list.push(sv);
            new_list.push(rv);
            reload.insert(v, rv);
        }
        let m = |x: Idx<Inst>| reload.get(&x).copied().unwrap_or(x);
        for id in old {
            remap_inst(&mut step.insts[id], &m);
            new_list.push(id);
            // Spill right after a frame value's def.
            if frame_values.contains(&id) && def_block.get(&id) == Some(&bid) {
                let sv = step.insts.alloc(Inst::Load(s_cell));
                let st = step.insts.alloc(Inst::StoreField { base: sv, field: vfield(id), value: id });
                new_list.push(sv);
                new_list.push(st);
            }
        }
        step.blocks[bid].insts = new_list;
        let mut term = step.blocks[bid].term.clone();
        remap_term(&mut term, &m);
        step.blocks[bid].term = term;
    }
    let mut names: Vec<String> = frame_values.iter().map(|&v| vfield(v).as_str().to_string()).collect();
    names.sort();
    names
}

/// Transform a lowered `co fn` into its `__step` state machine + frame info.
pub fn transform_coroutine(func: &MirFunc, channel: Option<&skull_mir::lower::YieldChannel>) -> CoroLowered {
    let frame_slot_channel = channel.is_some();
    let mut step = func.clone();
    let resume_arg = func.resume_arg.expect("co fn must carry a resume-arg cell");
    let params = func.params.clone();
    let s_cell = step.new_cell(Some(ustr::Ustr::from("s")));

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
    let mut hooks: Vec<(CoroHookKind, Idx<Inst>)> = Vec::new();
    let mut hook_lambdas: HashSet<Idx<Inst>> = HashSet::default();
    for &bid in &orig_blocks {
        for &id in &step.blocks[bid].insts {
            if let Inst::CoroHook { kind, lambda } = step.insts[id] {
                hooks.push((kind, lambda));
                hook_lambdas.insert(lambda);
            }
        }
    }
    if !hooks.is_empty() {
        for &bid in &orig_blocks {
            step.blocks[bid]
                .insts
                .retain(|&id| !matches!(step.insts[id], Inst::CoroHook { .. }) && !hook_lambdas.contains(&id));
        }
    }

    // 0b. Frame membership is OWNERSHIP-based (P1 of
    // docs/coroutine-frame-lambda-plan.md): a cell owned by a live lambda scope
    // is that lambda's plain local in its hoisted body, NOT a frame field.
    // Everything else (ROOT-owned, or accessed only from dead/peeled/detached
    // blocks -- hook bodies, defer copies -- which have no owner) stays in the
    // frame. Built post-peel so hook lambdas are dead, pre-rewrite so `Yield`
    // resume edges still link the CFG (`successors` follows them).
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
        // Ghost cells (cause B of the P2 finding): a cell referenced in NO block
        // anywhere -- scanned over ALL blocks incl. detached defer copies and
        // peeled hook bodies, deliberately broader than `scope::accesses` (which
        // only walks live-scope regions) -- is dropped. A dead lambda-local has
        // no owner so it would fall to ROOT/frame, but it is read/written
        // nowhere; a hook-only cell IS referenced in its detached block, so it
        // stays. Params always stay (the ctor seeds a field for each, used or
        // not) regardless of references.
        let referenced = referenced_cells(&step);
        let is_param: HashSet<Idx<Cell>> = params.iter().copied().collect();
        step.cells
            .iter()
            .map(|(id, _)| id)
            .filter(|&c| c != s_cell && c != resume_arg)
            .filter(|c| is_param.contains(c) || referenced.contains(c))
            .filter(|c| own.owner.get(c).is_none_or(|&o| o == crate::scope::ROOT))
            .collect()
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
                let sv = step.insts.alloc(Inst::Load(s_cell));
                step.blocks[bid].insts.push(sv);
                let store = step.insts.alloc(Inst::StoreField {
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
                    emit(&mut step, bid, Inst::StoreField { base: sv, field: skull_mir::lower::yield_slot(i), value: v });
                }
            }
            // The stamp is guarded on `state >= 0` (mirror the completion
            // stamp): a body that killed its own handle mid-run must stay
            // killed, not resurrect via a positive resume state. The killed
            // arm returns nothing -- a dead coroutine's resume yields nil.
            let sv = emit(&mut step, bid, Inst::Load(s_cell));
            let st = emit(&mut step, bid, Inst::Field { base: sv, field: ustr::Ustr::from(STATE) });
            let zero = emit(&mut step, bid, Inst::Const(Lit::Int(0)));
            let already_terminal = emit(&mut step, bid, Inst::IntLt(st, zero));
            let killed_bb = step.new_block();
            let stamp_bb = step.new_block();
            step.blocks[bid].term = Term::Branch { cond: already_terminal, then_bb: killed_bb, else_bb: stamp_bb };

            let kc = emit(&mut step, stamp_bb, Inst::Const(Lit::Int(k)));
            let sv2 = emit(&mut step, stamp_bb, Inst::Load(s_cell));
            emit(&mut step, stamp_bb, Inst::StoreField { base: sv2, field: ustr::Ustr::from(STATE), value: kc });
            step.blocks[stamp_bb].term = if frame_slot_channel { Term::Return(Vec::new()) } else { Term::Return(values) };
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
        let ret_val = match &step.blocks[bid].term {
            // A co fn's completion value is single (never scalarized); take it.
            Term::Return(vs) => Some(vs.first().copied()),
            _ => None,
        };
        if let Some(v0) = ret_val {
            if let Some(v) = v0 {
                let sv = emit(&mut step, bid, Inst::Load(s_cell));
                emit(&mut step, bid, Inst::StoreField { base: sv, field: ustr::Ustr::from("__return"), value: v });
            }
            let sv = emit(&mut step, bid, Inst::Load(s_cell));
            let st = emit(&mut step, bid, Inst::Field { base: sv, field: ustr::Ustr::from(STATE) });
            let zero = emit(&mut step, bid, Inst::Const(Lit::Int(0)));
            let already_terminal = emit(&mut step, bid, Inst::IntLt(st, zero));
            let killed_bb = step.new_block();
            let stamp_bb = step.new_block();
            step.blocks[bid].term = Term::Branch { cond: already_terminal, then_bb: killed_bb, else_bb: stamp_bb };

            let dc = emit(&mut step, stamp_bb, Inst::Const(Lit::Int(-3)));
            let sv2 = emit(&mut step, stamp_bb, Inst::Load(s_cell));
            emit(&mut step, stamp_bb, Inst::StoreField { base: sv2, field: ustr::Ustr::from(STATE), value: dc });
            step.blocks[stamp_bb].term = Term::Return(Vec::new());

            if v0.is_some() {
                let nil = emit(&mut step, killed_bb, Inst::Const(Lit::Nil));
                let sv3 = emit(&mut step, killed_bb, Inst::Load(s_cell));
                emit(&mut step, killed_bb, Inst::StoreField { base: sv3, field: ustr::Ustr::from("__return"), value: nil });
            }
            step.blocks[killed_bb].term = Term::Return(Vec::new());
        }
    }

    // 3b. Spill cross-block values (loop-carried collections/bounds, multi-yield
    // args) so they survive a suspend/resume — see `spill_cross_block_values`.
    let spill_fields = spill_cross_block_values(&mut step, s_cell, &orig_blocks);

    // 3c. Frame layout stamp. Hash everything that fixes the frame's field
    // meanings + the state numbering: (cell idx, source name) pairs, the spill
    // field set, per-state variant tags, and the yield-channel shape. Body
    // LOGIC/values/spans are deliberately excluded so a layout-stable edit keeps
    // live coroutines running (the live-update feature). The ctor writes the
    // stamp; the step's entry (below) rejects a frame with a different one --
    // through `error()`, so the pcall in `__advance` turns it into a clean
    // errored kill (docs/coroutine-allocation-plan.md "Layout drift").
    let mut canon = String::new();
    {
        use std::fmt::Write as _;
        // Slot order == raw-id order (dense_slots numbers by raw id, cells.iter()
        // yields by raw id), so hashing `c<slot>:<name>` here matches both the
        // emitted field names and the slot ordinals.
        for (id, cell) in step.cells.iter() {
            if let Some(&slot) = frame_slots.get(&id) {
                let _ = write!(canon, "c{}:{};", slot, cell.name.map(|n| n.as_str().to_string()).unwrap_or_default());
            }
        }
        for f in &spill_fields {
            let _ = write!(canon, "{f};");
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
            let st = emit(&mut step, bid, Inst::Field { base: sv, field: ustr::Ustr::from(STATE) });
            let kc = emit(&mut step, bid, Inst::Const(Lit::Int(k as i64 + 1)));
            let eq = emit(&mut step, bid, Inst::Eq(st, kc));
            let else_bb = if k + 1 < num_states { check_blocks[k + 1] } else { orig_entry };
            step.blocks[bid].term = Term::Branch { cond: eq, then_bb: resume_of_state[k], else_bb };
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
        let lay = emit(&mut step, chk, Inst::Field { base: sv, field: ustr::Ustr::from("__layout") });
        let kc = emit(&mut step, chk, Inst::Const(Lit::Int(layout_hash)));
        let ne = emit(&mut step, chk, Inst::NotEq(lay, kc));
        let raise = step.new_block();
        let err_fn = emit(&mut step, raise, Inst::Global(ustr::Ustr::from("error")));
        let msg = emit(
            &mut step,
            raise,
            Inst::Const(Lit::Str(ustr::Ustr::from(
                format!("coroutine killed: frame layout changed by hot reload ({})", func.name).as_str(),
            ))),
        );
        emit(&mut step, raise, Inst::CallValue { callee: err_fn, args: vec![msg] });
        // Unreachable (error() raised); a terminator is still required.
        step.blocks[raise].term = Term::Goto(dispatch);
        step.blocks[chk].term = Term::Branch { cond: ne, then_bb: raise, else_bb: dispatch };
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
        frame_cells,
        frame_slots,
        params,
        num_states,
        hooks,
        layout_hash,
        step_tree: tree,
        step_own: own,
    }
}

/// Cells referenced (read or written) anywhere in `func`'s block inst-lists --
/// over ALL blocks in the arena, including detached defer/hook bodies not
/// reachable from the entry. Matches `scope::accesses`'s reference set (`Load`,
/// `Store`, `MapNext` dests, multi-call dests) but is deliberately broader in
/// block coverage. Used to distinguish a genuinely-dead ghost cell (referenced
/// nowhere) from a hook-only cell (referenced in a detached block).
fn referenced_cells(func: &MirFunc) -> HashSet<Idx<Cell>> {
    let mut set: HashSet<Idx<Cell>> = HashSet::default();
    for (_, blk) in func.blocks.iter() {
        for &iid in &blk.insts {
            match &func.insts[iid] {
                Inst::Load(c) => {
                    set.insert(*c);
                }
                Inst::Store { cell, .. } => {
                    set.insert(*cell);
                }
                Inst::MapNext { key_cell, val_cell, .. } => {
                    set.insert(*key_cell);
                    set.insert(*val_cell);
                }
                Inst::CallMulti { dests, .. }
                | Inst::CallValueMulti { dests, .. }
                | Inst::CallBoundMulti { dests, .. } => {
                    set.extend(dests.iter().copied());
                }
                _ => {}
            }
        }
    }
    set
}

/// Dense per-frame slot map: the given cells numbered 0.. by raw-id order.
fn dense_slots(cells: impl Iterator<Item = Idx<Cell>>) -> HashMap<Idx<Cell>, u32> {
    let mut v: Vec<Idx<Cell>> = cells.collect();
    v.sort_by_key(|c| raw(*c));
    v.into_iter().enumerate().map(|(i, c)| (c, i as u32)).collect()
}

impl CoroLowered {
    /// The all-cells aux frame slot map for hook/defer bodies (D5): the step's
    /// slots shared verbatim (a hook reads the same field the ctor wrote),
    /// aux-only cells appended after the step's range in raw-id order. Total
    /// over every step cell except `s`/`resume_arg` -- so every reference in a
    /// detached hook/defer region resolves to `s.c<slot>`, never a bare local.
    pub fn aux_all_cells_slots(&self, resume_arg: Idx<Cell>) -> HashMap<Idx<Cell>, u32> {
        let mut s = self.frame_slots.clone();
        let base = self.frame_slots.len() as u32;
        let mut extra: Vec<Idx<Cell>> = self
            .step
            .cells
            .iter()
            .map(|(id, _)| id)
            .filter(|&c| c != self.s_cell && c != resume_arg && !s.contains_key(&c))
            .collect();
        extra.sort_by_key(|c| raw(*c));
        for (i, c) in extra.into_iter().enumerate() {
            s.insert(c, base + i as u32);
        }
        s
    }
}

/// Materialize a coroutine region's frame accesses as MIR (the JS path's
/// analog of codegen-lua's root `FrameCtx` rendering): over the blocks
/// reachable from `entry` via terminators, a bare `Load`/`Store` of a cell in
/// `slots` becomes a `Field`/`StoreField` on `s` (field `c<slot>`), and a
/// multi-call dest that is a frame cell is redirected through a fresh temp
/// cell + a spliced store-back. `MapNext` cells are left untouched (out of the
/// JS subset; its own inst rejects downstream). Afterwards every remaining
/// `Load`/`Store` cell in the region must be `s`, one of `mf.params`, or a
/// temp this pass minted -- anything else is a frame-membership finding, so it
/// panics rather than emitting an undeclared local.
pub fn materialize_frame(
    mf: &mut MirFunc,
    entry: Idx<Block>,
    s_cell: Idx<Cell>,
    slots: &HashMap<Idx<Cell>, u32>,
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
                    let sv = mf.insts.alloc(Inst::Load(s_cell));
                    new.push(sv);
                    mf.insts[id] = Inst::Field { base: sv, field: field_of(slots[&c]) };
                    new.push(id);
                }
                Inst::Store { cell, value } if slots.contains_key(&cell) => {
                    let sv = mf.insts.alloc(Inst::Load(s_cell));
                    new.push(sv);
                    mf.insts[id] = Inst::StoreField { base: sv, field: field_of(slots[&cell]), value };
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
                        let lt = mf.insts.alloc(Inst::Load(temp));
                        let sv = mf.insts.alloc(Inst::Load(s_cell));
                        let sf = mf.insts.alloc(Inst::StoreField {
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
                Inst::MapNext { key_cell, val_cell, table }
                    if slots.contains_key(&key_cell) || slots.contains_key(&val_cell) =>
                {
                    let k_use = if slots.contains_key(&key_cell) {
                        let t = mf.new_cell(None);
                        minted.insert(t);
                        let sv = mf.insts.alloc(Inst::Load(s_cell));
                        let fld = mf.insts.alloc(Inst::Field { base: sv, field: field_of(slots[&key_cell]) });
                        let st = mf.insts.alloc(Inst::Store { cell: t, value: fld });
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
                    mf.insts[id] = Inst::MapNext { key_cell: k_use, val_cell: v_use, table };
                    new.push(id);
                    for (frame_cell, temp) in
                        [(key_cell, k_use), (val_cell, v_use)].into_iter().filter(|(c, _)| slots.contains_key(c))
                    {
                        let lt = mf.insts.alloc(Inst::Load(temp));
                        let sv = mf.insts.alloc(Inst::Load(s_cell));
                        let sf = mf.insts.alloc(Inst::StoreField {
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
    // Exhaustiveness: a surviving bare cell is a frame-membership finding.
    let allowed = |c: Idx<Cell>| {
        c == s_cell || mf.params.contains(&c) || minted.contains(&c) || mf.resume_arg == Some(c)
    };
    for &bid in &region {
        for &id in &mf.blocks[bid].insts {
            let bad = match &mf.insts[id] {
                Inst::Load(c) if !allowed(*c) => Some(*c),
                Inst::Store { cell, .. } if !allowed(*cell) => Some(*cell),
                Inst::MapNext { key_cell, .. } if !allowed(*key_cell) => Some(*key_cell),
                Inst::MapNext { val_cell, .. } if !allowed(*val_cell) => Some(*val_cell),
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
