use rustc_hash::FxHashSet as HashSet;

use crate::visit::{raw, top_level_blocks};
use bone_core::workspace::Workspace;
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
    /// Whether applying `subst` creates a shape the next analysis proves more
    /// on: a field read whose base resolves to a fresh `MakeStruct`, or a store
    /// whose value resolves to a load of its own cell.
    pub(crate) exposes_more: bool,
}

/// Build the analysis without applying it. Returns `None` when the walk proves
/// nothing (the common case for small bodies), so callers skip the remap
/// entirely. [`cleanup`] folds the remap into DCE's count scan.
pub(crate) fn copy_propagate_analysis(func: &MirFunc, ws: Option<&Workspace>) -> Option<CopyProp> {
    // A call can invoke a closure that writes a cell.
    let mut main = vec![false; func.blocks.len()];
    for b in top_level_blocks(func) {
        main[raw(b)] = true;
    }
    // Borrowed cells can be written through aliases.
    let mut escaping: Vec<bool> = func.cells.iter().map(|(_, cell)| cell.borrowed).collect();
    let mut holds_value = vec![false; func.cells.len()];
    let mut field_written = vec![false; func.cells.len()];
    let mut has_fresh_projection = false;
    let mut preds = vec![0u32; func.blocks.len()];
    for (bid, b) in func.blocks.iter() {
        skull_mir::cfg::successors(&b.term, |t| preds[raw(t)] += 1);
        for &i in &b.insts {
            match &func.insts[i] {
                Inst::Store { cell, value } => {
                    escaping[raw(*cell)] |= !main[raw(bid)];
                    holds_value[raw(*cell)] |=
                        matches!(func.insts[*value], Inst::MakeValueStruct { .. });
                }
                Inst::StoreField { base, .. } => {
                    let mut root = *base;
                    while let Inst::Field { base, .. } = func.insts[root] {
                        root = base;
                    }
                    if let Inst::Load(cell) = func.insts[root] {
                        field_written[raw(cell)] = true;
                    }
                }
                _ => {
                    if let Some((base, _)) = func.field_projection(i) {
                        has_fresh_projection |= matches!(func.insts[base], Inst::MakeStruct { .. });
                    }
                }
            }
        }
    }
    // A value's field write mutates the cell; forwarding could read the ctor's
    // original operands instead. Table aliases still refer to the same object.
    for (c, escapes) in escaping.iter_mut().enumerate() {
        *escapes |= holds_value[c] && field_written[c];
    }
    // Only direct temporary projections need use counts. An aliased mutable
    // constructor cannot be forwarded using a pre-substitution count.
    let struct_uses = if has_fresh_projection {
        let mut uses = vec![0u32; func.insts.len()];
        for (_, b) in func.blocks.iter() {
            for &i in &b.insts {
                func.insts[i].uses_each(&mut |op| uses[raw(op)] += 1);
            }
            b.term.uses_each(&mut |op| uses[raw(op)] += 1);
        }
        uses
    } else {
        Vec::new()
    };
    let mut subst: Vec<Option<Idx<Inst>>> = vec![None; func.insts.len()];
    let mut self_stores = vec![false; func.insts.len()];
    let mut nonthrowing = vec![false; func.insts.len()];
    let mut any_subst = false;
    let mut exposes_more = false;
    let mut cur: Vec<Option<Idx<Inst>>> = vec![None; func.cells.len()];
    let mut touched: Vec<usize> = Vec::new();
    // Carry literals across a single goto predecessor so an inlined default
    // can fold without extending any value's live range.
    let mut entry_literals: Vec<Option<Vec<(usize, Idx<Inst>)>>> = vec![None; func.blocks.len()];
    for (bid, b) in func.blocks.iter() {
        for &t in &touched {
            cur[t] = None;
        }
        touched.clear();
        if let Some(carried) = entry_literals[raw(bid)].take() {
            for (ck, v) in carried {
                cur[ck] = Some(v);
                touched.push(ck);
            }
        }
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
            if !matches!(func.insts[id], Inst::Store { .. }) {
                // Multi-result calls and `MapNext` assign their cells directly.
                func.insts[id].cell_writes_each(&mut |c| cur[raw(c)] = None);
            }
            if let Inst::Store { cell, .. } = &func.insts[id] {
                let rebound = Operand::Cell(*cell);
                vec_checks.retain(|((v, i), _)| *v != rebound && *i != rebound);
                checked_vecs.remove(&rebound);
            }
            match &func.insts[id] {
                Inst::VecCheckedIndex { vec, index } => {
                    let key = (
                        operand_key(func, &subst, &escaping, *vec),
                        operand_key(func, &subst, &escaping, *index),
                    );
                    if let Some(&(_, earlier)) = vec_checks.iter().find(|(k, _)| *k == key) {
                        subst[raw(id)] = Some(earlier);
                        nonthrowing[raw(id)] = true;
                        any_subst = true;
                    } else {
                        vec_checks.push((key, id));
                    }
                    checked_vecs.insert(key.0);
                }
                Inst::CallValue { callee, args }
                    if args.len() == 2
                        && matches!(func.insts[resolve_subst(&subst, *callee)], Inst::Global(name) if name.as_str() == VALUE_INTEGER_CHECK) =>
                {
                    let value = resolve_subst(&subst, args[0]);
                    if let Inst::Const(Lit::Bool(unsigned)) =
                        func.insts[resolve_subst(&subst, args[1])]
                        && integer_field_constant_fits(&func.insts[value], unsigned)
                    {
                        subst[raw(id)] = Some(value);
                        nonthrowing[raw(id)] = true;
                        any_subst = true;
                    }
                }
                Inst::Call { func: target, args }
                    if args.len() == 2
                        && ws.is_some_and(|ws| {
                            let module = ws.module(target.module);
                            let f = &module.functions[target.func];
                            bone_core::modules::is_stdlib_module_file(module.file.as_str())
                                && f.name.as_str() == "__nb_checked_index"
                                && f.inline_body.is_some()
                                && f.annotations.iter().any(|a| a.is_builtin("nomangle"))
                        }) =>
                {
                    checked_native_buffers.insert(resolve_subst(&subst, args[0]));
                }
                Inst::Store { cell, value } if !escaping[raw(*cell)] => {
                    let ck = raw(*cell);
                    let v = resolve_subst(&subst, *value);
                    // Without a visible source, storing the current load is still a no-op.
                    let noop = match cur[ck] {
                        Some(w) => resolve_subst(&subst, w) == v,
                        None => matches!(&func.insts[*value], Inst::Load(d) if *d == *cell),
                    };
                    if noop {
                        self_stores[raw(id)] = true;
                    } else {
                        exposes_more |= cur[ck].is_none()
                            && matches!(&func.insts[v], Inst::Load(d) if *d == *cell);
                        if cur[ck].is_none() {
                            touched.push(ck);
                        }
                        cur[ck] = Some(v);
                    }
                }
                Inst::Load(c) => {
                    if let Some(v) = cur[raw(*c)] {
                        subst[raw(id)] = Some(v);
                        any_subst = true;
                    }
                }
                // A value field can forward through its constructor while the constructor's
                // contents are still the observed snapshot.
                Inst::Field { .. }
                | Inst::Index {
                    kind: ContainerKind::Record,
                    ..
                } => {
                    let Some((base, field)) = func.field_projection(id) else {
                        continue;
                    };
                    let rb = resolve_subst(&subst, base);
                    if checked_native_buffers.contains(&rb)
                        && skull_mir::lower::is_leaf_view_field(field.as_str())
                    {
                        // The retained bounds check already accessed this exact
                        // buffer. Its generated view keys are plain table fields.
                        nonthrowing[raw(id)] = true;
                    }
                    if checked_vecs.contains(&operand_key(func, &subst, &escaping, base)) {
                        // A completed bounds check read this Vec's `len`, so it is
                        // a plain table: its column and `_data` reads cannot raise.
                        nonthrowing[raw(id)] = true;
                    }
                    if let Inst::MakeValueStruct { fields, form, .. } = &func.insts[rb] {
                        nonthrowing[raw(id)] = fields.iter().any(|(name, _)| *name == field)
                            || (*form == ValueCtorForm::Leaves
                                && fields.iter().any(|(name, _)| {
                                    name.as_str()
                                        .strip_prefix(field.as_str())
                                        .is_some_and(|tail| tail.starts_with('.'))
                                }));
                        if let Some((_, op)) = fields.iter().find(|(n, _)| *n == field) {
                            subst[raw(id)] = Some(resolve_subst(&subst, *op));
                            any_subst = true;
                        }
                    } else if base != rb && matches!(func.insts[rb], Inst::MakeStruct { .. }) {
                        exposes_more = true;
                    } else if base == rb
                        && !struct_uses.is_empty()
                        && struct_uses[raw(base)] == 1
                        && let Inst::MakeStruct { fields, .. } = &func.insts[base]
                        && let Some((_, op)) = fields.iter().find(|(name, _)| *name == field)
                    {
                        // An aliased mutable record cannot be read from its original operands.
                        nonthrowing[raw(id)] = true;
                        subst[raw(id)] = Some(resolve_subst(&subst, *op));
                        any_subst = true;
                    } else if let Some((ctor, prefix)) = leaf_form_path(func, &subst, rb) {
                        // `Field(Field(ctor, c0), x)` is the ctor's `c0.x` leaf.
                        let Inst::MakeValueStruct { fields, .. } = &func.insts[ctor] else {
                            unreachable!("leaf_form_path roots at a value ctor")
                        };
                        let key = format!("{prefix}.{field}");
                        nonthrowing[raw(id)] = fields.iter().any(|(name, _)| {
                            name.as_str() == key
                                || name
                                    .as_str()
                                    .strip_prefix(&key)
                                    .is_some_and(|tail| tail.starts_with('.'))
                        });
                        if let Some((_, op)) = fields.iter().find(|(n, _)| n.as_str() == key) {
                            subst[raw(id)] = Some(resolve_subst(&subst, *op));
                            any_subst = true;
                        }
                    }
                }
                // The converse: a tuple rebuilt from every slot of one flat box,
                // in order, is that box. A tuple is immutable, so it is shared.
                Inst::MakeTuple(vs) if !vs.is_empty() => {
                    let slot_of = |v: Idx<Inst>| match func.insts[resolve_subst(&subst, v)] {
                        Inst::TupleIndex { base, index } => {
                            Some((resolve_subst(&subst, base), index))
                        }
                        _ => None,
                    };
                    if let Some((base, 0)) = slot_of(vs[0])
                        && func.flat_box_widths.get(&base) == Some(&vs.len())
                        && vs
                            .iter()
                            .enumerate()
                            .all(|(i, &v)| slot_of(v) == Some((base, i)))
                    {
                        subst[raw(id)] = Some(base);
                        any_subst = true;
                    }
                }
                // A fresh tuple is immutable.
                Inst::TupleIndex { base, index } => {
                    let (base, index) = (*base, *index);
                    let rb = resolve_subst(&subst, base);
                    if let Inst::MakeTuple(vs) = &func.insts[rb] {
                        if let Some(op) = vs.get(index) {
                            nonthrowing[raw(id)] = true;
                            subst[raw(id)] = Some(resolve_subst(&subst, *op));
                            any_subst = true;
                        }
                    }
                }
                _ => {}
            }
        }
        if let Term::Goto(next) = b.term
            && preds[raw(next)] == 1
            && raw(next) > raw(bid)
        {
            let carried = touched
                .iter()
                .filter_map(|&ck| {
                    let v = resolve_subst(&subst, cur[ck]?);
                    matches!(func.insts[v], Inst::Const(_)).then_some((ck, v))
                })
                .collect();
            entry_literals[raw(next)] = Some(carried);
        }
    }
    (any_subst || self_stores.iter().any(|&noop| noop) || nonthrowing.iter().any(|&safe| safe))
        .then_some(CopyProp {
            subst,
            self_stores,
            nonthrowing,
            exposes_more,
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
        Inst::Load(cell) if !escaping[raw(cell)] => Operand::Cell(cell),
        _ => Operand::Value(v),
    }
}

/// Whether an instruction can invalidate a vector bounds check.
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
                | ContainerKind::VecNonNullInBounds
                | ContainerKind::NativeBufferInBounds
                | ContainerKind::NativeBufferLeaf
        ),
        Inst::Index {
            kind:
                ContainerKind::Vec
                | ContainerKind::VecInBounds
                | ContainerKind::Map
                | ContainerKind::Record,
            ..
        } => false,
        // A boxed Vec, Map, or heap-record lookup can enter a Lua metamethod.
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
    let Inst::Field { base, field, .. } = &func.insts[x] else {
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
pub(crate) fn substitute_uses(func: &mut MirFunc, subst: &[Option<Idx<Inst>>]) {
    let remap = |value| resolve_subst(subst, value);
    for (_, inst) in func.insts.iter_mut() {
        inst.remap_uses(&remap);
    }
    for (_, block) in func.blocks.iter_mut() {
        block.term.remap_uses(&remap);
    }
}

pub(crate) fn resolve_subst(subst: &[Option<Idx<Inst>>], mut x: Idx<Inst>) -> Idx<Inst> {
    while let Some(y) = subst[raw(x)] {
        if y == x {
            break;
        }
        x = y;
    }
    x
}

fn integer_field_constant_fits(inst: &Inst, unsigned: bool) -> bool {
    let value = match inst {
        Inst::Const(Lit::Int(n)) => *n as f64,
        Inst::Const(Lit::Uint(n)) => f64::from(*n),
        Inst::Const(Lit::Float(n)) => *n,
        Inst::Const(Lit::Nil) => return true,
        _ => return false,
    };
    let (min, max) = if unsigned {
        (0.0, u32::MAX as f64)
    } else {
        (i32::MIN as f64, i32::MAX as f64)
    };
    value >= min && value <= max && value == value.floor()
}
