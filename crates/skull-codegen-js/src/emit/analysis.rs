//! Body analyses: operand walks, inlinability, cross-block temps and reachability.

use super::*;

/// The cells an inst writes: a `Store`'s target, or a multi-value call's dests.
/// These are the cells that need a hoisted `let` (the multi-assign / store binds
/// them, and a later block may read them across a branch merge).
pub(super) fn inst_writes(inst: &Inst) -> Vec<Idx<Cell>> {
    match inst {
        Inst::Store { cell, .. } => vec![*cell],
        Inst::CallMulti { dests, .. }
        | Inst::CallValueMulti { dests, .. }
        | Inst::CallBoundMulti { dests, .. } => dests.clone(),
        // MapNext writes both the cursor key cell (in place) and the value cell.
        Inst::MapNext {
            key_cell, val_cell, ..
        } => vec![*key_cell, *val_cell],
        _ => Vec::new(),
    }
}

/// The `NativeBuffer*` nominal names (T-typed + phantom-size variants); the
/// same set skull-mir's lowering recognizes as `ContainerKind::NativeBuffer`.
pub(super) fn is_native_buffer(name: &str) -> bool {
    matches!(
        name,
        "NativeBuffer"
            | "NativeBufferU8"
            | "NativeBufferI8"
            | "NativeBufferU16"
            | "NativeBufferI16"
            | "NativeBufferU32"
            | "NativeBufferI32"
    )
}

/// `Inst::uses_each`, but a `CoroHook`'s lambda also counts as a use here, so
/// the JS use counts and cross-block checks still see the hooked closure.
pub(super) fn for_each_operand(inst: &Inst, f: &mut impl FnMut(Idx<Inst>)) {
    inst.uses_each(f);
    if let Inst::CoroHook { lambda, .. } = inst {
        f(*lambda);
    }
}

/// An expression candidate for folding into its single use site. The shared
/// sinking check separately constrains potentially throwing evaluations.
/// EXCLUDES `Load`/`Call`/`Index`/`Field`/`Len` (impure or cell-order-sensitive)
/// -- those materialize at their program position. Mirrors codegen-lua's
/// `always_inlinable`.
pub(super) fn always_inlinable(inst: &Inst) -> bool {
    matches!(
        inst,
        Inst::Const(_)
            | Inst::IntAdd(..)
            | Inst::IntSub(..)
            | Inst::IntMul(..)
            | Inst::IntDiv(..)
            | Inst::IntMod(..)
            | Inst::UintAdd(..)
            | Inst::UintSub(..)
            | Inst::UintMul(..)
            | Inst::UintDiv(..)
            | Inst::UintMod(..)
            | Inst::FloatAdd(..)
            | Inst::FloatSub(..)
            | Inst::FloatMul(..)
            | Inst::FloatDiv(..)
            | Inst::FloatMod(..)
            | Inst::StringConcat(..)
            | Inst::IntLt(..)
            | Inst::IntLeq(..)
            | Inst::IntGt(..)
            | Inst::IntGeq(..)
            | Inst::UintLt(..)
            | Inst::UintLeq(..)
            | Inst::UintGt(..)
            | Inst::UintGeq(..)
            | Inst::FloatLt(..)
            | Inst::FloatLeq(..)
            | Inst::FloatGt(..)
            | Inst::FloatGeq(..)
            | Inst::Eq(..)
            | Inst::NotEq(..)
            | Inst::BitAnd(..)
            | Inst::BitOr(..)
            | Inst::BitXor(..)
            | Inst::Shl(..)
            | Inst::Shr(..)
            | Inst::UintBitAnd(..)
            | Inst::UintBitOr(..)
            | Inst::UintBitXor(..)
            | Inst::UintShl(..)
            | Inst::UintShr(..)
            | Inst::Neg(_)
            | Inst::Not(_)
            | Inst::BitNot(_)
            | Inst::UintBitNot(_)
            | Inst::ToString(_)
            | Inst::FloatToInt(_)
            | Inst::IntToFloat(_)
            | Inst::UintToFloat(_)
            | Inst::FloatToF32(_)
            | Inst::IntToUint(_)
            | Inst::UintToInt(_)
            | Inst::FloatToUint(_)
            | Inst::UintBitcastInt(_)
            | Inst::IntBitcastUint(_)
            | Inst::Format { .. }
            | Inst::FormatDyn { .. }
            | Inst::MakeTuple(_)
            | Inst::MakeVec(_)
            | Inst::MakeStruct { .. }
            | Inst::MakeValueStruct { .. }
            | Inst::MakeNullableValue { .. }
            | Inst::MakeEnum { .. }
            | Inst::MakeEnumDyn { .. }
            | Inst::MakeEnv { .. }
    )
}

/// Use counts (dense by raw inst id, over reachable blocks) + the inlinable
/// mask: an `always_inlinable` value used EXACTLY ONCE with def and use in the
/// SAME block folds into that use; everything else materializes at its program
/// position. The shared sinking check also preserves effect/error order.
pub(super) fn compute_inlinable(mf: &MirFunc) -> (Vec<u32>, Vec<bool>) {
    compute_inlinable_over(mf, &reachable_blocks(mf))
}

/// [`compute_inlinable`] over an explicit block region -- the trampoline mode
/// analyzes a detached hook/defer region, not the fn-entry region.
pub(super) fn compute_inlinable_over(mf: &MirFunc, blocks: &[Idx<Block>]) -> (Vec<u32>, Vec<bool>) {
    const NONE: u32 = u32::MAX;
    let ni = mf.insts.len();
    let mut use_count = vec![0u32; ni];
    let mut use_block = vec![NONE; ni];
    let mut def_block = vec![NONE; ni];
    for &b in blocks {
        let br = raw(b);
        for &id in &mf.blocks[b].insts {
            if mf.insts[id].produces_value() {
                def_block[raw(id) as usize] = br;
            }
            for_each_operand(&mf.insts[id], &mut |u| {
                let k = raw(u) as usize;
                use_count[k] += 1;
                use_block[k] = br;
            });
        }
        mf.blocks[b].term.uses_each(&mut |u| {
            let k = raw(u) as usize;
            use_count[k] += 1;
            use_block[k] = br;
        });
    }
    let mut inl = vec![false; ni];
    for &b in blocks {
        for &id in &mf.blocks[b].insts {
            let k = raw(id) as usize;
            if use_count[k] == 1
                && def_block[k] != NONE
                && def_block[k] == use_block[k]
                && always_inlinable(&mf.insts[id])
            {
                inl[k] = true;
            }
        }
    }
    skull_mir::sinking::preserve_effect_order(mf, blocks, &mut inl);
    (use_count, inl)
}

/// Materialized value temps (raw inst ids) whose use crosses their defining
/// block. The structured emitter nests blocks so such a temp stays in lexical
/// scope; the trampoline puts each block in a SIBLING switch case, so a
/// cross-case `let v<N>` would be undefined at the use -- these get hoist-
/// declared at the fn top and assigned in-place ([`Emitter::hoisted_temps`]). A
/// same-block temp is not returned (keeps its per-case `let`).
pub(super) fn cross_block_temps(
    mf: &MirFunc,
    region: &[Idx<Block>],
    use_count: &[u32],
    inlinable: &[bool],
) -> HashSet<u32> {
    let mut def_block: HashMap<u32, Idx<Block>> = HashMap::new();
    for &b in region {
        for &iid in &mf.blocks[b].insts {
            let k = raw(iid);
            if mf.insts[iid].produces_value() && !inlinable[k as usize] && use_count[k as usize] > 0
            {
                def_block.insert(k, b);
            }
        }
    }
    let mut cross: HashSet<u32> = HashSet::new();
    for &b in region {
        let mut check = |op: Idx<Inst>| {
            let k = raw(op);
            if let Some(&db) = def_block.get(&k) {
                if db != b {
                    cross.insert(k);
                }
            }
        };
        for &iid in &mf.blocks[b].insts {
            for_each_operand(&mf.insts[iid], &mut check);
        }
        mf.blocks[b].term.uses_each(&mut check);
    }
    cross
}

/// Successor blocks of `b` (`None` == the virtual exit).
fn succ(mf: &MirFunc, b: Idx<Block>) -> Vec<Option<Idx<Block>>> {
    match &mf.blocks[b].term {
        Term::Return(_) | Term::Unreachable => vec![None],
        Term::Goto(t) => vec![Some(*t)],
        Term::Branch {
            then_bb, else_bb, ..
        } => vec![Some(*then_bb), Some(*else_bb)],
        Term::Switch { cases, default, .. } => {
            let mut v: Vec<_> = cases.iter().map(|(_, b)| Some(*b)).collect();
            v.push(Some(*default));
            v
        }
        Term::Yield { resume, .. } => vec![Some(*resume)],
    }
}

/// Blocks reachable from the entry, in discovery order.
pub(super) fn reachable_blocks(mf: &MirFunc) -> Vec<Idx<Block>> {
    reachable_from(mf, mf.entry)
}

/// Blocks reachable over terminators from `root` (a fn entry OR a lambda entry).
pub(super) fn reachable_from(mf: &MirFunc, root: Idx<Block>) -> Vec<Idx<Block>> {
    let mut reachable = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(b) = stack.pop() {
        if !seen.insert(b) {
            continue;
        }
        reachable.push(b);
        for s in succ(mf, b).into_iter().flatten() {
            stack.push(s);
        }
    }
    reachable
}
