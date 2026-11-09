//! Body analyses: operand walks, inlinability, cross-block temps and reachability.

use super::*;

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
/// -- those materialize at their program position. JS also folds the
/// environment record of a closure.
pub(super) fn always_inlinable(inst: &Inst) -> bool {
    matches!(inst, Inst::MakeEnv { .. }) || skull_mir::sinking::always_inlinable(inst)
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
    inline_loads_before_branch(mf, blocks, &use_count, &use_block, &mut inl);
    skull_mir::sinking::preserve_effect_order(mf, blocks, &mut inl);
    cap_inline_depth(mf, blocks, &mut inl);
    (use_count, inl)
}

/// Fold the cell reads of an `else if` link into its condition. A link holding
/// only reads and pure values has no statement of its own once they fold, so it
/// can follow `else` directly; nothing in it can write the cell it reads.
fn inline_loads_before_branch(
    mf: &MirFunc,
    blocks: &[Idx<Block>],
    use_count: &[u32],
    use_block: &[u32],
    inl: &mut [bool],
) {
    let mut else_arms = vec![false; mf.blocks.len()];
    for &b in blocks {
        if let Term::Branch { else_bb, .. } = mf.blocks[b].term {
            let mut arm = else_bb;
            // Bounded by the block count: a cycle of empty blocks must not hang.
            for _ in 0..mf.blocks.len() {
                else_arms[raw(arm) as usize] = true;
                match mf.blocks[arm].term {
                    Term::Goto(next) if mf.blocks[arm].insts.is_empty() => arm = next,
                    _ => break,
                }
            }
        }
    }
    for &b in blocks {
        let insts = &mf.blocks[b].insts;
        if !else_arms[raw(b) as usize]
            || !matches!(mf.blocks[b].term, Term::Branch { .. })
            || !insts
                .iter()
                .all(|&id| matches!(mf.insts[id], Inst::Load(_)) || always_inlinable(&mf.insts[id]))
        {
            continue;
        }
        for &id in insts {
            let k = raw(id) as usize;
            if matches!(mf.insts[id], Inst::Load(_)) && use_count[k] == 1 && use_block[k] == raw(b)
            {
                inl[k] = true;
            }
        }
    }
}

/// The deepest expression a chain of inlinable values may fold into. Node's
/// parser overflows its stack near 1800 nested parentheses; the cap sits far
/// below that and costs one temp per 64 levels, which only long `a + b + ...`
/// chains ever reach.
const MAX_INLINE_DEPTH: u32 = 64;

/// Materialize a value wherever folding it would nest deeper than
/// [`MAX_INLINE_DEPTH`]; its consumers then start over at depth one.
fn cap_inline_depth(mf: &MirFunc, blocks: &[Idx<Block>], inl: &mut [bool]) {
    let mut depth = vec![0u32; mf.insts.len()];
    for &b in blocks {
        for &id in &mf.blocks[b].insts {
            let k = raw(id) as usize;
            if !inl[k] {
                continue;
            }
            let mut d = 0;
            for_each_operand(&mf.insts[id], &mut |u| {
                let j = raw(u) as usize;
                if inl[j] {
                    d = d.max(depth[j]);
                }
            });
            if d >= MAX_INLINE_DEPTH {
                inl[k] = false;
            } else {
                depth[k] = d + 1;
            }
        }
    }
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
        skull_mir::cfg::successors(&mf.blocks[b].term, |s| stack.push(s));
    }
    reachable
}
