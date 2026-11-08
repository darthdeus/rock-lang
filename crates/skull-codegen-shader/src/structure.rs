//! Natural-loop discovery shared by the shader relooper. Every CFG skull-mir
//! produces for a tier-0 fn is reducible by construction, so the simple
//! iterative algorithm suffices at shader scale (tens of blocks).

use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use skull_mir::cfg::dense::{bi, bx, collect_loop_body, dominates};
use skull_mir::mir::{Block, MirFunc};

/// One natural loop: its header (back-edge target), the set of blocks in the
/// loop body, and its single exit (the block outside the loop that a body block
/// branches to). Tier-0 `while`/`for` loops are single-exit by construction.
#[derive(Clone)]
pub struct LoopRegion {
    pub header: Idx<Block>,
    pub body: HashSet<Idx<Block>>,
    pub exit: Idx<Block>,
}

/// The natural loop body of back edge `latch -> header`: `header` plus every node
/// that reaches `latch` without passing through `header`. Shared by both shader
/// analysis consumers; each target's post-dominator pass stays its own.
pub fn natural_loops(
    f: &MirFunc,
    n: usize,
    succ: &[Vec<usize>],
    preds: &[Vec<usize>],
    idom: &[Option<usize>],
) -> HashMap<Idx<Block>, LoopRegion> {
    // Back edges grouped by header, keeping each latch (back-edge source).
    let mut latches: HashMap<usize, Vec<usize>> = HashMap::default();
    for latch in 0..n {
        if idom[latch].is_none() && latch != bx(f.entry) {
            continue; // unreachable
        }
        for &h in &succ[latch] {
            if dominates(idom, h, latch) {
                latches.entry(h).or_default().push(latch);
            }
        }
    }

    let mut out = HashMap::default();
    for (h, hlatches) in latches {
        let mut body: HashSet<usize> = HashSet::default();
        for &l in &hlatches {
            collect_loop_body(h, l, preds, &mut body);
        }
        // The loop's exit is the out-of-loop successor of the condition test: the
        // shallowest body block that dominates every latch and leaves the loop.
        // (Early `return`/`break` blocks branch out too, but don't dominate the
        // latch or aren't the shallowest, so they are not mistaken for the exit.)
        let mut candidates: Vec<usize> = body
            .iter()
            .copied()
            .filter(|&b| {
                succ[b].iter().any(|s| !body.contains(s))
                    && hlatches.iter().all(|&l| dominates(idom, b, l))
            })
            .collect();
        candidates.sort_by_key(|&b| b);
        let Some(&test) = candidates
            .iter()
            .find(|&&c| candidates.iter().all(|&c2| dominates(idom, c, c2)))
        else {
            continue; // no single condition test; emit rejects via the walker
        };
        let exits: Vec<usize> = succ[test]
            .iter()
            .copied()
            .filter(|s| !body.contains(s))
            .collect();
        if exits.len() != 1 {
            continue;
        }
        out.insert(
            bi(h),
            LoopRegion {
                header: bi(h),
                body: body.iter().map(|&x| bi(x)).collect(),
                exit: bi(exits[0]),
            },
        );
    }
    out
}
