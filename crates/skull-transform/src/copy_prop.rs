use rustc_hash::FxHashSet as HashSet;

use crate::visit::{raw, top_level_blocks};
use bone_semantic::inference::Workspace;
use comfy_la_arena::Idx;
use skull_mir::mir::*;

/// The intra-block copy-propagation analysis: the value->value substitution
/// chain, plus the stores the same walk proves are no-ops.
pub(crate) struct CopyProp {
    pub(crate) subst: Vec<Option<Idx<Inst>>>,
    /// Per-inst flags; a true entry is a `Store` writing back the value its cell
    /// already holds, i.e. removable even though the cell still has loads.
    pub(crate) self_stores: Vec<bool>,
    /// Projections proven not to raise; their abandoned nodes need not survive DCE.
    pub(crate) nonthrowing: Vec<bool>,
}

/// Build the analysis without applying it. Returns `None` when the walk proves
/// nothing (the common case for small bodies), so callers skip the remap
/// entirely. [`cleanup`] folds the remap into DCE's count scan.
pub(crate) fn copy_propagate_analysis(func: &MirFunc, ws: Option<&Workspace>) -> Option<CopyProp> {
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
        let mut checked_vecs: HashSet<Operand> = HashSet::default();
        // Live `VecCheckedIndex` results by (vec, index) operand.
        let mut vec_checks: Vec<((Operand, Operand), Idx<Inst>)> = Vec::new();
        for &id in &b.insts {
            // Clearing the cell-keyed facts too covers multi-result calls,
            // which write their dest cells without a `Store`.
            if !keeps_vec_lengths(&func.insts[id]) {
                vec_checks.clear();
                checked_vecs.clear();
            }
            if let Inst::Store { cell, .. } = &func.insts[id] {
                let rebound = Operand::Cell(*cell);
                vec_checks.retain(|((v, i), _)| *v != rebound && *i != rebound);
                checked_vecs.remove(&rebound);
            }
            match &func.insts[id] {
                // A repeat of a check that still holds: the operands are the same
                // values, and nothing since could have changed the Vec's length.
                // The first check raised already if the index was out of bounds.
                Inst::VecCheckedIndex { vec, index } => {
                    let key = (
                        operand_key(func, &subst, &escaping, *vec),
                        operand_key(func, &subst, &escaping, *index),
                    );
                    if let Some(&(_, earlier)) = vec_checks.iter().find(|(k, _)| *k == key) {
                        subst[raw(id) as usize] = Some(earlier);
                        nonthrowing[raw(id) as usize] = true;
                        any_subst = true;
                    } else {
                        vec_checks.push((key, id));
                    }
                    checked_vecs.insert(key.0);
                }
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
                    if checked_vecs.contains(&operand_key(func, &subst, &escaping, base)) {
                        // A completed bounds check read this Vec's `len`, so it is
                        // a plain table: its column and `_data` reads cannot raise.
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

/// What a checked operand is keyed by: a value, or the current contents of a
/// local cell, so two loads of one cell with no store between them match.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Operand {
    Value(Idx<Inst>),
    Cell(Idx<Cell>),
}

fn operand_key(
    func: &MirFunc,
    subst: &[Option<Idx<Inst>>],
    escaping: &[bool],
    v: Idx<Inst>,
) -> Operand {
    let v = resolve_subst(subst, v);
    match func.insts[v] {
        Inst::Load(cell) if !escaping[raw(cell) as usize] => Operand::Cell(cell),
        _ => Operand::Value(v),
    }
}

/// Whether `inst` certainly leaves every Vec's length alone, so a bounds check
/// before it still holds after it. An allow-list: any call (direct, indirect or
/// through a global) may push or pop, as may a `len` store, an append, a
/// whole-element write or a coroutine operation.
fn keeps_vec_lengths(inst: &Inst) -> bool {
    match inst {
        // A cell write rebinds a local; facts keyed on that cell are dropped
        // separately.
        Inst::Store { .. } => true,
        // Raw slot writes inside a checked element.
        Inst::StoreIndex { kind, .. } => matches!(
            kind,
            ContainerKind::VecColumn
                | ContainerKind::VecStrideSlot
                | ContainerKind::NativeBufferLeaf
        ),
        // Reads that may raise but change nothing.
        Inst::Index { .. }
        | Inst::VecCheckedIndex { .. }
        | Inst::Field { .. }
        | Inst::TupleIndex { .. }
        | Inst::Len { .. } => true,
        _ => inst.produces_value() && !skull_mir::sinking::may_throw(inst),
    }
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
pub(crate) fn resolve_subst(subst: &[Option<Idx<Inst>>], mut x: Idx<Inst>) -> Idx<Inst> {
    while let Some(y) = subst[raw(x) as usize] {
        if y == x {
            break;
        }
        x = y;
    }
    x
}
