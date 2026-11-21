use super::*;

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

/// A condition block without writes or calls can keep its loads in the
/// condition, so a long branch chain can emit as a flat JS else-if ladder.
pub(super) fn fold_branch_loads(
    func: &MirFunc,
    blocks: &[Idx<Block>],
    uses: &skull_mir::sinking::ScopeUses,
    inlinable: &mut [bool],
) {
    for &block in blocks {
        let body = &func.blocks[block];
        if matches!(body.term, Term::Branch { .. })
            && body.insts.iter().all(|&id| {
                let inst = &func.insts[id];
                matches!(inst, Inst::Load(_)) || skull_mir::sinking::always_inlinable(inst)
            })
        {
            for &id in &body.insts {
                let i = raw(id) as usize;
                if matches!(func.insts[id], Inst::Load(_))
                    && uses.count[i] == 1
                    && uses.block[i] == raw(block)
                {
                    inlinable[i] = true;
                }
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
pub(super) fn cap_inline_depth(mf: &MirFunc, blocks: &[Idx<Block>], inl: &mut [bool]) {
    let mut depth = vec![0u32; mf.insts.len()];
    for &b in blocks {
        for &id in &mf.blocks[b].insts {
            let k = raw(id) as usize;
            if !inl[k] {
                continue;
            }
            let mut d = 0;
            mf.insts[id].uses_each(&mut |u| {
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

/// Value temps whose use crosses their defining block: the trampoline puts blocks in sibling switch
/// cases, so these are hoist-declared ([`Emitter::hoisted_temps`]).
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
            mf.insts[iid].uses_each(&mut check);
        }
        mf.blocks[b].term.uses_each(&mut check);
    }
    cross
}
