//! CFG structuring analyses for the relooper: dominators, immediate
//! post-dominators (reversed CFG + a virtual exit), and natural loops. Every CFG
//! skull-mir produces for a tier-0 fn is reducible by construction, so the simple
//! iterative algorithms suffice at shader scale (tens of blocks).

use skull_mir::cfg::dense::{bi, bx, collect_loop_body, dominates, idoms, pred_list, rpo, succ_list};
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use skull_mir::mir::{Block, MirFunc, Term};

/// One natural loop: its header (back-edge target), the set of blocks in the
/// loop body, and its single exit (the block outside the loop that a body block
/// branches to). Tier-0 `while`/`for` loops are single-exit by construction.
#[derive(Clone)]
pub struct LoopRegion {
    pub header: Idx<Block>,
    pub body: HashSet<Idx<Block>>,
    pub exit: Idx<Block>,
}

pub struct Structure {
    /// Immediate post-dominator per block; `None` = the virtual exit (a diverging
    /// branch has no real join).
    ipdom: Vec<Option<Idx<Block>>>,
    pub loops: HashMap<Idx<Block>, LoopRegion>,
}

impl Structure {
    pub fn ipdom(&self, b: Idx<Block>) -> Option<Idx<Block>> {
        self.ipdom[bx(b)]
    }
}

pub fn analyze(f: &MirFunc) -> Structure {
    let n = f.blocks.len();
    let succ = succ_list(f);
    let preds = pred_list(n, &succ);

    let fwd_rpo = rpo(n, bx(f.entry), &succ);
    let idom = idoms(n, bx(f.entry), &preds, &fwd_rpo);

    let ipdom = post_idoms(f, n, &succ);
    let loops = natural_loops(f, n, &succ, &preds, &idom);

    Structure { ipdom, loops }
}

/// Immediate post-dominators: dominators on the reversed CFG rooted at a virtual
/// exit (node `n`) that every `Return`/`Unreachable` block flows into.
fn post_idoms(f: &MirFunc, n: usize, succ: &[Vec<usize>]) -> Vec<Option<Idx<Block>>> {
    let exit = n;
    let nn = n + 1;
    // Reverse graph: edge b->s becomes s->b; every terminal block gets an edge
    // from the virtual exit.
    let mut rev_adj = vec![Vec::new(); nn];
    let mut rev_preds = vec![Vec::new(); nn];
    for b in 0..n {
        for &s in &succ[b] {
            rev_adj[s].push(b);
        }
        // Reverse predecessors of b are its forward successors.
        rev_preds[b].extend_from_slice(&succ[b]);
        let terminal = matches!(f.blocks[bi(b)].term, Term::Return(_) | Term::Unreachable);
        if terminal {
            rev_adj[exit].push(b);
            rev_preds[b].push(exit);
        }
    }

    let rev_rpo = rpo(nn, exit, &rev_adj);
    let pidom = idoms(nn, exit, &rev_preds, &rev_rpo);

    let mut out = vec![None; n];
    for b in 0..n {
        out[b] = match pidom[b] {
            Some(x) if x != exit => Some(bi(x)),
            _ => None,
        };
    }
    out
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
        let Some(&test) = candidates.iter().find(|&&c| candidates.iter().all(|&c2| dominates(idom, c, c2))) else {
            continue; // no single condition test; emit rejects via the walker
        };
        let exits: Vec<usize> = succ[test].iter().copied().filter(|s| !body.contains(s)).collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use comfy_la_arena::RawIdx;
    use skull_mir::mir::{Inst, Lit};
    use ustr::Ustr;

    /// Build a MirFunc with `n` blocks (0 = entry) and the given terminators.
    /// Branch conds are a single shared dummy bool const.
    fn mk(n: usize, terms: Vec<Term>) -> MirFunc {
        let mut f = MirFunc::new(Ustr::from("t"));
        for _ in 1..n {
            f.new_block();
        }
        let _dummy = f.insts.alloc(Inst::Const(Lit::Bool(true)));
        for (i, t) in terms.into_iter().enumerate() {
            f.blocks[bi(i)].term = t;
        }
        f
    }

    fn cond() -> Idx<Inst> {
        Idx::from_raw(RawIdx::from_u32(0))
    }

    #[test]
    fn diamond() {
        // 0 -> {1, 2} -> 3(return)
        let f = mk(
            4,
            vec![
                Term::Branch { cond: cond(), then_bb: bi(1), else_bb: bi(2) },
                Term::Goto(bi(3)),
                Term::Goto(bi(3)),
                Term::Return(vec![]),
            ],
        );
        let s = analyze(&f);
        assert_eq!(s.ipdom(bi(0)), Some(bi(3)));
        assert_eq!(s.ipdom(bi(1)), Some(bi(3)));
        assert_eq!(s.ipdom(bi(2)), Some(bi(3)));
        assert_eq!(s.ipdom(bi(3)), None); // virtual exit
        assert!(s.loops.is_empty());
    }

    #[test]
    fn diverged_arm() {
        // 0 -> {1(return), 2}; 2 -> 3(return). ipdom(0) collapses to virtual exit.
        let f = mk(
            4,
            vec![
                Term::Branch { cond: cond(), then_bb: bi(1), else_bb: bi(2) },
                Term::Return(vec![]),
                Term::Goto(bi(3)),
                Term::Return(vec![]),
            ],
        );
        let s = analyze(&f);
        assert_eq!(s.ipdom(bi(0)), None);
        assert_eq!(s.ipdom(bi(2)), Some(bi(3)));
        assert!(s.loops.is_empty());
    }

    #[test]
    fn nested_loop() {
        // 0 -> 1(outer header) -> {2(inner header), 6(exit,return)}
        // 2 -> {3, 5}; 3 -> 4; 4 -> 2(inner back edge); 5 -> 1(outer back edge)
        let f = mk(
            7,
            vec![
                Term::Goto(bi(1)),
                Term::Branch { cond: cond(), then_bb: bi(2), else_bb: bi(6) },
                Term::Branch { cond: cond(), then_bb: bi(3), else_bb: bi(5) },
                Term::Goto(bi(4)),
                Term::Goto(bi(2)),
                Term::Goto(bi(1)),
                Term::Return(vec![]),
            ],
        );
        let s = analyze(&f);
        let outer = s.loops.get(&bi(1)).expect("outer loop");
        assert_eq!(outer.exit, bi(6));
        assert!(outer.body.contains(&bi(1)) && outer.body.contains(&bi(5)) && outer.body.contains(&bi(2)));
        assert!(!outer.body.contains(&bi(6)));
        let inner = s.loops.get(&bi(2)).expect("inner loop");
        assert_eq!(inner.exit, bi(5));
        assert!(inner.body.contains(&bi(2)) && inner.body.contains(&bi(4)));
        assert!(!inner.body.contains(&bi(1)) && !inner.body.contains(&bi(5)));
    }
}
