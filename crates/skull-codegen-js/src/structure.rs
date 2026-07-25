//! CFG structuring analyses for the JS relooper: dominators, immediate
//! post-dominators (reversed CFG + a virtual exit), and natural loops. A direct
//! port of `skull-codegen-glsl/src/structure.rs` -- both backends need
//! structured (goto-free) output and see the same by-construction reducible CFGs
//! skull-mir lowers for a tier-0 fn. Simple iterative algorithms at
//! test-fn scale.

use comfy_la_arena::Idx;
use skull_mir::cfg::dense::{bi, bx, collect_loop_body, dominates, idoms, pred_list, rpo, succ_list};
use skull_mir::mir::{Block, MirFunc, Term};
use rustc_hash::FxHashSet;
use std::collections::{HashMap, HashSet};

/// One natural loop: its header (back-edge target), the set of body blocks, and
/// its single exit (the out-of-loop block a body block branches to). Tier-0
/// `while`/`for` loops are single-exit by construction.
#[derive(Clone)]
pub(crate) struct LoopRegion {
    pub(crate) header: Idx<Block>,
    pub(crate) body: HashSet<Idx<Block>>,
    pub(crate) exit: Idx<Block>,
    /// The header-condition test block, when this is a `while cond`/`for` shape:
    /// a single body block that dominates every latch and has a lone out-of-loop
    /// successor, so the loop emits `while (true) { <header..test>; if (!cond)
    /// break; <body> }`. `None` for a `loop {}` whose exits are inline `break`s --
    /// no test dominates the back-edges (e.g. a `continue` precedes the `break`),
    /// so it emits as a bare `while (true) { <body> }` with the break/continue
    /// falling out of the body's own branch structure.
    pub(crate) test: Option<Idx<Block>>,
}

pub(crate) struct Structure {
    ipdom: Vec<Option<Idx<Block>>>,
    pub(crate) loops: HashMap<Idx<Block>, LoopRegion>,
}

impl Structure {
    pub(crate) fn ipdom(&self, b: Idx<Block>) -> Option<Idx<Block>> {
        self.ipdom[bx(b)]
    }

    /// A no-analysis placeholder for the trampoline mode, whose per-case
    /// emission never consults ipdoms or loops (the post-coroutine-transform
    /// CFG is irreducible by construction, so the relooper analyses don't apply).
    pub(crate) fn empty(n: usize) -> Structure {
        Structure { ipdom: vec![None; n], loops: HashMap::new() }
    }
}

pub(crate) fn analyze(f: &MirFunc) -> Structure {
    let n = f.blocks.len();
    let succ = succ_list(f);
    let preds = pred_list(n, &succ);

    // Forward dominators are rooted at a synthetic super-entry (node `n`) edging
    // to the fn entry. `idom[n]` is `None` (the super-root), which terminates the
    // `dominates` chain-walk. Closure bodies are separate `MirFunc`s post-
    // `convert_closures`, so there are no in-arena lambda sub-CFGs to seed.
    let sup = n;
    let mut succ_aug = succ.clone();
    succ_aug.push(vec![bx(f.entry)]);
    let preds_aug = pred_list(n + 1, &succ_aug);
    let fwd_rpo = rpo(n + 1, sup, &succ_aug);
    let idom = idoms(n + 1, sup, &preds_aug, &fwd_rpo);

    let ipdom = post_idoms(f, n, &succ);
    let loops = natural_loops(f, n, &succ, &preds, &idom);

    Structure { ipdom, loops }
}

/// Immediate post-dominators: dominators on the reversed CFG rooted at a virtual
/// exit (node `n`) that every `Return` block flows into. `Unreachable` blocks do
/// NOT connect to the exit: they mark control the compiler proved never runs (an
/// exhaustive-`match` default, code after a diverging call), so counting them as
/// an exit path would defeat post-dominance of the REAL join -- an exhaustive
/// enum match's `if/else-if` chain would then double-walk its return block and
/// reject as a shared branch tail.
fn post_idoms(f: &MirFunc, n: usize, succ: &[Vec<usize>]) -> Vec<Option<Idx<Block>>> {
    let exit = n;
    let nn = n + 1;
    let mut rev_adj = vec![Vec::new(); nn];
    let mut rev_preds = vec![Vec::new(); nn];
    for b in 0..n {
        for &s in &succ[b] {
            rev_adj[s].push(b);
        }
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

fn natural_loops(
    f: &MirFunc,
    n: usize,
    succ: &[Vec<usize>],
    preds: &[Vec<usize>],
    idom: &[Option<usize>],
) -> HashMap<Idx<Block>, LoopRegion> {
    // Back edges grouped by header, keeping each latch (back-edge source).
    let mut latches: HashMap<usize, Vec<usize>> = HashMap::new();
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

    let mut out = HashMap::new();
    for (h, hlatches) in latches {
        let mut body: FxHashSet<usize> = FxHashSet::default();
        for &l in &hlatches {
            collect_loop_body(h, l, preds, &mut body);
        }
        // The loop's exit is the out-of-loop successor of the condition test: the
        // shallowest body block that dominates every latch and leaves the loop.
        let mut candidates: Vec<usize> = body
            .iter()
            .copied()
            .filter(|&b| {
                succ[b].iter().any(|s| !body.contains(s))
                    && hlatches.iter().all(|&l| dominates(idom, b, l))
            })
            .collect();
        candidates.sort_by_key(|&b| b);
        // A `while cond`/`for` shape: a single body block dominating every latch,
        // with a lone out-of-loop successor, is the header-condition test. This is
        // the exact registration the tier-0 loops already used -- preserved as-is.
        let single_test = candidates
            .iter()
            .copied()
            .find(|&c| candidates.iter().all(|&c2| dominates(idom, c, c2)))
            .and_then(|test| {
                let mut exits = succ[test].iter().copied().filter(|s| !body.contains(s));
                let e = exits.next()?;
                exits.next().is_none().then_some((test, e))
            });
        let region = if let Some((test, exit)) = single_test {
            LoopRegion {
                header: bi(h),
                body: body.iter().map(|&x| bi(x)).collect(),
                exit: bi(exit),
                test: Some(bi(test)),
            }
        } else {
            // No dominating test (a `loop {}` whose `break` follows a `continue`):
            // a headerless `while (true)`. Structurable only if every out-of-loop
            // edge across the body targets ONE exit block; a multi-exit loop is
            // dropped so the walker rejects cleanly.
            let mut exit_targets: Vec<usize> = body
                .iter()
                .flat_map(|&b| succ[b].iter().copied().filter(|s| !body.contains(s)))
                .collect();
            exit_targets.sort_unstable();
            exit_targets.dedup();
            if exit_targets.len() != 1 {
                continue;
            }
            LoopRegion {
                header: bi(h),
                body: body.iter().map(|&x| bi(x)).collect(),
                exit: bi(exit_targets[0]),
                test: None,
            }
        };
        out.insert(bi(h), region);
    }
    out
}

/// The natural loop body of back edge `latch -> header`: `header` plus every node
/// that reaches `latch` without passing through `header`.
#[cfg(test)]
mod tests {
    use super::*;
    use comfy_la_arena::RawIdx;
    use skull_mir::mir::{Inst, Lit};
    use ustr::Ustr;

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
        assert_eq!(s.ipdom(bi(3)), None);
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
        assert!(outer.body.contains(&bi(5)) && !outer.body.contains(&bi(6)));
        let inner = s.loops.get(&bi(2)).expect("inner loop");
        assert_eq!(inner.exit, bi(5));
        assert!(inner.body.contains(&bi(4)) && !inner.body.contains(&bi(1)));
    }
}
