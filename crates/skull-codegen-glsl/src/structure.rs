//! CFG structuring analyses for the relooper: dominators, immediate
//! post-dominators (reversed CFG + a virtual exit), and natural loops. Every CFG
//! skull-mir produces for a tier-0 fn is reducible by construction, so the simple
//! iterative algorithms suffice at shader scale (tens of blocks).

use comfy_la_arena::Idx;
use rustc_hash::FxHashMap as HashMap;
use skull_codegen_shader::structure::{LoopRegion, natural_loops};
use skull_mir::cfg::dense::{bi, bx, idoms, pred_list, rpo, succ_list};
use skull_mir::mir::{Block, MirFunc, Term};

pub(crate) struct Structure {
    /// Immediate post-dominator per block; `None` = the virtual exit (a diverging
    /// branch has no real join).
    ipdom: Vec<Option<Idx<Block>>>,
    pub(crate) loops: HashMap<Idx<Block>, LoopRegion>,
}

impl Structure {
    pub(crate) fn ipdom(&self, b: Idx<Block>) -> Option<Idx<Block>> {
        self.ipdom[bx(b)]
    }
}

pub(crate) fn analyze(f: &MirFunc) -> Structure {
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
/// exit (node `n`) that every `Return` block flows into. `Unreachable` blocks do
/// NOT connect to the exit: they mark control the compiler proved never runs (an
/// exhaustive-`match` default, code after a diverging call), so counting them as
/// an exit path would defeat post-dominance of the REAL join -- an exhaustive
/// boolean match's `if/else` chain would then double-walk its join block and
/// reject as unstructured control flow. (Same rule as the JS relooper.)
fn post_idoms(f: &MirFunc, n: usize, succ: &[Vec<usize>]) -> Vec<Option<Idx<Block>>> {
    let exit = n;
    let nn = n + 1;
    // Reverse graph: edge b->s becomes s->b; every `Return` block gets an edge
    // from the virtual exit.
    let mut rev_adj = vec![Vec::new(); nn];
    let mut rev_preds = vec![Vec::new(); nn];
    for b in 0..n {
        for &s in &succ[b] {
            rev_adj[s].push(b);
        }
        // Reverse predecessors of b are its forward successors.
        rev_preds[b].extend_from_slice(&succ[b]);
        if matches!(f.blocks[bi(b)].term, Term::Return(_)) {
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
                Term::Branch {
                    cond: cond(),
                    then_bb: bi(1),
                    else_bb: bi(2),
                },
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
                Term::Branch {
                    cond: cond(),
                    then_bb: bi(1),
                    else_bb: bi(2),
                },
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
    fn unreachable_arm_is_not_an_exit_path() {
        // An exhaustive `match` as an expression: both arms store one result
        // cell and jump to a real join, while the dead default arm is a
        // REACHABLE `Unreachable` block (forward shape: 0 -> {1, 2};
        // 2 -> {3, 4-unreachable}; 1 and 3 -> 5; 5 returns). The dead arm must
        // not count as an exit path: the outer branch keeps the real join, and
        // the last test's ipdom is its LIVE arm -- the walker then skips that
        // provably-redundant test and falls through to the arm (sound because
        // bone proved the default unreachable, so the outer `else` already
        // implies the last condition). Counting the dead arm instead collapses
        // both branches to the virtual exit, both arms absorb the continuation,
        // and the join double-walks into `unstructured control flow`. (The JS
        // T6 rule; `diverged_arm` above is the control: a `Return` arm still
        // collapses.)
        let f = mk(
            6,
            vec![
                Term::Branch {
                    cond: cond(),
                    then_bb: bi(1),
                    else_bb: bi(2),
                },
                Term::Goto(bi(5)),
                Term::Branch {
                    cond: cond(),
                    then_bb: bi(3),
                    else_bb: bi(4),
                },
                Term::Goto(bi(5)),
                Term::Unreachable,
                Term::Return(vec![]),
            ],
        );
        let s = analyze(&f);
        assert_eq!(s.ipdom(bi(0)), Some(bi(5)));
        assert_eq!(s.ipdom(bi(1)), Some(bi(5)));
        assert_eq!(s.ipdom(bi(2)), Some(bi(3)));
        assert_eq!(s.ipdom(bi(3)), Some(bi(5)));
        assert_eq!(s.ipdom(bi(5)), None); // virtual exit
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
                Term::Branch {
                    cond: cond(),
                    then_bb: bi(2),
                    else_bb: bi(6),
                },
                Term::Branch {
                    cond: cond(),
                    then_bb: bi(3),
                    else_bb: bi(5),
                },
                Term::Goto(bi(4)),
                Term::Goto(bi(2)),
                Term::Goto(bi(1)),
                Term::Return(vec![]),
            ],
        );
        let s = analyze(&f);
        let outer = s.loops.get(&bi(1)).expect("outer loop");
        assert_eq!(outer.exit, bi(6));
        assert!(
            outer.body.contains(&bi(1))
                && outer.body.contains(&bi(5))
                && outer.body.contains(&bi(2))
        );
        assert!(!outer.body.contains(&bi(6)));
        let inner = s.loops.get(&bi(2)).expect("inner loop");
        assert_eq!(inner.exit, bi(5));
        assert!(inner.body.contains(&bi(2)) && inner.body.contains(&bi(4)));
        assert!(!inner.body.contains(&bi(1)) && !inner.body.contains(&bi(5)));
    }
}
